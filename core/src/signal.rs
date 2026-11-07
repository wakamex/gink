//! The Signal backend: one thread per account that links the device, receives, and sends.

use std::sync::Arc;
use std::time::Duration;

use futures::channel::oneshot;
use futures::{StreamExt, future, pin_mut};
use signal_client::Manager;
use signal_client::libsignal_service::configuration::SignalServers;
use signal_client::libsignal_service::content::{Content, ContentBody, DataMessage, GroupContextV2};
use signal_client::libsignal_service::prelude::{ProtobufMessage, Uuid};
use signal_client::libsignal_service::protocol::{DeviceId, IdentityKeyStore, ProtocolAddress, ServiceId};
use signal_client::libsignal_service::sender::AttachmentSpec;
use signal_client::libsignal_service::zkgroup::groups::{GroupMasterKey, GroupSecretParams};
use signal_client::manager::Registered;
use signal_client::model::identity::OnNewIdentity;
use signal_client::model::messages::Received;
use signal_client::proto::body_range::AssociatedValue;
use signal_client::proto::data_message::{Delete, Quote, Reaction};
use signal_client::proto::receipt_message::Type as ReceiptType;
use signal_client::proto::sync_message::message_request_response::Type as RequestResponse;
use signal_client::proto::sync_message::{Content as SyncContent, Sent};
use signal_client::proto::typing_message::Action;
use signal_client::proto::attachment_pointer::Flags as AttachmentFlags;
use signal_client::proto::{AttachmentPointer, BodyRange, EditMessage, ReceiptMessage, SyncMessage, TypingMessage};
use signal_client::store::{ContentsStore, StateStore, Store, Thread};
use signal_store::SqliteStore;
use tokio::sync::mpsc;

use crate::db::{NewAttachment, NewMessage, Target, Waiting};
use crate::shared::{Change, Command, Event, Shared, State, now_ms};

const DIRECT: i64 = 0;
const GROUP: i64 = 1;
const RETRY: Duration = Duration::from_secs(5);
/// The most text a message carries in its body, in bytes; the rest travels as an attachment of this type.
const BODY_MAX: usize = 2048;
const LONG_TEXT: &str = "text/x-signal-plain";
/// How often at most the core starts a session with one person afresh by itself, so two clients
/// whose messages keep failing do not reset each other without end; and how long it waits after an
/// attempt that failed. The service limits how often a person's keys may be fetched, which every
/// new session needs, and refuses for ten minutes once that is exceeded.
const REPAIR_EVERY_MS: i64 = 3_600_000;
const REPAIR_RETRY_MS: i64 = 600_000;
/// How old a message may be and still be sent again to someone who could not decrypt it.
const RESEND_WITHIN_MS: i64 = 86_400_000;
const NOTE_TO_SELF: &str = "Note to Self";
const VIEW_ONCE: &str = "View-once media. Open it on your phone: gink does not show these.";
const SENT_MESSAGE: i64 = 0;
const SENT_EDIT: i64 = 1;
const TEXT: i64 = 0;
const UNDECRYPTABLE: i64 = 1;
const IDENTITY_CHANGED: i64 = 2;
/// Something that happened in a conversation and is not a message: a call, a member joining, a new timer.
const EVENT: i64 = 4;
/// Attachments up to this size are fetched as they arrive.
const FETCH_MAX: i64 = 32 << 20;
/// What a message replied to shows of a long text.
pub const QUOTE_MAX: usize = 200;
/// A message is tried this many times, this far apart, before it is left for the user to send again.
/// A person's or a group's picture is asked about at most this often, and not at all for this long after the service refuses.
const PICTURES_EVERY_MS: i64 = 60_000;
const PICTURES_REFUSED: Duration = Duration::from_secs(600);
const SEND_TRIES: i64 = 3;
const SEND_RETRY: Duration = Duration::from_secs(20);

type Client = Manager<SqliteStore, Registered>;

/// Starts the account's thread and returns the channel its commands go to.
pub fn spawn(shared: Arc<Shared>, account: i64, device_name: String) -> mpsc::UnboundedSender<Command> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::Builder::new()
        .name(format!("signal-{account}"))
        // The client's futures are megabytes large in unoptimized builds, more than a default stack holds.
        .stack_size(32 << 20)
        .spawn(move || {
            // The client's futures are not Send, so they run on this thread alone.
            let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
            tokio::task::LocalSet::new().block_on(&runtime, run(shared, account, device_name, rx));
        })
        .expect("a thread");
    tx
}

/// Runs sessions until the app closes the core, which drops the command channel.
async fn run(shared: Arc<Shared>, account: i64, device_name: String, mut commands: mpsc::UnboundedReceiver<Command>) {
    loop {
        let error = match Box::pin(session(&shared, account, &device_name, &mut commands)).await {
            Ok(()) => return,
            Err(error) => error,
        };
        shared.update(account, |s| (s.state, s.error) = (State::Offline, error));
        tokio::select! {
            _ = tokio::time::sleep(RETRY) => {}
            command = commands.recv() => if command.is_none() { return },  // (a command given while offline is dropped)
        }
    }
}

