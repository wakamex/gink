//! The protocol core behind `include/gsignal_core.h`.

mod db;
mod log;
mod shared;
mod signal;
mod strip;
mod update;

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Arc, Mutex};

use tokio::sync::mpsc;

use crate::db::Db;
use crate::shared::{Change, Command, Event, Shared, State, Status, Wake};

const KEY_LEN: usize = 32;

pub struct Core {
    shared: Arc<Shared>,
    /// Runs the database queries the app waits for, on the app's thread.
    runtime: tokio::runtime::Runtime,
    commands: HashMap<i64, mpsc::UnboundedSender<Command>>,
    // What the last call of each function handed out, kept alive until its next call.
    account_strings: Vec<CString>,
    conversation_strings: Vec<CString>,
    /// The strings of the messages the app holds, by message: one read again replaces its own.
    message_strings: std::collections::HashMap<i64, Vec<CString>>,
    attachment_strings: Vec<CString>,
    hit_strings: Vec<CString>,
    member_strings: Vec<CString>,
    face_strings: Vec<CString>,
    draft_strings: Vec<CString>,
    attachment_bytes: Vec<u8>,
    avatar_bytes: Vec<u8>,
    event_text: CString,
    event_code: Vec<u8>,
}

#[repr(C)]
pub struct GscAccount {
    id: i64,
    protocol: *const c_char,
    label: *const c_char,
    state: c_int,
    last_received_ms: i64,
    undecryptable: c_int,
    error: *const c_char,
}

#[repr(C)]
pub struct GscConversation {
    id: i64,
    account: i64,
    kind: c_int,
    title: *const c_char,
    last_ms: i64,
    last_text: *const c_char,
    unread: c_int,
    muted: bool,
    request: bool,
    avatar_ms: i64,
    person: c_int,
    last_sender: *const c_char,
    admin: bool,
}

#[repr(C)]
pub struct GscMember {
    key: *const c_char,
    name: *const c_char,
}

#[repr(C)]
pub struct GscFace {
    key: *const c_char,
    name: *const c_char,
    person: c_int,
    picture_ms: i64,
}

#[repr(C)]
pub struct GscMention {
    key: *const c_char,
    start: c_int,
    length: c_int,
}

#[repr(C)]
pub struct GscHit {
    message: i64,
    conversation: i64,
    title: *const c_char,
    outgoing: bool,
    sender: *const c_char,
    sent_ms: i64,
    text: *const c_char,
    newer: c_int,
}

#[repr(C)]
pub struct GscMessage {
    id: i64,
    outgoing: bool,
    sender: *const c_char,
    sent_ms: i64,
    text: *const c_char,
    status: c_int,
    expires_ms: i64,
    kind: c_int,
    edited: bool,
    reply_to: *const c_char,
    reply_text: *const c_char,
    reactions: *const c_char,
    reacted: *const c_char,
    hidden: *const c_char,
    changing: c_int,
    reaction: *const c_char,
    attachments: c_int,
    person: c_int,
    picture_ms: i64,
    reply_id: i64,
    styles: *const c_char,
}

#[repr(C)]
pub struct GscAttachment {
    id: i64,
    content_type: *const c_char,
    name: *const c_char,
    size: i64,
    width: c_int,
    height: c_int,
    voice: bool,
    looped: bool,
    state: c_int,
}

#[repr(C)]
pub struct GscEvent {
    kind: c_int,
    account: i64,
    conversation: i64,
    message: i64,
    text: *const c_char,
    code: *const u8,
    code_size: c_int,
    typing: bool,
}

fn c_string(s: &str) -> CString {
    CString::new(s.replace('\0', " ")).unwrap()
}

/// Keeps `s` alive in `keep` and returns a pointer to it.
fn lend(keep: &mut Vec<CString>, s: &str) -> *const c_char {
    keep.push(c_string(s));
    keep.last().unwrap().as_ptr()
}

/// # Safety
/// `key` points to `KEY_LEN` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_key_new(key: *mut u8) -> c_int {
    let key = unsafe { std::slice::from_raw_parts_mut(key, KEY_LEN) };
    match getrandom::fill(key) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

