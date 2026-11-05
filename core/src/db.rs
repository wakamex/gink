//! The message database: accounts, conversations and messages for every protocol. A backend keeps
//! its own protocol state elsewhere and refers to rows here by id.

use std::path::Path;

use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode};
use sqlx::{QueryBuilder, Row, Sqlite, SqlitePool};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS accounts (
    id INTEGER PRIMARY KEY,
    protocol TEXT NOT NULL,
    device_name TEXT NOT NULL,
    label TEXT NOT NULL DEFAULT ''
);
CREATE TABLE IF NOT EXISTS conversations (
    id INTEGER PRIMARY KEY,
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    key TEXT NOT NULL,              -- the backend's identifier for it
    kind INTEGER NOT NULL,          -- 0 direct, 1 group
    title TEXT NOT NULL DEFAULT '',
    UNIQUE (account_id, key)
);
CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    outgoing INTEGER NOT NULL,
    sender TEXT NOT NULL,           -- the backend's identifier for the sender; '' when outgoing
    sender_name TEXT NOT NULL DEFAULT '',
    sent_ms INTEGER NOT NULL,
    received_ms INTEGER NOT NULL,
    text TEXT NOT NULL,
    UNIQUE (conversation_id, sender, sent_ms)
);
CREATE INDEX IF NOT EXISTS messages_by_conversation ON messages (conversation_id, sent_ms);
";

/// Changes to the schema above, in order. The database's `user_version` counts those applied.
const MIGRATIONS: &[&str] = &[
    // People are named when a message is shown, so a name that changes is right everywhere. Messages
    // gain what unread counts, receipts and disappearing messages need.
    "
CREATE TABLE people (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    key TEXT NOT NULL,              -- the backend's identifier for the person
    name TEXT NOT NULL,
    PRIMARY KEY (account_id, key)
);
INSERT INTO people (account_id, key, name)
    SELECT c.account_id, m.sender, MAX(m.sender_name) FROM messages m JOIN conversations c ON c.id = m.conversation_id
    WHERE m.sender != '' GROUP BY c.account_id, m.sender;
ALTER TABLE messages DROP COLUMN sender_name;
ALTER TABLE messages ADD COLUMN status INTEGER NOT NULL DEFAULT 0;   -- outgoing: 0 sent, 1 delivered, 2 read
ALTER TABLE messages ADD COLUMN read_ms INTEGER;                     -- incoming: when it was read; NULL while unread
ALTER TABLE messages ADD COLUMN expire_s INTEGER NOT NULL DEFAULT 0; -- its disappearing timer; 0 for none
ALTER TABLE messages ADD COLUMN expires_ms INTEGER;                  -- when it disappears; NULL until the timer starts
UPDATE messages SET read_ms = received_ms WHERE outgoing = 0;
CREATE INDEX messages_by_expiry ON messages (expires_ms) WHERE expires_ms IS NOT NULL;
",
    // What the account's owner chose about telling other people; NULL until the account says.
    "
ALTER TABLE accounts ADD COLUMN read_receipts INTEGER;
ALTER TABLE accounts ADD COLUMN typing INTEGER;
",
    // Notices the core writes into a conversation, and the key each person was last seen with.
    "
ALTER TABLE messages ADD COLUMN kind INTEGER NOT NULL DEFAULT 0;     -- 0 text, 1 undecryptable, 2 identity changed
ALTER TABLE people ADD COLUMN identity BLOB;
",
    // The version of the account's shared records that was last read.
    "ALTER TABLE accounts ADD COLUMN storage_version INTEGER;",
    // When a new session may next be started with a person by the core itself.
    "ALTER TABLE people ADD COLUMN repair_after_ms INTEGER NOT NULL DEFAULT 0;",
    // Edits, replies, deletions (kind 3, with no text) and reactions.
    "
ALTER TABLE messages ADD COLUMN edit_ms INTEGER;     -- when its latest edit was sent; NULL when never edited
ALTER TABLE messages ADD COLUMN quote_sender TEXT;   -- who wrote what it replies to ('' for the account); NULL when it replies to nothing
ALTER TABLE messages ADD COLUMN quote_text TEXT;
CREATE TABLE reactions (
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    sender TEXT NOT NULL,           -- '' for the account
    emoji TEXT NOT NULL,
    PRIMARY KEY (message_id, sender)
);
",
    // Files that come with messages, kept in the database so that they are encrypted like the rest.
    "
CREATE TABLE attachments (
    id INTEGER PRIMARY KEY,
    message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE,
    content_type TEXT NOT NULL,
    file_name TEXT NOT NULL,
    size INTEGER NOT NULL,
    width INTEGER NOT NULL DEFAULT 0,    -- of an image; 0 when unknown
    height INTEGER NOT NULL DEFAULT 0,
    voice INTEGER NOT NULL DEFAULT 0,    -- a voice note
    pointer BLOB,                        -- what the backend fetches it with; NULL for one sent from here
    attempts INTEGER NOT NULL DEFAULT 0, -- fetches that failed
    bytes BLOB                           -- NULL until fetched
);
CREATE INDEX attachments_by_message ON attachments (message_id);
",
    // A muted conversation raises no notifications.
    "ALTER TABLE conversations ADD COLUMN muted INTEGER NOT NULL DEFAULT 0;",
    // Who is in each group, apart from the account, for naming them in a message.
    "
CREATE TABLE members (
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    key TEXT NOT NULL,
    PRIMARY KEY (conversation_id, key)
);
",
    // An attachment the user asked for is fetched whatever its size.
    "ALTER TABLE attachments ADD COLUMN wanted INTEGER NOT NULL DEFAULT 0;",
    // What the account sent lately, as it was sent, for sending again to someone who could not decrypt it.
    "
CREATE TABLE sent_log (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    sent_ms INTEGER NOT NULL,
    form INTEGER NOT NULL,          -- what content holds: 0 a message, 1 an edit
    content BLOB NOT NULL,
    PRIMARY KEY (account_id, sent_ms)
);
",
    // The backend's identifier for the account itself, whose conversation is its notes to itself.
    "ALTER TABLE accounts ADD COLUMN self_key TEXT;",
    // Each conversation's picture as its file's bytes; NULL when it was asked for and has none.
    "
CREATE TABLE avatars (
    conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE,
    bytes BLOB,
    fetched_ms INTEGER NOT NULL,    -- when the picture last changed, which is what a shown copy is known by
    asked_ms INTEGER NOT NULL       -- when the service was last asked for it
);
",
    // A video sent as a moving picture: to be played silent and over again, as Signal shows a GIF.
    "ALTER TABLE attachments ADD COLUMN looped INTEGER NOT NULL DEFAULT 0;",
    // Pictures belong to people and groups, not to conversations, so that a person's shows beside
    // what they write in a group. And each person who writes gets a number, in the order they were
    // first heard from, which the window turns into a colour that no one else has.
    "
CREATE TABLE pictures (
    account_id INTEGER NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
    key TEXT NOT NULL,              -- a person's or a group's, as conversations and messages have it
    bytes BLOB,
    fetched_ms INTEGER NOT NULL,
    asked_ms INTEGER NOT NULL,
    PRIMARY KEY (account_id, key)
);
INSERT INTO pictures SELECT c.account_id, c.key, a.bytes, a.fetched_ms, a.asked_ms FROM avatars a JOIN conversations c ON c.id = a.conversation_id;
DROP TABLE avatars;
ALTER TABLE people ADD COLUMN colour INTEGER;
INSERT OR IGNORE INTO people (account_id, key, name)
    SELECT DISTINCT c.account_id, m.sender, '' FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE m.sender != '';
UPDATE people SET colour = (
    SELECT COUNT(*) FROM (SELECT m.sender s, MIN(m.id) first FROM messages m JOIN conversations c ON c.id = m.conversation_id
                          WHERE c.account_id = people.account_id AND m.sender != '' GROUP BY m.sender) earlier
    WHERE earlier.first < (SELECT MIN(m.id) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.account_id = people.account_id AND m.sender = people.key))
WHERE EXISTS (SELECT 1 FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.account_id = people.account_id AND m.sender = people.key);
",
    // Groups take numbers from the same count as people, so that a group's colour is no person's.
    "
ALTER TABLE conversations ADD COLUMN colour INTEGER;
UPDATE conversations SET colour = (SELECT COALESCE(MAX(p.colour), -1) + 1 FROM people p WHERE p.account_id = conversations.account_id)
    + (SELECT COUNT(*) FROM conversations g WHERE g.account_id = conversations.account_id AND g.kind = 1 AND g.id < conversations.id
       AND EXISTS (SELECT 1 FROM messages WHERE conversation_id = g.id))
WHERE kind = 1 AND EXISTS (SELECT 1 FROM messages WHERE conversation_id = conversations.id);
",
    // What the account writes is stored before it is sent, and sent from here: so a message written
    // while offline, or whose sending fails, is kept with all it needs to be sent later.
    "
ALTER TABLE messages ADD COLUMN unsent INTEGER NOT NULL DEFAULT 0;   -- outgoing: 0 sent, 1 waiting to be sent, 2 given up until asked again
ALTER TABLE messages ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0; -- sendings that failed
ALTER TABLE messages ADD COLUMN mentions TEXT;                       -- who its text names: a line each of key, start and length in bytes, tab between
ALTER TABLE messages ADD COLUMN quote_ms INTEGER;                    -- when the message it replies to was sent
",
    // Messages are read in the order they arrived, which is the order of their ids.
    "CREATE INDEX messages_in_order ON messages (conversation_id, id);",
    // Where the service keeps each picture, by which a changed picture is known without fetching it.
    "ALTER TABLE pictures ADD COLUMN path TEXT;",
    // A person who writes unasked is held apart until the account's owner accepts them: nothing
    // tells them their messages were read, or that an answer is being typed. Conversations that
    // exist are accepted; people are not until the account's records or its owner say so.
    "
ALTER TABLE conversations ADD COLUMN accepted INTEGER NOT NULL DEFAULT 1;
ALTER TABLE people ADD COLUMN accepted INTEGER NOT NULL DEFAULT 0;
",
    // A message to someone whose safety number changed is held (unsent 3) until the user says to
    // send it all the same, which this records.
    "ALTER TABLE messages ADD COLUMN trusted INTEGER NOT NULL DEFAULT 0;",
    // A name the account's owner gave someone stays; one the person chose for their profile is
    // replaced when their profile says another. Which is which is learnt from the account's
    // records, which are read again for it.
    "
ALTER TABLE people ADD COLUMN named_by_owner INTEGER NOT NULL DEFAULT 0;
UPDATE accounts SET storage_version = NULL;
",
    // What is being written in a conversation and not yet sent: its text, and the paths of the
    // files that are to go with it, a line each.
    "CREATE TABLE drafts (conversation_id INTEGER PRIMARY KEY REFERENCES conversations(id) ON DELETE CASCADE, text TEXT NOT NULL, files TEXT NOT NULL);",
    // A conversation's disappearing timer as last known, in seconds, for saying when it changes.
    "ALTER TABLE conversations ADD COLUMN timer_s INTEGER;",
    // The revision of a group's state that its conversation was last brought up to, so that an
    // older state, read earlier and applied late, does not take it back.
    "ALTER TABLE conversations ADD COLUMN revision INTEGER;",
    // A message with something its sender hid (a spoiler) is kept twice: its text with those parts
    // covered, which is what everything shows, and here as it was written, for when the user asks.
    "ALTER TABLE messages ADD COLUMN clear TEXT;",
    // A reaction, an edit or a deletion the user asked for and that has not been sent yet: 0 a
    // reaction with its emoji ("" to take it back), 1 an edit with the new text, 2 a deletion.
    "CREATE TABLE changes (id INTEGER PRIMARY KEY, message_id INTEGER NOT NULL REFERENCES messages(id) ON DELETE CASCADE, what INTEGER NOT NULL, value TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0);",
    // The client's store keeps each received message until the core has dealt with it. Before
    // this was so it kept them all, and those are cleared once, which this records.
    "ALTER TABLE accounts ADD COLUMN journal INTEGER NOT NULL DEFAULT 0;",
    // A change given up after its tries stays, marked, until the user sends it again or drops it.
    "ALTER TABLE changes ADD COLUMN given_up INTEGER NOT NULL DEFAULT 0;",
    // Messages the account sent from another device were dealt with a second time at a start, as
    // if someone else had written them under the account's own key, and stood as new messages at
    // the end of their conversations. Nobody else writes under that key: such rows go.
    "DELETE FROM messages WHERE outgoing = 0 AND kind = 0 AND sender != '' AND sender IN (SELECT self_key FROM accounts WHERE self_key IS NOT NULL);",
    // A reply is joined to the message it answers by when that was sent, as it was for those the
    // account wrote; for one received, that time is kept from here on.
    "CREATE INDEX messages_by_time ON messages (conversation_id, sent_ms);",
    // Whether the account is an administrator of a group, which may change who is in it.
    "ALTER TABLE conversations ADD COLUMN admin INTEGER NOT NULL DEFAULT 0;",
    // The styles its sender gave parts of a message's text: bold, italic, struck through, monospace.
    "ALTER TABLE messages ADD COLUMN styles TEXT;",
    // When the account's contacts were last read whole from its primary device; 0 until they have been.
    "ALTER TABLE accounts ADD COLUMN contacts_ms INTEGER NOT NULL DEFAULT 0;",
];