/// One connection: opens the account's store, links the device if it is not linked, then receives and
/// sends until the connection ends. Ok means the core is closing.
async fn session(shared: &Arc<Shared>, account: i64, device_name: &str, commands: &mut mpsc::UnboundedReceiver<Command>) -> Result<(), String> {
    let path = shared.dir.join(format!("account-{account}.db"));
    let store = SqliteStore::open_with_key(&path, &shared.key, OnNewIdentity::AskToSend).await.map_err(text)?;
    let mut client = if store.is_registered().await {
        let mut client = Manager::load_registered(store).await.map_err(text)?;
        // (contacts that never arrived, or could not be read whole, are asked for again)
        if !shared.db.contacts_read(account).await.map_err(text)? {
            client.request_contacts().await.map_err(text)?;
            crate::log::line(&format!("account {account}: asked the primary device for its contacts"));
        }
        client
    } else {
        Box::pin(link(shared, account, device_name, store)).await?
    };
    let label = client.registration_data().phone_number.to_string();
    shared.db.set_account_label(account, &label).await.map_err(text)?;
    // The account's conversation with itself is there from the start, under the name Signal gives it.
    let own = ServiceId::Aci(client.registration_data().service_ids.aci.into()).service_id_string();
    shared.db.set_self_key(account, &own).await.map_err(text)?;
    shared.db.conversation(account, &own, DIRECT, NOTE_TO_SELF).await.map_err(text)?;
    shared.update(account, |s| (s.state, s.label) = (State::Connecting, label));

    let stream = match client.receive_messages().await {
        Ok(stream) => stream,
        // The account's other device removed this one: forget the registration, so the next session
        // starts by linking again.
        Err(error) if error.is_unauthorized() => {
            let mut store = client.store().clone();
            store.clear_registration().await.map_err(text)?;
            return Err("this device was unlinked".into());
        }
        Err(error) => return Err(detail(error)),
    };
    // What arrives is decrypted by a task of its own and handed over here, and everything below that
    // sends holds the client's lock on its sessions while it does. Decrypting a message and sending
    // one each read a session, advance it and write it back; done by turns inside one task, a send
    // could come between the two halves of a decryption and have its change overwritten, after
    // which the other person can no longer decrypt.
    let sessions = client.sessions();
    let (received_tx, mut received_rx) = mpsc::unbounded_channel();
    let _receiving = AbortOnDrop(tokio::task::spawn_local(async move {
        pin_mut!(stream);
        while let Some(received) = stream.next().await {
            if received_tx.send(received).is_err() {
                break;
            }
        }
    }));
    shared.update(account, |s| (s.state, s.error) = (State::CatchingUp, String::new()));
    shared.arrival("connecting");
    conversations_from_store(shared, account, &client).await;
    // The service is told a message has arrived as soon as it is off the connection, before it is
    // opened. The client keeps each one it has opened until the core has dealt with it, so those
    // the program was ended in the middle of are still there, and are dealt with now.
    if shared.db.journal_started(account).await.map_err(text)? {
        let kept = client.store().kept_messages().await.map_err(text)?;
        if !kept.is_empty() {
            crate::log::line(&format!("account {account}: dealing with {} messages received before the program last ended", kept.len()));
        }
        // (what the account sent from another device comes back from the client's store as if
        // someone had written it under the account's own key, and is left: its sending device has
        // it, and the copy sent here arrives again from the service if it was never acknowledged)
        let own = client.registration_data().service_ids.aci.to_string();
        for content in kept.iter().filter(|c| c.metadata.sender.service_id_string() != own) {
            if let Err(error) = store_content(shared, account, &client, content).await {
                crate::log::line(&format!("account {account}: a message kept from before could not be dealt with: {error}"));
            }
        }
        // (all of them have been dealt with now, including any a lookup failed to take off)
        client.store().clone().clear_messages().await.map_err(text)?;
    } else {
        // (until now the client kept every message it ever received, dealt with or not)
        client.store().clone().clear_messages().await.map_err(text)?;
    }
    // The primary device says what the account tells other people; it answers like any other message.
    if shared.db.settings(account).await.map_err(text)?.is_none() {
        let _held = sessions.lock().await;
        if let Err(error) = client.request_configuration().await {
            shared.update(account, |s| s.error = text(error));
        }
    }

    // The account's shared records are read once per session, after catching up and beside the
    // receiving so that it is not held up. A device just linked waits for the primary device to
    // send the keys they need.
    let (caught_up, when_caught_up) = oneshot::channel::<()>();
    let mut caught_up = Some(caught_up);
    let _storage = AbortOnDrop(tokio::task::spawn_local({
        let (shared, mut client) = (shared.clone(), client.clone());
        async move {
            let _ = when_caught_up.await;
            while !read_storage(&shared, account, &mut client).await {
                tokio::time::sleep(RETRY).await;
            }
        }
    }));
    // Attachments are fetched beside the receiving too: those left from earlier, then each as it arrives.
    let _fetching = AbortOnDrop(tokio::task::spawn_local({
        let (shared, client) = (shared.clone(), client.clone());
        async move {
            loop {
                fetch_attachments(&shared, account, &client).await;
                shared.fetch.notified().await;
            }
        }
    }));
    // Pictures of people and groups are looked after beside the receiving as well: whoever writes
    // and whatever is opened is asked about then, and everyone once a day. When the service
    // refuses, it is left alone for a while.
    let _picturing = AbortOnDrop(tokio::task::spawn_local({
        let (shared, mut client) = (shared.clone(), client.clone());
        async move {
            loop {
                match fetch_avatars(&shared, account, &mut client).await {
                    Ok(Some(wait)) => tokio::select! {
                        _ = shared.pictures.notified() => {}
                        _ = tokio::time::sleep(wait) => {}
                    },
                    Ok(None) => tokio::select! {
                        _ = shared.pictures.notified() => {}
                        _ = tokio::time::sleep(Duration::from_secs(3600)) => {}
                    },
                    Err(()) => tokio::time::sleep(PICTURES_REFUSED).await,
                }
            }
        }
    }));
    let mut again = None;
    loop {
        // Messages whose disappearing timer has run out go first, then the wait is until the next one.
        let now = now_ms();
        for conversation in shared.db.remove_expired(account, now).await.map_err(text)? {
            shared.push(Event::ConversationChanged { account, conversation });
        }
        let next_expiry = match shared.db.next_expiry(account).await.map_err(text)? {
            Some(at) => Duration::from_millis((at - now).max(0) as u64),
            None => Duration::from_secs(3600),
        };
        tokio::select! {
            _ = tokio::time::sleep(next_expiry) => {}
            // What was written while offline is sent once the account is caught up, and a sending
            // that failed is tried again a little later.
            _ = tokio::time::sleep_until(again.unwrap_or_else(tokio::time::Instant::now)), if again.is_some() => {
                let _held = sessions.lock().await;
                let result = flush(shared, account, &mut client).await;
                again = result.is_err().then(|| tokio::time::Instant::now() + SEND_RETRY);
                shared.update(account, |s| s.error = result.err().unwrap_or_default());
            }
            received = received_rx.recv() => match received {
                None => return Err("the connection closed".into()),
                Some(Received::QueueEmpty) => {
                    shared.update(account, |s| s.state = State::Online);
                    shared.arrival("online");
                    caught_up.take().map(|tx| tx.send(()));
                    again = Some(tokio::time::Instant::now());
                }
                Some(Received::Contacts { whole }) => {
                    let held = client.store().contacts().await.map(|c| c.count()).unwrap_or(0);
                    crate::log::line(&format!("account {account}: contacts arrived, {}; {held} held", if whole { "read whole" } else { "not read whole" }));
                    if whole {
                        let _ = shared.db.set_contacts_read(account, now_ms()).await;
                    }
                    conversations_from_store(shared, account, &client).await
                },
                Some(Received::Content(content)) => {
                    // (a message that was dealt with is taken off the client's store; one that
                    // failed stays there, to be tried once more at the next start)
                    // (GINK_HOLD_RECEIVED=1 leaves every message where the client kept it, as
                    // if the program had been ended before dealing with it: for the live check
                    // of what the next start does with such messages)
                    if std::env::var_os("GINK_HOLD_RECEIVED").is_some() {
                        continue;
                    }
                    match store_content(shared, account, &client, &content).await {
                        Ok(()) => forget(&client, &content).await,
                        Err(error) => shared.update(account, |s| s.error = error),
                    }
                    shared.update(account, |s| s.last_received_ms = now_ms());
                }
                Some(Received::DecryptionError { sender, device, timestamp }) => {
                    shared.update(account, |s| (s.undecryptable, s.last_received_ms) = (s.undecryptable + 1, now_ms()));
                    let _held = sessions.lock().await;
                    let result = undecryptable(shared, account, &mut client, &sender, device, timestamp).await;
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                Some(Received::RetryRequest { sender, timestamp }) => {
                    let _held = sessions.lock().await;
                    let result = send_again(shared, account, &mut client, &sender, timestamp as i64).await;
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
            },
            command = commands.recv() => { let _held = sessions.lock().await; match command {
                None => return Ok(()),
                Some(Command::Flush) => {
                    let result = flush(shared, account, &mut client).await;
                    again = result.is_err().then(|| tokio::time::Instant::now() + SEND_RETRY);
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                Some(Command::SetTimer { conversation, seconds }) => {
                    let result = set_timer(shared, account, &mut client, conversation, seconds).await;
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                Some(Command::RemoveMember { conversation, key }) => {
                    let result = remove_member(shared, account, &mut client, conversation, &key).await;
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                Some(Command::Accept { conversation }) => {
                    let result = accept(shared, account, &mut client, conversation).await;
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                Some(Command::MarkRead { conversation }) => {
                    // (a conversation that is opened has its picture asked about again)
                    if let Ok(Some((_, key, kind))) = shared.db.conversation_key(conversation).await {
                        shared.pictures_due.lock().unwrap().insert((key, kind));
                        shared.pictures.notify_one();
                    }
                    let result = mark_read(shared, account, &mut client, conversation).await;
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                // (the user asked, so the limit on how often the core does it by itself does not apply)
                Some(Command::ResetSession { key }) => {
                    let result = match ServiceId::parse_from_service_id_string(&key) {
                        Some(person @ ServiceId::Aci(_)) => client.send_session_reset(&person, now_ms() as u64).await.map_err(text),
                        _ => Ok(()),
                    };
                    shared.update(account, |s| s.error = result.err().unwrap_or_default());
                }
                // (a typing notice that fails is not worth telling the user about)
                Some(Command::Typing { key, kind, started }) => {
                    let _ = send_typing(shared, account, &mut client, &key, kind, started).await;
                }
            } },
        }
    }
}

/// Links this device to an account: hands the app the link to show as a QR code and waits for the
/// account's primary device to accept it.
async fn link(shared: &Arc<Shared>, account: i64, device_name: &str, store: SqliteStore) -> Result<Client, String> {
    shared.update(account, |s| s.state = State::SigningIn);
    let (tx, rx) = oneshot::channel();
    let (client, _) = future::join(Manager::link_secondary_device(store, SignalServers::Production, device_name.to_owned(), tx), async {
        if let Ok(url) = rx.await {
            let link = url.to_string();
            if let Ok(code) = qrcode::QrCode::new(link.as_bytes()) {
                let modules = code.to_colors().iter().map(|c| (*c == qrcode::Color::Dark) as u8).collect();
                shared.push(Event::SignInCode { account, link, modules, size: code.width() as i32, made_ms: crate::shared::now_ms() });
            }
        }
    })
    .await;
    let mut client = client.map_err(text)?;
    // The primary device sends its contacts when asked; they arrive like any other message.
    client.request_contacts().await.map_err(text)?;
    Ok(client)
}

/// The best name the store has for a person: the name the account's owner gave the contact, else the
/// name the person gave their own profile, else their phone number. Empty when it has none of them.
/// A person's name: the one the account's own records gave, and failing that what the client can
/// find out.
async fn name_of(shared: &Arc<Shared>, account: i64, client: &Client, id: &ServiceId) -> String {
    let name = match shared.db.person_name(account, &id.service_id_string()).await {
        Ok(name) if !name.is_empty() => name,
        _ => person_name(client, id).await,
    };
    // (with no name, number or username known, as Signal Desktop calls them)
    if name.is_empty() { UNKNOWN_CONTACT.to_owned() } else { name }
}

const UNKNOWN_CONTACT: &str = "Unknown contact";

async fn person_name(client: &Client, id: &ServiceId) -> String {
    let store = client.store();
    let contact = store.contact_by_id(id).await.ok().flatten();
    if let Some(contact) = &contact
        && !contact.name.is_empty()
    {
        return contact.name.clone();
    }
    // (the profile comes from the store when it was fetched before, and from the service otherwise)
    if let ServiceId::Aci(aci) = id
        && let Ok(Some(key)) = store.profile_key(id).await
        && let Ok(profile) = client.clone().retrieve_profile_by_uuid(*aci, key).await
        && let Some(name) = profile.name
    {
        return name.to_string();
    }
    contact.and_then(|c| c.phone_number).map(|n| n.to_string()).unwrap_or_default()
}

/// Makes a conversation for every contact and group the store knows, and renames those whose
/// titles changed.
async fn conversations_from_store(shared: &Arc<Shared>, account: i64, client: &Client) {
    let mut changed = false;
    if let Ok(contacts) = client.store().contacts().await {
        for contact in contacts.flatten() {
            let id = ServiceId::Aci(contact.uuid.into());
            // (the account's own entry keeps the name of its notes, not the owner's)
            let title = if id == ServiceId::Aci(client.registration_data().service_ids.aci.into()) { NOTE_TO_SELF.to_owned() } else { name_of(shared, account, client, &id).await };
            changed |= matches!(shared.db.conversation(account, &id.service_id_string(), DIRECT, &title).await, Ok((_, true)));
        }
    }
    if let Ok(groups) = client.store().groups().await {
        for (master_key, group) in groups.flatten() {
            changed |= refresh_group(shared, account, client, &master_key, &group).await;
        }
    }
    if changed {
        shared.push(Event::Conversations);
    }
}

/// Takes a received message off the client's store, where it was kept until dealt with.
async fn forget(client: &Client, content: &Content) {
    if let Ok(thread) = Thread::try_from(content) {
        let _ = client.store().clone().delete_message(&thread, content.metadata.client_timestamp.timestamp_millis() as u64).await;
    }
}

/// Takes a group's title, members and timer from the client's kept state into the conversation,
/// and says in the conversation what became different: a new name, who joined, who left, a new
/// timer. Who made the change is not known here, so it is not said. A state older than the one
/// the conversation was last brought up to changes nothing. Returns whether the conversation is
/// new or was renamed.
async fn refresh_group(shared: &Arc<Shared>, account: i64, client: &Client, master_key: &[u8; 32], group: &signal_client::model::groups::Group) -> bool {
    let key = hex::encode(master_key);
    let before = shared.db.conversation_by_key(account, &key).await.ok().flatten();
    // (a state read a moment ago and brought here after a newer one has been applied is set aside:
    // the storage records and each arriving message are read by tasks of their own)
    if let Some((conversation, _)) = before
        && !shared.db.advance_revision(conversation, group.revision as i64).await.unwrap_or(true)
    {
        return false;
    }
    let Ok((conversation, new)) = shared.db.conversation(account, &key, GROUP, &group.title).await else { return false };
    if before.is_none() {
        let _ = shared.db.advance_revision(conversation, group.revision as i64).await;
    }
    let was: Vec<String> = match before {
        Some(_) => shared.db.members(conversation).await.unwrap_or_default().into_iter().map(|m| m.0).collect(),
        None => vec![],
    };
    let own = client.registration_data().service_ids.aci;
    // Its members are kept for naming them in a message; one never met is asked for a name once.
    let members: Vec<ServiceId> = group.members.iter().map(|m| ServiceId::Aci(m.aci)).filter(|id| *id != ServiceId::Aci(own.into())).collect();
    // (the group gives each member's profile key, which is what their name is read with)
    let mut store = client.store().clone();
    for member in &group.members {
        let _ = store.upsert_profile_key(&member.aci.into(), member.profile_key).await;
    }
    for member in &members {
        if shared.db.person_name(account, &member.service_id_string()).await.unwrap_or_default().is_empty() {
            let _ = shared.db.person(account, &member.service_id_string(), &person_name(client, member).await).await;
        }
    }
    let now: Vec<String> = members.iter().map(|m| m.service_id_string()).collect();
    let _ = shared.db.set_members(conversation, &now).await;
    let admin = group.members.iter().any(|m| Uuid::from(m.aci) == own && m.role == signal_client::libsignal_service::groups_v2::Role::Administrator);
    let admin_changed = shared.db.set_admin(conversation, admin).await.unwrap_or(false);
    let timer = group.disappearing_messages_timer.as_ref().map_or(0, |t| t.duration as i64);
    let timer_was = shared.db.timer_changed(conversation, timer).await.ok().flatten();
    // (a group seen for the first time has nothing to be different from)
    let Some((_, title_was)) = before else { return new };
    let mut said = vec![];
    if !title_was.is_empty() && title_was != group.title && !group.title.is_empty() {
        said.push(format!("The group is now called \"{}\".", group.title));
    }
    if !was.is_empty() {
        for (keys, others, what) in [(&now, &was, "joined the group"), (&was, &now, "is no longer in the group")] {
            for key in keys.iter().filter(|k| !others.contains(k)) {
                let name = shared.db.person_name(account, key).await.unwrap_or_default();
                said.push(format!("{} {what}.", if name.is_empty() { "Someone" } else { &name }));
            }
        }
    }
    if timer_was.is_some() {
        said.push(timer_text("Disappearing messages are", timer));
    }
    // (the window shows what an admin may do, so it reads the conversations again)
    if admin_changed {
        said.push(if admin { "You are now an admin of the group." } else { "You are no longer an admin of the group." }.to_owned());
    }
    for text in said {
        let _ = event(shared, account, conversation, &text, false).await;
    }
    new || admin_changed
}

/// "… now off." or "… now set to 1 week.", for a timer in seconds.
fn timer_text(start: &str, seconds: i64) -> String {
    let (n, unit) = match seconds {
        0 => return format!("{start} now off."),
        s if s % 604_800 == 0 => (s / 604_800, "week"),
        s if s % 86_400 == 0 => (s / 86_400, "day"),
        s if s % 3600 == 0 => (s / 3600, "hour"),
        s if s % 60 == 0 => (s / 60, "minute"),
        s => (s, "second"),
    };
    format!("{start} now set to {n} {unit}{}.", if n == 1 { "" } else { "s" })
}

/// Writes into a conversation something that happened there and is not a message. One that the
/// user should be told of (a call) counts as unread; the others start out read.
async fn event(shared: &Arc<Shared>, account: i64, conversation: i64, text: &str, unread: bool) -> Result<(), String> {
    // (a message is known by who sent it and when, so each of these is given a time of its own)
    static LAST: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);
    let now = now_ms();
    let at = LAST.fetch_max(now, std::sync::atomic::Ordering::Relaxed).max(now) + 1;
    LAST.store(at, std::sync::atomic::Ordering::Relaxed);
    let new = NewMessage { conversation, outgoing: false, sender: "", sent_ms: at, received_ms: now, text, kind: EVENT, quote: None, expire_s: 0, expire_start_ms: (!unread).then_some(now) };
    if let Some(message) = shared.db.add_message(&new).await.map_err(self::text)? {
        shared.push(Event::Message { account, conversation, message });
    }
    Ok(())
}

/// Ends a task when the session that started it ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Reads what the account keeps on the storage service for all its devices, when it changed since
/// it was last read: its groups, which become conversations, and what it tells other people. False
/// when this device has no keys for it yet.
async fn read_storage(shared: &Arc<Shared>, account: i64, client: &mut Client) -> bool {
    let known = shared.db.storage_version(account).await.ok().flatten().map(|v| v as u64);
    match client.sync_storage(known).await {
        Ok(None) => return false,
        Ok(Some(read)) if !read.changed => return true,
        Ok(Some(read)) => {
            if let Some(record) = read.account {
                let _ = shared.db.set_settings(account, record.read_receipts, record.typing_indicators).await;
            }
            // The names the account's owner knows people by go to everything that shows a name, and
            // to the conversations already held with them.
            for (id, name, known, given_by_owner) in &read.contacts {
                let key = ServiceId::Aci((*id).into()).service_id_string();
                let renamed = shared.db.record_name(account, &key, name, *given_by_owner).await.unwrap_or(false);
                // (someone the owner accepted on another device is accepted here)
                if *known {
                    let _ = shared.db.accept_person(account, &key).await;
                }
                if renamed && *id != client.registration_data().service_ids.aci {
                    let _ = shared.db.rename_conversation(account, &key, name).await;
                }
            }
            shared.push(Event::Conversations);
            let _ = shared.db.set_storage_version(account, read.version as i64).await;
        }
        // (it is tried again in the next session; the groups still appear as their messages arrive)
        Err(error) => shared.update(account, |s| s.error = format!("the account's groups could not be read: {error}")),
    }
    conversations_from_store(shared, account, client).await;
    true
}

/// Acts on what was received: stores a message that carries text (one sent to this account, or one
/// this account sent from another of its devices), and applies receipts and read state.
async fn store_content(shared: &Arc<Shared>, account: i64, client: &Client, content: &Content) -> Result<(), String> {
    let changed = |conversation: i64, message: i64| shared.push(Event::MessageChanged { account, conversation, message });
    // An edit carries the message as it now reads and names the one it replaces.
    let (message, sent, replaces) = match &content.body {
        ContentBody::DataMessage(message) => (message, None, None),
        ContentBody::EditMessage(EditMessage { target_sent_timestamp: Some(target), data_message: Some(message) }) => (message, None, Some(*target as i64)),
        ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Sent(sent @ Sent { message: Some(message), .. })), .. }) => (message, Some(sent), None),
        ContentBody::SynchronizeMessage(SyncMessage {
            content: Some(SyncContent::Sent(sent @ Sent { edit_message: Some(EditMessage { target_sent_timestamp: Some(target), data_message: Some(message) }), .. })),
            ..
        }) => (message, Some(sent), Some(*target as i64)),
        // The recipients' devices report a message delivered, and the recipients themselves report it read.
        ContentBody::ReceiptMessage(ReceiptMessage { r#type, timestamp }) => {
            let status = if *r#type == Some(ReceiptType::Delivery as i32) { 1 } else { 2 };
            let times: Vec<i64> = timestamp.iter().map(|t| *t as i64).collect();
            for (conversation, message) in shared.db.set_status(account, &times, status).await.map_err(text)? {
                changed(conversation, message);
            }
            return Ok(());
        }
        // Another of the account's devices read these.
        ContentBody::SynchronizeMessage(SyncMessage { read, .. }) if !read.is_empty() => {
            let mut conversations = vec![];
            for item in read {
                if let (Some(sender), Some(sent_ms)) = (&item.sender_aci, item.timestamp) {
                    conversations.extend(shared.db.mark_read_elsewhere(account, sender, sent_ms as i64, now_ms()).await.map_err(text)?);
                }
            }
            conversations.sort_unstable();
            conversations.dedup();
            // (what is read shows in the list's counts, not in the messages)
            for conversation in conversations {
                shared.push(Event::Listed { account, conversation });
            }
            return Ok(());
        }
        // The owner answered a request on another device; accepting is the answer taken up here.
        ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::MessageRequestResponse(response)), .. }) => {
            if let (Some(person), true) = (&response.thread_aci, response.r#type == Some(RequestResponse::Accept as i32)) {
                if shared.db.accept_person(account, person).await.map_err(text)? {
                    shared.push(Event::Conversations);
                }
            }
            return Ok(());
        }
        ContentBody::SynchronizeMessage(SyncMessage { content: Some(SyncContent::Configuration(c)), .. }) => {
            return shared.db.set_settings(account, c.read_receipts.unwrap_or(false), c.typing_indicators.unwrap_or(false)).await.map_err(text);
        }
        // A call cannot be taken here; the conversation says that one came, so that it is not
        // only the phone that knows.
        ContentBody::CallMessage(call) => {
            let (Some(offer), person) = (&call.offer, &content.metadata.sender) else { return Ok(()) };
            if person.service_id_string() == client.registration_data().service_ids.aci.to_string() {
                return Ok(());
            }
            let (key, name) = (person.service_id_string(), name_of(shared, account, client, person).await);
            shared.db.person(account, &key, &name).await.map_err(text)?;
            let (conversation, new) = shared.db.conversation(account, &key, DIRECT, &name).await.map_err(text)?;
            if new || shared.db.hold_if_unasked(conversation, &key).await.map_err(text)? {
                shared.push(Event::Conversations);
            }
            let kind = if offer.r#type == Some(1) { "a video call" } else { "a call" };
            let who = if name.is_empty() { "Someone" } else { &name };
            return event(shared, account, conversation, &format!("{who} started {kind}. gink cannot take calls: answer on your phone."), true).await;
        }
        ContentBody::TypingMessage(typing) => {
            let sender = &content.metadata.sender;
            let conversation = match &typing.group_id {
                Some(group) => {
                    let groups = shared.db.conversation_keys(account, GROUP).await.map_err(text)?;
                    groups.into_iter().find(|(_, key)| group_id(key).as_deref() == Some(group)).map(|(id, _)| id)
                }
                None => shared.db.conversation_keys(account, DIRECT).await.map_err(text)?.into_iter().find(|(_, key)| *key == sender.service_id_string()).map(|(id, _)| id),
            };
            if let Some(conversation) = conversation {
                let name = shared.db.person_name(account, &sender.service_id_string()).await.map_err(text)?;
                shared.push(Event::Typing { account, conversation, name, started: typing.action() == Action::Started });
            }
            return Ok(());
        }
        _ => return Ok(()),
    };
    let outgoing = sent.is_some();
    let own = client.registration_data().service_ids.aci.to_string();
    let sender = if outgoing { String::new() } else { content.metadata.sender.service_id_string() };
    let sent_ms = message.timestamp.map(|t| t as i64).unwrap_or_else(|| content.metadata.client_timestamp.timestamp_millis());
    // A reaction and a deletion name a message by who wrote it and when; only its writer deletes it.
    if let Some(Reaction { emoji: Some(emoji), remove, target_author_aci, target_author_aci_binary, target_sent_timestamp: Some(at) }) = &message.reaction {
        let author = author(target_author_aci, target_author_aci_binary, &own);
        if let Some(target) = shared.db.message_at(account, &author, *at as i64).await.map_err(text)? {
            shared.db.react(target.id, &sender, if remove.unwrap_or(false) { "" } else { emoji }).await.map_err(text)?;
            changed(target.conversation, target.id);
        }
        return Ok(());
    }
    if let Some(Delete { target_sent_timestamp: Some(at) }) = &message.delete {
        if let Some(target) = shared.db.message_at(account, &sender, *at as i64).await.map_err(text)? {
            shared.db.delete(target.id).await.map_err(text)?;
            changed(target.conversation, target.id);
        }
        return Ok(());
    }
    // A message in a group may come with the group changed: the client has brought its kept state
    // up to date by now, and the conversation follows it and says what is different.
    if let Some(Ok(master_key)) = message.group_v2.as_ref().and_then(|g| g.master_key.as_deref()).map(<[u8; 32]>::try_from)
        && let Ok(Some(group)) = client.store().group(master_key).await
        && refresh_group(shared, account, client, &master_key, &group).await
    {
        shared.push(Event::Conversations);
    }
    // A change of the timer in a conversation with one person comes as a message of its own.
    if message.flags.unwrap_or(0) & 2 != 0 && message.group_v2.is_none() {
        let other = if outgoing { sent.and_then(|s| s.destination_service_id.clone()).unwrap_or_default() } else { sender.clone() };
        if let Some((conversation, _)) = shared.db.conversation_by_key(account, &other).await.map_err(text)? {
            let timer = message.expire_timer.unwrap_or(0) as i64;
            if shared.db.timer_changed(conversation, timer).await.map_err(text)?.is_some() {
                let who = if outgoing { "You".to_owned() } else { name_of(shared, account, client, &content.metadata.sender).await };
                event(shared, account, conversation, &timer_text(&format!("{} changed the timer: disappearing messages are", if who.is_empty() { "Someone" } else { &who }), timer), false).await?;
            }
        }
        return Ok(());
    }
    let mut files: Vec<&AttachmentPointer> = message.attachments.iter().filter(|a| a.content_type.as_deref() != Some(LONG_TEXT)).collect();
    // A picture sent to be seen once is meant to be gone after one look, on every device. gsignal
    // cannot yet show it once, remove it and tell the account's other devices, so it does not
    // fetch it at all, and says where it can be seen: kept and shown like any other picture, it
    // would be the opposite of what its sender asked for.
    let view_once = message.is_view_once.unwrap_or(false);
    if view_once {
        files.clear();
    }
    if message.body.is_none() && files.is_empty() && !view_once {
        return Ok(());
    }
    let body = if view_once { VIEW_ONCE } else { message.body.as_deref().unwrap_or("") };
    // Text too long for the body comes whole as an attachment; the body is its beginning.
    let long = match message.attachments.iter().find(|a| a.content_type.as_deref() == Some(LONG_TEXT)) {
        Some(pointer) => client.get_attachment(pointer).await.ok().and_then(|bytes| String::from_utf8(bytes).ok()),
        None => None,
    };
    // What the sender marked as a spoiler is covered in the text everything shows, and the text as
    // written is kept beside it for the user to ask for.
    let (covered, clear, styles) = cover_spoilers(&with_mentions(client, long.as_deref().unwrap_or(body), &message.body_ranges).await);
    let body = &covered;
    if let Some(replaces) = replaces {
        if let Some(target) = shared.db.message_at(account, &sender, replaces).await.map_err(text)? {
            shared.db.edit(target.id, body, sent_ms).await.map_err(text)?;
            shared.db.set_clear(target.id, clear.as_deref()).await.map_err(text)?;
            shared.db.set_styles(target.id, &styles).await.map_err(text)?;
            changed(target.conversation, target.id);
        }
        return Ok(());
    }
    let Ok(thread) = Thread::try_from(content) else { return Ok(()) };
    let (key, kind) = match &thread {
        Thread::Contact(id) => (id.service_id_string(), DIRECT),
        Thread::Group(master_key) => (hex::encode(master_key), GROUP),
    };
    let title = match &thread {
        Thread::Contact(id) if id.service_id_string() == own => NOTE_TO_SELF.to_owned(),
        Thread::Contact(id) => name_of(shared, account, client, id).await,
        Thread::Group(_) => client.thread_title(&thread).await.unwrap_or_default(),
    };
    let (conversation, new) = shared.db.conversation(account, &key, kind, &title).await.map_err(text)?;
    if new {
        shared.push(Event::Conversations);
    }
    // Whoever writes may have a new picture, and so may the group they write in.
    {
        let mut due = shared.pictures_due.lock().unwrap();
        due.insert((key.clone(), kind));
        if !outgoing {
            due.insert((sender.clone(), DIRECT));
        }
    }
    shared.pictures.notify_one();
    // What the account wrote to someone, here or elsewhere, accepts them; a first message from
    // someone it has no dealings with holds the conversation apart until its owner accepts it.
    if outgoing && kind == DIRECT {
        shared.db.accept_person(account, &key).await.map_err(text)?;
    } else if !outgoing && kind == DIRECT && key != own && shared.db.hold_if_unasked(conversation, &sender).await.map_err(text)? {
        shared.push(Event::Conversations);
    }
    if !outgoing {
        let name = name_of(shared, account, client, &content.metadata.sender).await;
        shared.db.person(account, &sender, &name).await.map_err(text)?;
        check_identity(shared, account, client, &content.metadata.sender, sent_ms - 1).await?;
    }
    // (the timer a message with one person carries is the conversation's; noted here, a change
    // whose own message was missed is still said)
    if kind == DIRECT && shared.db.timer_changed(conversation, message.expire_timer.unwrap_or(0) as i64).await.map_err(text)?.is_some() {
        event(shared, account, conversation, &timer_text("Disappearing messages are", message.expire_timer.unwrap_or(0) as i64), false).await?;
    }
    let quoted = message.quote.as_ref().map(|q| (author(&q.author_aci, &q.author_aci_binary, &own), q.text.as_deref().unwrap_or(""), q.id.unwrap_or(0) as i64));
    let quote = quoted.as_ref().map(|(author, text, at)| (author.as_str(), *text, *at));
    let expire_s = message.expire_timer.unwrap_or(0) as i64;
    // A message sent from another device says when its timer started; one received starts when read.
    let expire_start_ms = sent.map(|s| s.expiration_start_timestamp.map(|t| t as i64).unwrap_or(sent_ms));
    let stored = shared
        .db
        .add_message(&NewMessage { conversation, outgoing, sender: &sender, sent_ms, received_ms: now_ms(), text: body, kind: TEXT, quote, expire_s, expire_start_ms })
        .await
        .map_err(text)?;
    if let Some(message) = stored {
        if clear.is_some() {
            shared.db.set_clear(message, clear.as_deref()).await.map_err(text)?;
        }
        if !styles.is_empty() {
            shared.db.set_styles(message, &styles).await.map_err(text)?;
        }
        for file in files {
            let new = NewAttachment {
                message,
                content_type: file.content_type.as_deref().unwrap_or("application/octet-stream"),
                file_name: file.file_name.as_deref().unwrap_or(""),
                size: file.size.unwrap_or(0) as i64,
                width: file.width.unwrap_or(0) as i64,
                height: file.height.unwrap_or(0) as i64,
                voice: file.flags.unwrap_or(0) & AttachmentFlags::VoiceMessage as u32 != 0,
                looped: file.flags.unwrap_or(0) & AttachmentFlags::Gif as u32 != 0,
                pointer: Some(&file.encode_to_vec()),
                bytes: None,
            };
            shared.db.add_attachment(&new).await.map_err(text)?;
            shared.fetch.notify_one();
        }
        // (a message that could not be decrypted and was sent again has arrived: its notice goes)
        if !outgoing && let Some(noticed) = shared.db.undecryptable_arrived(account, &sender, sent_ms).await.map_err(text)? {
            shared.push(Event::ConversationChanged { account, conversation: noticed });
        }
        // (what the account wrote elsewhere is not an arrival: the device it was written on had it first)
        if !outgoing {
            shared.arrival(&format!("message {sent_ms}"));
        }
        shared.push(Event::Message { account, conversation, message });
    }
    Ok(())
}

/// Brings pictures up to date: of whoever was named in `pictures_due`, unless asked about within
/// the last minute, and of everyone not asked about for a day. Asking costs little: a person's
/// profile is one small request that says where their picture is kept, a group's kept state says
/// the same of its picture with no request at all, and a picture is fetched only when that address
/// is not the one it was last fetched from. Returns how long until someone passed over for having
/// been asked about just now is due, if anyone was; an error when the service would not answer.
async fn fetch_avatars(shared: &Arc<Shared>, account: i64, client: &mut Client) -> Result<Option<Duration>, ()> {
    let now = now_ms();
    let mut wanted = shared.db.avatars_wanted(account, now - 86_400_000).await.unwrap_or_default();
    let due: Vec<(String, i64)> = shared.pictures_due.lock().unwrap().iter().cloned().collect();
    let (mut changed, mut soonest, mut renamed) = (false, None::<i64>, false);
    for (key, kind) in due {
        let asked = shared.db.picture_asked(account, &key).await.ok().flatten().map_or(0, |p| p.1);
        if now - asked < PICTURES_EVERY_MS {
            soonest = Some(soonest.map_or(asked, |s| s.min(asked)));
            continue;
        }
        shared.pictures_due.lock().unwrap().remove(&(key.clone(), kind));
        if !wanted.contains(&(key.clone(), kind)) {
            wanted.push((key, kind));
        }
    }
    let mut result = Ok(());
    for (key, kind) in wanted {
        let known = shared.db.picture_asked(account, &key).await.ok().flatten().and_then(|p| p.0);
        // (the address now, and the picture when that is not the address it was last fetched from)
        let fetched: Result<(String, Option<Option<Vec<u8>>>), String> = async {
            if kind == GROUP {
                let master_key: [u8; 32] = hex::decode(&key).ok().and_then(|k| k.try_into().ok()).ok_or("not a group")?;
                match client.group_avatar_path(master_key).await.map_err(text)? {
                    Some(path) if known.as_deref() == Some(&path) => Ok((path, None)),
                    Some(path) if path.is_empty() => Ok((path, Some(None))),
                    Some(path) => Ok((path.clone(), Some(client.fetch_group_avatar(master_key, &path).await.map_err(text)?.map(|a| a.to_vec())))),
                    // (a group not kept yet is fetched whole, and its address is known the next time)
                    None => {
                        let context = GroupContextV2 { master_key: Some(master_key.to_vec()), revision: Some(0), ..Default::default() };
                        let picture = client.retrieve_group_avatar(context).await.map_err(text)?.map(|a| a.to_vec());
                        Ok((client.group_avatar_path(master_key).await.map_err(text)?.unwrap_or_default(), Some(picture)))
                    }
                }
            } else {
                let Some(ServiceId::Aci(aci)) = ServiceId::parse_from_service_id_string(&key) else { return Err("not a person".into()) };
                let own = Uuid::from(aci) == client.registration_data().service_ids.aci;
                let profile_key = if own { Some(client.registration_data().profile_key()) } else { client.store().profile_key(&ServiceId::Aci(aci)).await.ok().flatten() };
                let Some(profile_key) = profile_key else { return Ok((String::new(), Some(None))) };
                let (path, name) = client.profile_avatar_path(aci.into(), profile_key).await.map_err(|e| format!("refused: {e}"))?;
                // (the same answer gives the name they go by now, which is taken unless the owner gave them one)
                if !own && shared.db.profile_name(account, &key, &name).await.map_err(text)? {
                    shared.db.rename_conversation(account, &key, &name).await.map_err(text)?;
                    renamed = true;
                }
                if known.as_deref() == Some(&path) {
                    Ok((path, None))
                } else if path.is_empty() {
                    Ok((path, Some(None)))
                } else {
                    Ok((path.clone(), Some(Some(client.fetch_profile_avatar(aci.into(), profile_key, &path).await.map_err(text)?.to_vec()))))
                }
            }
        }
        .await;
        match fetched {
            Ok((_, None)) => drop(shared.db.picture_unchanged(account, &key, now_ms()).await),
            Ok((path, Some(picture))) => changed |= shared.db.set_avatar(account, &key, picture.as_deref(), &path, now_ms()).await.unwrap_or(false),
            // (a profile the service would not give ends the round; any other failure is asked about again in the next)
            Err(error) if error.starts_with("refused") => {
                result = Err(());
                break;
            }
            Err(_) => {}
        }
    }
    if changed || renamed {
        shared.push(Event::Conversations);
    }
    result.map(|()| soonest.map(|asked| Duration::from_millis((asked + PICTURES_EVERY_MS - now_ms()).max(0) as u64 + 100)))
}

/// Fetches the account's attachments that wait for it, and says so in their conversations.
async fn fetch_attachments(shared: &Arc<Shared>, account: i64, client: &Client) {
    for (id, conversation, message, pointer) in shared.db.pending_attachments(account, FETCH_MAX).await.unwrap_or_default() {
        let fetched = match AttachmentPointer::decode(&pointer[..]) {
            Ok(pointer) => client.get_attachment(&pointer).await.ok(),
            Err(_) => None,
        };
        let _ = match fetched {
            Some(bytes) => shared.db.set_attachment_bytes(id, &bytes, image_size(&bytes)).await,
            None => shared.db.attachment_failed(id).await,
        };
        shared.push(Event::MessageChanged { account, conversation, message });
    }
}

/// The width and height of a PNG, GIF, JPEG or WebP image, read from its first bytes.
pub fn image_size(b: &[u8]) -> Option<(i64, i64)> {
    let be16 = |at: usize| b.get(at..at + 2).map(|v| u16::from_be_bytes([v[0], v[1]]) as i64);
    let le16 = |at: usize| b.get(at..at + 2).map(|v| u16::from_le_bytes([v[0], v[1]]) as i64);
    let le24 = |at: usize| b.get(at..at + 3).map(|v| (v[0] as i64 | (v[1] as i64) << 8 | (v[2] as i64) << 16) + 1);
    if b.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be16(18)?, be16(22)?));
    }
    if b.starts_with(b"GIF8") {
        return Some((le16(6)?, le16(8)?));
    }
    if b.starts_with(b"RIFF") && b.get(8..12) == Some(b"WEBP") {
        return match b.get(12..16)? {
            b"VP8X" => Some((le24(24)?, le24(27)?)),
            b"VP8 " => Some((le16(26)? & 0x3fff, le16(28)? & 0x3fff)),
            // (lossless: 14 bits each of width and height less one, packed after a signature byte)
            b"VP8L" => {
                let bits = u32::from_le_bytes(b.get(21..25)?.try_into().ok()?);
                Some(((bits & 0x3fff) as i64 + 1, (bits >> 14 & 0x3fff) as i64 + 1))
            }
            _ => None,
        };
    }
    if b.starts_with(&[0xff, 0xd8]) {
        // The size is in the first start-of-frame segment; each segment says how long it is. A
        // camera that was held on its side says so in an Exif segment before it, and the picture is
        // then shown with its sides exchanged.
        let (mut at, mut turned) = (2, false);
        while b.get(at) == Some(&0xff) {
            let marker = *b.get(at + 1)?;
            if matches!(marker, 0xc0..=0xcf) && !matches!(marker, 0xc4 | 0xc8 | 0xcc) {
                let (w, h) = (be16(at + 7)?, be16(at + 5)?);
                return Some(if turned { (h, w) } else { (w, h) });
            }
            if marker == 0xe1 && b.get(at + 4..at + 10) == Some(b"Exif\0\0") {
                turned = exif_orientation(b.get(at + 10..at + 2 + be16(at + 2)? as usize)?).is_some_and(|o| (5..=8).contains(&o));
            }
            at += 2 + be16(at + 2)? as usize;
        }
    }
    None
}

