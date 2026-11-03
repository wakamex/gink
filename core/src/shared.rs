//! What the core's threads share: the database, the event queue, and each account's status.

use std::collections::{BTreeMap, VecDeque};
use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::db::Db;

pub enum Event {
    Account(i64),
    /// The code to show while an account waits to be linked, with when it was made (ms since 1970).
    SignInCode { account: i64, link: String, modules: Vec<u8>, size: i32, made_ms: i64 },
    Conversations,
    Message { account: i64, conversation: i64, message: i64 },
    /// Many of a conversation's messages changed or went (some expired, a group change was noted): read them again.
    ConversationChanged { account: i64, conversation: i64 },
    /// Someone started or stopped typing in a conversation.
    Typing { account: i64, conversation: i64, name: String, started: bool },
    /// A newer build has been put in place of this one's files: the program should start again.
    Updated,
    /// One message changed: its status, reactions, text, files or pending changes, or it is gone.
    MessageChanged { account: i64, conversation: i64, message: i64 },
    /// What the conversation list shows of a conversation changed (its unread count), and none of
    /// its messages as they are shown.
    Listed { account: i64, conversation: i64 },
}

/// What a backend is told to do.
pub enum Command {
    /// Send the messages that wait in the database.
    Flush,
    MarkRead { conversation: i64 },
    /// Set the disappearing timer of a conversation with one person, in seconds; 0 turns it off.
    SetTimer { conversation: i64, seconds: u32 },
    /// Accept a conversation that was held apart as a request.
    Accept { conversation: i64 },
    /// Remove a member, by key, from a group the account administers.
    RemoveMember { conversation: i64, key: String },
    Typing { key: String, kind: i64, started: bool },
    ResetSession { key: String },
}

/// What is done to a message already in a conversation.
pub enum Change {
    /// The emoji the account gives it; "" takes the account's away.
    React(String),
    Edit(String),
    Delete,
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum State {
    SigningIn = 0,
    Connecting = 1,
    CatchingUp = 2,
    Online = 3,
    Offline = 4,
}

#[derive(Clone)]
pub struct Status {
    pub protocol: &'static str,
    pub label: String,
    pub state: State,
    pub last_received_ms: i64,
    pub undecryptable: i32,
    pub error: String,
}

/// The app's wake function and its argument.
pub struct Wake(pub Option<unsafe extern "C" fn(*mut c_void)>, pub *mut c_void);
// The app promises its wake function may be called from any thread.
unsafe impl Send for Wake {}
unsafe impl Sync for Wake {}

pub struct Shared {
    pub db: Db,
    pub dir: PathBuf,
    pub key: [u8; 32],
    pub events: Mutex<VecDeque<Event>>,
    pub statuses: Mutex<BTreeMap<i64, Status>>,
    pub wake: Wake,
    /// Told when an attachment waits to be fetched.
    pub fetch: tokio::sync::Notify,
    /// Told when a conversation may have a picture to fetch.
    pub pictures: tokio::sync::Notify,
    /// Whose picture to ask about again, as (key, kind): who just wrote, and what was just opened.
    pub pictures_due: Mutex<std::collections::BTreeSet<(String, i64)>>,
}

impl Shared {
    pub fn push(&self, event: Event) {
        let was_empty = {
            let mut events = self.events.lock().unwrap();
            // (a newer code replaces one still waiting: only the newest can be scanned)
            if let Event::SignInCode { account, .. } = &event {
                events.retain(|e| !matches!(e, Event::SignInCode { account: a, .. } if a == account));
            }
            events.push_back(event);
            events.len() == 1
        };
        if was_empty && let Some(wake) = self.wake.0 {
            unsafe { wake(self.wake.1) };
        }
    }

    /// Adds a line to arrivals.txt in the data folder, with the time: when the account began to
    /// connect ("connecting"), had caught up ("online"), or stored a message from someone else
    /// ("message" and when it was sent). It holds times alone, for comparing how soon messages arrive with another
    /// program on the same machine; tools/arrival-lag reads it.
    pub fn arrival(&self, what: &str) {
        use std::io::Write;
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(self.dir.join("arrivals.txt")) {
            let _ = writeln!(file, "{} {what}", now_ms());
        }
    }

    /// Changes an account's status and tells the app.
    pub fn update(&self, account: i64, change: impl FnOnce(&mut Status)) {
        if let Some(status) = self.statuses.lock().unwrap().get_mut(&account) {
            let (state, error, undecryptable) = (status.state as i32, status.error.clone(), status.undecryptable);
            change(status);
            // (going online or leaving it, a new error and a message that could not be decrypted go
            // to the log; the tries at connecting while offline, every few seconds, do not)
            let online = State::Online as i32;
            if (state != status.state as i32 && (state == online || status.state as i32 == online)) || (error != status.error && !status.error.is_empty()) || undecryptable != status.undecryptable {
                crate::log::line(&format!("account {account}: state {:?}, {} undecryptable, error: {}", status.state, status.undecryptable, if status.error.is_empty() { "none" } else { &status.error }));
            }
        }
        self.push(Event::Account(account));
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}