pub struct Account {
    pub id: i64,
    pub protocol: String,
    pub device_name: String,
    pub label: String,
}

pub struct Conversation {
    pub id: i64,
    pub account: i64,
    pub key: String,
    pub kind: i64,
    pub title: String,
    pub last_ms: i64,
    pub last_text: String,
    /// In a group, who wrote the newest message ("me" for the account); "" elsewhere.
    pub last_sender: String,
    pub unread: i64,
    pub muted: bool,
    /// Whether it is from someone who wrote unasked and has not been accepted.
    pub request: bool,
    /// When its picture was fetched; 0 when it has none.
    pub avatar_ms: i64,
    /// The number its colour goes by: for a conversation with one person, that person's, as their
    /// messages carry it, and for a group one of its own from the same count; -1 for the account's
    /// own notes and for a conversation nothing has been written in.
    pub person: i64,
    /// For a group, whether the account is one of its administrators.
    pub admin: bool,
}

/// A message found by `Db::search`.
pub struct Hit {
    pub message: i64,
    pub conversation: i64,
    pub title: String,
    pub outgoing: bool,
    pub sender: String,
    pub sent_ms: i64,
    pub text: String,
    /// How many messages of its conversation are newer than it.
    pub newer: i64,
}

pub struct Message {
    pub id: i64,
    pub outgoing: bool,
    pub sender: String,
    pub sent_ms: i64,
    pub text: String,
    /// For outgoing messages: 0 sent, 1 delivered, 2 read; -1 waiting to be sent, -2 not sent after
    /// several tries, -3 held because the safety number of someone it goes to changed.
    pub status: i64,
    pub expires_ms: i64,
    pub kind: i64,
    pub edited: bool,
    /// Who wrote what it replies to: None when it replies to nothing, a name otherwise ("" for the account).
    pub reply_to: Option<String>,
    pub reply_text: String,
    /// Each emoji it was given, with how many gave it when more than one.
    pub reactions: String,
    /// The same with who gave each in place of how many: "me" for the account.
    pub reacted: String,
    /// The text as it was written, when `text` has parts of it covered that its sender hid; else "".
    pub hidden: String,
    /// A change the user asked of it: 0 none, 1 one waits to be sent, 2 one was given up.
    pub changing: i64,
    /// The message it replies to, when that is held; 0 when it is not, or it replies to nothing.
    pub reply_id: i64,
    /// The styles its sender gave parts of its text, a line "STYLE START END" each, counted in characters.
    pub styles: String,
    /// The emoji the account gave it, or "".
    pub reaction: String,
    /// How many files came with it.
    pub attachments: i64,
    /// The number of the person who wrote it, by which the window colours their name; -1 for the account.
    pub person: i64,
    /// When that person's picture last changed; 0 when they have none.
    pub picture_ms: i64,
}

/// A fetch is given up after this many failures.
const FETCH_ATTEMPTS: i64 = 3;

/// A file that came with a message. `state` is 0 while it waits to be fetched, 1 when its bytes are
/// held, 2 when fetching it was given up.
pub struct Attachment {
    pub id: i64,
    pub content_type: String,
    pub file_name: String,
    pub size: i64,
    pub width: i64,
    pub height: i64,
    pub voice: bool,
    /// A video meant to play silent and over again.
    pub looped: bool,
    pub state: i64,
}

/// A message that waits to be sent, with what sending it needs.
pub struct Waiting {
    pub id: i64,
    pub account: i64,
    pub conversation: i64,
    pub key: String,
    pub kind: i64,
    pub text: String,
    pub sent_ms: i64,
    /// Who wrote what it replies to ("" for the account); None when it replies to nothing.
    pub quote_sender: Option<String>,
    pub quote_text: String,
    pub quote_ms: i64,
    /// A line for each person its text names: key, start and length in bytes, with tabs between.
    pub mentions: String,
    /// The user said to send it although the safety number of someone it goes to changed.
    pub trusted: bool,
}

/// A new attachment for `Db::add_attachment`: fetched later with `pointer`, or held as `bytes`.
pub struct NewAttachment<'a> {
    pub message: i64,
    pub content_type: &'a str,
    pub file_name: &'a str,
    pub size: i64,
    pub width: i64,
    pub height: i64,
    pub voice: bool,
    pub looped: bool,
    pub pointer: Option<&'a [u8]>,
    pub bytes: Option<&'a [u8]>,
}

/// A stored message as a backend needs it to act on it.
pub struct Target {
    pub id: i64,
    pub account: i64,
    pub conversation: i64,
    pub key: String,
    pub kind: i64,
    /// Who wrote it; "" for the account.
    pub sender: String,
    pub sent_ms: i64,
    /// When its latest version was sent, which is how an edit names it.
    pub latest_ms: i64,
    pub text: String,
}

/// A new message for `Db::add_message`.
pub struct NewMessage<'a> {
    pub conversation: i64,
    pub outgoing: bool,
    pub sender: &'a str,
    pub sent_ms: i64,
    pub received_ms: i64,
    pub text: &'a str,
    /// 0 for a message someone wrote; otherwise which notice of the core's it is.
    pub kind: i64,
    /// What it replies to: who wrote that ("" for the account) and its text.
    pub quote: Option<(&'a str, &'a str, i64)>,
    /// The disappearing timer it was sent with, in seconds; 0 for none.
    pub expire_s: i64,
    /// When the timer started, for a message already read or sent; None for one not yet read.
    pub expire_start_ms: Option<i64>,
}

#[derive(Clone)]
pub struct Db(SqlitePool);

/// SQLCipher's form for a raw key that is used as it is, without key derivation.
pub fn key_pragma(key: &[u8; 32]) -> String {
    format!("\"x'{}'\"", hex::encode(key))
}