/// The orientation an Exif block gives its picture (1 upright, 5 to 8 on its side), if it gives one.
pub fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let little = tiff.get(..2)? == b"II";
    let u16_at = |at: usize| tiff.get(at..at + 2).map(|v| if little { u16::from_le_bytes([v[0], v[1]]) } else { u16::from_be_bytes([v[0], v[1]]) });
    let u32_at = |at: usize| tiff.get(at..at + 4).map(|v| if little { u32::from_le_bytes([v[0], v[1], v[2], v[3]]) } else { u32::from_be_bytes([v[0], v[1], v[2], v[3]]) });
    let directory = u32_at(4)? as usize;
    // (each entry is 12 bytes: a tag, a type, a count, and the value when it is short)
    (0..u16_at(directory)? as usize).map(|i| directory + 2 + i * 12).find(|entry| u16_at(*entry) == Some(0x0112)).and_then(|entry| u16_at(entry + 8))
}

/// The type a file is sent as, from the end of its name.
pub fn content_type(name: &str) -> &'static str {
    match name.rsplit('.').next().unwrap_or("").to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/aac",
        "ogg" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        "pdf" => "application/pdf",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

/// Who a reaction or a reply names as a message's writer, as messages record their senders: "" for
/// the account itself. Clients give the identifier as text, as bytes, or both.
fn author(text: &Option<String>, bytes: &Option<Vec<u8>>, own: &str) -> String {
    let id = text.clone().or_else(|| bytes.as_deref().and_then(|b| Uuid::from_slice(b).ok()).map(|u| u.to_string())).unwrap_or_default();
    if id == own { String::new() } else { id }
}

