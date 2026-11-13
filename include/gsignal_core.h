// The protocol core: a Rust static library that owns every account, its connections and the
// encrypted message database. The app calls it through this header and holds no protocol state.
//
// The core speaks of accounts, conversations and messages, whatever protocol an account uses.
// Three kinds of call:
//   - commands return at once and do their work on the core's own threads;
//   - events are queued by the core and drained by the app with gsc_event_next;
//   - queries read the database.
// Strings and arrays the core hands out stay valid until the next call of the same function on the
// same core, unless the function says otherwise. All calls are made from one thread, the app's.
#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#define GSC_KEY_LEN 32

typedef struct gsc_core gsc_core;

// Fills key with GSC_KEY_LEN bytes from the system's random source. Returns 0, or -1 on failure.
int gsc_key_new(uint8_t *key);

// Called on one of the core's threads when the event queue goes from empty to non-empty, so the app
// can wake its event loop. It must not call back into the core.
typedef void gsc_wake_fn(void *user);

// Opens the databases in dir (which ends with a path separator), encrypted with the raw key, and
// reconnects every account already signed in. Returns NULL on failure, with the reason in *error
// when error is not NULL (free it with gsc_string_free).
gsc_core *gsc_open(const char *dir, const uint8_t *key, gsc_wake_fn *wake, void *user, char **error);
void gsc_close(gsc_core *core);
void gsc_string_free(char *s);

// ---- Accounts ----

typedef enum {
    GSC_ACCOUNT_SIGNING_IN,   // waiting for the user to approve this device; a GSC_EVENT_SIGN_IN_CODE says how
    GSC_ACCOUNT_CONNECTING,
    GSC_ACCOUNT_CATCHING_UP,  // connected, receiving what arrived while away
    GSC_ACCOUNT_ONLINE,
    GSC_ACCOUNT_OFFLINE,      // the connection was lost; the core retries on its own
} gsc_account_state;

typedef struct {
    int64_t id;
    const char *protocol;      // "signal" or "irc"
    const char *label;         // the account's own name for itself, such as a phone number; "" until known
    gsc_account_state state;
    int64_t last_received_ms;  // when anything last arrived, in Unix milliseconds; 0 for never
    int undecryptable;         // messages received since the core opened that could not be decrypted
    const char *error;         // the last error, or ""
} gsc_account;

// Adds an account and starts signing it in as a device called device_name. Returns its id, or -1
// for an unknown protocol.
// For "irc", device_name is the address of the IRC bouncer, irc[s]://USER:PASSWORD@HOST:PORT[?sha256=FINGERPRINT].
int64_t gsc_account_add(gsc_core *core, const char *protocol, const char *device_name);
// Writes up to max accounts to out and returns how many there are in all.
int gsc_accounts(gsc_core *core, gsc_account *out, int max);

// ---- Conversations and messages ----

typedef enum { GSC_DIRECT, GSC_GROUP } gsc_conversation_kind;

typedef struct {
    int64_t id, account;
    gsc_conversation_kind kind;
    const char *title;
    int64_t last_ms;        // its newest message's time, in Unix milliseconds; 0 when it has none
    const char *last_text;  // that message's text, or ""
    int unread;             // received messages not yet read
    bool muted;             // the user wants no notifications from it
    bool request;           // it is from someone who wrote unasked and has not been accepted: gsc_accept accepts it
    int64_t avatar_ms;      // when its picture last changed, 0 when it has none; gsc_avatar gives the picture
    int person;             // the number its colour goes by: with one person, theirs as gsc_message has it; a group's own, from the same count; -1 for the account's notes and for one nothing was written in
    const char *last_sender;  // in a group, who wrote the newest message ("me" for the account); "" elsewhere
    bool admin;             // a group the account administers, which may change who is in it
    int unread_mentions;    // of the unread messages, those that name the account (on IRC, its nick)
} gsc_conversation;

// Conversations, newest first; those without messages follow, by title. Writes up to max and
// returns how many there are in all.
int gsc_conversations(gsc_core *core, gsc_conversation *out, int max);

// A conversation's picture, a person's or a group's, as the bytes of an image file (JPEG as a rule):
// valid until the next call, with their count in *len; NULL when it has none. The core fetches the
// pictures of conversations that hold messages, and of the people who wrote in them, and asks the
// service whether one has changed when its person writes or its conversation is opened (at most
// once a minute each) and once a day besides; a GSC_EVENT_CONVERSATIONS follows when one changes,
// with a new avatar_ms.
const void *gsc_avatar(gsc_core *core, int64_t conversation, size_t *len);

// The picture of the person who wrote a message, in the form and for the time gsc_avatar gives a
// conversation's (the two share their buffer); NULL when they have none.
const void *gsc_writer_picture(gsc_core *core, int64_t message, size_t *len);