impl Db {
    pub async fn open(path: &Path, key: &[u8; 32]) -> sqlx::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true)
            .foreign_keys(true)
            .pragma("key", key_pragma(key))
            .pragma("cache_size", "-16384")
            .journal_mode(SqliteJournalMode::Wal);
        let pool = SqlitePool::connect_with(options).await?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await?;
        let applied: i64 = sqlx::query_scalar("PRAGMA user_version").fetch_one(&pool).await?;
        for (i, migration) in MIGRATIONS.iter().enumerate().skip(applied as usize) {
            let mut tx = pool.begin().await?;
            // (both strings are built from constants in this file)
            sqlx::raw_sql(*migration).execute(&mut *tx).await?;
            sqlx::raw_sql(sqlx::AssertSqlSafe(format!("PRAGMA user_version = {}", i + 1))).execute(&mut *tx).await?;
            tx.commit().await?;
        }
        Ok(Self(pool))
    }

    /// Records a person's name. An empty name leaves the stored one. Returns whether it changed.
    pub async fn person(&self, account: i64, key: &str, name: &str) -> sqlx::Result<bool> {
        if name.is_empty() {
            return Ok(false);
        }
        let done = sqlx::query("INSERT INTO people (account_id, key, name) VALUES (?1, ?2, ?3) ON CONFLICT DO UPDATE SET name = ?3 WHERE name != ?3")
            .bind(account)
            .bind(key)
            .bind(name)
            .execute(&self.0)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn accounts(&self) -> sqlx::Result<Vec<Account>> {
        let rows = sqlx::query("SELECT id, protocol, device_name, label FROM accounts ORDER BY id").fetch_all(&self.0).await?;
        Ok(rows.iter().map(|r| Account { id: r.get(0), protocol: r.get(1), device_name: r.get(2), label: r.get(3) }).collect())
    }

    pub async fn add_account(&self, protocol: &str, device_name: &str) -> sqlx::Result<i64> {
        let done = sqlx::query("INSERT INTO accounts (protocol, device_name) VALUES (?, ?)").bind(protocol).bind(device_name).execute(&self.0).await?;
        Ok(done.last_insert_rowid())
    }

    pub async fn set_account_label(&self, id: i64, label: &str) -> sqlx::Result<()> {
        sqlx::query("UPDATE accounts SET label = ? WHERE id = ?").bind(label).bind(id).execute(&self.0).await?;
        Ok(())
    }

    /// What the account should have pictures of and has not asked about since `before_ms`: the
    /// conversations that hold messages and the people who wrote in them, as (key, kind).
    pub async fn avatars_wanted(&self, account: i64, before_ms: i64) -> sqlx::Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "SELECT key, kind FROM (
                 SELECT c.key key, c.kind kind FROM conversations c WHERE c.account_id = ?1 AND EXISTS (SELECT 1 FROM messages WHERE conversation_id = c.id)
                 UNION SELECT DISTINCT m.sender, 0 FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.account_id = ?1 AND m.sender != '') wanted
             WHERE NOT EXISTS (SELECT 1 FROM pictures p WHERE p.account_id = ?1 AND p.key = wanted.key AND p.asked_ms >= ?2)",
        )
        .bind(account)
        .bind(before_ms)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Where the service kept a person's or a group's picture when it was last asked, and when
    /// that was; None for one never asked about, and no address for one fetched before addresses were kept.
    pub async fn picture_asked(&self, account: i64, key: &str) -> sqlx::Result<Option<(Option<String>, i64)>> {
        let row = sqlx::query("SELECT path, asked_ms FROM pictures WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// Notes that the service was asked at `now_ms` and the picture's address had not changed.
    pub async fn picture_unchanged(&self, account: i64, key: &str, now_ms: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE pictures SET asked_ms = ? WHERE account_id = ? AND key = ?").bind(now_ms).bind(account).bind(key).execute(&self.0).await?;
        Ok(())
    }

    /// Keeps the picture of a person or a group as the service gave it at `now_ms` from the address
    /// `path`, or that it has none. Returns whether the picture changed.
    pub async fn set_avatar(&self, account: i64, key: &str, bytes: Option<&[u8]>, path: &str, now_ms: i64) -> sqlx::Result<bool> {
        let old: Option<Option<Vec<u8>>> = sqlx::query_scalar("SELECT bytes FROM pictures WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await?;
        if old.as_ref().is_some_and(|o| o.as_deref() == bytes) {
            sqlx::query("UPDATE pictures SET asked_ms = ?, path = ? WHERE account_id = ? AND key = ?").bind(now_ms).bind(path).bind(account).bind(key).execute(&self.0).await?;
            return Ok(false);
        }
        sqlx::query("INSERT OR REPLACE INTO pictures (account_id, key, bytes, fetched_ms, asked_ms, path) VALUES (?1, ?2, ?3, ?4, ?4, ?5)").bind(account).bind(key).bind(bytes).bind(now_ms).bind(path).execute(&self.0).await?;
        Ok(old.flatten().is_some() || bytes.is_some())
    }

    /// A conversation's picture: the group's, or the person's it is with.
    pub async fn avatar(&self, conversation: i64) -> sqlx::Result<Option<Vec<u8>>> {
        let bytes = sqlx::query_scalar("SELECT p.bytes FROM pictures p JOIN conversations c ON c.account_id = p.account_id AND c.key = p.key WHERE c.id = ?").bind(conversation).fetch_optional(&self.0).await?;
        Ok(bytes.flatten())
    }

    /// The picture of the person who wrote a message.
    pub async fn writer_picture(&self, message: i64) -> sqlx::Result<Option<Vec<u8>>> {
        let bytes = sqlx::query_scalar(
            "SELECT p.bytes FROM pictures p JOIN messages m ON m.sender = p.key JOIN conversations c ON c.id = m.conversation_id AND c.account_id = p.account_id WHERE m.id = ?",
        )
        .bind(message)
        .fetch_optional(&self.0)
        .await?;
        Ok(bytes.flatten())
    }

    pub async fn set_self_key(&self, id: i64, key: &str) -> sqlx::Result<()> {
        sqlx::query("UPDATE accounts SET self_key = ? WHERE id = ?").bind(key).bind(id).execute(&self.0).await?;
        Ok(())
    }

    /// The conversation in which an account writes to itself, once the account is known.
    pub async fn note_to_self(&self, account: i64) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar("SELECT c.id FROM conversations c JOIN accounts a ON a.id = c.account_id AND a.self_key = c.key WHERE a.id = ?").bind(account).fetch_optional(&self.0).await
    }

    /// Whether the account sends read receipts and typing indicators; None until it has said.
    pub async fn settings(&self, id: i64) -> sqlx::Result<Option<(bool, bool)>> {
        let row = sqlx::query("SELECT read_receipts, typing FROM accounts WHERE id = ?").bind(id).fetch_one(&self.0).await?;
        let (read_receipts, typing): (Option<bool>, Option<bool>) = (row.get(0), row.get(1));
        Ok(read_receipts.zip(typing))
    }

    pub async fn set_settings(&self, id: i64, read_receipts: bool, typing: bool) -> sqlx::Result<()> {
        sqlx::query("UPDATE accounts SET read_receipts = ?, typing = ? WHERE id = ?").bind(read_receipts).bind(typing).bind(id).execute(&self.0).await?;
        Ok(())
    }

    /// Whether the account's contacts have been read whole from its primary device.
    pub async fn contacts_read(&self, id: i64) -> sqlx::Result<bool> {
        sqlx::query_scalar("SELECT contacts_ms > 0 FROM accounts WHERE id = ?").bind(id).fetch_one(&self.0).await
    }

    pub async fn set_contacts_read(&self, id: i64, now_ms: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE accounts SET contacts_ms = ?2 WHERE id = ?1").bind(id).bind(now_ms).execute(&self.0).await.map(|_| ())
    }

    pub async fn storage_version(&self, id: i64) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar("SELECT storage_version FROM accounts WHERE id = ?").bind(id).fetch_one(&self.0).await
    }

    /// Notes that the account's store of received messages now holds only those not yet dealt
    /// with. Returns whether it was so already.
    pub async fn journal_started(&self, id: i64) -> sqlx::Result<bool> {
        let was: i64 = sqlx::query_scalar("SELECT journal FROM accounts WHERE id = ?").bind(id).fetch_one(&self.0).await?;
        sqlx::query("UPDATE accounts SET journal = 1 WHERE id = ?").bind(id).execute(&self.0).await?;
        Ok(was != 0)
    }

    pub async fn set_storage_version(&self, id: i64, version: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE accounts SET storage_version = ? WHERE id = ?").bind(version).bind(id).execute(&self.0).await?;
        Ok(())
    }

    /// (id, key) of each of the account's conversations of one kind.
    pub async fn conversation_keys(&self, account: i64, kind: i64) -> sqlx::Result<Vec<(i64, String)>> {
        let rows = sqlx::query("SELECT id, key FROM conversations WHERE account_id = ? AND kind = ?").bind(account).bind(kind).fetch_all(&self.0).await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// A person's name, or "" when none is recorded.
    pub async fn person_name(&self, account: i64, key: &str) -> sqlx::Result<String> {
        let name = sqlx::query_scalar("SELECT name FROM people WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await?;
        Ok(name.unwrap_or_default())
    }

    /// The conversation with this key, made if absent. A non-empty title replaces the stored one.
    /// Returns its id and whether anything changed.
    pub async fn conversation(&self, account: i64, key: &str, kind: i64, title: &str) -> sqlx::Result<(i64, bool)> {
        if let Some(row) = sqlx::query("SELECT id, title FROM conversations WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await? {
            let (id, old): (i64, String) = (row.get(0), row.get(1));
            if title.is_empty() || title == old {
                return Ok((id, false));
            }
            sqlx::query("UPDATE conversations SET title = ? WHERE id = ?").bind(title).bind(id).execute(&self.0).await?;
            return Ok((id, true));
        }
        let done = sqlx::query("INSERT INTO conversations (account_id, key, kind, title) VALUES (?, ?, ?, ?)").bind(account).bind(key).bind(kind).bind(title).execute(&self.0).await?;
        Ok((done.last_insert_rowid(), true))
    }

    /// Gives a conversation that exists a new title, when the title is not empty.
    pub async fn rename_conversation(&self, account: i64, key: &str, title: &str) -> sqlx::Result<()> {
        if !title.is_empty() {
            sqlx::query("UPDATE conversations SET title = ?3 WHERE account_id = ?1 AND key = ?2").bind(account).bind(key).bind(title).execute(&self.0).await?;
        }
        Ok(())
    }

    /// (account, key, kind) of a conversation.
    pub async fn conversation_key(&self, id: i64) -> sqlx::Result<Option<(i64, String, i64)>> {
        let row = sqlx::query("SELECT account_id, key, kind FROM conversations WHERE id = ?").bind(id).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1), r.get(2))))
    }

    pub async fn conversations(&self) -> sqlx::Result<Vec<Conversation>> {
        let rows = sqlx::query(
            "SELECT c.id, c.account_id, c.key, c.kind, c.title, COALESCE(m.sent_ms, 0),
                    COALESCE(NULLIF(m.text, ''), (SELECT 'Attachment' FROM attachments WHERE message_id = m.id LIMIT 1), ''),
                    (SELECT COUNT(*) FROM messages WHERE conversation_id = c.id AND outgoing = 0 AND read_ms IS NULL), c.muted,
                    COALESCE((SELECT fetched_ms FROM pictures WHERE account_id = c.account_id AND key = c.key AND bytes IS NOT NULL), 0),
                    COALESCE(c.colour, (SELECT colour FROM people WHERE c.kind = 0 AND account_id = c.account_id AND key = c.key), -1),
                    c.accepted = 0,
                    CASE WHEN c.kind = 1 AND m.outgoing = 0 AND m.kind = 0 THEN COALESCE(NULLIF((SELECT name FROM people WHERE account_id = c.account_id AND key = m.sender), ''), 'Someone') WHEN c.kind = 1 AND m.outgoing = 1 THEN 'me' ELSE '' END,
                    c.admin
             FROM conversations c
             LEFT JOIN messages m ON m.id = (SELECT MAX(id) FROM messages WHERE conversation_id = c.id)
             ORDER BY COALESCE(m.id, 0) DESC, c.title COLLATE NOCASE",
        )
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| Conversation { id: r.get(0), account: r.get(1), key: r.get(2), kind: r.get(3), title: r.get(4), last_ms: r.get(5), last_text: r.get(6), unread: r.get(7), muted: r.get(8), avatar_ms: r.get(9), person: r.get(10), request: r.get(11), last_sender: r.get(12), admin: r.get(13) }).collect())
    }

    /// Notes whether the account is an administrator of a group. Returns whether that changed.
    pub async fn set_admin(&self, conversation: i64, admin: bool) -> sqlx::Result<bool> {
        let done = sqlx::query("UPDATE conversations SET admin = ?2 WHERE id = ?1 AND admin != ?2").bind(conversation).bind(admin).execute(&self.0).await?;
        Ok(done.rows_affected() > 0)
    }

    /// Replaces the list of a group's members.
    pub async fn set_members(&self, conversation: i64, keys: &[String]) -> sqlx::Result<()> {
        let mut tx = self.0.begin().await?;
        sqlx::query("DELETE FROM members WHERE conversation_id = ?").bind(conversation).execute(&mut *tx).await?;
        for key in keys {
            sqlx::query("INSERT OR IGNORE INTO members (conversation_id, key) VALUES (?, ?)").bind(conversation).bind(key).execute(&mut *tx).await?;
        }
        tx.commit().await
    }

    /// The group's members its picture is made of, when it has none of its own: up to `max` of them
    /// (the account aside), those who wrote most there since `since_ms` first, the rest of a quiet
    /// group by key. Each is (key, name, the number their colour goes by or -1, when their picture
    /// last changed or 0 for none).
    pub async fn faces(&self, conversation: i64, since_ms: i64, max: i64) -> sqlx::Result<Vec<(String, String, i64, i64)>> {
        let rows = sqlx::query(
            "SELECT m.key, COALESCE(p.name, ''), COALESCE(p.colour, -1),
                    COALESCE((SELECT fetched_ms FROM pictures WHERE account_id = c.account_id AND key = m.key AND bytes IS NOT NULL), 0)
             FROM members m JOIN conversations c ON c.id = m.conversation_id
             LEFT JOIN people p ON p.account_id = c.account_id AND p.key = m.key
             WHERE m.conversation_id = ?1
             ORDER BY (SELECT COUNT(*) FROM messages WHERE conversation_id = ?1 AND sent_ms > ?2 AND sender = m.key AND outgoing = 0) DESC, m.key
             LIMIT ?3",
        )
        .bind(conversation)
        .bind(since_ms)
        .bind(max)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect())
    }

    /// The picture of a person, by their key, as the account that holds `conversation` has it.
    pub async fn person_picture(&self, conversation: i64, key: &str) -> sqlx::Result<Option<Vec<u8>>> {
        let bytes = sqlx::query_scalar("SELECT p.bytes FROM pictures p JOIN conversations c ON c.account_id = p.account_id WHERE c.id = ? AND p.key = ?")
            .bind(conversation)
            .bind(key)
            .fetch_optional(&self.0)
            .await?;
        Ok(bytes.flatten())
    }

    /// (key, name) of a group's members, by name; the name is "" for one never named.
    pub async fn members(&self, conversation: i64) -> sqlx::Result<Vec<(String, String)>> {
        let rows = sqlx::query(
            "SELECT m.key, COALESCE(p.name, '') FROM members m JOIN conversations c ON c.id = m.conversation_id
             LEFT JOIN people p ON p.account_id = c.account_id AND p.key = m.key WHERE m.conversation_id = ? ORDER BY 2 COLLATE NOCASE",
        )
        .bind(conversation)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Takes a person's name from the account's records: one the owner gave replaces whatever is
    /// kept and stays; the one from their profile is taken only when no name is kept, since the
    /// profile itself, read from the service, is the newer word on it. Returns whether the name changed.
    pub async fn record_name(&self, account: i64, key: &str, name: &str, given_by_owner: bool) -> sqlx::Result<bool> {
        if name.is_empty() {
            return Ok(false);
        }
        let done = sqlx::query(
            "INSERT INTO people (account_id, key, name, named_by_owner) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (account_id, key) DO UPDATE SET name = ?3, named_by_owner = ?4 WHERE (?4 = 1 OR name = '') AND (name != ?3 OR named_by_owner != ?4)",
        )
        .bind(account)
        .bind(key)
        .bind(name)
        .bind(given_by_owner)
        .execute(&self.0)
        .await?;
        if !given_by_owner {
            sqlx::query("UPDATE people SET named_by_owner = 0 WHERE account_id = ? AND key = ? AND named_by_owner = 1").bind(account).bind(key).execute(&self.0).await?;
        }
        Ok(done.rows_affected() > 0)
    }

    /// Takes the name a person's profile gives now, unless the owner gave them one. Returns whether the name changed.
    pub async fn profile_name(&self, account: i64, key: &str, name: &str) -> sqlx::Result<bool> {
        if name.is_empty() {
            return Ok(false);
        }
        let done = sqlx::query("UPDATE people SET name = ?3 WHERE account_id = ?1 AND key = ?2 AND named_by_owner = 0 AND name != ?3").bind(account).bind(key).bind(name).execute(&self.0).await?;
        Ok(done.rows_affected() > 0)
    }

    /// Notes that the account's owner has dealings with a person, and accepts the conversation with them if there is one.
    pub async fn accept_person(&self, account: i64, key: &str) -> sqlx::Result<bool> {
        sqlx::query("INSERT INTO people (account_id, key, name, accepted) VALUES (?1, ?2, '', 1) ON CONFLICT (account_id, key) DO UPDATE SET accepted = 1").bind(account).bind(key).execute(&self.0).await?;
        let done = sqlx::query("UPDATE conversations SET accepted = 1 WHERE account_id = ? AND key = ? AND accepted = 0").bind(account).bind(key).execute(&self.0).await?;
        Ok(done.rows_affected() > 0)
    }

    /// Whether a conversation is accepted: its messages are answered with receipts and typing notices.
    pub async fn accepted(&self, conversation: i64) -> sqlx::Result<bool> {
        Ok(sqlx::query_scalar("SELECT accepted FROM conversations WHERE id = ?").bind(conversation).fetch_optional(&self.0).await?.unwrap_or(true))
    }

    /// Holds a conversation apart as a request, when it is with a person the account's owner has
    /// no dealings with and nothing has been written in it yet. Returns whether it did.
    pub async fn hold_if_unasked(&self, conversation: i64, sender: &str) -> sqlx::Result<bool> {
        let done = sqlx::query(
            "UPDATE conversations SET accepted = 0 WHERE id = ?1 AND kind = 0 AND accepted = 1
               AND NOT EXISTS (SELECT 1 FROM messages WHERE conversation_id = ?1)
               AND NOT EXISTS (SELECT 1 FROM people p WHERE p.account_id = conversations.account_id AND p.key = ?2 AND p.accepted = 1)",
        )
        .bind(conversation)
        .bind(sender)
        .execute(&self.0)
        .await?;
        Ok(done.rows_affected() > 0)
    }

    /// A conversation of the account's by its key: its id and title.
    pub async fn conversation_by_key(&self, account: i64, key: &str) -> sqlx::Result<Option<(i64, String)>> {
        let row = sqlx::query("SELECT id, title FROM conversations WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// Notes that a group's conversation is being brought up to this revision of the group's
    /// state. False when it has already been brought up to a later one, and nothing is noted.
    pub async fn advance_revision(&self, conversation: i64, revision: i64) -> sqlx::Result<bool> {
        let done = sqlx::query("UPDATE conversations SET revision = ?2 WHERE id = ?1 AND COALESCE(revision, -1) <= ?2").bind(conversation).bind(revision).execute(&self.0).await?;
        Ok(done.rows_affected() > 0)
    }

    /// A conversation's disappearing timer as last known, in seconds; 0 when none is known.
    pub async fn timer(&self, conversation: i64) -> sqlx::Result<i64> {
        let timer: Option<Option<i64>> = sqlx::query_scalar("SELECT timer_s FROM conversations WHERE id = ?").bind(conversation).fetch_optional(&self.0).await?;
        Ok(timer.flatten().unwrap_or(0))
    }

    /// Notes a conversation's disappearing timer. Returns the timer it had when that was known and
    /// was another; nothing the first time one is noted.
    pub async fn timer_changed(&self, conversation: i64, timer_s: i64) -> sqlx::Result<Option<i64>> {
        let old: Option<Option<i64>> = sqlx::query_scalar("SELECT timer_s FROM conversations WHERE id = ?").bind(conversation).fetch_optional(&self.0).await?;
        if old.flatten() != Some(timer_s) {
            sqlx::query("UPDATE conversations SET timer_s = ? WHERE id = ?").bind(timer_s).bind(conversation).execute(&self.0).await?;
        }
        Ok(old.flatten().filter(|o| *o != timer_s))
    }

    /// Keeps what is being written in a conversation, or forgets it when there is nothing.
    pub async fn set_draft(&self, conversation: i64, text: &str, files: &str) -> sqlx::Result<()> {
        if text.is_empty() && files.is_empty() {
            sqlx::query("DELETE FROM drafts WHERE conversation_id = ?").bind(conversation).execute(&self.0).await?;
        } else {
            sqlx::query("INSERT OR REPLACE INTO drafts (conversation_id, text, files) VALUES (?, ?, ?)").bind(conversation).bind(text).bind(files).execute(&self.0).await?;
        }
        Ok(())
    }

    /// What was being written in a conversation: its text and its files' paths, a line each.
    pub async fn draft(&self, conversation: i64) -> sqlx::Result<(String, String)> {
        let row = sqlx::query("SELECT text, files FROM drafts WHERE conversation_id = ?").bind(conversation).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))).unwrap_or_default())
    }

    pub async fn set_muted(&self, conversation: i64, muted: bool) -> sqlx::Result<()> {
        sqlx::query("UPDATE conversations SET muted = ? WHERE id = ?").bind(muted).bind(conversation).execute(&self.0).await?;
        Ok(())
    }

    /// Up to `max` messages whose text contains `query`, whatever its case in ASCII, newest first.
    pub async fn search(&self, query: &str, max: i64) -> sqlx::Result<Vec<Hit>> {
        // (the three characters that mean something to LIKE stand for themselves)
        let pattern = format!("%{}%", query.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        let rows = sqlx::query(
            "SELECT m.id, c.id, c.title, m.outgoing, COALESCE(p.name, ''), m.sent_ms, m.text,
                    (SELECT COUNT(*) FROM messages n WHERE n.conversation_id = m.conversation_id AND n.id > m.id)
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             LEFT JOIN people p ON p.account_id = c.account_id AND p.key = m.sender
             WHERE m.kind = 0 AND m.text LIKE ?1 ESCAPE '\\'
             ORDER BY m.id DESC LIMIT ?2",
        )
        .bind(pattern)
        .bind(max)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| Hit { message: r.get(0), conversation: r.get(1), title: r.get(2), outgoing: r.get(3), sender: r.get(4), sent_ms: r.get(5), text: r.get(6), newer: r.get(7) }).collect())
    }

    /// Stores a message and returns its id, or None when it is already stored (the service delivers
    /// a message again when its acknowledgement was lost).
    pub async fn add_message(&self, m: &NewMessage<'_>) -> sqlx::Result<Option<i64>> {
        // An outgoing message counts as read, and so does an incoming one whose timer has started.
        let read_ms = (m.outgoing || m.expire_start_ms.is_some()).then_some(m.expire_start_ms.unwrap_or(m.received_ms));
        let expires_ms = m.expire_start_ms.filter(|_| m.expire_s > 0).map(|start| start + m.expire_s * 1000);
        let done = sqlx::query("INSERT OR IGNORE INTO messages (conversation_id, outgoing, sender, sent_ms, received_ms, text, read_ms, expire_s, expires_ms, kind, quote_sender, quote_text, quote_ms) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(m.conversation)
            .bind(m.outgoing)
            .bind(m.sender)
            .bind(m.sent_ms)
            .bind(m.received_ms)
            .bind(m.text)
            .bind(read_ms)
            .bind(m.expire_s)
            .bind(expires_ms)
            .bind(m.kind)
            .bind(m.quote.map(|q| q.0))
            .bind(m.quote.map(|q| q.1))
            .bind(m.quote.map(|q| q.2).filter(|t| *t > 0))
            .execute(&self.0)
            .await?;
        // A group written in for the first time gets the next number, and so does whoever writes for
        // the first time: the numbers are one count, and each is a colour of its own.
        const NEXT: &str = "(SELECT MAX(COALESCE((SELECT MAX(colour) FROM people WHERE account_id = a.id), -1), COALESCE((SELECT MAX(colour) FROM conversations WHERE account_id = a.id), -1)) + 1
                            FROM accounts a WHERE a.id = (SELECT account_id FROM conversations WHERE id = ?1))";
        sqlx::query(sqlx::AssertSqlSafe(format!("UPDATE conversations SET colour = {NEXT} WHERE id = ?1 AND kind = 1 AND colour IS NULL"))).bind(m.conversation).execute(&self.0).await?;
        if !m.outgoing && !m.sender.is_empty() {
            sqlx::query("INSERT OR IGNORE INTO people (account_id, key, name) SELECT account_id, ?2, '' FROM conversations WHERE id = ?1").bind(m.conversation).bind(m.sender).execute(&self.0).await?;
            sqlx::query(
                sqlx::AssertSqlSafe(format!("UPDATE people SET colour = {NEXT} WHERE key = ?2 AND colour IS NULL AND account_id = (SELECT account_id FROM conversations WHERE id = ?1)")),
            )
            .bind(m.conversation)
            .bind(m.sender)
            .execute(&self.0)
            .await?;
        }
        Ok((done.rows_affected() > 0).then(|| done.last_insert_rowid()))
    }

    /// (received_ms, sent_ms) of every message someone else wrote, in the order received.
    pub async fn arrivals(&self) -> sqlx::Result<Vec<(i64, i64)>> {
        let rows = sqlx::query("SELECT received_ms, sent_ms FROM messages WHERE outgoing = 0 AND kind = 0 ORDER BY received_ms").fetch_all(&self.0).await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Stores a message the account wrote, to be sent: with what it replies to (the writer, "" for
    /// the account, the text and when it was sent) and whom it names. Returns its id.
    pub async fn enqueue(&self, conversation: i64, text: &str, sent_ms: i64, quote: Option<(&str, &str, i64)>, mentions: &str) -> sqlx::Result<i64> {
        let done = sqlx::query(
            "INSERT INTO messages (conversation_id, outgoing, sender, sent_ms, received_ms, text, read_ms, kind, quote_sender, quote_text, quote_ms, mentions, unsent)
             VALUES (?1, 1, '', ?2, ?2, ?3, ?2, 0, ?4, ?5, ?6, ?7, 1)",
        )
        .bind(conversation)
        .bind(sent_ms)
        .bind(text)
        .bind(quote.map(|q| q.0))
        .bind(quote.map(|q| q.1))
        .bind(quote.map(|q| q.2))
        .bind(mentions)
        .execute(&self.0)
        .await?;
        Ok(done.last_insert_rowid())
    }

    /// The account's messages that wait to be sent, oldest first.
    pub async fn waiting(&self, account: i64) -> sqlx::Result<Vec<Waiting>> {
        let rows = sqlx::query(
            "SELECT m.id, c.id, c.key, c.kind, m.text, m.sent_ms, m.quote_sender, COALESCE(m.quote_text, ''), COALESCE(m.quote_ms, 0), COALESCE(m.mentions, ''), m.trusted
             FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.account_id = ? AND m.unsent = 1 ORDER BY m.id",
        )
        .bind(account)
        .fetch_all(&self.0)
        .await?;
        Ok(rows
            .iter()
            .map(|r| Waiting { id: r.get(0), account, conversation: r.get(1), key: r.get(2), kind: r.get(3), text: r.get(4), sent_ms: r.get(5), quote_sender: r.get(6), quote_text: r.get(7), quote_ms: r.get(8), mentions: r.get(9), trusted: r.get(10) })
            .collect())
    }

    /// The files of a message as they are held: (name, type, width, height, bytes).
    pub async fn files(&self, message: i64) -> sqlx::Result<Vec<(String, String, i64, i64, Vec<u8>)>> {
        let rows = sqlx::query("SELECT file_name, content_type, width, height, bytes FROM attachments WHERE message_id = ? AND bytes IS NOT NULL ORDER BY id").bind(message).fetch_all(&self.0).await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3), r.get(4))).collect())
    }

    /// A waiting message was sent at `now_ms`, with the conversation's disappearing timer, which starts at once.
    pub async fn mark_sent(&self, message: i64, expire_s: i64, now_ms: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET unsent = 0, status = 0, expire_s = ?2, expires_ms = CASE WHEN ?2 > 0 THEN ?3 + ?2 * 1000 END WHERE id = ?1").bind(message).bind(expire_s).bind(now_ms).execute(&self.0).await?;
        Ok(())
    }

    /// A sending failed. After `tries` of them the message stops being tried until the user asks.
    pub async fn send_failed(&self, message: i64, tries: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET attempts = attempts + 1, unsent = CASE WHEN attempts + 1 >= ?2 THEN 2 ELSE 1 END WHERE id = ?1").bind(message).bind(tries).execute(&self.0).await?;
        Ok(())
    }

    /// Holds a waiting message until the user says to send it: the safety number of someone it goes to changed.
    pub async fn hold(&self, message: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET unsent = 3 WHERE id = ? AND unsent = 1").bind(message).execute(&self.0).await?;
        Ok(())
    }

    /// Puts a message that was given up or held back among those waiting; one that was held is then sent all the same. Returns its (account, conversation), if it was one.
    pub async fn send_again(&self, message: i64) -> sqlx::Result<Option<(i64, i64)>> {
        let row = sqlx::query("UPDATE messages SET trusted = CASE WHEN unsent = 3 THEN 1 ELSE trusted END, unsent = 1, attempts = 0 WHERE id = ? AND unsent IN (2, 3) RETURNING (SELECT account_id FROM conversations WHERE id = conversation_id), conversation_id").bind(message).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// Removes a message that has not been sent. Returns its (account, conversation), if it was one.
    pub async fn discard(&self, message: i64) -> sqlx::Result<Option<(i64, i64)>> {
        let row = sqlx::query("DELETE FROM messages WHERE id = ? AND unsent != 0 RETURNING (SELECT account_id FROM conversations WHERE id = conversation_id), conversation_id").bind(message).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// Up to `max` messages in the order they came to this device, ending before the message
    /// `before` (or at the newest). That is the order of their ids, and the order the user saw them
    /// in: the time a message says it was sent is its sender's clock's, and clocks differ, so by
    /// that time an answer can come to stand above what it answers.
    const MESSAGE: &str = "SELECT m.id, m.outgoing, COALESCE(p.name, ''), m.sent_ms, m.text, CASE m.unsent WHEN 1 THEN -1 WHEN 2 THEN -2 WHEN 3 THEN -3 ELSE m.status END, COALESCE(m.expires_ms, 0), m.kind, m.edit_ms IS NOT NULL,
                    CASE WHEN m.quote_sender IS NOT NULL THEN COALESCE(q.name, '') END, COALESCE(m.quote_text, ''),
                    COALESCE((SELECT group_concat(emoji || CASE WHEN n > 1 THEN ' ' || n ELSE '' END, '  ')
                              FROM (SELECT emoji, COUNT(*) n FROM reactions WHERE message_id = m.id GROUP BY emoji ORDER BY MIN(rowid))), ''),
                    COALESCE((SELECT emoji FROM reactions WHERE message_id = m.id AND sender = ''), ''),
                    (SELECT COUNT(*) FROM attachments WHERE message_id = m.id),
                    CASE WHEN m.outgoing THEN -1 ELSE COALESCE(p.colour, -1) END,
                    COALESCE((SELECT fetched_ms FROM pictures WHERE account_id = c.account_id AND key = m.sender AND bytes IS NOT NULL), 0),
                    COALESCE((SELECT group_concat(emoji || ' ' || who, '   ')
                              FROM (SELECT r.emoji emoji, group_concat(CASE WHEN r.sender = '' THEN 'me' ELSE COALESCE(NULLIF(rp.name, ''), 'someone') END, ', ') who
                                    FROM reactions r LEFT JOIN people rp ON rp.account_id = c.account_id AND rp.key = r.sender
                                    WHERE r.message_id = m.id GROUP BY r.emoji ORDER BY MIN(r.rowid))), ''),
                    COALESCE(m.clear, ''),
                    (SELECT CASE WHEN MAX(given_up) = 1 THEN 2 WHEN COUNT(*) > 0 THEN 1 ELSE 0 END FROM changes WHERE message_id = m.id),
                    COALESCE((SELECT r.id FROM messages r WHERE r.conversation_id = m.conversation_id AND r.sent_ms = m.quote_ms AND r.sender = m.quote_sender AND r.kind = 0), 0),
                    COALESCE(m.styles, ''), m.conversation_id
             FROM messages m
             JOIN conversations c ON c.id = m.conversation_id
             LEFT JOIN people p ON p.account_id = c.account_id AND p.key = m.sender
             LEFT JOIN people q ON q.account_id = c.account_id AND q.key = m.quote_sender";

    fn message_of(r: &sqlx::sqlite::SqliteRow) -> Message {
        Message { id: r.get(0), outgoing: r.get(1), sender: r.get(2), sent_ms: r.get(3), text: r.get(4), status: r.get(5), expires_ms: r.get(6), kind: r.get(7), edited: r.get(8), reply_to: r.get(9), reply_text: r.get(10), reactions: r.get(11), reaction: r.get(12), attachments: r.get(13), person: r.get(14), picture_ms: r.get(15), reacted: r.get(16), hidden: r.get(17), changing: r.get(18), reply_id: r.get(19), styles: r.get(20) }
    }

    pub async fn messages(&self, conversation: i64, before: i64, max: i64) -> sqlx::Result<Vec<Message>> {
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!("{} WHERE m.conversation_id = ?1 AND (?2 = 0 OR m.id < ?2) ORDER BY m.id DESC LIMIT ?3", Self::MESSAGE)))
            .bind(conversation)
            .bind(before)
            .bind(max)
            .fetch_all(&self.0)
            .await?;
        Ok(rows.iter().rev().map(Self::message_of).collect())
    }

    /// One message as `messages` gives it, with its conversation; None when it is gone.
    pub async fn message_shown(&self, id: i64) -> sqlx::Result<Option<(i64, Message)>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("{} WHERE m.id = ?1", Self::MESSAGE))).bind(id).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get::<i64, _>(21), Self::message_of(&r))))
    }

    const TARGET: &str = "SELECT m.id, c.account_id, c.id, c.key, c.kind, m.sender, m.sent_ms, COALESCE(m.edit_ms, m.sent_ms), m.text
                          FROM messages m JOIN conversations c ON c.id = m.conversation_id";

    fn target(r: sqlx::sqlite::SqliteRow) -> Target {
        Target { id: r.get(0), account: r.get(1), conversation: r.get(2), key: r.get(3), kind: r.get(4), sender: r.get(5), sent_ms: r.get(6), latest_ms: r.get(7), text: r.get(8) }
    }

    /// A message someone wrote, by its id.
    pub async fn message(&self, id: i64) -> sqlx::Result<Option<Target>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("{} WHERE m.id = ? AND m.kind = 0", Self::TARGET))).bind(id).fetch_optional(&self.0).await?;
        Ok(row.map(Self::target))
    }

    /// The account's message that this person ("" for the account) sent at this time, which is how
    /// the service names a message; an edited one also answers to the time of its latest edit.
    pub async fn message_at(&self, account: i64, sender: &str, sent_ms: i64) -> sqlx::Result<Option<Target>> {
        let row = sqlx::query(sqlx::AssertSqlSafe(format!("{} WHERE c.account_id = ?1 AND m.sender = ?2 AND m.kind = 0 AND (m.sent_ms = ?3 OR m.edit_ms = ?3)", Self::TARGET)))
            .bind(account)
            .bind(sender)
            .bind(sent_ms)
            .fetch_optional(&self.0)
            .await?;
        Ok(row.map(Self::target))
    }

    /// Records the emoji a person ("" for the account) gives a message, in place of any they gave it
    /// before; an empty emoji takes theirs away.
    pub async fn react(&self, message: i64, sender: &str, emoji: &str) -> sqlx::Result<()> {
        if emoji.is_empty() {
            sqlx::query("DELETE FROM reactions WHERE message_id = ? AND sender = ?").bind(message).bind(sender).execute(&self.0).await?;
        } else {
            sqlx::query("INSERT INTO reactions (message_id, sender, emoji) VALUES (?1, ?2, ?3) ON CONFLICT DO UPDATE SET emoji = ?3").bind(message).bind(sender).bind(emoji).execute(&self.0).await?;
        }
        Ok(())
    }

    pub async fn add_attachment(&self, a: &NewAttachment<'_>) -> sqlx::Result<()> {
        sqlx::query("INSERT INTO attachments (message_id, content_type, file_name, size, width, height, voice, looped, pointer, bytes) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .bind(a.message)
            .bind(a.content_type)
            .bind(a.file_name)
            .bind(a.size)
            .bind(a.width)
            .bind(a.height)
            .bind(a.voice)
            .bind(a.looped)
            .bind(a.pointer)
            .bind(a.bytes)
            .execute(&self.0)
            .await?;
        Ok(())
    }

    pub async fn attachments(&self, message: i64) -> sqlx::Result<Vec<Attachment>> {
        let rows = sqlx::query("SELECT id, content_type, file_name, size, width, height, voice, CASE WHEN bytes IS NOT NULL THEN 1 WHEN attempts >= ?2 THEN 2 ELSE 0 END, looped FROM attachments WHERE message_id = ?1 ORDER BY id")
            .bind(message)
            .bind(FETCH_ATTEMPTS)
            .fetch_all(&self.0)
            .await?;
        Ok(rows.iter().map(|r| Attachment { id: r.get(0), content_type: r.get(1), file_name: r.get(2), size: r.get(3), width: r.get(4), height: r.get(5), voice: r.get(6), state: r.get(7), looped: r.get(8) }).collect())
    }

    pub async fn attachment_bytes(&self, id: i64) -> sqlx::Result<Option<Vec<u8>>> {
        Ok(sqlx::query_scalar("SELECT bytes FROM attachments WHERE id = ?").bind(id).fetch_optional(&self.0).await?.flatten())
    }

    /// The account's attachments that wait to be fetched, being of at most `max_size` bytes or asked
    /// for: (id, conversation, pointer).
    pub async fn pending_attachments(&self, account: i64, max_size: i64) -> sqlx::Result<Vec<(i64, i64, i64, Vec<u8>)>> {
        let rows = sqlx::query(
            "SELECT a.id, m.conversation_id, a.message_id, a.pointer FROM attachments a JOIN messages m ON m.id = a.message_id JOIN conversations c ON c.id = m.conversation_id
             WHERE c.account_id = ? AND a.bytes IS NULL AND a.pointer IS NOT NULL AND a.attempts < ? AND (a.size <= ? OR a.wanted) ORDER BY a.id",
        )
        .bind(account)
        .bind(FETCH_ATTEMPTS)
        .bind(max_size)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect())
    }

    /// Keeps a fetched attachment's bytes, with its width and height when it is an image that came without them.
    pub async fn set_attachment_bytes(&self, id: i64, bytes: &[u8], size: Option<(i64, i64)>) -> sqlx::Result<()> {
        sqlx::query("UPDATE attachments SET bytes = ?2, size = ?3, width = COALESCE(?4, width), height = COALESCE(?5, height) WHERE id = ?1")
            .bind(id)
            .bind(bytes)
            .bind(bytes.len() as i64)
            .bind(size.map(|s| s.0))
            .bind(size.map(|s| s.1))
            .execute(&self.0)
            .await?;
        Ok(())
    }

    /// Has an attachment fetched whatever its size, and again if fetching it was given up.
    pub async fn want_attachment(&self, id: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE attachments SET wanted = 1, attempts = 0 WHERE id = ? AND bytes IS NULL").bind(id).execute(&self.0).await?;
        Ok(())
    }

    pub async fn attachment_failed(&self, id: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE attachments SET attempts = attempts + 1 WHERE id = ?").bind(id).execute(&self.0).await?;
        Ok(())
    }

    /// The emoji the account gave a message, or "".
    pub async fn own_reaction(&self, message: i64) -> sqlx::Result<String> {
        let emoji = sqlx::query_scalar("SELECT emoji FROM reactions WHERE message_id = ? AND sender = ''").bind(message).fetch_optional(&self.0).await?;
        Ok(emoji.unwrap_or_default())
    }

    /// Replaces a message's text with an edit sent at `edit_ms`. An outgoing message starts again as sent.
    pub async fn edit(&self, message: i64, text: &str, edit_ms: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET text = ?2, edit_ms = ?3, status = 0 WHERE id = ?1").bind(message).bind(text).bind(edit_ms).execute(&self.0).await?;
        Ok(())
    }

    /// A message from this person, sent at `sent_ms`, has arrived. If a notice says that a message
    /// of theirs could not be decrypted, and was written after that time, this is that message
    /// sent again, and the notice is taken away. Returns the notice's conversation.
    pub async fn undecryptable_arrived(&self, account: i64, sender: &str, sent_ms: i64) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar(
            "DELETE FROM messages WHERE id = (
                 SELECT m.id FROM messages m JOIN conversations c ON c.id = m.conversation_id
                 WHERE c.account_id = ?1 AND m.sender = ?2 AND m.kind = 1 AND m.sent_ms > ?3 ORDER BY m.sent_ms LIMIT 1)
             RETURNING conversation_id",
        )
        .bind(account)
        .bind(sender)
        .bind(sent_ms)
        .fetch_optional(&self.0)
        .await
    }

    /// Keeps a change to a message that is to be sent: what (0 reaction, 1 edit, 2 deletion) and its value.
    pub async fn add_change(&self, message: i64, what: i64, value: &str) -> sqlx::Result<()> {
        sqlx::query("INSERT INTO changes (message_id, what, value) VALUES (?, ?, ?)").bind(message).bind(what).bind(value).execute(&self.0).await?;
        Ok(())
    }

    /// The account's changes that wait to be sent, oldest first: (id, message, what, value).
    pub async fn changes(&self, account: i64) -> sqlx::Result<Vec<(i64, i64, i64, String)>> {
        let rows = sqlx::query("SELECT g.id, g.message_id, g.what, g.value FROM changes g JOIN messages m ON m.id = g.message_id JOIN conversations c ON c.id = m.conversation_id WHERE c.account_id = ? AND g.given_up = 0 ORDER BY g.id")
            .bind(account)
            .fetch_all(&self.0)
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2), r.get(3))).collect())
    }

    /// A change was sent and goes, or failed: after `tries` failures it is given up, and stays
    /// marked so for the user to send again or drop. Returns whether it was given up.
    pub async fn change_done(&self, id: i64, sent: bool, tries: i64) -> sqlx::Result<bool> {
        if sent {
            sqlx::query("DELETE FROM changes WHERE id = ?").bind(id).execute(&self.0).await?;
            return Ok(false);
        }
        let given_up: Option<bool> = sqlx::query_scalar("UPDATE changes SET attempts = attempts + 1, given_up = attempts + 1 >= ?2 WHERE id = ?1 RETURNING given_up").bind(id).bind(tries).fetch_optional(&self.0).await?;
        Ok(given_up.unwrap_or(false))
    }

    /// Puts a message's given-up changes back among those waiting (`again`), or drops them.
    /// Returns the message's (account, conversation) when it had any.
    pub async fn changes_again(&self, message: i64, again: bool) -> sqlx::Result<Option<(i64, i64)>> {
        let done = if again {
            sqlx::query("UPDATE changes SET given_up = 0, attempts = 0 WHERE message_id = ? AND given_up = 1").bind(message).execute(&self.0).await?
        } else {
            sqlx::query("DELETE FROM changes WHERE message_id = ? AND given_up = 1").bind(message).execute(&self.0).await?
        };
        if done.rows_affected() == 0 {
            return Ok(None);
        }
        let row = sqlx::query("SELECT c.account_id, c.id FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE m.id = ?").bind(message).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// Keeps a message's text as it was written beside the text with its hidden parts covered, or
    /// forgets it when nothing in it is hidden.
    /// Keeps the styles of a message's text, as `cover_spoilers` gives them ("" for none).
    pub async fn set_styles(&self, message: i64, styles: &str) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET styles = NULLIF(?2, '') WHERE id = ?1").bind(message).bind(styles).execute(&self.0).await?;
        Ok(())
    }

    pub async fn set_clear(&self, message: i64, clear: Option<&str>) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET clear = ?2 WHERE id = ?1").bind(message).bind(clear).execute(&self.0).await?;
        Ok(())
    }

    /// Leaves a note that the message was deleted where the message was.
    pub async fn delete(&self, message: i64) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET kind = 3, text = '', edit_ms = NULL, quote_sender = NULL, quote_text = NULL WHERE id = ?1").bind(message).execute(&self.0).await?;
        sqlx::query("DELETE FROM reactions WHERE message_id = ?").bind(message).execute(&self.0).await?;
        Ok(())
    }

    /// Marks a conversation's unread messages as read at `now_ms`, which starts their disappearing
    /// timers. Returns each one's (sender, sent_ms).
    pub async fn mark_read(&self, conversation: i64, now_ms: i64) -> sqlx::Result<Vec<(String, i64)>> {
        let rows = sqlx::query(
            "UPDATE messages SET read_ms = ?2, expires_ms = CASE WHEN expire_s > 0 THEN ?2 + expire_s * 1000 END
             WHERE conversation_id = ?1 AND outgoing = 0 AND read_ms IS NULL RETURNING sender, sent_ms",
        )
        .bind(conversation)
        .bind(now_ms)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Marks as read one message read on another of the account's devices. Returns its conversation.
    /// The time of a conversation's newest message, 0 when it has none: where a backend that keeps
    /// history on its server asks for what came after.
    pub async fn newest_ms(&self, conversation: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar("SELECT COALESCE(MAX(sent_ms), 0) FROM messages WHERE conversation_id = ?").bind(conversation).fetch_one(&self.0).await
    }

    /// The time of the newest message in any of an account's conversations, 0 when it has none.
    pub async fn newest_ms_of(&self, account: i64) -> sqlx::Result<i64> {
        sqlx::query_scalar("SELECT COALESCE(MAX(m.sent_ms), 0) FROM messages m JOIN conversations c ON c.id = m.conversation_id WHERE c.account_id = ?").bind(account).fetch_one(&self.0).await
    }

    /// Marks a conversation's messages read up to a time, as another device of the account said.
    /// Returns whether any were unread.
    pub async fn mark_read_until(&self, conversation: i64, until_ms: i64, now_ms: i64) -> sqlx::Result<bool> {
        let done = sqlx::query("UPDATE messages SET read_ms = ?3 WHERE conversation_id = ?1 AND outgoing = 0 AND read_ms IS NULL AND sent_ms <= ?2")
            .bind(conversation)
            .bind(until_ms)
            .bind(now_ms)
            .execute(&self.0)
            .await?;
        Ok(done.rows_affected() > 0)
    }

    /// An account's messages whose text holds IRC formatting codes.
    pub async fn coded_texts(&self, account: i64) -> sqlx::Result<Vec<(i64, String)>> {
        let rows = sqlx::query(
            "SELECT m.id, m.text FROM messages m JOIN conversations c ON c.id = m.conversation_id
             WHERE c.account_id = ? AND (instr(m.text, char(2)) OR instr(m.text, char(3)) OR instr(m.text, char(4)) OR instr(m.text, char(15))
               OR instr(m.text, char(17)) OR instr(m.text, char(22)) OR instr(m.text, char(29)) OR instr(m.text, char(30)) OR instr(m.text, char(31)))",
        )
        .bind(account)
        .fetch_all(&self.0)
        .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    pub async fn set_text_and_styles(&self, message: i64, text: &str, styles: &str) -> sqlx::Result<()> {
        sqlx::query("UPDATE messages SET text = ?2, styles = NULLIF(?3, '') WHERE id = ?1").bind(message).bind(text).bind(styles).execute(&self.0).await.map(|_| ())
    }

    /// The text of a conversation's message from a sender at a time, if one is held.
    pub async fn text_at(&self, conversation: i64, sender: &str, sent_ms: i64) -> sqlx::Result<Option<String>> {
        sqlx::query_scalar("SELECT text FROM messages WHERE conversation_id = ? AND sender = ? AND sent_ms = ?").bind(conversation).bind(sender).bind(sent_ms).fetch_optional(&self.0).await
    }

    /// Gives a sent message a time and text, unless another of the conversation's messages from the
    /// same sender has that time. Returns whether it did.
    pub async fn set_sent(&self, message: i64, sent_ms: i64, text: &str) -> sqlx::Result<bool> {
        let done = sqlx::query("UPDATE OR IGNORE messages SET sent_ms = ?2, text = ?3 WHERE id = ?1").bind(message).bind(sent_ms).bind(text).execute(&self.0).await?;
        Ok(done.rows_affected() > 0)
    }

    pub async fn forget_message(&self, message: i64) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM messages WHERE id = ?").bind(message).execute(&self.0).await.map(|_| ())
    }

    pub async fn mark_read_elsewhere(&self, account: i64, sender: &str, sent_ms: i64, now_ms: i64) -> sqlx::Result<Option<i64>> {
        let row = sqlx::query(
            "UPDATE messages SET read_ms = ?4, expires_ms = CASE WHEN expire_s > 0 THEN ?4 + expire_s * 1000 END
             WHERE outgoing = 0 AND read_ms IS NULL AND sender = ?2 AND sent_ms = ?3
               AND conversation_id IN (SELECT id FROM conversations WHERE account_id = ?1)
             RETURNING conversation_id",
        )
        .bind(account)
        .bind(sender)
        .bind(sent_ms)
        .bind(now_ms)
        .fetch_optional(&self.0)
        .await?;
        Ok(row.map(|r| r.get(0)))
    }

    /// Raises the status of the account's outgoing messages sent at these times (a receipt names
    /// messages by when they were sent). Returns the conversations changed.
    pub async fn set_status(&self, account: i64, sent_ms: &[i64], status: i64) -> sqlx::Result<Vec<(i64, i64)>> {
        if sent_ms.is_empty() {
            return Ok(vec![]);
        }
        let mut query: QueryBuilder<Sqlite> = QueryBuilder::new("UPDATE messages SET status = ");
        query.push_bind(status).push(" WHERE outgoing = 1 AND status < ").push_bind(status);
        query.push(" AND conversation_id IN (SELECT id FROM conversations WHERE account_id = ").push_bind(account).push(") AND COALESCE(edit_ms, sent_ms) IN (");
        let mut times = query.separated(", ");
        for t in sent_ms {
            times.push_bind(*t);
        }
        query.push(") RETURNING conversation_id, id");
        Ok(query.build().fetch_all(&self.0).await?.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    /// Records the key a person is seen with. Returns whether they had another one before.
    pub async fn identity_changed(&self, account: i64, key: &str, identity: &[u8]) -> sqlx::Result<bool> {
        let old: Option<Option<Vec<u8>>> = sqlx::query_scalar("SELECT identity FROM people WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await?;
        if old.as_ref().and_then(|o| o.as_deref()) == Some(identity) {
            return Ok(false);
        }
        sqlx::query("INSERT INTO people (account_id, key, name, identity) VALUES (?1, ?2, '', ?3) ON CONFLICT DO UPDATE SET identity = ?3").bind(account).bind(key).bind(identity).execute(&self.0).await?;
        Ok(old.flatten().is_some())
    }

    /// The time before which the core starts no new session with a person by itself.
    pub async fn repair_after(&self, account: i64, key: &str) -> sqlx::Result<i64> {
        let at = sqlx::query_scalar("SELECT repair_after_ms FROM people WHERE account_id = ? AND key = ?").bind(account).bind(key).fetch_optional(&self.0).await?;
        Ok(at.unwrap_or(0))
    }

    pub async fn set_repair_after(&self, account: i64, key: &str, at_ms: i64) -> sqlx::Result<()> {
        sqlx::query("INSERT INTO people (account_id, key, name, repair_after_ms) VALUES (?1, ?2, '', ?3) ON CONFLICT DO UPDATE SET repair_after_ms = ?3").bind(account).bind(key).bind(at_ms).execute(&self.0).await?;
        Ok(())
    }

    /// Notes what the account sends at `sent_ms`, and forgets what it sent before `keep_from_ms`.
    pub async fn log_sent(&self, account: i64, sent_ms: i64, form: i64, content: &[u8], keep_from_ms: i64) -> sqlx::Result<()> {
        sqlx::query("DELETE FROM sent_log WHERE account_id = ? AND sent_ms < ?").bind(account).bind(keep_from_ms).execute(&self.0).await?;
        sqlx::query("INSERT OR REPLACE INTO sent_log (account_id, sent_ms, form, content) VALUES (?, ?, ?, ?)").bind(account).bind(sent_ms).bind(form).bind(content).execute(&self.0).await?;
        Ok(())
    }

    /// (form, content) of what the account sent at this time, while it is still noted.
    pub async fn sent(&self, account: i64, sent_ms: i64) -> sqlx::Result<Option<(i64, Vec<u8>)>> {
        let row = sqlx::query("SELECT form, content FROM sent_log WHERE account_id = ? AND sent_ms = ?").bind(account).bind(sent_ms).fetch_optional(&self.0).await?;
        Ok(row.map(|r| (r.get(0), r.get(1))))
    }

    /// When the account's next message disappears, if any is waiting to.
    pub async fn next_expiry(&self, account: i64) -> sqlx::Result<Option<i64>> {
        sqlx::query_scalar("SELECT MIN(expires_ms) FROM messages WHERE expires_ms IS NOT NULL AND conversation_id IN (SELECT id FROM conversations WHERE account_id = ?)")
            .bind(account)
            .fetch_one(&self.0)
            .await
    }

    /// Removes the account's messages whose time has come. Returns the conversations changed.
    pub async fn remove_expired(&self, account: i64, now_ms: i64) -> sqlx::Result<Vec<i64>> {
        let rows = sqlx::query("DELETE FROM messages WHERE expires_ms <= ?2 AND conversation_id IN (SELECT id FROM conversations WHERE account_id = ?1) RETURNING conversation_id")
            .bind(account)
            .bind(now_ms)
            .fetch_all(&self.0)
            .await?;
        let mut changed: Vec<i64> = rows.iter().map(|r| r.get(0)).collect();
        changed.sort_unstable();
        changed.dedup();
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::Db;

    #[test]
    fn a_written_message_waits_until_it_is_sent_or_given_up() {
        let dir = std::env::temp_dir().join(format!("gsignal-db-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(async {
            let db = Db::open(&dir.join("test.db"), &[7; 32]).await.unwrap();
            let account = db.add_account("signal", "test").await.unwrap();
            sqlx::query("INSERT INTO conversations (account_id, key, kind, title) VALUES (?, 'k', 0, 't')").bind(account).execute(&db.0).await.unwrap();
            let first = db.enqueue(1, "one", 1000, None, "").await.unwrap();
            let second = db.enqueue(1, "two", 1001, Some(("", "one", 1000)), "who\t0\t4\n").await.unwrap();
            let status = |db: Db| async move { db.messages(1, 0, 10).await.unwrap().iter().map(|m| m.status).collect::<Vec<_>>() };
            assert_eq!(status(db.clone()).await, [-1, -1]);
            let waiting = db.waiting(account).await.unwrap();
            assert_eq!(waiting.iter().map(|w| w.id).collect::<Vec<_>>(), [first, second]);
            assert_eq!((waiting[1].quote_sender.as_deref(), waiting[1].quote_ms, waiting[1].mentions.as_str()), (Some(""), 1000, "who\t0\t4\n"));
            // Sent, it waits no longer, and its disappearing timer runs from then.
            db.mark_sent(first, 60, 5000).await.unwrap();
            assert_eq!(status(db.clone()).await, [0, -1]);
            assert_eq!(db.next_expiry(account).await.unwrap(), Some(65000));
            // Three failures leave it for the user, who can ask again or throw it away.
            for _ in 0..3 {
                assert_eq!(db.waiting(account).await.unwrap().len(), 1);
                db.send_failed(second, 3).await.unwrap();
            }
            assert_eq!(status(db.clone()).await, [0, -2]);
            assert!(db.waiting(account).await.unwrap().is_empty());
            assert_eq!(db.send_again(first).await.unwrap(), None);
            assert_eq!(db.send_again(second).await.unwrap(), Some((account, 1)));
            assert_eq!(db.waiting(account).await.unwrap().len(), 1);
            // One held for a changed safety number waits for the user, and is then sent all the same.
            db.hold(second).await.unwrap();
            assert_eq!(status(db.clone()).await, [0, -3]);
            assert!(db.waiting(account).await.unwrap().is_empty());
            assert_eq!(db.send_again(second).await.unwrap(), Some((account, 1)));
            assert!(db.waiting(account).await.unwrap()[0].trusted);
            assert_eq!(db.discard(first).await.unwrap(), None);
            assert_eq!(db.discard(second).await.unwrap(), Some((account, 1)));
            assert_eq!(status(db.clone()).await, [0]);
            // A message from someone whose clock is ahead, and the account's answer to it, stay in
            // the order they came in, here and when read a page at a time.
            let ahead = super::NewMessage { conversation: 1, outgoing: false, sender: "them", sent_ms: 9000, received_ms: 6000, text: "question", kind: 0, quote: None, expire_s: 0, expire_start_ms: None };
            db.add_message(&ahead).await.unwrap();
            let answer = db.enqueue(1, "answer", 6500, None, "").await.unwrap();
            let texts = |m: Vec<super::Message>| m.into_iter().map(|m| m.text).collect::<Vec<_>>();
            assert_eq!(texts(db.messages(1, 0, 10).await.unwrap()), ["one", "question", "answer"]);
            assert_eq!(texts(db.messages(1, answer, 1).await.unwrap()), ["question"]);
            assert_eq!(db.conversations().await.unwrap()[0].last_text, "answer");
            assert_eq!(db.search("question", 5).await.unwrap()[0].newer, 1);
            // A first message from someone the owner has no dealings with holds its conversation
            // apart; one from someone they have, or in a conversation already begun, does not.
            sqlx::query("INSERT INTO conversations (account_id, key, kind, title) VALUES (?1, 'stranger', 0, 's'), (?1, 'friend', 0, 'f')").bind(account).execute(&db.0).await.unwrap();
            assert!(!db.hold_if_unasked(1, "them").await.unwrap());
            assert!(db.hold_if_unasked(2, "stranger").await.unwrap());
            assert!(!db.accept_person(account, "friend").await.unwrap());
            assert!(!db.hold_if_unasked(3, "friend").await.unwrap());
            assert_eq!((db.accepted(1).await.unwrap(), db.accepted(2).await.unwrap(), db.accepted(3).await.unwrap()), (true, false, true));
            assert_eq!(db.conversations().await.unwrap().iter().filter(|c| c.request).map(|c| c.key.clone()).collect::<Vec<_>>(), ["stranger"]);
            assert!(db.accept_person(account, "stranger").await.unwrap());
            // A group's conversation goes forward through the group's revisions and never back.
            assert!(db.advance_revision(1, 20).await.unwrap());
            assert!(db.advance_revision(1, 20).await.unwrap());
            assert!(!db.advance_revision(1, 19).await.unwrap());
            assert!(db.advance_revision(1, 21).await.unwrap());
            // A timer is noted silently the first time and reported when it becomes another.
            assert_eq!(db.timer_changed(1, 0).await.unwrap(), None);
            assert_eq!(db.timer_changed(1, 0).await.unwrap(), None);
            assert_eq!(db.timer_changed(1, 3600).await.unwrap(), Some(0));
            assert_eq!(db.conversation_by_key(account, "stranger").await.unwrap(), Some((2, "s".to_owned())));
            // A notice that a message could not be decrypted goes when that message arrives after
            // all, which is known by its having been sent before the notice was written.
            let notice = super::NewMessage { conversation: 1, outgoing: false, sender: "them", sent_ms: 20_000, received_ms: 20_000, text: "could not be decrypted", kind: 1, quote: None, expire_s: 0, expire_start_ms: Some(20_000) };
            db.add_message(&notice).await.unwrap();
            assert_eq!(db.undecryptable_arrived(account, "them", 21_000).await.unwrap(), None);
            assert_eq!(db.undecryptable_arrived(account, "someone else", 19_000).await.unwrap(), None);
            assert_eq!(db.undecryptable_arrived(account, "them", 19_000).await.unwrap(), Some(1));
            assert_eq!(db.undecryptable_arrived(account, "them", 19_000).await.unwrap(), None);
            // A change to a message waits until it is sent, or has failed as often as it may.
            db.add_change(first, 0, "\u{1F44D}").await.unwrap();
            db.add_change(first, 1, "one, edited").await.unwrap();
            let waiting = db.changes(account).await.unwrap();
            assert_eq!(waiting.iter().map(|c| (c.2, c.3.as_str())).collect::<Vec<_>>(), [(0, "\u{1F44D}"), (1, "one, edited")]);
            let changing = |db: Db| async move { db.messages(1, 0, 10).await.unwrap().iter().find(|m| m.id == first).unwrap().changing };
            assert_eq!(changing(db.clone()).await, 1);
            assert!(!db.change_done(waiting[0].0, true, 3).await.unwrap());
            assert!(!db.change_done(waiting[1].0, false, 3).await.unwrap());
            assert!(!db.change_done(waiting[1].0, false, 3).await.unwrap());
            assert!(db.change_done(waiting[1].0, false, 3).await.unwrap());
            // Given up, it waits no longer but stays marked, to be sent again or dropped.
            assert!(db.changes(account).await.unwrap().is_empty());
            assert_eq!(changing(db.clone()).await, 2);
            assert_eq!(db.changes_again(first, true).await.unwrap(), Some((account, 1)));
            assert_eq!(db.changes(account).await.unwrap().len(), 1);
            db.change_done(waiting[1].0, false, 1).await.unwrap();
            assert_eq!(db.changes_again(first, false).await.unwrap(), Some((account, 1)));
            assert_eq!(changing(db.clone()).await, 0);
            assert_eq!(db.changes_again(first, false).await.unwrap(), None);
            // What is being written is kept with its conversation until there is nothing to keep.
            db.set_draft(2, "half a thought", "/a/b.png\n/c d.txt").await.unwrap();
            assert_eq!(db.draft(2).await.unwrap(), ("half a thought".to_owned(), "/a/b.png\n/c d.txt".to_owned()));
            assert_eq!(db.draft(3).await.unwrap(), (String::new(), String::new()));
            db.set_draft(2, "", "").await.unwrap();
            assert_eq!(db.draft(2).await.unwrap(), (String::new(), String::new()));
            // A name from someone's profile gives way to a newer one, and to one the owner gives;
            // the owner's stays whatever the profile says, until the owner's is gone.
            let name = |db: Db| async move { db.person_name(account, "friend").await.unwrap() };
            assert!(db.record_name(account, "friend", "Ann", false).await.unwrap());
            assert!(db.profile_name(account, "friend", "Ann B").await.unwrap());
            assert!(!db.record_name(account, "friend", "Ann", false).await.unwrap());
            assert_eq!(name(db.clone()).await, "Ann B");
            assert!(db.record_name(account, "friend", "Annie from work", true).await.unwrap());
            assert!(!db.profile_name(account, "friend", "Ann C").await.unwrap());
            assert_eq!(name(db.clone()).await, "Annie from work");
            db.record_name(account, "friend", "Ann", false).await.unwrap();
            assert!(db.profile_name(account, "friend", "Ann C").await.unwrap());
            assert_eq!(name(db.clone()).await, "Ann C");
            assert!(db.accepted(2).await.unwrap());
        });
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