fn start(shared: &Arc<Shared>, commands: &mut HashMap<i64, mpsc::UnboundedSender<Command>>, id: i64, protocol: &str, device_name: &str, label: &str) -> bool {
    if protocol != "signal" {
        return false;
    }
    let status = Status { protocol: "signal", label: label.to_owned(), state: State::Connecting, last_received_ms: 0, undecryptable: 0, error: String::new() };
    shared.statuses.lock().unwrap().insert(id, status);
    commands.insert(id, signal::spawn(shared.clone(), id, device_name.to_owned()));
    true
}

fn open(dir: &str, key: [u8; KEY_LEN], wake: Wake) -> Result<Core, String> {
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().map_err(|e| e.to_string())?;
    let dir = PathBuf::from(dir);
    log::start(&dir);
    let db = runtime.block_on(Db::open(&dir.join("gsignal.db"), &key)).map_err(|e| {
        log::line(&format!("the database could not be opened: {e}"));
        e.to_string()
    })?;
    let accounts = runtime.block_on(db.accounts()).map_err(|e| e.to_string())?;
    // arrivals.txt starts with the messages already held, whose times of arrival the database has.
    // Whether the account was caught up as each of those came is not known, and they count as if it was.
    let arrivals = dir.join("arrivals.txt");
    if !arrivals.exists() {
        let held = runtime.block_on(db.arrivals()).unwrap_or_default();
        let lines: String = std::iter::once("1 online\n".to_owned()).chain(held.iter().map(|(received, sent)| format!("{received} message {sent}\n"))).collect();
        let _ = std::fs::write(&arrivals, lines);
    }
    let shared = Arc::new(Shared { db, dir, key, events: Mutex::new(VecDeque::new()), statuses: Mutex::new(BTreeMap::new()), wake, fetch: tokio::sync::Notify::new(), pictures: tokio::sync::Notify::new(), pictures_due: Mutex::new(Default::default()) });
    let mut commands = HashMap::new();
    for account in accounts {
        start(&shared, &mut commands, account.id, &account.protocol, &account.device_name, &account.label);
    }
    Ok(Core { shared, runtime, commands, account_strings: vec![], conversation_strings: vec![], message_strings: Default::default(), attachment_strings: vec![], hit_strings: vec![], member_strings: vec![], face_strings: vec![], draft_strings: vec![], attachment_bytes: vec![], avatar_bytes: vec![], event_text: CString::default(), event_code: vec![] })
}

/// # Safety
/// `dir` is a NUL-terminated UTF-8 path, `key` points to `KEY_LEN` readable bytes, and `error` is
/// NULL or points to a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_open(dir: *const c_char, key: *const u8, wake: Option<unsafe extern "C" fn(*mut c_void)>, user: *mut c_void, error: *mut *mut c_char) -> *mut Core {
    let dir = unsafe { CStr::from_ptr(dir) }.to_string_lossy();
    let key: [u8; KEY_LEN] = unsafe { std::slice::from_raw_parts(key, KEY_LEN) }.try_into().unwrap();
    match open(&dir, key, Wake(wake, user)) {
        Ok(core) => Box::into_raw(Box::new(core)),
        Err(reason) => {
            if !error.is_null() {
                unsafe { *error = c_string(&reason).into_raw() };
            }
            ptr::null_mut()
        }
    }
}

/// # Safety
/// `core` came from `gsc_open` and is not used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_close(core: *mut Core) {
    if !core.is_null() {
        drop(unsafe { Box::from_raw(core) });
    }
}

/// # Safety
/// `s` came from this library and has not been freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_string_free(s: *mut c_char) {
    if !s.is_null() {
        drop(unsafe { CString::from_raw(s) });
    }
}