/// A text as it is sent when it names people: each "@name" becomes one placeholder character, and a
/// range says where it is, in UTF-16 code units, and who is meant. A mention that does not lie on
/// whole characters of the text, or overlaps the one before, is left as the text it was.
fn mention_ranges(body: &str, mentions: &[(String, usize, usize)]) -> (String, Vec<BodyRange>) {
    let mut sorted: Vec<&(String, usize, usize)> = mentions.iter().collect();
    sorted.sort_by_key(|m| m.1);
    let (mut wire, mut ranges, mut at) = (String::new(), vec![], 0);
    for (who, start, len) in sorted {
        let end = start + len;
        if *start < at || *len == 0 || end > body.len() || !body.is_char_boundary(*start) || !body.is_char_boundary(end) {
            continue;
        }
        wire += &body[at..*start];
        ranges.push(BodyRange { start: Some(wire.encode_utf16().count() as u32), length: Some(1), associated_value: Some(AssociatedValue::MentionAci(who.clone())) });
        wire.push('\u{FFFC}');
        at = end;
    }
    wire += &body[at..];
    (wire, ranges)
}

/// The text with each mention, which arrives as a placeholder character, replaced by "@" and the
/// person's name.
async fn with_mentions(client: &Client, body: &str, ranges: &[BodyRange]) -> String {
    let mut names = vec![];
    for range in ranges {
        let id = match &range.associated_value {
            Some(AssociatedValue::MentionAci(id)) => Uuid::parse_str(id).ok(),
            Some(AssociatedValue::MentionAciBinary(id)) => Uuid::from_slice(id).ok(),
            _ => None,
        };
        if let Some(id) = id {
            let name = person_name(client, &ServiceId::Aci(id.into())).await;
            names.push((range.start() as usize, range.length() as usize, format!("@{}", if name.is_empty() { "someone" } else { &name })));
        }
        // (a style's beginning and end are marked in the text, for cover_spoilers to find
        // wherever the names put in have moved them)
        if let Some(AssociatedValue::Style(style)) = range.associated_value
            && let Some((from, to)) = style_marks(style)
        {
            names.push((range.start() as usize, 0, from.to_string()));
            names.push(((range.start() + range.length()) as usize, 0, to.to_string()));
        }
    }
    replace_utf16(body, names)
}