// The conversation in which an account writes notes to itself, which its other devices show too;
// 0 until the account has signed in once.
int64_t gsc_note_to_self(gsc_core *core, int64_t account);

// What is being written in a conversation and not yet sent is kept in the database, so that it is
// there after the program has been closed or has stopped: its text, and in `files` the paths of
// the files to go with it, a line each. gsc_draft_set with both empty forgets it. gsc_draft
// returns the text and sets *files; both strings stay valid until the next call.
void gsc_draft_set(gsc_core *core, int64_t conversation, const char *text, const char *files);
const char *gsc_draft(gsc_core *core, int64_t conversation, const char **files);

// Keeps the program installed in `folder` up to date: every minute the core reads a signed manifest
// from the update server, and when it describes another build, fetches and checks the files that
// differ, puts them in place of the old ones and sends GSC_EVENT_UPDATED. Each step is in the log.
void gsc_update_start(gsc_core *core, const char *folder);
// Whether it looks: off until the app turns it on, which it does once the user agrees. A build
// named on a "skip BUILD" line of update.txt in the data folder is not taken; one taken is named in
// updated.txt there, for the restarted program to say so.
void gsc_update_enable(bool on);
// The build this program is (a commit's short hash), as the update server names it.
const char *gsc_build(void);

// Each look for updates also reports, unless gsc_report_use(false) says not to, a random number
// kept in the data folder's install.txt, the build, and the window's state: hidden, shown behind
// others, in front, or in front and used within the last minute. gsc_window_state keeps that
// state: 0 hidden, 1 shown behind others, 2 in front; `input` when a key or the pointer was used.
void gsc_window_state(int state, bool input);
void gsc_report_use(bool on);

// Adds a line to gsignal.log in the data folder, where the core writes the account's changes of
// state and errors, the warnings of the libraries beneath it and a panic's last words. The log is
// for explaining afterwards what went wrong; nothing put in it may hold message text. It does
// nothing before gsc_open, unless gsc_log_start has named the data folder: that is for a program
// that opens no core, such as one that only watches over another.
void gsc_log_start(const char *dir);
void gsc_log(const char *text);

// Accepts a conversation held apart as a request. Until then the core tells its writer neither that
// their messages were read nor that an answer is being typed; sending them a message accepts it
// too, and so does accepting it on another of the account's devices. The account's other devices
// are told when the account is online. A GSC_EVENT_CONVERSATIONS follows.
void gsc_accept(gsc_core *core, int64_t conversation);

// Sets the disappearing timer of a conversation with one person, in seconds (0 turns it off): it
// is sent to them, applies to what is sent after, and a line in the conversation says so. A
// group's timer cannot be set yet. gsc_timer gives the timer as last known. It needs the account
// online.
void gsc_set_timer(gsc_core *core, int64_t conversation, int seconds);
int gsc_timer(gsc_core *core, int64_t conversation);

// Mutes a conversation or lets it speak again, on this device only. A GSC_EVENT_CONVERSATIONS follows.
void gsc_mute(gsc_core *core, int64_t conversation, bool muted);

// How far an outgoing message has got. A protocol without receipts leaves it at sent. One that
// waits is sent when the account is online; one not sent after several tries waits for gsc_resend,
// and so does one held because the safety number of someone it goes to changed, which a notice
// in the conversation explains.
typedef enum { GSC_HELD = -3, GSC_NOT_SENT = -2, GSC_WAITING = -1, GSC_SENT, GSC_DELIVERED, GSC_READ } gsc_message_status;

// What a message is. The notices are the core's own, written as a sentence in the message's text.
typedef enum {
    GSC_TEXT,
    GSC_UNDECRYPTABLE,      // a message from the sender arrived that could not be decrypted
    GSC_IDENTITY_CHANGED,   // the sender's key changed: a new phone or reinstall, or someone in between
    GSC_DELETED,            // the sender deleted it for everyone; its text is ""
    GSC_EVENT,              // something that happened in the conversation: a call that came, a member joining or leaving, a new name or timer
} gsc_message_kind;