/// # Safety
/// `core` came from `gsc_open`; the strings are NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_account_add(core: *mut Core, protocol: *const c_char, device_name: *const c_char) -> i64 {
    let core = unsafe { &mut *core };
    let protocol = unsafe { CStr::from_ptr(protocol) }.to_string_lossy();
    let device_name = unsafe { CStr::from_ptr(device_name) }.to_string_lossy();
    if protocol != "signal" {
        return -1;
    }
    let Ok(id) = core.runtime.block_on(core.shared.db.add_account(&protocol, &device_name)) else { return -1 };
    start(&core.shared, &mut core.commands, id, &protocol, &device_name, "");
    core.shared.push(Event::Account(id));
    id
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` accounts.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_accounts(core: *mut Core, out: *mut GscAccount, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let statuses = core.shared.statuses.lock().unwrap().clone();
    core.account_strings.clear();
    for (i, (id, s)) in statuses.iter().enumerate().take(max.max(0) as usize) {
        let account = GscAccount {
            id: *id,
            protocol: lend(&mut core.account_strings, s.protocol),
            label: lend(&mut core.account_strings, &s.label),
            state: s.state as c_int,
            last_received_ms: s.last_received_ms,
            undecryptable: s.undecryptable,
            error: lend(&mut core.account_strings, &s.error),
        };
        unsafe { out.add(i).write(account) };
    }
    statuses.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` conversations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_conversations(core: *mut Core, out: *mut GscConversation, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let all = core.runtime.block_on(core.shared.db.conversations()).unwrap_or_default();
    core.conversation_strings.clear();
    for (i, c) in all.iter().enumerate().take(max.max(0) as usize) {
        // (a conversation the service has given no name shows its identifier)
        let title = if c.title.is_empty() { &c.key } else { &c.title };
        let conversation = GscConversation {
            id: c.id,
            account: c.account,
            kind: c.kind as c_int,
            title: lend(&mut core.conversation_strings, title),
            last_ms: c.last_ms,
            last_text: lend(&mut core.conversation_strings, &c.last_text),
            unread: c.unread as c_int,
            muted: c.muted,
            request: c.request,
            avatar_ms: c.avatar_ms,
            person: c.person as c_int,
            last_sender: lend(&mut core.conversation_strings, &c.last_sender),
            admin: c.admin,
        };
        unsafe { out.add(i).write(conversation) };
    }
    all.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` messages.
#[unsafe(no_mangle)]
/// A message for the app, its strings lent from those kept for it, which this replaces.
fn shown(core: &mut Core, m: &db::Message) -> GscMessage {
    let mut keep = vec![];
    let message = GscMessage {
        id: m.id,
        outgoing: m.outgoing,
        sender: lend(&mut keep, &m.sender),
        sent_ms: m.sent_ms,
        text: lend(&mut keep, &m.text),
        status: m.status as c_int,
        expires_ms: m.expires_ms,
        kind: m.kind as c_int,
        edited: m.edited,
        reply_to: m.reply_to.as_deref().map_or(ptr::null(), |name| lend(&mut keep, name)),
        reply_text: lend(&mut keep, &m.reply_text),
        reactions: lend(&mut keep, &m.reactions),
        reacted: lend(&mut keep, &m.reacted),
        hidden: lend(&mut keep, &m.hidden),
        changing: m.changing as c_int,
        reaction: lend(&mut keep, &m.reaction),
        attachments: m.attachments as c_int,
        person: m.person as c_int,
        picture_ms: m.picture_ms,
        reply_id: m.reply_id,
        styles: lend(&mut keep, &m.styles),
    };
    core.message_strings.insert(m.id, keep);
    message
}

/// # Safety
/// `core` came from `gsc_open` and `out` points to one message.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_message_read(core: *mut Core, id: i64, out: *mut GscMessage) -> bool {
    let core = unsafe { &mut *core };
    match core.runtime.block_on(core.shared.db.message_shown(id)) {
        Ok(Some((_, m))) => {
            let message = shown(core, &m);
            unsafe { out.write(message) };
            true
        }
        _ => {
            core.message_strings.remove(&id);
            false
        }
    }
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` messages.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_messages(core: *mut Core, conversation: i64, before: i64, out: *mut GscMessage, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let messages = core.runtime.block_on(core.shared.db.messages(conversation, before, max.max(0) as i64)).unwrap_or_default();
    // (a read of older messages adds to what the app holds, so the strings it has stay)
    if before == 0 {
        core.message_strings.clear();
    }
    for (i, m) in messages.iter().enumerate() {
        let message = shown(core, m);
        unsafe { out.add(i).write(message) };
    }
    messages.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open` and `len` points to a writable size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_avatar(core: *mut Core, conversation: i64, len: *mut usize) -> *const u8 {
    let core = unsafe { &mut *core };
    let Ok(Some(bytes)) = core.runtime.block_on(core.shared.db.avatar(conversation)) else { return ptr::null() };
    core.avatar_bytes = bytes;
    unsafe { len.write(core.avatar_bytes.len()) };
    core.avatar_bytes.as_ptr()
}

/// # Safety
/// `core` came from `gsc_open` and `len` points to a writable size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_writer_picture(core: *mut Core, message: i64, len: *mut usize) -> *const u8 {
    let core = unsafe { &mut *core };
    let Ok(Some(bytes)) = core.runtime.block_on(core.shared.db.writer_picture(message)) else { return ptr::null() };
    core.avatar_bytes = bytes;
    unsafe { len.write(core.avatar_bytes.len()) };
    core.avatar_bytes.as_ptr()
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_note_to_self(core: *mut Core, account: i64) -> i64 {
    let core = unsafe { &mut *core };
    core.runtime.block_on(core.shared.db.note_to_self(account)).ok().flatten().unwrap_or(0)
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_set_timer(core: *mut Core, conversation: i64, seconds: c_int) {
    let core = unsafe { &mut *core };
    let Ok(Some((account, _, _))) = core.runtime.block_on(core.shared.db.conversation_key(conversation)) else { return };
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::SetTimer { conversation, seconds: seconds.max(0) as u32 });
    }
}

/// The disappearing timer a conversation was last known to have, in seconds; 0 for none.
///
/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_timer(core: *mut Core, conversation: i64) -> c_int {
    let core = unsafe { &mut *core };
    core.runtime.block_on(core.shared.db.timer(conversation)).unwrap_or(0) as c_int
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_mute(core: *mut Core, conversation: i64, muted: bool) {
    let core = unsafe { &mut *core };
    if core.runtime.block_on(core.shared.db.set_muted(conversation, muted)).is_ok() {
        core.shared.push(Event::Conversations);
    }
}

/// # Safety
/// `core` came from `gsc_open`, and `text` and `files` are NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_draft_set(core: *mut Core, conversation: i64, text: *const c_char, files: *const c_char) {
    let core = unsafe { &mut *core };
    let (text, files) = unsafe { (CStr::from_ptr(text).to_string_lossy(), CStr::from_ptr(files).to_string_lossy()) };
    let _ = core.runtime.block_on(core.shared.db.set_draft(conversation, &text, &files));
}

/// # Safety
/// `core` came from `gsc_open` and `files` points to a writable pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_draft(core: *mut Core, conversation: i64, files: *mut *const c_char) -> *const c_char {
    let core = unsafe { &mut *core };
    let (text, paths) = core.runtime.block_on(core.shared.db.draft(conversation)).unwrap_or_default();
    core.draft_strings.clear();
    unsafe { files.write(lend(&mut core.draft_strings, &paths)) };
    lend(&mut core.draft_strings, &text)
}

/// Whether the updater looks for updates; off until the user agrees.
#[unsafe(no_mangle)]
pub extern "C" fn gsc_update_enable(on: bool) {
    update::enable(on);
}

/// The build this program is, as the update server names it.
#[unsafe(no_mangle)]
pub extern "C" fn gsc_build() -> *const c_char {
    static BUILD: std::sync::OnceLock<CString> = std::sync::OnceLock::new();
    BUILD.get_or_init(|| c_string(update::build())).as_ptr()
}

/// # Safety
/// `core` came from `gsc_open` and `folder` is NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_update_start(core: *mut Core, folder: *const c_char) {
    let core = unsafe { &mut *core };
    update::start(core.shared.clone(), PathBuf::from(unsafe { CStr::from_ptr(folder) }.to_string_lossy().into_owned()));
}

/// The window's state for the use reported with each look for updates: 0 hidden, 1 shown behind
/// others, 2 in front; `input` when the user just pressed a key or used the pointer in it.
#[unsafe(no_mangle)]
pub extern "C" fn gsc_window_state(state: c_int, input: bool) {
    update::window(state, input);
}

/// Whether the look for updates reports the installation's use.
#[unsafe(no_mangle)]
pub extern "C" fn gsc_report_use(on: bool) {
    update::report(on);
}

/// # Safety
/// `dir` is NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_log_start(dir: *const c_char) {
    log::start(&PathBuf::from(unsafe { CStr::from_ptr(dir) }.to_string_lossy().into_owned()));
}

/// # Safety
/// `text` is NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_log(text: *const c_char) {
    log::line(&unsafe { CStr::from_ptr(text) }.to_string_lossy());
}

/// # Safety
/// `core` came from `gsc_open` and `key` is NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_remove_member(core: *mut Core, conversation: i64, key: *const c_char) {
    let core = unsafe { &mut *core };
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy().into_owned();
    let Ok(Some((account, _, _))) = core.runtime.block_on(core.shared.db.conversation_key(conversation)) else { return };
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::RemoveMember { conversation, key });
    }
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_accept(core: *mut Core, conversation: i64) {
    let core = unsafe { &mut *core };
    let Ok(Some((account, key, kind))) = core.runtime.block_on(core.shared.db.conversation_key(conversation)) else { return };
    // (accepted here at once, online or not; the account's other devices are told when it is online)
    if kind != 0 || !core.runtime.block_on(core.shared.db.accept_person(account, &key)).unwrap_or(false) {
        return;
    }
    core.shared.push(Event::Conversations);
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::Accept { conversation });
    }
}

/// # Safety
/// `core` came from `gsc_open`, `query` is NUL-terminated UTF-8, and `out` has room for `max` hits.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_search(core: *mut Core, query: *const c_char, out: *mut GscHit, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let query = unsafe { CStr::from_ptr(query) }.to_string_lossy();
    let hits = if query.is_empty() { vec![] } else { core.runtime.block_on(core.shared.db.search(&query, max.max(0) as i64)).unwrap_or_default() };
    core.hit_strings.clear();
    for (i, h) in hits.iter().enumerate() {
        let hit = GscHit {
            message: h.message,
            conversation: h.conversation,
            title: lend(&mut core.hit_strings, &h.title),
            outgoing: h.outgoing,
            sender: lend(&mut core.hit_strings, &h.sender),
            sent_ms: h.sent_ms,
            text: lend(&mut core.hit_strings, &h.text),
            newer: h.newer as c_int,
        };
        unsafe { out.add(i).write(hit) };
    }
    hits.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` attachments.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_attachments(core: *mut Core, message: i64, out: *mut GscAttachment, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let all = core.runtime.block_on(core.shared.db.attachments(message)).unwrap_or_default();
    core.attachment_strings.clear();
    for (i, a) in all.iter().enumerate().take(max.max(0) as usize) {
        let attachment = GscAttachment {
            id: a.id,
            content_type: lend(&mut core.attachment_strings, &a.content_type),
            name: lend(&mut core.attachment_strings, &a.file_name),
            size: a.size,
            width: a.width as c_int,
            height: a.height as c_int,
            voice: a.voice,
            looped: a.looped,
            state: a.state as c_int,
        };
        unsafe { out.add(i).write(attachment) };
    }
    all.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_attachment_fetch(core: *mut Core, attachment: i64) {
    let core = unsafe { &mut *core };
    if core.runtime.block_on(core.shared.db.want_attachment(attachment)).is_ok() {
        core.shared.fetch.notify_one();
    }
}

/// # Safety
/// `core` came from `gsc_open` and `len` points to a writable size.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_attachment_data(core: *mut Core, attachment: i64, len: *mut usize) -> *const u8 {
    let core = unsafe { &mut *core };
    let Ok(Some(bytes)) = core.runtime.block_on(core.shared.db.attachment_bytes(attachment)) else { return ptr::null() };
    core.attachment_bytes = bytes;
    unsafe { len.write(core.attachment_bytes.len()) };
    core.attachment_bytes.as_ptr()
}

/// # Safety
/// `core` came from `gsc_open` and `path` is a NUL-terminated UTF-8 path.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_attachment_save(core: *mut Core, attachment: i64, path: *const c_char) -> bool {
    let core = unsafe { &mut *core };
    let path = unsafe { CStr::from_ptr(path) }.to_string_lossy().into_owned();
    match core.runtime.block_on(core.shared.db.attachment_bytes(attachment)) {
        Ok(Some(bytes)) => std::fs::write(path, bytes).is_ok(),
        _ => false,
    }
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` members.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_members(core: *mut Core, conversation: i64, out: *mut GscMember, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let all = core.runtime.block_on(core.shared.db.members(conversation)).unwrap_or_default();
    core.member_strings.clear();
    for (i, (key, name)) in all.iter().enumerate().take(max.max(0) as usize) {
        let member = GscMember { key: lend(&mut core.member_strings, key), name: lend(&mut core.member_strings, name) };
        unsafe { out.add(i).write(member) };
    }
    all.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open` and `out` has room for `max` faces.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_faces(core: *mut Core, conversation: i64, out: *mut GscFace, max: c_int) -> c_int {
    let core = unsafe { &mut *core };
    let since = shared::now_ms() - 30 * 86_400_000;
    let faces = core.runtime.block_on(core.shared.db.faces(conversation, since, max.max(0) as i64)).unwrap_or_default();
    core.face_strings.clear();
    for (i, (key, name, person, picture_ms)) in faces.iter().enumerate() {
        let face = GscFace { key: lend(&mut core.face_strings, key), name: lend(&mut core.face_strings, name), person: *person as c_int, picture_ms: *picture_ms };
        unsafe { out.add(i).write(face) };
    }
    faces.len() as c_int
}

/// # Safety
/// `core` came from `gsc_open`, `key` is NUL-terminated UTF-8 and `len` is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_face_picture(core: *mut Core, conversation: i64, key: *const c_char, len: *mut usize) -> *const u8 {
    let core = unsafe { &mut *core };
    let key = unsafe { CStr::from_ptr(key) }.to_string_lossy().into_owned();
    let Ok(Some(bytes)) = core.runtime.block_on(core.shared.db.person_picture(conversation, &key)) else { return ptr::null() };
    core.avatar_bytes = bytes;
    unsafe { len.write(core.avatar_bytes.len()) };
    core.avatar_bytes.as_ptr()
}

/// # Safety
/// `core` came from `gsc_open`, `text` is NUL-terminated UTF-8, `files` points to `nfiles`
/// NUL-terminated UTF-8 paths, and `mentions` to `nmentions` mentions with NUL-terminated keys.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_send(core: *mut Core, conversation: i64, text: *const c_char, reply_to: i64, files: *const *const c_char, nfiles: c_int, mentions: *const GscMention, nmentions: c_int) {
    let core = unsafe { &mut *core };
    let mentions: Vec<(String, usize, usize)> = (0..nmentions.max(0) as usize)
        .map(|i| unsafe { &*mentions.add(i) })
        .map(|m| (unsafe { CStr::from_ptr(m.key) }.to_string_lossy().into_owned(), m.start.max(0) as usize, m.length.max(0) as usize))
        .collect();
    let files: Vec<String> = (0..nfiles.max(0) as usize).map(|i| unsafe { CStr::from_ptr(*files.add(i)) }.to_string_lossy().into_owned()).collect();
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned();
    if text.is_empty() && files.is_empty() {
        return;
    }
    let (shared, db) = (&core.shared, &core.shared.db);
    let Ok(Some((account, _, _))) = core.runtime.block_on(db.conversation_key(conversation)) else { return };
    // The message is stored whole before anything is sent, its files with it, so that it outlasts
    // being offline, a sending that fails, and the program closing.
    let stored: Result<Vec<i64>, String> = core.runtime.block_on(async {
        let mut read = vec![];
        for path in &files {
            let bytes = std::fs::read(path).map_err(|e| format!("{path}: {e}"))?;
            let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            // A photo or video goes without where, when and with what it was taken; one that cannot
            // be gone through is not sent, since it may still say those things.
            let bytes = strip::strip(bytes, signal::content_type(&name).starts_with("video/")).ok_or_else(|| format!("{name} was not sent: it is damaged, and what it says of where it was taken could not be removed"))?;
            read.push((name, bytes));
        }
        let replied = match reply_to {
            0 => None,
            id => db.message(id).await.map_err(|e| e.to_string())?.filter(|t| t.conversation == conversation),
        };
        let quote = replied.as_ref().map(|t| (t.sender.as_str(), &t.text[..t.text.floor_char_boundary(signal::QUOTE_MAX)], t.sent_ms));
        let named: String = mentions.iter().map(|(key, start, length)| format!("{key}\t{start}\t{length}\n")).collect();
        // Signal's own programs show the pictures and videos of a message together, and of any
        // other file only the first. So those go together, with the text, and every other file in a
        // message of its own after them, as Signal's programs send them; the text goes with the
        // first file when there are no pictures or videos.
        let media = |name: &str| ["image/", "video/"].iter().any(|kind| signal::content_type(name).starts_with(kind));
        let mut groups: Vec<Vec<&(String, Vec<u8>)>> = vec![read.iter().filter(|(name, _)| media(name)).collect()];
        groups.extend(read.iter().filter(|(name, _)| !media(name)).map(|file| vec![file]));
        if groups[0].is_empty() && groups.len() > 1 {
            groups.remove(0);
        }
        let mut stored = vec![];
        let now = shared::now_ms();
        for (i, group) in groups.iter().enumerate() {
            // (each is its own message, a millisecond apart, by which its receivers know it and keep the order)
            let (body, quote, named) = if i == 0 { (text.as_str(), quote, named.as_str()) } else { ("", None, "") };
            let message = db.enqueue(conversation, body, now + i as i64, quote, named).await.map_err(|e| e.to_string())?;
            for (name, bytes) in group {
                let (width, height) = signal::image_size(bytes).unwrap_or((0, 0));
                let new = db::NewAttachment { message, content_type: signal::content_type(name), file_name: name, size: bytes.len() as i64, width, height, voice: false, looped: false, pointer: None, bytes: Some(bytes) };
                db.add_attachment(&new).await.map_err(|e| e.to_string())?;
            }
            stored.push(message);
        }
        Ok(stored)
    });
    match stored {
        Ok(messages) => {
            for message in messages {
                shared.push(Event::Message { account, conversation, message });
            }
            flush(core, account);
        }
        Err(error) => shared.update(account, |s| s.error = error),
    }
}