const SPOILER_FROM: char = '\u{E000}';
const SPOILER_TO: char = '\u{E001}';

/// The characters that mark where a style of Signal's begins and ends in a text, from private use;
/// None for a style that is not drawn. Signal numbers them BOLD 1, ITALIC 2, SPOILER 3,
/// STRIKETHROUGH 4 and MONOSPACE 5.
fn style_marks(style: i32) -> Option<(char, char)> {
    match style {
        3 => Some((SPOILER_FROM, SPOILER_TO)),
        1 | 2 | 4 | 5 => Some((char::from_u32(0xE010 + 2 * style as u32)?, char::from_u32(0xE011 + 2 * style as u32)?)),
        _ => None,
    }
}

/// Takes the marks `with_mentions` put round each style out of a text. Returns the text with every
/// letter of a spoiler covered; the text as written when anything was covered; and the other
/// styles as "STYLE START END" lines, START and END counted in characters, which are the same in
/// either text.
fn cover_spoilers(marked: &str) -> (String, Option<String>, String) {
    let (mut covered, mut clear, mut inside, mut hid) = (String::new(), String::new(), 0, false);
    let (mut open, mut styles): (Vec<(i32, usize)>, Vec<(i32, usize, usize)>) = (vec![], vec![]);
    let mut at = 0;
    for c in marked.chars() {
        match c {
            SPOILER_FROM => inside += 1,
            SPOILER_TO => inside = (inside - 1).max(0),
            c if ('\u{E012}'..='\u{E01B}').contains(&c) => {
                let (style, end) = (((c as u32 - 0xE010) / 2) as i32, (c as u32 - 0xE010) % 2 == 1);
                if !end {
                    open.push((style, at));
                } else if let Some(i) = open.iter().rposition(|o| o.0 == style) {
                    let (_, start) = open.remove(i);
                    if start < at {
                        styles.push((style, start, at));
                    }
                }
            }
            c => {
                clear.push(c);
                hid |= inside > 0 && !c.is_whitespace();
                covered.push(if inside > 0 && !c.is_whitespace() { '\u{2592}' } else { c });
                at += 1;
            }
        }
    }
    let styles = styles.iter().map(|(style, start, end)| format!("{style} {start} {end}\n")).collect();
    (covered, hid.then_some(clear), styles)
}