typedef struct {
    int64_t id;
    bool outgoing;
    const char *sender;         // the sender's name as the core knows it now; "" for outgoing messages
    int64_t sent_ms;            // Unix milliseconds
    const char *text;
    gsc_message_status status;  // for outgoing messages
    int64_t expires_ms;         // when it will disappear, in Unix milliseconds; 0 when it will not, or not yet
    gsc_message_kind kind;
    bool edited;                // its text was changed after it was sent
    const char *reply_to;       // who wrote the message it replies to, "" for the account; NULL when it replies to nothing
    const char *reply_text;     // the start of that message's text
    const char *reactions;      // each emoji it was given, followed by how many gave it when more than one; or ""
    const char *reacted;        // the same with who gave each in place of how many, "me" for the account; or ""
    const char *hidden;         // the text as written, when `text` has parts covered that its sender hid (a spoiler), for showing when asked; or ""
    int changing;               // a reaction, edit or deletion the user asked of it: 0 none, 1 waiting to be sent, 2 given up after several tries (gsc_changes_again)
    const char *reaction;       // the emoji the account gave it, or ""
    int attachments;            // how many files came with it; gsc_attachments lists them
    int person;                 // the number of the person who wrote it, the same for all they write and no one else's, counted from 0 in the order people were first heard from; -1 for the account
    int64_t picture_ms;         // when that person's picture last changed, 0 when they have none; gsc_writer_picture gives it
    int64_t reply_id;           // the message it replies to, when that is held; 0 otherwise
    const char *styles;         // the styles its sender gave parts of its text: a line "STYLE START END" each,
                                // in characters of text or hidden alike; STYLE 1 bold, 2 italic, 4 struck through, 5 monospace
} gsc_message;

// Up to max messages of a conversation in the order they came to this device (the time each says
// it was sent is its sender's clock's, and may be out of that order), ending just before the message
// with id before, or at the newest when before is 0. Returns how many were written. Their strings
// stay valid until the next call with before 0, so older messages can be read to go in front of
// those already held.
int gsc_messages(gsc_core *core, int64_t conversation, int64_t before, gsc_message *out, int max);
// One message, as gsc_messages gives it, for one that arrived or changed: false when it is gone. Its
// strings replace those this message had and last until it is read again or gsc_messages reads
// from the newest; others' stay.
bool gsc_message_read(gsc_core *core, int64_t id, gsc_message *out);

// A member of a group other than the account: key is the core's identifier for them, name "" when
// they have none yet.
typedef struct {
    const char *key, *name;
} gsc_member;

// Writes up to max of a group's members to out, by name, and returns how many it has.
int gsc_members(gsc_core *core, int64_t conversation, gsc_member *out, int max);

// Removes a member, by key, from a group the account administers: the service makes the change and
// the group's members, the removed one among them, are told. The conversation says so when it is
// done; when it cannot be, the account's error says why.
void gsc_remove_member(gsc_core *core, int64_t conversation, const char *key);

// The members a group's picture is made of when it has none of its own: up to max of them, those
// who wrote most there in the last 30 days first, the rest of a quiet group in a fixed order.
// person is the number their colour goes by (-1 for none yet), picture_ms when their picture last
// changed (0 for none); gsc_face_picture gives it, as gsc_avatar does. Strings last until the next call.
typedef struct {
    const char *key, *name;
    int person;
    int64_t picture_ms;
} gsc_face;
int gsc_faces(gsc_core *core, int64_t conversation, gsc_face *out, int max);
const void *gsc_face_picture(gsc_core *core, int64_t conversation, const char *key, size_t *len);

// A person named in a text being sent: the member's key, and where their "@name" starts in the
// text's bytes and how many bytes long it is. They are told they were mentioned.
typedef struct {
    const char *key;
    int start, length;
} gsc_mention;

// Sends text to a conversation, as a reply to the message reply_to when that is not 0, with the
// nfiles files at the paths in files and the nmentions mentions; the text may be "" when there are
// files. Pictures and videos go together with the text; any other file goes in a message of its
// own after them, since Signal's own programs show only the first such file of a message. Each
// message is stored at once, as GSC_WAITING, and appears through a GSC_EVENT_MESSAGE;
// it is sent when the account is online, and a GSC_EVENT_CONVERSATION follows each change of its
// status. A file that cannot be read appears as the account's error, and nothing is stored.
void gsc_send(gsc_core *core, int64_t conversation, const char *text, int64_t reply_to, const char *const *files, int nfiles, const gsc_mention *mentions, int nmentions);

// A file that came with a message. Files are fetched by the core as their messages arrive, up to
// 32 MB each, and kept in its database; a GSC_EVENT_CONVERSATION follows when one's state changes.
typedef enum {
    GSC_ATTACHMENT_WAITING,   // not fetched yet: on its way, or over 32 MB and waiting to be asked for
    GSC_ATTACHMENT_HERE,
    GSC_ATTACHMENT_FAILED,    // fetching it was given up
} gsc_attachment_state;

typedef struct {
    int64_t id;
    const char *type;         // its media type, such as "image/png"
    const char *name;         // its file name, or ""
    int64_t size;             // in bytes
    int width, height;        // of an image, in pixels; 0 when unknown
    bool voice;               // a voice note
    bool looped;              // a video sent as a moving picture: to be played silent and over again
    gsc_attachment_state state;
} gsc_attachment;