/// Tells the account's backend that messages wait to be sent; offline, it finds them when it is back.
fn flush(core: &Core, account: i64) {
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::Flush);
    }
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_resend(core: *mut Core, message: i64) {
    let core = unsafe { &mut *core };
    if let Ok(Some((account, conversation))) = core.runtime.block_on(core.shared.db.send_again(message)) {
        core.shared.push(Event::MessageChanged { account, conversation, message });
        flush(core, account);
    }
}

fn change(core: &mut Core, message: i64, change: Change) {
    let Ok(Some(target)) = core.runtime.block_on(core.shared.db.message(message)) else { return };
    // The change is kept before it is sent, like a message, so that one asked for while offline is
    // sent when the account is back. It shows in the conversation once it has been sent.
    let (what, value) = match change {
        Change::React(emoji) => (0, emoji),
        Change::Edit(text) => (1, text),
        Change::Delete => (2, String::new()),
    };
    if core.runtime.block_on(core.shared.db.add_change(message, what, &value)).is_ok() {
        core.shared.push(Event::MessageChanged { account: target.account, conversation: target.conversation, message });
        flush(core, target.account);
    }
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_changes_again(core: *mut Core, message: i64, again: bool) {
    let core = unsafe { &mut *core };
    if let Ok(Some((account, conversation))) = core.runtime.block_on(core.shared.db.changes_again(message, again)) {
        core.shared.push(Event::MessageChanged { account, conversation, message });
        if again {
            flush(core, account);
        }
    }
}

/// # Safety
/// `core` came from `gsc_open` and `emoji` is NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_react(core: *mut Core, message: i64, emoji: *const c_char) {
    let emoji = unsafe { CStr::from_ptr(emoji) }.to_string_lossy().into_owned();
    change(unsafe { &mut *core }, message, Change::React(emoji));
}