/// Replaces parts of a text, each given by where it starts and how long it is in UTF-16 code units,
/// which is how Signal counts positions in a message. A part outside the text is left alone.
fn replace_utf16(text: &str, mut parts: Vec<(usize, usize, String)>) -> String {
    if parts.is_empty() {
        return text.to_owned();
    }
    let mut units: Vec<u16> = text.encode_utf16().collect();
    // (from the end, so an earlier part's position is not moved by a later replacement)
    parts.sort_by_key(|p| std::cmp::Reverse(p.0));
    for (start, length, with) in parts {
        if start + length <= units.len() {
            units.splice(start..start + length, with.encode_utf16());
        }
    }
    String::from_utf16_lossy(&units)
}

/// The identifier a group is named by in a typing message, from the master key a conversation is keyed by.
fn group_id(key: &str) -> Option<Vec<u8>> {
    let master_key: [u8; 32] = hex::decode(key).ok()?.try_into().ok()?;
    Some(GroupSecretParams::derive_from_master_key(GroupMasterKey::new(master_key)).get_group_identifier().to_vec())
}

/// Tells a conversation's other members that the user is typing or has stopped, when the account
/// sends typing indicators.
async fn send_typing(shared: &Arc<Shared>, account: i64, client: &mut Client, key: &str, kind: i64, started: bool) -> Result<(), String> {
    // (the account's notes to itself have no one to tell, and the service refuses the attempt)
    if !matches!(shared.db.settings(account).await, Ok(Some((_, true)))) || key == client.registration_data().service_ids.aci.to_string() {
        return Ok(());
    }
    // (nor that an answer to them is being typed)
    let held = shared.db.conversation_keys(account, kind).await.map_err(text)?.into_iter().find(|(_, k)| k == key);
    if let Some((conversation, _)) = held {
        if !shared.db.accepted(conversation).await.map_err(text)? {
            return Ok(());
        }
    }
    let now = now_ms() as u64;
    let action = Some(if started { Action::Started } else { Action::Stopped } as i32);
    if kind == GROUP {
        let message = TypingMessage { timestamp: Some(now), action, group_id: group_id(key) };
        client.send_message_to_group(&hex::decode(key).map_err(text)?, message, now).await.map_err(text)
    } else {
        let recipient = ServiceId::parse_from_service_id_string(key).ok_or("the conversation's identifier is not a Signal address")?;
        client.send_message(recipient, TypingMessage { timestamp: Some(now), action, group_id: None }, now).await.map_err(text)
    }
}