// Writes up to max of a message's attachments to out and returns how many it has.
int gsc_attachments(gsc_core *core, int64_t message, gsc_attachment *out, int max);
// Fetches an attachment the core did not fetch by itself, because it is over 32 MB or fetching it
// was given up. A GSC_EVENT_CONVERSATION follows when it is here or has failed again.
void gsc_attachment_fetch(gsc_core *core, int64_t attachment);
// An attachment's bytes, valid until the next call, with their count in *len; NULL when not held.
const void *gsc_attachment_data(gsc_core *core, int64_t attachment, size_t *len);
// Writes an attachment's bytes to a file. False when they are not held or the file cannot be written.
bool gsc_attachment_save(gsc_core *core, int64_t attachment, const char *path);

// These three change a message for everyone in its conversation and show through a
// GSC_EVENT_CONVERSATION once sent. A person has one reaction to a message: emoji replaces the
// account's, and "" takes it away. Only the account's own messages can be edited or deleted, and
// the service refuses an edit of a message more than a day old.
void gsc_react(gsc_core *core, int64_t message, const char *emoji);
void gsc_edit(gsc_core *core, int64_t message, const char *text);
void gsc_delete(gsc_core *core, int64_t message);

// A reaction, edit or deletion the user asked of a message is kept until it is sent, and shows in
// the message once it has been. One given up after several tries leaves the message's `changing`
// at 2: `again` true sends such changes once more, false drops them.
void gsc_changes_again(gsc_core *core, int64_t message, bool again);

// Tries a GSC_NOT_SENT message again, and sends a GSC_HELD one although the safety number changed.
// gsc_delete removes one that is waiting, not sent or held.
void gsc_resend(gsc_core *core, int64_t message);

// Marks every message of a conversation as read, here and on the account's other devices, and
// tells the people who wrote them when the account sends read receipts. A disappearing message's
// time starts when it is read.
void gsc_mark_read(gsc_core *core, int64_t conversation);

// Tells the conversation's other members that the user is typing, or has stopped. They take the
// user to have stopped 15 seconds after the last call, so call it again every 10 while typing goes
// on. Sending a message stops it without a call. Nothing is sent when the account has typing
// indicators turned off.
void gsc_typing(gsc_core *core, int64_t conversation, bool typing);

// Starts a new session with the person a direct conversation is with, for when their messages do not
// decrypt or they report the same of this account's. The core does this by itself after a message
// it cannot decrypt, at most once an hour per person; this is for the user to ask again. It does
// nothing for a group.
void gsc_reset_session(gsc_core *core, int64_t conversation);

// A message found by a search.
typedef struct {
    int64_t message, conversation;
    const char *title;      // the conversation's
    bool outgoing;
    const char *sender;     // "" for outgoing messages
    int64_t sent_ms;
    const char *text;
    int newer;              // how many messages of its conversation are newer than it
} gsc_hit;

// Up to max messages, of every conversation, whose text contains query (whatever its case, for
// ASCII letters), newest first. Returns how many were written; an empty query finds none.
int gsc_search(gsc_core *core, const char *query, gsc_hit *out, int max);

// ---- Events ----

typedef enum {
    GSC_EVENT_ACCOUNT,        // an account's state, label or counters changed
    GSC_EVENT_SIGN_IN_CODE,   // show this code to the user: text is a link, code its QR code
    GSC_EVENT_CONVERSATIONS,  // conversations were added or renamed
    GSC_EVENT_MESSAGE,        // a message was stored: conversation and message say which
    GSC_EVENT_CONVERSATION,   // many of a conversation's messages changed or were removed (they expired, a notice was added): read them again
    GSC_EVENT_TYPING,         // text names who started or stopped typing in conversation, as typing says
    GSC_EVENT_UPDATED,        // a newer build has replaced this one's files: start the program again
    GSC_EVENT_MESSAGE_CHANGED,  // one message changed (status, reactions, text, files, changes waiting) or is gone: message says which; gsc_message_read reads it
    GSC_EVENT_LISTED,         // what the list shows of conversation changed (its unread count), and none of its messages as shown
} gsc_event_type;

typedef struct {
    gsc_event_type type;
    int64_t account, conversation, message;
    const char *text;
    const uint8_t *code;  // code_size * code_size modules, row by row, 1 for dark
    int code_size;
    bool typing;  // take it as false again 15 seconds after the last such event from the same person
    int64_t made_ms;  // when a sign-in code was made, in ms since 1970: it can wait unread while the app is paused
} gsc_event;

// Takes the oldest event. Returns false when there is none.
bool gsc_event_next(gsc_core *core, gsc_event *out);