/// # Safety
/// `core` came from `gsc_open` and `text` is NUL-terminated UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_edit(core: *mut Core, message: i64, text: *const c_char) {
    let text = unsafe { CStr::from_ptr(text) }.to_string_lossy().into_owned();
    change(unsafe { &mut *core }, message, Change::Edit(text));
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_delete(core: *mut Core, message: i64) {
    let core = unsafe { &mut *core };
    // (one that was never sent is in no one else's hands, so it is simply removed)
    if let Ok(Some((account, conversation))) = core.runtime.block_on(core.shared.db.discard(message)) {
        return core.shared.push(Event::MessageChanged { account, conversation, message });
    }
    change(core, message, Change::Delete);
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_mark_read(core: *mut Core, conversation: i64) {
    let core = unsafe { &mut *core };
    let Ok(Some((account, _, _))) = core.runtime.block_on(core.shared.db.conversation_key(conversation)) else { return };
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::MarkRead { conversation });
    }
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_typing(core: *mut Core, conversation: i64, typing: bool) {
    let core = unsafe { &mut *core };
    let Ok(Some((account, key, kind))) = core.runtime.block_on(core.shared.db.conversation_key(conversation)) else { return };
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::Typing { key, kind, started: typing });
    }
}

/// # Safety
/// `core` came from `gsc_open`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_reset_session(core: *mut Core, conversation: i64) {
    let core = unsafe { &mut *core };
    let Ok(Some((account, key, _))) = core.runtime.block_on(core.shared.db.conversation_key(conversation)) else { return };
    if let Some(commands) = core.commands.get(&account) {
        let _ = commands.send(Command::ResetSession { key });
    }
}