/// Marks a conversation read here, tells the account's other devices, and tells the people who
/// wrote the messages when the account sends read receipts.
async fn mark_read(shared: &Arc<Shared>, account: i64, client: &mut Client, conversation: i64) -> Result<(), String> {
    let read = shared.db.mark_read(conversation, now_ms()).await.map_err(text)?;
    if read.is_empty() {
        return Ok(());
    }
    shared.push(Event::Listed { account, conversation });
    let messages: Vec<(ServiceId, u64)> = read.iter().filter_map(|(sender, sent_ms)| Some((ServiceId::parse_from_service_id_string(sender)?, *sent_ms as u64))).collect();
    client.sync_read(&messages).await.map_err(text)?;
    // (someone who wrote unasked is not told their message was read until they are accepted)
    if matches!(shared.db.settings(account).await, Ok(Some((true, _)))) && shared.db.accepted(conversation).await.map_err(text)? {
        let mut senders: Vec<ServiceId> = messages.iter().map(|(sender, _)| *sender).collect();
        senders.sort_unstable_by_key(|s| s.service_id_string());
        senders.dedup();
        for sender in senders {
            let timestamp = messages.iter().filter(|(s, _)| *s == sender).map(|(_, t)| *t).collect();
            let receipt = ReceiptMessage { r#type: Some(ReceiptType::Read as i32), timestamp };
            client.send_message(sender, receipt, now_ms() as u64).await.map_err(text)?;
        }
    }
    Ok(())
}

/// Sets the disappearing timer of a conversation with one person: a message carrying the new timer
/// goes to them, the client keeps the timer for what is sent after, and the conversation says so.
async fn set_timer(shared: &Arc<Shared>, account: i64, client: &mut Client, conversation: i64, seconds: u32) -> Result<(), String> {
    let Some((_, key, kind)) = shared.db.conversation_key(conversation).await.map_err(text)? else { return Ok(()) };
    if kind != DIRECT {
        return Err("the timer of a group is set by changing the group, which gink cannot do yet".into());
    }
    let now = now_ms();
    let mut message = data_message(client, &key, kind, "", now).await?;
    // (a timer that is given is sent as it is, with the next version, which the client works out)
    message.flags = Some(2);
    message.expire_timer = Some(seconds);
    send(shared, account, client, &key, kind, message, now).await?;
    if shared.db.timer_changed(conversation, seconds as i64).await.map_err(text)?.is_some() {
        event(shared, account, conversation, &timer_text("You changed the timer: disappearing messages are", seconds as i64), false).await?;
    }
    Ok(())
}

/// Removes a member from a group the account administers, and says so in the conversation as any
/// change of its members is said.
async fn remove_member(shared: &Arc<Shared>, account: i64, client: &mut Client, conversation: i64, key: &str) -> Result<(), String> {
    let Some((_, group_key, kind)) = shared.db.conversation_key(conversation).await.map_err(text)? else { return Ok(()) };
    if kind != GROUP {
        return Ok(());
    }
    let master_key: [u8; 32] = hex::decode(&group_key).ok().and_then(|k| k.try_into().ok()).ok_or("not a group")?;
    let Some(ServiceId::Aci(member)) = ServiceId::parse_from_service_id_string(key) else { return Err("not a person's identifier".into()) };
    client.remove_group_member(master_key, member).await.map_err(|e| match e.to_string() {
        e if e.contains("403") => "only the group's administrators can remove a member".to_owned(),
        e => format!("the member could not be removed: {e}"),
    })?;
    if let Ok(Some(group)) = client.store().group(master_key).await
        && refresh_group(shared, account, client, &master_key, &group).await
    {
        shared.push(Event::Conversations);
    }
    shared.push(Event::ConversationChanged { account, conversation });
    Ok(())
}

/// Tells the account's other devices that a conversation held apart as a request was accepted
/// here. What was read in it before is not reported to its writer afterwards.
async fn accept(shared: &Arc<Shared>, _account: i64, client: &mut Client, conversation: i64) -> Result<(), String> {
    let Some((_, key, kind)) = shared.db.conversation_key(conversation).await.map_err(text)? else { return Ok(()) };
    if kind != DIRECT {
        return Ok(());
    }
    let person = ServiceId::parse_from_service_id_string(&key).ok_or("the conversation's identifier is not a Signal address")?;
    client.sync_request_accepted(&person).await.map_err(text)
}

/// Writes a notice of the core's own into the conversation with a person, placed at the time `at`;
/// "{}" in `what` is their name.
async fn notice(shared: &Arc<Shared>, account: i64, client: &Client, person: &ServiceId, kind: i64, what: &str, at: i64) -> Result<(), String> {
    let (key, name) = (person.service_id_string(), name_of(shared, account, client, person).await);
    shared.db.person(account, &key, &name).await.map_err(text)?;
    let (conversation, new) = shared.db.conversation(account, &key, DIRECT, &name).await.map_err(text)?;
    if new {
        shared.push(Event::Conversations);
    }
    let now = now_ms();
    let body = what.replace("{}", if name.is_empty() { "this person" } else { &name });
    // (it starts out read: there is nothing to tell the account's other devices or the person about it)
    let new = NewMessage { conversation, outgoing: false, sender: &key, sent_ms: at, received_ms: now, text: &body, kind, quote: None, expire_s: 0, expire_start_ms: Some(now) };
    if let Some(message) = shared.db.add_message(&new).await.map_err(text)? {
        shared.push(Event::Message { account, conversation, message });
    }
    Ok(())
}

/// Says so in the conversation when a person's key is not the one they were last seen with, at the
/// time `at`: just before the message that showed it.
async fn check_identity(shared: &Arc<Shared>, account: i64, client: &Client, person: &ServiceId, at: i64) -> Result<(), String> {
    let address = ProtocolAddress::new(person.service_id_string(), DeviceId::new(1).expect("a device id"));
    if let Ok(Some(identity)) = client.store().aci_protocol_store().get_identity(&address).await
        && shared.db.identity_changed(account, &person.service_id_string(), &identity.serialize()).await.map_err(text)?
    {
        notice(shared, account, client, person, IDENTITY_CHANGED, "{}'s safety number changed.", at).await?;
    }
    Ok(())
}

/// Starts a new session with a person when the old one has stopped working in either direction.
async fn repair(shared: &Arc<Shared>, account: i64, client: &mut Client, person: &ServiceId) -> Result<(), String> {
    let (key, now) = (person.service_id_string(), now_ms());
    if !matches!(person, ServiceId::Aci(_)) || now < shared.db.repair_after(account, &key).await.map_err(text)? {
        return Ok(());
    }
    let result = client.send_session_reset(person, now as u64).await.map_err(text);
    let wait = if result.is_ok() { REPAIR_EVERY_MS } else { REPAIR_RETRY_MS };
    shared.db.set_repair_after(account, &key, now + wait).await.map_err(text)?;
    result
}

/// A message from this person could not be decrypted: says so where the message would have been,
/// and starts a new session so their next one can be.
async fn undecryptable(shared: &Arc<Shared>, account: i64, client: &mut Client, person: &ServiceId, device: DeviceId, sent_ms: u64) -> Result<(), String> {
    notice(shared, account, client, person, UNDECRYPTABLE, "A message from {} could not be decrypted.", now_ms()).await?;
    let mended = repair(shared, account, client, person).await;
    if let Err(error) = &mended {
        crate::log::line(&format!("account {account}: a new session could not be started: {error}"));
    }
    mended?;
    // With a session that works again, the sender is asked to send that message once more; their
    // program keeps what it sent for a day for this. Each message is asked for once, so that one
    // that fails again does not set the two programs asking and sending for ever.
    static ASKED: std::sync::Mutex<Vec<(String, u64)>> = std::sync::Mutex::new(Vec::new());
    let key = (person.service_id_string(), sent_ms);
    if sent_ms == 0 || !matches!(person, ServiceId::Aci(_)) || ASKED.lock().unwrap().contains(&key) {
        return Ok(());
    }
    ASKED.lock().unwrap().push(key);
    let asked = client.send_retry_request(person, sent_ms, device).await.map_err(text);
    crate::log::line(&format!("account {account}: asked for the message sent at {sent_ms} again: {}", asked.as_ref().err().map_or("sent", |e| e.as_str())));
    asked
}

/// This person could not decrypt what the account sent at `sent_ms`: starts a new session, when
/// one may be started, and sends it to them again as it was, whatever it was: a message, an edit, a
/// reaction or a deletion, in a group or not.
async fn send_again(shared: &Arc<Shared>, account: i64, client: &mut Client, person: &ServiceId, sent_ms: i64) -> Result<(), String> {
    let repaired = repair(shared, account, client, person).await;
    let Some((form, content)) = shared.db.sent(account, sent_ms).await.map_err(text)? else { return repaired };
    let body: ContentBody = match form {
        SENT_EDIT => EditMessage::decode(&content[..]).map_err(text)?.into(),
        _ => DataMessage::decode(&content[..]).map_err(text)?.into(),
    };
    client.send_message(*person, body, sent_ms as u64).await.map_err(text)
}

/// The message that carries a text to a conversation.
async fn data_message(client: &Client, key: &str, kind: i64, body: &str, sent_ms: i64) -> Result<DataMessage, String> {
    let mut message = DataMessage { body: (!body.is_empty()).then(|| body.to_owned()), timestamp: Some(sent_ms as u64), ..Default::default() };
    // Text too long for the body goes whole as an attachment, with its beginning as the body.
    if body.len() > BODY_MAX {
        let spec = AttachmentSpec { content_type: LONG_TEXT.into(), length: body.len(), file_name: None, preview: None, voice_note: None, borderless: None, width: None, height: None, caption: None, blur_hash: None };
        let pointer = client.upload_attachment(spec, body.as_bytes().to_vec()).await.map_err(text)?.map_err(text)?;
        message.attachments.push(pointer);
        message.body = Some(body[..body.floor_char_boundary(BODY_MAX)].to_owned());
    }
    if kind == GROUP {
        message.group_v2 = Some(GroupContextV2 { master_key: Some(hex::decode(key).map_err(text)?), revision: Some(0), ..Default::default() });
    }
    Ok(message)
}

/// Sends to a conversation's members.
async fn send(shared: &Arc<Shared>, account: i64, client: &mut Client, key: &str, kind: i64, body: impl Into<ContentBody>, sent_ms: i64) -> Result<(), String> {
    // What is sent is noted for a day, so that it can be sent again to someone who asks.
    let body: ContentBody = body.into();
    let noted = match &body {
        ContentBody::DataMessage(message) => Some((SENT_MESSAGE, message.encode_to_vec())),
        ContentBody::EditMessage(edit) => Some((SENT_EDIT, edit.encode_to_vec())),
        _ => None,
    };
    if let Some((form, content)) = noted {
        shared.db.log_sent(account, sent_ms, form, &content, sent_ms - RESEND_WITHIN_MS).await.map_err(text)?;
    }
    if kind == GROUP {
        return client.send_message_to_group(&hex::decode(key).map_err(text)?, body, sent_ms as u64).await.map_err(text);
    }
    let recipient = ServiceId::parse_from_service_id_string(key).ok_or("the conversation's identifier is not a Signal address")?;
    client.send_message(recipient, body, sent_ms as u64).await.map_err(text)?;
    // (sending is also when a person's new key is first met, if they have not written since)
    check_identity(shared, account, client, &recipient, sent_ms - 1).await
}

/// Gives a message a reaction, edits it or deletes it, for everyone in its conversation. Only the
/// account's own messages can be edited or deleted.
async fn send_change(shared: &Arc<Shared>, account: i64, client: &mut Client, message: i64, change: Change) -> Result<(), String> {
    let Some(target) = shared.db.message(message).await.map_err(text)? else { return Ok(()) };
    let Target { key, kind, .. } = &target;
    let (now, own) = (now_ms(), client.registration_data().service_ids.aci.to_string());
    let mut carrier = DataMessage { timestamp: Some(now as u64), ..Default::default() };
    if *kind == GROUP {
        carrier.group_v2 = Some(GroupContextV2 { master_key: Some(hex::decode(key).map_err(text)?), revision: Some(0), ..Default::default() });
    }
    match change {
        Change::React(emoji) => {
            // (taking a reaction away names the emoji that was given)
            let old = shared.db.own_reaction(target.id).await.map_err(text)?;
            if emoji.is_empty() && old.is_empty() {
                return Ok(());
            }
            carrier.reaction = Some(Reaction {
                emoji: Some(if emoji.is_empty() { old } else { emoji.clone() }),
                remove: Some(emoji.is_empty()),
                target_author_aci: Some(if target.sender.is_empty() { own } else { target.sender.clone() }),
                target_sent_timestamp: Some(target.sent_ms as u64),
                ..Default::default()
            });
            send(shared, account, client, key, *kind, carrier, now).await?;
            shared.db.react(target.id, "", &emoji).await.map_err(text)?;
        }
        Change::Edit(body) if target.sender.is_empty() => {
            let edit = EditMessage { target_sent_timestamp: Some(target.latest_ms as u64), data_message: Some(data_message(client, key, *kind, &body, now).await?) };
            send(shared, account, client, key, *kind, edit, now).await?;
            shared.db.edit(target.id, &body, now).await.map_err(text)?;
        }
        Change::Delete if target.sender.is_empty() => {
            carrier.delete = Some(Delete { target_sent_timestamp: Some(target.sent_ms as u64) });
            send(shared, account, client, key, *kind, carrier, now).await?;
            shared.db.delete(target.id).await.map_err(text)?;
        }
        _ => return Ok(()),
    }
    shared.push(Event::MessageChanged { account, conversation: target.conversation, message: target.id });
    Ok(())
}

/// Sends the account's waiting messages, oldest first, and stops at the first that fails: the
/// later ones would likely fail the same way, and those of one conversation keep their order.
async fn flush(shared: &Arc<Shared>, account: i64, client: &mut Client) -> Result<(), String> {
    for waiting in shared.db.waiting(account).await.map_err(text)? {
        let result = send_waiting(shared, client, &waiting).await;
        // A message to someone whose safety number changed is held for the user to decide, and the
        // conversation says why; it is not a failure to try again, and the others go on.
        if result.as_ref().is_err_and(|e| e.to_lowercase().contains("untrusted identity")) {
            shared.db.hold(waiting.id).await.map_err(text)?;
            if let (DIRECT, Some(person)) = (waiting.kind, ServiceId::parse_from_service_id_string(&waiting.key)) {
                notice(shared, account, client, &person, IDENTITY_CHANGED, "{}'s safety number changed, so a message to them was not sent. They may have a new phone or have reinstalled Signal; if in doubt, ask them another way. \"Send anyway\" in the message's menu sends it.", now_ms()).await?;
            }
            shared.push(Event::MessageChanged { account, conversation: waiting.conversation, message: waiting.id });
            continue;
        }
        // A person who is no longer on Signal cannot be sent to, now or on a second or third try:
        // the message is marked not sent at once, the conversation says why, and the others go on.
        // "Send again" tries once more, for someone who has come back.
        if waiting.kind == DIRECT && result.as_ref().is_err_and(|e| e.contains("Recipient not found")) {
            shared.db.send_failed(waiting.id, 1).await.map_err(text)?;
            let name = shared.db.person_name(account, &waiting.key).await.unwrap_or_default();
            let who = if name.is_empty() { "This person" } else { &name };
            event(shared, account, waiting.conversation, &format!("{who} is not on Signal any more, so the message was not sent."), false).await?;
            shared.push(Event::MessageChanged { account, conversation: waiting.conversation, message: waiting.id });
            continue;
        }
        if result.is_err() {
            shared.db.send_failed(waiting.id, SEND_TRIES).await.map_err(text)?;
        }
        shared.push(Event::MessageChanged { account, conversation: waiting.conversation, message: waiting.id });
        result?;
    }
    // Then the reactions, edits and deletions that wait, in the order they were asked for. One
    // that fails is tried with the next sending, and after as many failures as a message gets it
    // is given up, with the error left for the status bar and the log.
    for (id, message, what, value) in shared.db.changes(account).await.map_err(text)? {
        let change = match what {
            0 => Change::React(value),
            1 => Change::Edit(value),
            _ => Change::Delete,
        };
        let result = send_change(shared, account, client, message, change).await;
        let given_up = shared.db.change_done(id, result.is_ok(), SEND_TRIES).await.map_err(text)?;
        if let Some(target) = shared.db.message(message).await.map_err(text)? {
            shared.push(Event::MessageChanged { account, conversation: target.conversation, message });
        }
        if let Err(error) = result {
            return Err(if given_up { format!("a change to a message was given up after {SEND_TRIES} tries, and the message is marked: {error}") } else { error });
        }
    }
    Ok(())
}

/// Sends one stored message as it was written: at the time it was stored, by which the service
/// and every receiver know it, so that sending it twice shows it once.
async fn send_waiting(shared: &Arc<Shared>, client: &mut Client, waiting: &Waiting) -> Result<(), String> {
    let Waiting { id, key, kind, sent_ms, .. } = waiting;
    let mentions: Vec<(String, usize, usize)> = waiting
        .mentions
        .lines()
        .filter_map(|line| {
            let mut parts = line.split('\t');
            Some((parts.next()?.to_owned(), parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
        })
        .collect();
    // The user said to send it although a safety number changed: the store is told that those it
    // goes to may be sent to under their new keys.
    if waiting.trusted {
        let to = if *kind == GROUP { shared.db.members(waiting.conversation).await.map_err(text)?.into_iter().map(|m| m.0).collect() } else { vec![key.clone()] };
        for name in to {
            client.store().approve_identity(&name);
        }
    }
    let (wire, ranges) = mention_ranges(&waiting.text, &mentions);
    let mut message = data_message(client, key, *kind, &wire, *sent_ms).await?;
    message.body_ranges = ranges;
    // Each file is sent to the service's store and named in the message by where it is.
    for (name, content_type, width, height, bytes) in shared.db.files(*id).await.map_err(text)? {
        let spec = AttachmentSpec {
            content_type,
            length: bytes.len(),
            file_name: Some(name),
            preview: None,
            voice_note: None,
            borderless: None,
            width: (width > 0).then_some(width as u32),
            height: (height > 0).then_some(height as u32),
            caption: None,
            blur_hash: None,
        };
        message.attachments.push(client.upload_attachment(spec, bytes).await.map_err(text)?.map_err(text)?);
    }
    // A reply carries the start of what it answers, for those who no longer hold that message.
    if let Some(sender) = &waiting.quote_sender {
        let author = if sender.is_empty() { client.registration_data().service_ids.aci.to_string() } else { sender.clone() };
        message.quote = Some(Quote { id: Some(waiting.quote_ms as u64), author_aci: Some(author), text: Some(waiting.quote_text.clone()), ..Default::default() });
    }
    send(shared, waiting.account, client, key, *kind, message, *sent_ms).await?;
    // The client sent it with the conversation's disappearing timer, which starts at once.
    let thread = match *kind {
        GROUP => hex::decode(key).ok().and_then(|k| k.try_into().ok()).map(Thread::Group),
        _ => ServiceId::parse_from_service_id_string(key).map(Thread::Contact),
    };
    let expire_s = match thread {
        Some(thread) => client.store().expire_timer(&thread).await.ok().flatten().map(|(timer, _)| timer as i64).unwrap_or(0),
        None => 0,
    };
    // (writing to someone accepts them, and a new key that was sent to is the one now known for them)
    if *kind == DIRECT {
        shared.db.accept_person(waiting.account, key).await.map_err(text)?;
        let address = ProtocolAddress::new(key.clone(), DeviceId::new(1).expect("a device id"));
        if let (true, Ok(Some(identity))) = (waiting.trusted, client.store().aci_protocol_store().get_identity(&address).await) {
            shared.db.identity_changed(waiting.account, key, &identity.serialize()).await.map_err(text)?;
        }
    }
    shared.db.mark_sent(*id, expire_s, now_ms()).await.map_err(text)
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// An error with the causes beneath it, which a connection failure needs to be understood.
fn detail(error: impl std::error::Error) -> String {
    let mut out = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        out += &format!(": {cause}");
        source = cause.source();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{SPOILER_FROM, SPOILER_TO, cover_spoilers, image_size, mention_ranges, replace_utf16, timer_text};

    #[test]
    fn a_spoiler_is_covered_letter_by_letter_and_kept_as_written() {
        // (marked as with_mentions marks it, here round "the butler" after a name was put in)
        let marked = replace_utf16("It was X.", vec![(7, 1, "the butler".into()), (7, 0, SPOILER_FROM.into()), (8, 0, SPOILER_TO.into())]);
        let (covered, clear, _) = cover_spoilers(&marked);
        assert_eq!(covered, "It was \u{2592}\u{2592}\u{2592} \u{2592}\u{2592}\u{2592}\u{2592}\u{2592}\u{2592}.");
        assert_eq!(clear.as_deref(), Some("It was the butler."));
    }

    #[test]
    fn styles_are_kept_by_character_where_names_and_spoilers_moved_them() {
        // "Hi @X, bold and hidden": X is a mention, "bold" bold, "and" italic within it, "hidden" a spoiler.
        let (bold, italic) = (super::style_marks(1).unwrap(), super::style_marks(2).unwrap());
        let marked = replace_utf16("Hi \u{FFFC}, bold and hidden", vec![(3, 1, "@Eris".into()), (6, 0, bold.0.into()), (14, 0, bold.1.into()), (11, 0, italic.0.into()), (14, 0, italic.1.into()), (15, 0, SPOILER_FROM.into()), (21, 0, SPOILER_TO.into())]);
        let (covered, clear, styles) = cover_spoilers(&marked);
        assert_eq!(clear.as_deref(), Some("Hi @Eris, bold and hidden"));
        assert_eq!(covered.chars().count(), clear.unwrap().chars().count());
        assert_eq!(styles, "2 15 18\n1 10 18\n");
        assert_eq!(cover_spoilers("nothing hidden"), ("nothing hidden".to_owned(), None, String::new()));
    }

    #[test]
    fn a_timer_is_said_in_its_largest_whole_unit() {
        assert_eq!(timer_text("Disappearing messages are", 0), "Disappearing messages are now off.");
        assert_eq!(timer_text("They are", 604_800), "They are now set to 1 week.");
        assert_eq!(timer_text("They are", 172_800), "They are now set to 2 days.");
        assert_eq!(timer_text("They are", 5400), "They are now set to 90 minutes.");
        assert_eq!(timer_text("They are", 30), "They are now set to 30 seconds.");
    }

    #[test]
    fn mentions_become_placeholders_counted_in_utf16() {
        let body = "\u{1F600} @Ann and @Bob Lee!";
        let (wire, ranges) = mention_ranges(body, &[("b".into(), 14, 8), ("a".into(), 5, 4), ("x".into(), 6, 2)]);
        assert_eq!(wire, "\u{1F600} \u{FFFC} and \u{FFFC}!");
        let at: Vec<(u32, u32)> = ranges.iter().map(|r| (r.start(), r.length())).collect();
        assert_eq!(at, [(3, 1), (9, 1)]);
        assert_eq!(mention_ranges("plain", &[]).0, "plain");
    }

    #[test]
    fn image_sizes_are_read_from_the_first_bytes() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend([0, 0, 1, 44, 0, 0, 0, 200]);
        assert_eq!(image_size(&png), Some((300, 200)));
        assert_eq!(image_size(b"GIF89a\x40\x01\xf0\x00"), Some((320, 240)));
        // A JPEG with an application segment before its start of frame.
        let jpeg = [0xff, 0xd8, 0xff, 0xe0, 0, 4, 0, 0, 0xff, 0xc2, 0, 17, 8, 0, 100, 1, 0];
        assert_eq!(image_size(&jpeg), Some((256, 100)));
        // The same after an Exif segment saying the camera was turned a quarter (orientation 6).
        let mut turned = vec![0xff, 0xd8, 0xff, 0xe1, 0, 34];
        turned.extend(b"Exif\0\0MM\0\x2a\0\0\0\x08\0\x01\x01\x12\0\x03\0\0\0\x01\0\x06\0\0\0\0\0\0");
        turned.extend(&jpeg[8..]);
        assert_eq!(image_size(&turned), Some((100, 256)));
        assert_eq!(image_size(b"RIFF\0\0\0\0WEBPVP8X\0\0\0\0\0\0\0\0\x3f\x01\0\xef\0\0"), Some((320, 240)));
        assert_eq!(image_size(b"plain text"), None);
        assert_eq!(image_size(&png[..20]), None);
    }

    #[test]
    fn positions_count_utf16_units() {
        // The emoji is two units, so the placeholder after it starts at unit 3.
        let text = "\u{1F600} \u{FFFC} and \u{FFFC}!";
        let parts = vec![(3, 1, "@Ann".to_owned()), (9, 1, "@Bob".to_owned())];
        assert_eq!(replace_utf16(text, parts), "\u{1F600} @Ann and @Bob!");
        assert_eq!(replace_utf16("short", vec![(4, 5, "x".to_owned())]), "short");
    }
}