/// # Safety
/// `core` came from `gsc_open` and `out` points to a writable event.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn gsc_event_next(core: *mut Core, out: *mut GscEvent) -> bool {
    let core = unsafe { &mut *core };
    let Some(event) = core.shared.events.lock().unwrap().pop_front() else { return false };
    let mut e = GscEvent { kind: 0, account: 0, conversation: 0, message: 0, text: c"".as_ptr(), code: ptr::null(), code_size: 0, typing: false };
    match event {
        Event::Account(account) => (e.kind, e.account) = (0, account),
        Event::SignInCode { account, link, modules, size } => {
            core.event_text = c_string(&link);
            core.event_code = modules;
            (e.kind, e.account, e.text, e.code, e.code_size) = (1, account, core.event_text.as_ptr(), core.event_code.as_ptr(), size);
        }
        Event::Conversations => e.kind = 2,
        Event::Message { account, conversation, message } => (e.kind, e.account, e.conversation, e.message) = (3, account, conversation, message),
        Event::ConversationChanged { account, conversation } => (e.kind, e.account, e.conversation) = (4, account, conversation),
        Event::Typing { account, conversation, name, started } => {
            core.event_text = c_string(&name);
            (e.kind, e.account, e.conversation, e.text, e.typing) = (5, account, conversation, core.event_text.as_ptr(), started);
        }
        Event::Updated => e.kind = 6,
        Event::MessageChanged { account, conversation, message } => (e.kind, e.account, e.conversation, e.message) = (7, account, conversation, message),
        Event::Listed { account, conversation } => (e.kind, e.account, e.conversation) = (8, account, conversation),
    }
    unsafe { out.write(e) };
    true
}
