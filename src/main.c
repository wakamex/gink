// gink: the window over the core. It shows each account's health, the code that links a new
// device, the conversations, and the selected conversation's messages with a field to send from.
// Enter sends and Shift+Enter starts a new line.
//   gink [--data DIR] [--open TITLE] [--shot F.png [--after S]] [--scroll-test N] [--offscreen] [--size W H]
// --open starts on the conversation with that title; --scroll-test scrolls the messages up by one
// wheel step a frame for N frames, prints how long the frames took to draw, and exits; --shot writes the frame drawn S seconds after starting (default 5) and exits; --offscreen draws
// without a display; --size makes the window W by H points, in place of 900 by 600.
#include <SDL3/SDL.h>
#include <SDL3/SDL_main.h>
#include <stdio.h>
#include <string.h>
#ifdef _WIN32
#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <shellapi.h>
#endif

#include "gs_background.h"
#include "gs_desk.h"
#include "gs_draw.h"
#include "gs_gif.h"
#include "gs_image.h"
#include "gs_mix.h"
#include "gs_movie.h"
#include "gs_notify.h"
#include "gs_sound.h"
#include "gs_ui.h"
#include "gs_video.h"
#include "gsignal_core.h"
#include "key.h"

#define MAX_ACCOUNTS 8
#define MAX_CONVERSATIONS 1024
#define FACES 3             // the most members a group's own picture is made of
#define MAX_MESSAGES 10000
#define FOUND_MS 2000       // how long a message gone to from a search or a reply stays marked
#define MESSAGES_FIRST 200  // how many of a conversation's newest messages are read when it is opened
#define MESSAGES_STEP 50    // how many older ones are read each time the reader nears the oldest held

// The fonts to choose between: the system's, and two shipped beside the program, each of which
// draws what it has and leaves the rest to the system's.
enum { FONT_SYSTEM, FONT_MONO, FONT_INTER, FONTS };
static const char *const font_names[FONTS] = { "System font", "Clanker Mono", "Inter" };
static const char *const font_files[FONTS] = { NULL, "ClankerMono-Regular.ttf", "Inter-Regular.ttf" };

// The ways of telling one person's messages from the next's.
enum {
    TURNS_PLAIN,    // a grey name over each message
    TURNS_NAMES,    // each person's name in a colour of their own, and more room where the speaker changes
    TURNS_BARS,     // the same, with a bar of that colour down the left of each message
    TURNS_COLUMNS,  // as a chat log: the time and the name in columns at the left, the text beside them
    TURNS_WAYS,
};
#define MAX_FILES 4096      // attachments of the messages held that the window keeps track of
#define MAX_HITS 200        // messages a search shows
#define MAX_NAMED_LIKE 20    // conversations a search shows by name
#define MAX_MEMBERS 256      // members of a group that can be named in a message
#define MAX_NAMED 16         // people one message can name
#define MAX_MOVIES 6         // videos kept open at once, as stills or playing
#define MAX_GIFS 8           // moving pictures kept open at once, as stills or playing
#define MAX_PICTURES 2048   // pictures of people and groups given to the image cache in one run
#define MAX_ATTACH 32       // files that can go with one message, which is as many as Signal's own programs allow
#define FETCHED_UNASKED (32 << 20)  // the size up to which the core fetches a file by itself
#define IMAGE_MAX 320.0f    // the longest side an image is shown at, in points

// A file that came with one of the messages held.
typedef struct {
    int64_t message, id;
    char name[96], type[40];
    int64_t size;
    int w, h;
    bool looped;            // a video to play silent and over again
    gsc_attachment_state state;
} held_file;

typedef struct {
    gsc_core *core;
    SDL_Window *window;
    SDL_Renderer *renderer;
    gs_images *images;
    Uint64 pictures[MAX_PICTURES];  // the pictures the image cache was given, each by a number made from its name there
    int npictures;
    // The choices made in the status bar, kept in a file between runs.
    bool square_pictures;   // pictures are shown whole in a square, not cut to a disc
    int font;               // which of the fonts below text is drawn in
    double font_size;       // of message text, in points; the other sizes follow it
    double line_extra;      // points of room added between lines, over what the font itself asks
    bool controls;          // the status bar shows every choice and the figures for judging them, not the text size alone
    int turns;              // how one person's messages are told from the next's: one of the TURNS_ ways below
    bool compact;           // a message soon after the same person's last is a line under it, with no name over it
    bool hardware_video;    // videos are decoded by the graphics card where that can be had
    bool start_at_login;    // the system starts the program, hidden, when the user signs in (Windows only so far)
    char video_path[24];    // how the last video drawn was decoded, as gesso names it
    char renderer_path[1100];  // the file that says what draws and what decodes
    char open_path[1100];   // the file that keeps which conversation is open, for the next start
    bool soft;              // the colours are the softer ones: greyer behind, less bright in front
    gs_ui_style sharp;      // the colours the interface came with
    char settings_path[1100];
    gs_fontset *fonts[FONTS];
    // The faces for styled text, for a proportional font and a monospaced one: bold, italic, both.
    gs_fontset *styled[2][3];
    held_file files[MAX_FILES];  // in the order of their messages
    int nfiles;
    int64_t pictured[MAX_FILES]; // the attachments whose bytes the image cache was given
    int npictured;
    char attach[MAX_ATTACH][1024];  // the files that will go with the draft
    int nattach;
    Uint8 attach_kind[MAX_ATTACH];  // of each: not looked at yet, a picture the image cache was given, or neither (0 past the last)
    const char *reacted;    // who gave the reactions the pointer is on this frame, and where that line is
    gs_rect reacted_at;
    bool input;             // a key or the pointer was used since the last frame
    bool report_use;        // each look for updates reports the installation's use (on unless the settings file says not)
    bool updated;           // a newer build is in place, and the program ends to start again as it once the user leaves it be
    bool updating;          // this copy updates itself (an installed one): a test build, which says so
    bool update_asked;      // the user is being asked whether it may
    bool update_on;         // they agreed, and have not turned it off since
    bool restart_now;       // they asked for the new build at once
    Uint64 last_input;      // when they last pressed a key or used the pointer, in ticks
    char update_path[1100]; // update.txt in the data folder: the answer, and builds not to take again
    char notice[160];       // said once in the status bar after an update or its undoing, until clicked
    bool timer_open;        // the choices of disappearing timer are shown, in this place
    gs_rect timer_panel, timer_button;
    bool to_draft;          // the keyboard goes to the message field when it is next drawn
    bool to_search;         // the keyboard goes to the search field, its words selected, when it is next drawn
    bool members_open;      // a group's members are listed, in this place
    gs_rect members_panel, members_button;
    int removing;           // the member the user asked to remove, as a place in `members`, until they confirm; -1 for none
    int64_t unread_from;    // the first message that was unread when the open conversation was opened; 0 when none was
    bool show_unread;       // it has yet to be brought into view
    char opened[1024];      // the folder a file is written to for the system to open it
    char pasted[1024];      // where a picture pasted from the clipboard is written until it is sent: the data folder and the start of a name
    int npasted;            // how many have been, which numbers them
    int shown_chars;        // the characters of message text in view when they were last counted
    Uint64 counted;         // when that was, in ticks
    float counted_y;        // and where the list stood
    gs_rect area;           // the whole window, in points
    gs_rect within;         // the list being drawn, which what its rows draw must stay inside
    int64_t viewing;        // the picture shown whole in place of the messages, 0 for none
    char viewing_name[256]; // its file's name, offered when it is saved
    bool to_play;           // a video just opened, to be played when it is first drawn
    // The audio attachment that is playing or paused: its sound, and the copy of its bytes the sound reads.
    int64_t sounding;
    gs_sound *sound;
    void *sound_bytes;
    // The videos in view: each open on a copy of its file's bytes, at rest as a still until played.
    struct { int64_t id; gs_movie *movie; void *bytes; Uint64 drawn; bool sounding; } movies[MAX_MOVIES];
    // GIFs, which play on their own while they are drawn: the frame each was last drawn in.
    struct { int64_t id; gs_gif *gif; void *bytes; Uint64 drawn; } gifs[MAX_GIFS];
    Uint64 frame;           // counts the frames drawn
    int64_t no_movie[64];   // video attachments found not to be ones gesso plays
    int nno_movie;
    bool mixing;            // the playback device was opened
    int64_t unplayable;     // the attachment last found not to be a sound that can be played
    float pointer_x;        // where the pointer is, for a click along a sound's bar
    bool pressed, held;     // the left button went down since the last frame; it is down
    float list_w;           // how wide the list of conversations is; LIST_ICONS for their pictures alone
    bool sizing_list;       // the edge of the list is being dragged
    bool at_edge;           // the pointer has the shape it takes over that edge
    int64_t revealed;       // the message whose spoiler the user asked to see
    // The messages some of whose text is selected, and from which byte to which (in either order):
    // a drag may run across several.
    struct { int64_t id; int from, to; } picked[MAX_MESSAGES];
    int npicked;
    SDL_Cursor *across, *arrow;
    int64_t asked[MAX_ATTACH * 8];  // attachments the user asked to have fetched in this run
    int nasked;
    // The open group's members, and those the draft names with "@" and their name.
    struct { char key[64], name[96]; } members[MAX_MEMBERS], named[MAX_NAMED];
    int nmembers, nnamed;
    char query[128];        // what is searched for; the list shows what was found while it is not empty
    gsc_hit hits[MAX_HITS];
    int named_like[MAX_NAMED_LIKE], nnamed_like;  // conversations whose names hold the words, as places in `conversations`
    int nhits, hit;         // how many messages were found, and the row selected among the names and them
    int64_t found;          // the message last opened from a search, marked among the others
    bool show_found;        // it has yet to be brought into view
    Uint64 found_until;     // when its mark goes, in ticks
    // What the core last reported, read again when an event says it changed. The strings belong to
    // the core and stay valid until the same list is read again.
    gsc_account accounts[MAX_ACCOUNTS];
    gsc_conversation conversations[MAX_CONVERSATIONS];
    // For each group without a picture of its own, the members its picture is made of instead.
    struct { int n; struct { char key[80], name[64]; int person; int64_t picture_ms; } face[FACES]; } faces[MAX_CONVERSATIONS];
    gsc_message messages[MAX_MESSAGES];
    float heights[MAX_MESSAGES];
    float heights_width;    // the width the heights were measured at; 0 when the messages changed since
    int naccounts, nconversations, nmessages;
    int listed[MAX_CONVERSATIONS], nlisted;  // the conversations the list shows, as places in `conversations`
    int wanted;             // how many of the open conversation's newest messages to hold
    bool oldest_held;       // the open conversation has no messages older than those held
    int selected;           // the row selected in the conversation list, a place in `listed`
    int64_t open;           // the conversation whose messages are shown, 0 for none
    bool narrow;            // the window has room for the list or a conversation, not both (a phone held upright)
    bool adding_irc;        // the field for an IRC bouncer's address is open under the status bar
    bool settings_open;     // in a narrow window, the settings are shown on a row under the status bar
    char irc_address[512];
    bool focused;           // the window has the keyboard: only then is what it shows counted as read
    // The code to show while an account waits to be linked.
    int64_t code_account;
    char code_link[512];
    const char *kept;       // the text of the notice that keeps the app online in the background, NULL when not kept
    Uint64 code_since;      // when the code came, while one is shown (Signal takes a code for about 90 seconds)
    uint8_t code[177 * 177];  // (the largest QR code is 177 modules a side)
    int code_size;
    char draft[64 * 1024];  // what is being written: as long as Signal Desktop takes, beyond 2,000 bytes sent as a text attachment
    // What was being written in other conversations, each kept for when its conversation is opened again.
    Uint32 draft_kept, draft_seen;  // the mark of the draft as it was last kept in the database, and as it last was
    Uint64 draft_changed;   // when it last changed without being kept; 0 when kept
    Uint64 told_typing;     // when the open conversation was last told the user is typing, in ticks; 0 when it was not
    // Who is typing in the open conversation, until the time their last notice runs out.
    char typing_name[128];
    Uint64 typing_until;
    int64_t menu;           // the message whose menu was last opened
    bool menu_open;         // and whether it is open now: while it is, the rows behind it take no clicks
    int64_t replying, editing;  // the message the draft answers, or replaces; 0 for none
    char answered[160];     // one line saying which message that is
} app;

#define TYPING_RESEND_MS 10000
#define TYPING_SHOWN_MS 15000

static void wake(void *user) {
    SDL_Event e = { .type = SDL_EVENT_USER };
    SDL_PushEvent(&e);
}

// Notes the files of messages[first..end), after those already noted.
static void read_files(app *a, int first, int end) {
    for (int i = first; i < end; i++) {
        gsc_attachment got[MAX_ATTACH];
        int n = a->messages[i].attachments ? SDL_min(gsc_attachments(a->core, a->messages[i].id, got, MAX_ATTACH), MAX_ATTACH) : 0;
        for (int k = 0; k < n && a->nfiles < MAX_FILES; k++) {
            held_file *f = &a->files[a->nfiles++];
            *f = (held_file){ .message = a->messages[i].id, .id = got[k].id, .size = got[k].size, .w = got[k].width, .h = got[k].height, .looped = got[k].looped, .state = got[k].state };
            // (a video that came without its size has it from its open movie, once known, so that
            // reading the messages again does not lay it out at the stand-in size for a frame)
            for (int m = 0; m < MAX_MOVIES && !f->w; m++)
                if (a->movies[m].movie && a->movies[m].id == f->id) gs_movie_size(a->movies[m].movie, &f->w, &f->h);
            snprintf(f->name, sizeof f->name, "%s", got[k].name);
            snprintf(f->type, sizeof f->type, "%s", got[k].type);
        }
    }
}

// The files of a message, which lie together; *count says how many.
static const held_file *files_of(const app *a, const gsc_message *m, int *count) {
    *count = 0;
    if (!m->attachments) return NULL;
    int first = 0;
    while (first < a->nfiles && a->files[first].message != m->id) first++;
    while (first + *count < a->nfiles && a->files[first + *count].message == m->id) ++*count;
    return &a->files[first];
}

static void read_members(app *a) {
    static gsc_member got[MAX_MEMBERS];
    a->nmembers = a->open ? SDL_min(gsc_members(a->core, a->open, got, MAX_MEMBERS), MAX_MEMBERS) : 0;
    for (int i = 0; i < a->nmembers; i++) {
        snprintf(a->members[i].key, sizeof a->members[i].key, "%s", got[i].key);
        snprintf(a->members[i].name, sizeof a->members[i].name, "%s", got[i].name);
    }
}

static void read_messages(app *a) {
    a->nmessages = a->open ? gsc_messages(a->core, a->open, 0, a->messages, a->wanted) : 0;
    a->heights_width = 0;
    a->nfiles = 0;
    read_files(a, 0, a->nmessages);
}

// The conversation whose notification the user clicked, handed over from the notifications' thread.
static SDL_AtomicInt notified;
// Asked for from the tray's menu, or by the program being started a second time: the window, or the end.
static SDL_AtomicInt wants_window, wants_quit;

static void SDLCALL tray_show(void *user, SDL_TrayEntry *entry) { SDL_SetAtomicInt(&wants_window, 1), wake(NULL); }
static void SDLCALL tray_quit(void *user, SDL_TrayEntry *entry) { SDL_SetAtomicInt(&wants_quit, 1), wake(NULL); }
static void started_again(int argc, char **argv) { SDL_SetAtomicInt(&wants_window, 1), wake(NULL); }

// The tray's picture: a disc in the accent colour, with a dot when something is unread.
static SDL_Surface *tray_icon(bool unread) {
    SDL_Surface *icon = SDL_CreateSurface(32, 32, SDL_PIXELFORMAT_RGBA32);
    if (!icon) return NULL;
    for (int y = 0; y < 32; y++)
        for (int x = 0; x < 32; x++) {
            float dx = x - 15.5f, dy = y - 15.5f, ux = x - 24.5f, uy = y - 7.5f;
            Uint8 *p = (Uint8 *)icon->pixels + y * icon->pitch + x * 4;
            if (unread && ux * ux + uy * uy < 6.5f * 6.5f) p[0] = 255, p[1] = 255, p[2] = 255, p[3] = 255;
            else if (dx * dx + dy * dy < 15 * 15) p[0] = 46, p[1] = 196, p[2] = 166, p[3] = 255;
            else p[3] = 0;
        }
    return icon;
}

static void notification_clicked(const char *tag) {
    SDL_SetAtomicInt(&notified, SDL_atoi(tag));
    wake(NULL);
}

// Works out which conversations the list shows: those with messages, and the open one. A person
// the account knows but has exchanged nothing with is found by searching for their name.
static void list_conversations(app *a) {
    a->nlisted = 0;
    for (int i = 0; i < a->nconversations; i++) {
        if (!a->conversations[i].last_ms && a->conversations[i].id != a->open) continue;
        if (a->conversations[i].id == a->open) a->selected = a->nlisted;
        a->listed[a->nlisted++] = i;
    }
}

// Reads, for each group without a picture of its own, the members whose faces make one.
static void read_faces(app *a) {
    for (int i = 0; i < a->nconversations; i++) {
        const gsc_conversation *c = &a->conversations[i];
        a->faces[i].n = 0;
        if (c->kind != GSC_GROUP || c->avatar_ms) continue;
        gsc_face got[FACES];
        int n = SDL_min(gsc_faces(a->core, c->id, got, FACES), FACES);
        for (int k = 0; k < n; k++) {
            snprintf(a->faces[i].face[k].key, sizeof a->faces[i].face[k].key, "%s", got[k].key);
            snprintf(a->faces[i].face[k].name, sizeof a->faces[i].face[k].name, "%s", got[k].name[0] ? got[k].name : "?");
            a->faces[i].face[k].person = got[k].person, a->faces[i].face[k].picture_ms = got[k].picture_ms;
        }
        a->faces[i].n = n;
    }
}

// Reads the conversations again. One that has more unread messages than before gets a notification,
// unless the user is looking at it, and one whose messages were all read loses its own.
static void read_conversations_and_notify(app *a) {
    static struct { int64_t id; int unread; } before[MAX_CONVERSATIONS];
    int nbefore = a->nconversations;
    for (int i = 0; i < nbefore; i++) before[i].id = a->conversations[i].id, before[i].unread = a->conversations[i].unread;
    a->nconversations = SDL_min(gsc_conversations(a->core, a->conversations, MAX_CONVERSATIONS), MAX_CONVERSATIONS);
    read_faces(a);
    for (int i = 0; i < a->nconversations; i++) {
        const gsc_conversation *c = &a->conversations[i];
        int was = 0;
        for (int k = 0; k < nbefore; k++)
            if (before[k].id == c->id) was = before[k].unread;
        char tag[24];
        snprintf(tag, sizeof tag, "%lld", (long long)c->id);
        // (in a group the notification says who wrote, as the list's line does)
        char body[600];
        snprintf(body, sizeof body, c->last_sender[0] ? "%s: %s" : "%s%s", c->last_sender[0] ? c->last_sender : "", c->last_text);
        if (c->unread > was && !c->muted && !(c->id == a->open && a->focused)) gs_notify_show(tag, c->title, body);
        else if (was && !c->unread) gs_notify_close(tag);
    }
    list_conversations(a);
}

static void read_conversations(app *a) {
    a->nconversations = SDL_min(gsc_conversations(a->core, a->conversations, MAX_CONVERSATIONS), MAX_CONVERSATIONS);
    read_faces(a);
    list_conversations(a);
}

// One message that arrived or changed, read alone and put in its place among those held, or taken out
// when it is gone; the rest are not read again, and their heights are kept. A message newer than all
// held goes at the end. One not held and older than the newest held is left for the next reading.
static void take_message(app *a, int64_t id) {
    gsc_message m;
    bool here = gsc_message_read(a->core, id, &m);
    int i = 0;
    while (i < a->nmessages && a->messages[i].id != id) i++;
    bool held = i < a->nmessages;
    if (!held && (!here || (a->nmessages && id < a->messages[a->nmessages - 1].id))) return;
    // (its files go, and come back read again after the others)
    if (held) {
        int kept = 0;
        for (int k = 0; k < a->nfiles; k++)
            if (a->files[k].message != id) a->files[kept++] = a->files[k];
        a->nfiles = kept;
    }
    if (held && !here) {
        memmove(&a->messages[i], &a->messages[i + 1], (size_t)(a->nmessages - i - 1) * sizeof a->messages[0]);
        a->nmessages--;
    } else {
        if (!held && a->nmessages == MAX_MESSAGES) {  // (the oldest held makes room)
            int64_t oldest = a->messages[0].id, kept = 0;
            for (int k = 0; k < a->nfiles; k++)
                if (a->files[k].message != oldest) a->files[kept++] = a->files[k];
            a->nfiles = (int)kept;
            memmove(&a->messages[0], &a->messages[1], (size_t)(a->nmessages - 1) * sizeof a->messages[0]);
            a->nmessages--, i--;
        }
        if (!held) i = a->nmessages++;
        a->messages[i] = m;
        read_files(a, i, i + 1);
    }
    a->heights_width = 0;  // (measuring again costs little: a text's height is kept)
}

static void drain_events(app *a) {
    gsc_event e;
    bool accounts = false, conversations = false, messages = false, arrived = false;
    while (gsc_event_next(a->core, &e)) {
        if (e.type == GSC_EVENT_ACCOUNT) accounts = true;
        else if (e.type == GSC_EVENT_CONVERSATIONS) conversations = messages = true;  // (a picture may have changed, of someone in the open conversation too)
        else if (e.type == GSC_EVENT_LISTED) conversations = true;
        else if (e.type == GSC_EVENT_MESSAGE_CHANGED || (e.type == GSC_EVENT_MESSAGE && e.message)) {
            // (one message: it alone is read, unless the whole conversation is to be read anyway)
            conversations = true;
            if (e.conversation == a->open && !messages) take_message(a, e.message);
            if (e.type == GSC_EVENT_MESSAGE && e.conversation == a->open) {
                arrived = true, a->typing_until = 0;  // (a message from someone ends their typing)
                if (a->wanted < MAX_MESSAGES) a->wanted++;
            }
        } else if (e.type == GSC_EVENT_MESSAGE || e.type == GSC_EVENT_CONVERSATION) {
            conversations = true, messages |= e.conversation == a->open;
            // (a message from someone ends their typing)
            if (e.type == GSC_EVENT_MESSAGE && e.conversation == a->open) a->typing_until = 0;
            // (and is one more to hold, so that the oldest held does not drop out as each arrives)
            if (e.type == GSC_EVENT_MESSAGE && e.conversation == a->open && a->wanted < MAX_MESSAGES) a->wanted++;
        }
        else if (e.type == GSC_EVENT_TYPING && e.conversation == a->open) {
            snprintf(a->typing_name, sizeof a->typing_name, "%s", e.text);
            a->typing_until = e.typing ? SDL_GetTicks() + TYPING_SHOWN_MS : 0;
        } else if (e.type == GSC_EVENT_UPDATED) {
            a->updated = true;
        } else if (e.type == GSC_EVENT_SIGN_IN_CODE && e.code_size * e.code_size <= (int)sizeof a->code) {
            a->code_account = e.account, a->code_size = e.code_size;
            memcpy(a->code, e.code, (size_t)e.code_size * e.code_size);
            snprintf(a->code_link, sizeof a->code_link, "%s", e.text);
            // (counted from when the core made it: on a phone the app may read it later, when it comes back in front)
            SDL_Time now = 0;
            SDL_GetCurrentTime(&now);
            Sint64 age = SDL_clamp(now / 1000000 - e.made_ms, 0, (Sint64)SDL_GetTicks() - 1);
            a->code_since = SDL_GetTicks() - (Uint64)age;
            gsc_log("new linking code");  // (to match a scan to the code it read)
        }
    }
    if (accounts) a->naccounts = SDL_min(gsc_accounts(a->core, a->accounts, MAX_ACCOUNTS), MAX_ACCOUNTS);
    if (messages) read_messages(a);
    // A message that arrives in the open conversation is read at once.
    if ((messages || arrived) && a->open && a->focused) gsc_mark_read(a->core, a->open);
    if (conversations) read_conversations_and_notify(a), read_members(a);
}

static void clock_text(int64_t ms, char *out, size_t size) {
    SDL_DateTime t;
    if (ms && SDL_TimeToDateTime(SDL_MS_TO_NS(ms), &t, true)) snprintf(out, size, "%02d:%02d:%02d", t.hour, t.minute, t.second);
    else snprintf(out, size, "never");
}

static void open_conversation(app *a, int row, int wanted);
static void clear_attached(app *a);
static void save_draft(app *a);
static void load_draft(app *a);
static void set_update_consent(const char *path, bool yes);
static void close_movies(app *a);

#define FONT_SIZE 15        // the size of message text unless the user chose another
#define FONT_MIN 9
#define FONT_MAX 28
#define LINE_EXTRA_MAX 12

// Puts the chosen sizes into effect, has every message measured again, and with `keep` writes the choices down for the next run.
static void apply_settings(app *a, gs_ui *ui, bool keep) {
    gs_ui_style *s = gs_ui_style_of(ui);
    a->font_size = SDL_clamp(a->font_size, FONT_MIN, FONT_MAX);
    // (the small text and the height of a control keep their proportions to the text: 12 and 30 at 14)
    s->text_px = (float)a->font_size, s->small_px = SDL_roundf(s->text_px * 12 / 14), s->row = SDL_roundf(s->text_px * 30 / 14);
    // (the rest of this frame measures and draws with the font just chosen: the messages' heights are
    // worked out again below, and must not be worked out with the font the frame began with)
    gs_ui_font(ui, a->fonts[a->font]);
    // Lines are as far apart as the font asks (gesso's own spacing, at 0) and then by as much more as chosen.
    float ascent, descent, gap;
    a->line_extra = SDL_clamp(a->line_extra, 0, LINE_EXTRA_MAX);
    s->line_height = a->line_extra > 0 && gs_fontset_vmetrics(a->fonts[a->font], 100, &ascent, &descent, &gap) ? (ascent - descent + gap) / 100 + (float)a->line_extra / s->text_px : 0;
    // The softer colours are near those of Signal's own dark theme: a grey behind in place of
    // near-black, and text a step less bright, which together lower the contrast.
    s->background = a->soft ? (SDL_FColor){ 0.118f, 0.118f, 0.118f, 1 } : a->sharp.background;
    s->panel = a->soft ? (SDL_FColor){ 0.150f, 0.150f, 0.150f, 1 } : a->sharp.panel;
    s->raised = a->soft ? (SDL_FColor){ 0.215f, 0.215f, 0.215f, 1 } : a->sharp.raised;
    s->border = a->soft ? (SDL_FColor){ 0.27f, 0.27f, 0.27f, 1 } : a->sharp.border;
    s->text = a->soft ? (SDL_FColor){ 0.86f, 0.86f, 0.86f, 1 } : a->sharp.text;
    s->muted = a->soft ? (SDL_FColor){ 0.60f, 0.60f, 0.60f, 1 } : a->sharp.muted;
    a->heights_width = 0;
    if (!keep) return;
    char text[360];
    int n = snprintf(text, sizeof text, "square_pictures %d\nfont %d\nfont_size %d\nline_extra %d\nsoft %d\ncontrols %d\nhardware_video %d\nturns %d\nlist_width %d\nstart_at_login %d\nreport_use %d\ncompact %d\n", a->square_pictures, a->font, (int)a->font_size, (int)a->line_extra, a->soft, a->controls, a->hardware_video, a->turns, (int)a->list_w, a->start_at_login, a->report_use, a->compact);
    SDL_SaveFile(a->settings_path, text, n);
}

// One row per account: who it is, how its connection is, and what last went wrong. A client that has
// stopped receiving should say so here instead of looking idle.
// The settings, drawn from the right of a row of the status bar: the size of the text and, with
// "controls 1" in the settings file, the rest; the build and its notices. True when one changed.
static bool settings_row(app *a, gs_ui *ui, gs_rect *at) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    gs_rect row = *at;
    bool changed = false;
    // At the far right, the size of the text: a field to type it in, then buttons that take one
    // off and add one. The buttons are outermost so that they stay under the pointer as the text,
    // and with it everything else in this bar, grows or shrinks.
    float button = s->row - 8;  // (their height; they are a row wide, which a "+" needs at the smallest sizes)
    gs_rect more = gs_cut_right(&row, s->row), less = gs_cut_right(&row, s->row), size = gs_cut_right(&row, s->row * 1.4f);
    more.y += 4, more.h = button, less.y += 4, less.h = button, size.y += 4, size.h = button;
    double before = a->font_size;
    if (gs_ui_button(ui, more, "+")) a->font_size += 1;
    if (gs_ui_button(ui, less, "-")) a->font_size -= 1;
    gs_ui_number(ui, size, "font-size", &a->font_size, FONT_MIN, FONT_MAX, 1, 0);
    changed = a->font_size != before;
    gs_cut_right(&row, 16);
    // Another account: an IRC bouncer, by its address (see gsc_account_add).
    gs_rect add = gs_cut_right(&row, gs_ui_text_width(ui, "Add IRC", s->small_px));
    gs_ui_text(ui, add, "Add IRC", s->small_px, gs_ui_hovered(ui, add) || a->adding_irc ? s->accent : s->muted, 1);
    if (gs_ui_clicked(ui, add)) a->adding_irc = !a->adding_irc, gs_ui_focus(ui, "irc-address");
    gs_cut_right(&row, 16);
    // The rest of the choices and the figures for judging them are drawn only when the settings
    // file says "controls 1": the font, the colours, the room between lines, the font's height and
    // the count of characters in view. Without it they stay as the file, or the defaults, have them.
    if (a->controls) {
        // (which renderer draws, and so whether videos can be decoded by the graphics card)
        const char *drawn_by = SDL_GetRendererName(a->renderer);
        gs_ui_text(ui, gs_cut_right(&row, gs_ui_text_width(ui, drawn_by, s->small_px)), drawn_by, s->small_px, s->muted, 1);
        gs_cut_right(&row, 16);
        // Its font, which a click exchanges for the next of those there are.
        const char *font = font_names[a->font];
        gs_rect choice = gs_cut_right(&row, gs_ui_text_width(ui, font, s->small_px));
        gs_ui_text(ui, choice, font, s->small_px, gs_ui_hovered(ui, choice) ? s->accent : s->muted, 1);
        if (gs_ui_clicked(ui, choice)) a->font = (a->font + 1) % FONTS, changed = true;
        gs_cut_right(&row, 16);
        // How much the messages in view say, as a count of their characters, for judging a font and a
        // size by what they fit. (Square or round pictures has no link here for now; square is the default.)
        // With it, in pixels, how tall the font's letters stand from their lowest to their highest, and how
        // much room is left between that height on one line and on the next.
        // The room between lines is a field: what the font asks is 0, and a number adds that many points.
        char shown[96];
        float ascent = 0, descent = 0, pixels = s->text_px * gs_ui_scale(ui);
        gs_fontset_vmetrics(a->fonts[a->font], pixels, &ascent, &descent, NULL);
        snprintf(shown, sizeof shown, "%d chars shown", a->shown_chars);
        gs_ui_text(ui, gs_cut_right(&row, gs_ui_text_width(ui, shown, s->small_px)), shown, s->small_px, s->muted, 1);
        gs_cut_right(&row, 16);
        gs_rect extra = gs_cut_right(&row, s->row * 1.2f);
        extra.y += 4, extra.h = button;
        double spaced = a->line_extra;
        gs_ui_number(ui, extra, "line-extra", &a->line_extra, 0, LINE_EXTRA_MAX, 1, 0);
        changed |= a->line_extra != spaced;
        gs_cut_right(&row, 6);
        snprintf(shown, sizeof shown, "font %.1f px    line spacing", ascent - descent);
        gs_ui_text(ui, gs_cut_right(&row, gs_ui_text_width(ui, shown, s->small_px)), shown, s->small_px, s->muted, 1);
        gs_cut_right(&row, 16);
        // The colours: as the interface came, or softer.
        const char *colours = a->soft ? "Soft colours" : "Sharp colours";
        choice = gs_cut_right(&row, gs_ui_text_width(ui, colours, s->small_px));
        gs_ui_text(ui, choice, colours, s->small_px, gs_ui_hovered(ui, choice) ? s->accent : s->muted, 1);
        if (gs_ui_clicked(ui, choice)) a->soft = !a->soft, changed = true;
        gs_cut_right(&row, 16);
    }
    // Whether each message has a thin bar in its writer's colour down its left, beside the
    // coloured names (the other ways of marking turns are chosen in the settings file alone).
    if (a->turns == TURNS_NAMES || a->turns == TURNS_BARS) {
        const char *bars = a->turns == TURNS_BARS ? "Bars on" : "Bars off";
        gs_rect choice = gs_cut_right(&row, gs_ui_text_width(ui, bars, s->small_px));
        gs_ui_text(ui, choice, bars, s->small_px, gs_ui_hovered(ui, choice) ? s->accent : s->muted, 1);
        if (gs_ui_clicked(ui, choice)) a->turns = a->turns == TURNS_BARS ? TURNS_NAMES : TURNS_BARS, changed = true;
        gs_cut_right(&row, 16);
    }
    // Whether a person's messages in quick succession stand as lines under one name.
    {
        const char *compact = a->compact ? "Compact on" : "Compact off";
        gs_rect choice = gs_cut_right(&row, gs_ui_text_width(ui, compact, s->small_px));
        gs_ui_text(ui, choice, compact, s->small_px, gs_ui_hovered(ui, choice) ? s->accent : s->muted, 1);
        if (gs_ui_clicked(ui, choice)) a->compact = !a->compact, a->heights_width = 0, changed = true;
        gs_cut_right(&row, 16);
    }
    // A test build that updates itself: whether it does, and, once a new build is in place, a way
    // to start it now rather than when the user next leaves the program be.
    if (a->updating) {
        // (it says it is a test build, and which, as the title does)
        char said[80];
        snprintf(said, sizeof said, a->updated ? "Restart to update" : a->update_on ? "Test build %s, updates on" : "Test build %s, updates off", gsc_build());
        gs_rect choice = gs_cut_right(&row, gs_ui_text_width(ui, said, s->small_px));
        gs_ui_text(ui, choice, said, s->small_px, a->updated || gs_ui_hovered(ui, choice) ? s->accent : s->muted, 1);
        if (gs_ui_clicked(ui, choice)) {
            if (a->updated) a->restart_now = true, wake(NULL);
            else a->update_on = !a->update_on, set_update_consent(a->update_path, a->update_on), gsc_update_enable(a->update_on);
        }
        gs_cut_right(&row, 16);
    }
    // (what an update did, said once until clicked)
    if (a->notice[0]) {
        gs_rect choice = gs_cut_right(&row, gs_ui_text_width(ui, a->notice, s->small_px));
        gs_ui_text(ui, choice, a->notice, s->small_px, s->accent, 1);
        if (gs_ui_clicked(ui, choice)) a->notice[0] = 0;
        gs_cut_right(&row, 16);
    }
    *at = row;
    return changed;
}

static void health(app *a, gs_ui *ui, gs_rect *r) {
    static const char *const states[] = { "waiting to be linked", "connecting", "catching up", "online", "offline" };
    const gs_ui_style *s = gs_ui_style_of(ui);
    for (int i = 0; i < a->naccounts; i++) {
        const gsc_account *ac = &a->accounts[i];
        gs_rect row = gs_cut_top(r, s->row);
        gs_ui_fill(ui, row, s->panel);
        row = gs_inset(row, 8), row.y -= 8, row.h += 16;
        // The settings, the build and its notices are on the first account's row; in a narrow window,
        // a phone's, behind a button that shows them on a row of their own, so the account's state shows.
        bool changed = false;
        if (i == 0 && !a->narrow) changed = settings_row(a, ui, &row);
        else if (i == 0) {
            gs_rect more = gs_cut_right(&row, gs_ui_text_width(ui, "Settings", s->small_px));
            gs_ui_text(ui, more, "Settings", s->small_px, gs_ui_hovered(ui, more) || a->settings_open ? s->accent : s->muted, 1);
            if (gs_ui_clicked(ui, more)) a->settings_open = !a->settings_open;
            gs_cut_right(&row, 16);
        }
        // The way to a Signal account's notes to itself, which the list shows only once written.
        bool signal = !strcmp(ac->protocol, "signal");
        gs_rect note = signal ? gs_cut_right(&row, gs_ui_text_width(ui, "Note to Self", s->small_px)) : (gs_rect){ 0 };
        if (signal) gs_ui_text(ui, note, "Note to Self", s->small_px, gs_ui_hovered(ui, note) ? s->accent : s->muted, 1);
        if (signal && gs_ui_clicked(ui, note)) {
            int64_t id = gsc_note_to_self(a->core, ac->id);
            for (int c = 0; id && c < a->nconversations; c++)
                if (a->conversations[c].id == id) a->query[0] = 0, open_conversation(a, c, MESSAGES_FIRST), a->to_draft = true;
        }
        if (signal) gs_cut_right(&row, 16);
        if (changed) apply_settings(a, ui, true), wake(NULL);  // (the next frame is drawn with what was chosen)
        // (the account's state alone: its number, its last arrival and its count of undecryptable
        // messages are in the log and in the conversations themselves)
        char line[160];
        snprintf(line, sizeof line, "%s    %s", ac->protocol, states[ac->state]);
        float w = gs_ui_text(ui, row, line, s->small_px, ac->state == GSC_ACCOUNT_ONLINE ? s->text : s->muted, -1);
        gs_cut_left(&row, w + 16);
        if (ac->error[0]) gs_ui_text(ui, row, ac->error, s->small_px, s->warning, -1);
    }
    if (a->narrow && a->settings_open) {
        gs_rect row = gs_cut_top(r, s->row);
        gs_ui_fill(ui, row, s->panel);
        row = gs_inset(row, 8), row.y -= 8, row.h += 16;
        if (settings_row(a, ui, &row)) apply_settings(a, ui, true), wake(NULL);
    }
    // (the address holds the bouncer's password, so it is shown only while typed)
    if (a->adding_irc) {
        gs_rect row = gs_cut_top(r, s->row + 8);
        gs_ui_fill(ui, row, s->panel);
        row = gs_inset(row, 4);
        gs_rect add = gs_cut_right(&row, 72);
        gs_cut_right(&row, 8);
        gs_ui_input(ui, row, "irc-address", a->irc_address, sizeof a->irc_address, "ircs://USER:PASSWORD@HOST:PORT?sha256=FINGERPRINT", GS_UI_MASKED | GS_UI_REVEAL);
        if ((gs_ui_button(ui, add, "Add") || gs_ui_key(ui, SDLK_RETURN, 0)) && a->irc_address[0]) {
            // (without the spaces or line ending a paste from a file may bring)
            char *address = a->irc_address;
            while (SDL_isspace((unsigned char)*address)) address++;
            for (size_t n = strlen(address); n && SDL_isspace((unsigned char)address[n - 1]); n--) address[n - 1] = 0;
            gsc_account_add(a->core, "irc", address);
            SDL_memset(a->irc_address, 0, sizeof a->irc_address), a->adding_irc = false;
        }
        if (gs_ui_key(ui, SDLK_ESCAPE, 0) && !a->irc_address[0]) a->adding_irc = false;
    }
}

#define CODE_LIFE_S 90
// The code a waiting account wants scanned by its primary device, as dark squares on white with the
// clear margin a scanner needs, and the same link as text below it.
static void sign_in(app *a, gs_ui *ui, gs_rect r) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    gs_rect caption = gs_cut_top(&r, s->row * 2), link = gs_cut_bottom(&r, s->row * 2);
#ifdef __ANDROID__
    // (a phone cannot scan its own screen, and Signal on the same phone takes a link only through its
    // camera: given the link itself, it opens its scanner)
    gs_ui_text(ui, gs_cut_top(&caption, caption.h / 2), "Scan with Signal on your main phone", s->text_px, s->text, 0);
    gs_ui_text(ui, caption, "Same phone? Show the code on another screen", s->small_px, s->muted, 0);
#else
    gs_ui_text(ui, caption, "Scan with Signal on your phone: Settings, Linked devices, Link new device", s->text_px, s->text, 0);
#endif
    gs_ui_text(ui, gs_inset(link, 8), a->code_link, s->small_px, s->muted, 0);
    // How long the code is good for: Signal's server lets the link wait about 90 seconds, then gink is
    // given a new code. A scan after that reaches nothing.
    int left = CODE_LIFE_S - (int)((SDL_GetTicks() - a->code_since) / 1000);
    char fresh[80];
    if (left > 0) snprintf(fresh, sizeof fresh, "This code is good for about %d more seconds", left);
    else snprintf(fresh, sizeof fresh, "This code has run out; a new one is coming");
    gs_ui_text(ui, gs_cut_bottom(&r, s->row * 1.5f), fresh, s->text_px, left > 15 ? s->text : s->warning, 0);
    int n = a->code_size + 8;
    float side = SDL_floorf(SDL_min(r.w, r.h) / n) * n, m = side / n;
    gs_rect box = { r.x + (r.w - side) / 2, r.y + (r.h - side) / 2, side, side };
    gs_ui_fill(ui, box, (SDL_FColor){ 1, 1, 1, 1 });
    for (int y = 0; y < a->code_size; y++)
        for (int x = 0; x < a->code_size; x++)
            if (a->code[y * a->code_size + x]) gs_ui_fill(ui, (gs_rect){ box.x + (x + 4) * m, box.y + (y + 4) * m, m, m }, (SDL_FColor){ 0, 0, 0, 1 });
}

// Tells the open conversation the user stopped typing, if it was told they started.
static void stop_typing(app *a) {
    if (a->told_typing && a->open) gsc_typing(a->core, a->open, false);
    a->told_typing = 0;
}

// One line of text from what may be several.
static void one_line(char *out, size_t size, const char *format, const char *a, const char *b) {
    snprintf(out, size, format, a, b);
    for (char *p = out; *p; p++)
        if (*p == '\n' || *p == '\r' || *p == '\t') *p = ' ';
}

// Shows the messages of a conversation, by its place in `conversations`: at least the newest
// `wanted` of them, which counts as reading them.
static void open_conversation(app *a, int row, int wanted) {
    if (a->conversations[row].id == a->open && wanted <= a->nmessages) return;
    stop_typing(a);
    // What is being written stays with its conversation, its files too: it is kept in the
    // database, and what was kept for this conversation comes back.
    save_draft(a);
    a->npicked = 0, a->timer_open = a->members_open = false, a->removing = -1;
    a->open = a->conversations[row].id, list_conversations(a);
    a->to_draft = true;
    if (a->open_path[0]) {
        char id[24];
        SDL_SaveFile(a->open_path, id, (size_t)snprintf(id, sizeof id, "%lld\n", (long long)a->open));
    }
    load_draft(a);
    close_movies(a);
    a->typing_until = 0, a->replying = a->editing = 0, a->viewing = 0, a->nnamed = 0, a->wanted = wanted, a->oldest_held = false;
    read_messages(a);
    read_members(a);
    // Where the messages not yet read begin is marked, and brought into view, until another
    // conversation is opened. (Those received count; the core's own notices start out read.)
    a->unread_from = 0, a->show_unread = false;
    for (int i = a->nmessages - 1, left = a->conversations[row].unread; i >= 0 && left > 0; i--) {
        const gsc_message *m = &a->messages[i];
        if (m->outgoing || m->kind == GSC_UNDECRYPTABLE || m->kind == GSC_IDENTITY_CHANGED) continue;
        a->unread_from = m->id, a->show_unread = true, left--;
    }
    gsc_mark_read(a->core, a->open);
}

// Whether text holds the words somewhere, whatever their case in ASCII.
static bool holds(const char *text, const char *words) {
    size_t n = strlen(words);
    for (; *text; text++)
        if (!SDL_strncasecmp(text, words, n)) return true;
    return false;
}

// Colours for people, by the numbers the core gives them in the order they were first heard from,
// so that no two have the same one until there are more people than colours. The list is made by
// tools/person-colours.py: colours that read as text on the window's dark grey, each as far as can
// be from all those before it and from the account's own green.
static const Uint32 person_colours[] = {
    0xe87ce0, 0xfc8800, 0xc8d424, 0x84acfc, 0x0cecfc, 0xfca0a8, 0x4cf09c, 0xb4b068,
    0xdcb4fc, 0xfcc478, 0x70c850, 0xb0d89c, 0x00c4ec, 0x8cd0fc, 0xc898cc, 0xe8ac00,
    0xfc7cac, 0xdc9c78, 0x74d4c8, 0xc090fc, 0xfc98dc, 0xb0b800, 0x00d894, 0xfc8478,
    0xfcc800, 0x84c88c, 0xfcb8dc, 0xd0c468, 0x94dc70, 0x00eccc, 0xfca45c, 0xe090a4,
    0xb8a8f4, 0xe498fc, 0xf8b494, 0x5cbcc4, 0x7cbce8, 0x78e0a8, 0x00d8e8, 0xd8b070,
    0x00d8bc, 0x84e4e4, 0xe4a4cc, 0xe09c40, 0xc8dc74, 0xa8c864, 0xa4bcfc, 0xd8c004,
    0xe488c4, 0x4cb4fc, 0xfcacfc, 0x8cbc60, 0xfc9c80, 0xc8ac24, 0xdcd48c, 0xfcb83c,
    0x50d0fc, 0x98e4c0, 0xa8cc2c, 0x58c87c, 0xf48c58, 0xfc7ccc, 0x5cdc78, 0xb0e454,
};

static SDL_FColor numbered_colour(int number) {
    Uint32 c = person_colours[(unsigned)number % SDL_arraysize(person_colours)];
    return (SDL_FColor){ (c >> 16 & 255) / 255.0f, (c >> 8 & 255) / 255.0f, (c & 255) / 255.0f, 1 };
}

// A person's or a group's picture, square or cut to a disc as chosen, and where there is none, or it
// is still being read, the first letter of `title` on `colour`. `name` is what the image cache knows
// the picture by (NULL for none), and `bytes` with `of` fetch it from the core the first time.
static void picture_at(app *a, gs_ui *ui, gs_rect at, SDL_FColor behind, const char *name, const void *(*bytes)(gsc_core *, int64_t, size_t *), int64_t of, const char *title, SDL_FColor colour) {
    float radius = SDL_min(at.w, at.h) / 2, cx = at.x + at.w / 2, cy = at.y + at.h / 2, scale = gs_ui_scale(ui);
    SDL_Texture *picture = NULL;
    if (name) {
        Uint64 number = 1469598103934665603ull;
        for (const char *p = name; *p; p++) number = (number ^ (unsigned char)*p) * 1099511628211ull;
        bool given = false;
        for (int i = 0; i < a->npictures && !given; i++) given = a->pictures[i] == number;
        if (!given && a->npictures < MAX_PICTURES) {
            size_t len;
            const void *data = bytes(a->core, of, &len);
            if (data && gs_images_put(a->images, name, data, len)) a->pictures[a->npictures++] = number;
        }
        int side = (int)SDL_ceilf(radius * 2 * scale);
        picture = gs_images_get(a->images, name, side, side);
    }
    gs_rect square = { cx - radius, cy - radius, radius * 2, radius * 2 };
    if (!picture) {
        gs_ui_round(ui, square, a->square_pictures ? SDL_min(3, radius / 3) : radius, colour);
        // (one character of the name, which may be several bytes; a small letter is made a capital)
        char letter[8] = { 0 };
        int n = 1;
        while (((unsigned char)title[n] & 0xC0) == 0x80 && n < 6) n++;
        memcpy(letter, title, title[0] ? n : 0);
        if (n == 1) letter[0] = (char)SDL_toupper((unsigned char)letter[0]);
        gs_ui_text(ui, at, letter, radius * 0.95f, (SDL_FColor){ 1, 1, 1, 1 }, 0);
        return;
    }
    if (a->square_pictures) { gs_ui_texture(ui, square, picture, NULL); return; }
    // The picture is drawn as a fan of triangles about its middle, which cuts it to a disc, and a
    // thin ring of what lies behind it then smooths the fan's stepped edge.
    enum { SIDES = 48 };
    SDL_Vertex fan[SIDES + 2];
    int order[SIDES * 3];
    fan[0] = (SDL_Vertex){ { cx * scale, cy * scale }, { 1, 1, 1, 1 }, { 0.5f, 0.5f } };
    for (int i = 0; i <= SIDES; i++) {
        float turn = i * 2 * SDL_PI_F / SIDES, x = SDL_cosf(turn), y = SDL_sinf(turn);
        fan[i + 1] = (SDL_Vertex){ { (cx + x * radius) * scale, (cy + y * radius) * scale }, { 1, 1, 1, 1 }, { 0.5f + x / 2, 0.5f + y / 2 } };
    }
    for (int i = 0; i < SIDES; i++) order[i * 3] = 0, order[i * 3 + 1] = i + 1, order[i * 3 + 2] = i + 2;
    SDL_RenderGeometry(a->renderer, picture, fan, SIDES + 2, order, SIDES * 3);
    gs_ui_outline(ui, (gs_rect){ cx - radius - 0.5f, cy - radius - 0.5f, radius * 2 + 1, radius * 2 + 1 }, radius + 0.5f, 1, behind);
}

// Cuts a polygon to the side of a line where (p - at) . normal >= 0. Returns the new count.
static int cut_polygon(const SDL_FPoint *in, int n, SDL_FPoint at, SDL_FPoint normal, SDL_FPoint *out) {
    int m = 0;
    for (int i = 0; i < n; i++) {
        SDL_FPoint p = in[i], q = in[(i + 1) % n];
        float dp = (p.x - at.x) * normal.x + (p.y - at.y) * normal.y, dq = (q.x - at.x) * normal.x + (q.y - at.y) * normal.y;
        if (dp >= 0) out[m++] = p;
        if ((dp >= 0) != (dq >= 0)) out[m++] = (SDL_FPoint){ p.x + (q.x - p.x) * dp / (dp - dq), p.y + (q.y - p.y) * dp / (dp - dq) };
    }
    return m;
}

#define FACE_TILT 0.3f  // how far the cuts between faces lean from upright and level, in radians
#define FACE_PICTURE_WEIGHT 1.8f  // a face with a picture's share of the disc, against 1 for a letter

// Where a straight cut must run, as a distance from the middle in radii (towards the second side),
// for the first side to hold `share` of a disc's area, or of a square's.
static float cut_for_share(float share, bool square) {
    if (square) return 2 * share - 1;
    float lo = -1, hi = 1;
    for (int i = 0; i < 30; i++) {
        float d = (lo + hi) / 2, beyond = SDL_acosf(SDL_fabsf(d)) - SDL_fabsf(d) * SDL_sqrtf(1 - d * d);  // (the smaller segment's area, over r^2)
        float first = d >= 0 ? SDL_PI_F - beyond : beyond;
        if (first / SDL_PI_F < share) lo = d;
        else hi = d;
    }
    return (lo + hi) / 2;
}

// A group's picture made of the faces of those who write there, for a group with none of its own:
// two side by side, or one beside two, each in a slice of the disc cut on a slant. A face is the
// person's picture, or the first letter of their name on their colour, so one without a picture
// still has a slice of their own, and a group can never be taken for one person's conversation.
static void group_faces(app *a, gs_ui *ui, int64_t conversation, int n, const char (*keys)[80], const char (*names)[64], const int *people, const int64_t *ms, gs_rect at, SDL_FColor behind) {
    float radius = SDL_min(at.w, at.h) / 2, cx = at.x + at.w / 2, cy = at.y + at.h / 2, scale = gs_ui_scale(ui);
    enum { SIDES = 64 };
    SDL_FPoint whole[SIDES], half[SIDES + 4], part[SIDES + 8];
    int nwhole = 0;
    if (a->square_pictures) {
        float r = radius;
        whole[0] = (SDL_FPoint){ cx - r, cy - r }, whole[1] = (SDL_FPoint){ cx + r, cy - r }, whole[2] = (SDL_FPoint){ cx + r, cy + r }, whole[3] = (SDL_FPoint){ cx - r, cy + r }, nwhole = 4;
    } else {
        for (int i = 0; i < SIDES; i++) whole[nwhole++] = (SDL_FPoint){ cx + SDL_cosf(i * 2 * SDL_PI_F / SIDES) * radius, cy + SDL_sinf(i * 2 * SDL_PI_F / SIDES) * radius };
    }
    // The first cut runs through the middle, leaning; the second, for a third face, crosses the
    // right-hand slice a little below level.
    // A face with a picture is given more of the disc than one shown by a letter: the first cut is
    // moved off the middle until the first face's share of the area is its share of the weights,
    // and the second divides what is right of it the same way.
    float weight[FACES], total = 0;
    for (int k = 0; k < n; k++) total += weight[k] = ms[k] ? FACE_PICTURE_WEIGHT : 1;
    SDL_FPoint up = { SDL_sinf(FACE_TILT), -SDL_cosf(FACE_TILT) }, across = { up.y * -1, up.x };
    SDL_FPoint along = { SDL_cosf(FACE_TILT), SDL_sinf(FACE_TILT) }, down = { -along.y, along.x };
    float d1 = n >= 2 ? cut_for_share(weight[0] / total, a->square_pictures) * radius : 0;
    SDL_FPoint middle = { cx + across.x * d1, cy + across.y * d1 };
    float x2 = (d1 + radius) / 2, h2 = 2 * SDL_sqrtf(SDL_max(0, radius * radius - x2 * x2));
    float d2 = n >= 3 ? (2 * weight[1] / (weight[1] + weight[2]) - 1) * h2 / 2 : 0;
    SDL_FPoint right = { cx + across.x * x2 + down.x * d2, cy + across.y * x2 + down.y * d2 };
    for (int k = 0; k < n; k++) {
        SDL_FPoint *slice = whole;
        int count = nwhole;
        if (n >= 2) {
            SDL_FPoint side = k == 0 ? (SDL_FPoint){ -across.x, -across.y } : across;
            count = cut_polygon(whole, nwhole, middle, side, half), slice = half;
        }
        if (n >= 3 && k > 0) count = cut_polygon(half, count, right, k == 1 ? (SDL_FPoint){ -down.x, -down.y } : down, part), slice = part;
        if (count < 3) continue;
        float lx = slice[0].x, hx = lx, ly = slice[0].y, hy = ly, mx = 0, my = 0;
        for (int i = 0; i < count; i++) {
            lx = SDL_min(lx, slice[i].x), hx = SDL_max(hx, slice[i].x), ly = SDL_min(ly, slice[i].y), hy = SDL_max(hy, slice[i].y);
            mx += slice[i].x / count, my += slice[i].y / count;
        }
        // The picture covers the slice's bounds, centred on them; a letter sits at its middle.
        SDL_Texture *picture = NULL;
        float side = SDL_max(hx - lx, hy - ly), sx = (lx + hx - side) / 2, sy = (ly + hy - side) / 2;
        if (ms[k]) {
            char name[140];
            snprintf(name, sizeof name, "face-%s-%lld", keys[k], (long long)ms[k]);
            Uint64 number = 1469598103934665603ull;
            for (const char *p = name; *p; p++) number = (number ^ (unsigned char)*p) * 1099511628211ull;
            bool given = false;
            for (int i = 0; i < a->npictures && !given; i++) given = a->pictures[i] == number;
            if (!given && a->npictures < MAX_PICTURES) {
                size_t len;
                const void *data = gsc_face_picture(a->core, conversation, keys[k], &len);
                if (data && gs_images_put(a->images, name, data, len)) a->pictures[a->npictures++] = number;
            }
            int px = (int)SDL_ceilf(side * scale);
            picture = gs_images_get(a->images, name, px, px);
        }
        SDL_FColor colour = people[k] >= 0 ? numbered_colour(people[k]) : gs_ui_style_of(ui)->muted;
        colour.r *= 0.72f, colour.g *= 0.72f, colour.b *= 0.72f;
        SDL_Vertex fan[SIDES + 9];
        int order[(SIDES + 8) * 3];
        SDL_FColor tint = picture ? (SDL_FColor){ 1, 1, 1, 1 } : colour;
        fan[0] = (SDL_Vertex){ { mx * scale, my * scale }, tint, { (mx - sx) / side, (my - sy) / side } };
        for (int i = 0; i < count; i++) fan[i + 1] = (SDL_Vertex){ { slice[i].x * scale, slice[i].y * scale }, tint, { (slice[i].x - sx) / side, (slice[i].y - sy) / side } };
        for (int i = 0; i < count; i++) order[i * 3] = 0, order[i * 3 + 1] = i + 1, order[i * 3 + 2] = (i + 1) % count + 1;
        SDL_RenderGeometry(a->renderer, picture, fan, count + 1, order, count * 3);
        if (!picture) {
            char letter[8] = { 0 };
            int bytes = 1;
            while (((unsigned char)names[k][bytes] & 0xC0) == 0x80 && bytes < 6) bytes++;
            memcpy(letter, names[k], bytes);
            if (bytes == 1) letter[0] = (char)SDL_toupper((unsigned char)letter[0]);
            // (at the slice's centre of area, which a narrow slice's bounds would put too near the cut)
            float size = radius * (n >= 3 && k > 0 ? 0.5f : n >= 2 ? 0.7f : 0.95f), line = gs_ui_line_height(ui, size);
            float area = 0, x = 0, y = 0;
            for (int i = 0; i < count; i++) {
                SDL_FPoint p = slice[i], q = slice[(i + 1) % count];
                float cross = p.x * q.y - q.x * p.y;
                area += cross, x += (p.x + q.x) * cross, y += (p.y + q.y) * cross;
            }
            if (SDL_fabsf(area) > 1e-3f) x /= 3 * area, y /= 3 * area;
            else x = (lx + hx) / 2, y = (ly + hy) / 2;
            gs_ui_text(ui, (gs_rect){ x - size, y - line / 2, size * 2, line }, letter, size, (SDL_FColor){ 1, 1, 1, 1 }, 0);
        }
    }
    // The cuts are drawn in the colour behind, as a narrow gap; past the disc's edge they are lost on it.
    float gap = 1.5f;
    for (int c = 0; c < (n >= 3 ? 2 : n >= 2 ? 1 : 0); c++) {
        SDL_FPoint from, to, dir;
        if (c == 0) dir = up, from = (SDL_FPoint){ middle.x - up.x * radius * 1.2f, middle.y - up.y * radius * 1.2f }, to = (SDL_FPoint){ middle.x + up.x * radius * 1.2f, middle.y + up.y * radius * 1.2f };
        else {
            // (from where it meets the first cut, rightwards)
            float t = ((middle.x - right.x) * across.x + (middle.y - right.y) * across.y) / (along.x * across.x + along.y * across.y);
            dir = along, from = (SDL_FPoint){ right.x + along.x * t, right.y + along.y * t }, to = (SDL_FPoint){ right.x + along.x * radius * 1.2f, right.y + along.y * radius * 1.2f };
        }
        SDL_FPoint off = { -dir.y * gap / 2, dir.x * gap / 2 };
        SDL_Vertex q[4] = {
            { { (from.x + off.x) * scale, (from.y + off.y) * scale }, behind, { 0, 0 } }, { { (to.x + off.x) * scale, (to.y + off.y) * scale }, behind, { 0, 0 } },
            { { (to.x - off.x) * scale, (to.y - off.y) * scale }, behind, { 0, 0 } }, { { (from.x - off.x) * scale, (from.y - off.y) * scale }, behind, { 0, 0 } },
        };
        int quad[6] = { 0, 1, 2, 0, 2, 3 };
        SDL_RenderGeometry(a->renderer, NULL, q, 4, quad, 6);
    }
    if (!a->square_pictures) gs_ui_outline(ui, (gs_rect){ cx - radius - 0.5f, cy - radius - 0.5f, radius * 2 + 1, radius * 2 + 1 }, radius + 0.5f, 1, behind);
}

// A conversation's picture, and where it has none a letter on a colour: for a conversation with one
// person, the colour their name has over their messages; for a group, the colour of its own number
// in the same count; and for the account's notes to itself, the accent. The colours are darkened a
// little, to carry a white letter.
static void avatar(app *a, gs_ui *ui, const gsc_conversation *c, gs_rect at, SDL_FColor behind) {
    // (a group without a picture of its own is pictured by the faces of those who write in it)
    ptrdiff_t i = c - a->conversations;
    if (i >= 0 && i < a->nconversations && c->kind == GSC_GROUP && !c->avatar_ms && a->faces[i].n) {
        char keys[FACES][80], names[FACES][64];
        int people[FACES];
        int64_t ms[FACES];
        int n = a->faces[i].n;
        // The large slice goes to whoever writes most among those with a picture, or among all when
        // none has one; the others follow, those with a picture first, then in a fixed order by key,
        // so that the picture does not shuffle as they talk.
        int order[FACES], left = 0;
        for (int k = n - 1; k >= 0; k--)
            if (a->faces[i].face[k].picture_ms) left = k;
        order[0] = left;
        for (int k = 0, m = 1; k < n; k++)
            if (k != left) order[m++] = k;
        if (n == 3) {
            int64_t one = a->faces[i].face[order[1]].picture_ms, two = a->faces[i].face[order[2]].picture_ms;
            bool swap = !one != !two ? !one : strcmp(a->faces[i].face[order[1]].key, a->faces[i].face[order[2]].key) > 0;
            if (swap) order[1] ^= order[2], order[2] ^= order[1], order[1] ^= order[2];
        }
        for (int k = 0; k < n; k++) {
            int from = order[k];
            memcpy(keys[k], a->faces[i].face[from].key, 80), memcpy(names[k], a->faces[i].face[from].name, 64);
            people[k] = a->faces[i].face[from].person, ms[k] = a->faces[i].face[from].picture_ms;
        }
        group_faces(a, ui, c->id, n, keys, names, people, ms, at, behind);
        return;
    }
    SDL_FColor colour = c->person >= 0 ? numbered_colour(c->person) : c->kind == GSC_GROUP ? gs_ui_style_of(ui)->muted : gs_ui_style_of(ui)->accent;
    colour.r *= 0.72f, colour.g *= 0.72f, colour.b *= 0.72f;
    char name[64];
    snprintf(name, sizeof name, "avatar-%lld-%lld", (long long)c->id, (long long)c->avatar_ms);
    picture_at(a, ui, at, behind, c->avatar_ms ? name : NULL, gsc_avatar, c->id, c->title, colour);
}

// What a search found, in place of the conversations: first the conversations and people whose
// names hold the words, then each message that does, under its conversation's name. Choosing a
// name opens that conversation, and choosing a message opens its conversation far enough back to
// hold it.
static void hit_list(app *a, gs_ui *ui, gs_rect r) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    float row_h = s->row * 1.6f;
    a->nnamed_like = 0;
    for (int i = 0; i < a->nconversations && a->nnamed_like < MAX_NAMED_LIKE; i++)
        if (holds(a->conversations[i].title, a->query)) a->named_like[a->nnamed_like++] = i;
    int names = a->nnamed_like;
    if (!a->nhits && !names) gs_ui_text(ui, gs_cut_top(&r, s->row), "Nothing found", s->small_px, s->muted, 0);
    gs_ui_rows rows = gs_ui_list(ui, r, "hits", names + a->nhits, row_h, &a->hit);
    gs_ui_clip(ui, &r);
    for (int i = rows.first; i < rows.end; i++) {
        gs_rect row = { r.x, rows.y + i * row_h, r.w, row_h };
        if (i < names) {
            const gsc_conversation *c = &a->conversations[a->named_like[i]];
            if (c->id == a->open) gs_ui_fill(ui, row, s->raised);
            row = gs_inset(row, 8);
            avatar(a, ui, c, gs_cut_left(&row, row.h), c->id == a->open ? s->raised : s->background);
            gs_cut_left(&row, 8), row.y -= 4, row.h += 8;
            gs_ui_text(ui, gs_cut_top(&row, row.h / 2), c->title, s->text_px, s->text, -1);
            gs_ui_text(ui, row, c->kind == GSC_GROUP ? "group" : "person", s->small_px, s->muted, -1);
            continue;
        }
        const gsc_hit *h = &a->hits[i - names];
        if (h->message == a->found) gs_ui_fill(ui, row, s->raised);
        row = gs_inset(row, 8), row.y -= 4, row.h += 8;
        gs_rect top = gs_cut_top(&row, row.h / 2);
        char when[16], line[256];
        clock_text(h->sent_ms, when, sizeof when);
        gs_ui_text(ui, gs_cut_right(&top, gs_ui_text_width(ui, when, s->small_px)), when, s->small_px, s->muted, 1);
        gs_cut_right(&top, 6);
        gs_ui_text(ui, top, h->title, s->text_px, s->text, -1);
        one_line(line, sizeof line, "%s%s", h->outgoing ? "me: " : "", h->text);
        gs_ui_text(ui, row, line, s->small_px, s->muted, -1);
    }
    gs_ui_clip(ui, NULL);
    gs_ui_list_end(ui);
    if (rows.activated && a->hit >= 0 && a->hit < names) open_conversation(a, a->named_like[a->hit], MESSAGES_FIRST);
    else if (rows.activated && a->hit >= names && a->hit < names + a->nhits)
        for (int i = 0; i < a->nconversations; i++)
            if (a->conversations[i].id == a->hits[a->hit - names].conversation) {
                a->found = a->hits[a->hit - names].message, a->show_found = true;
                open_conversation(a, i, SDL_clamp(a->hits[a->hit - names].newer + 30, MESSAGES_FIRST, MAX_MESSAGES));
            }
}

// The list of conversations is as wide as it was last dragged to: its pictures alone, or anything
// from LIST_MIN up, with LIST_W where it starts.
#define LIST_ICONS 0
#define LIST_MIN 95
#define LIST_W 260
// Narrower than this, the window shows the list or one conversation, filling it, with a way back.
#define NARROW 560

// The list drawn as pictures alone, each with its count of unread messages on its corner.
static void conversation_icons(app *a, gs_ui *ui, gs_rect r) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    float row_h = r.w;
    int before = a->selected;
    gs_ui_rows rows = gs_ui_list(ui, r, "conversations", a->nlisted, row_h, &a->selected);
    gs_ui_clip(ui, &r);
    for (int i = rows.first; i < rows.end; i++) {
        const gsc_conversation *c = &a->conversations[a->listed[i]];
        gs_rect row = { r.x, rows.y + i * row_h, r.w, row_h };
        if (c->id == a->open) gs_ui_fill(ui, row, s->raised);
        avatar(a, ui, c, gs_inset(row, 8), c->id == a->open ? s->raised : s->background);
        if (!c->unread) continue;
        char count[16];
        snprintf(count, sizeof count, c->unread > 99 ? "99+" : "%d", c->unread);
        float w = gs_ui_text_width(ui, count, s->small_px) + 8, h = gs_ui_line_height(ui, s->small_px);
        gs_rect badge = { row.x + row.w - w - 2, row.y + 2, w, h };
        gs_ui_fill(ui, badge, c->muted ? s->muted : s->accent);
        gs_ui_text(ui, badge, count, s->small_px, s->accent_text, 0);
    }
    gs_ui_clip(ui, NULL);
    gs_ui_list_end(ui);
    if ((rows.activated || a->selected != before || (!a->open && !a->narrow)) && a->selected >= 0 && a->selected < a->nlisted) open_conversation(a, a->listed[a->selected], MESSAGES_FIRST);
}

static void conversation_list(app *a, gs_ui *ui, gs_rect r) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    float row_h = s->row * 1.6f;
    if (a->list_w == LIST_ICONS) { conversation_icons(a, ui, r); return; }
    // A search over every conversation's messages, made again as its words change; Escape ends it.
    gs_rect field = gs_inset(gs_cut_top(&r, s->row + 16), 8);
    if (a->to_search) gs_ui_focus(ui, "search"), gs_ui_textarea_select(ui, "search", 0, (int)strlen(a->query)), a->to_search = false;
    // (with a button beside it that ends the search, while there is one)
    gs_rect clear = { 0 };
    if (a->query[0]) clear = gs_cut_right(&field, field.h), gs_cut_right(&field, 4);
    bool changed = gs_ui_field(ui, field, "search", a->query, sizeof a->query, "Search");
    if (a->query[0] && (gs_ui_key(ui, SDLK_ESCAPE, 0) || gs_ui_button(ui, clear, "\xC3\x97"))) a->query[0] = 0, changed = true, a->to_draft = a->open != 0;
    if (changed) a->nhits = gsc_search(a->core, a->query, a->hits, MAX_HITS), a->hit = -1;
    if (a->query[0]) { hit_list(a, ui, r); return; }
    int before = a->selected;
    gs_ui_rows rows = gs_ui_list(ui, r, "conversations", a->nlisted, row_h, &a->selected);
    gs_ui_clip(ui, &r);
    for (int i = rows.first; i < rows.end; i++) {
        const gsc_conversation *c = &a->conversations[a->listed[i]];
        gs_rect row = { r.x, rows.y + i * row_h, r.w, row_h };
        if (c->id == a->open) gs_ui_fill(ui, row, s->raised);
        // (the picture takes the row's height as it is, and the name and preview the room beside it)
        row = gs_inset(row, 8);
        avatar(a, ui, c, gs_cut_left(&row, row.h), c->id == a->open ? s->raised : s->background);
        gs_cut_left(&row, 8), row.y -= 4, row.h += 8;
        gs_rect top = gs_cut_top(&row, row.h / 2);
        if (c->unread) {
            char count[16];
            snprintf(count, sizeof count, "%d", c->unread);
            gs_rect badge = gs_cut_right(&top, gs_ui_text_width(ui, count, s->small_px) + 12);
            gs_ui_fill(ui, badge, s->accent);
            gs_ui_text(ui, badge, count, s->small_px, s->accent_text, 0);
            gs_cut_right(&top, 6);
        }
        if (c->muted) gs_cut_right(&top, 6), gs_ui_text(ui, gs_cut_right(&top, gs_ui_text_width(ui, "muted", s->small_px)), "muted", s->small_px, s->muted, 1);
        if (c->request) gs_cut_right(&top, 6), gs_ui_text(ui, gs_cut_right(&top, gs_ui_text_width(ui, "request", s->small_px)), "request", s->small_px, s->warning, 1);
        gs_ui_text(ui, top, c->title, s->text_px, c->unread ? s->accent : s->text, -1);
        char preview[256];  // (the newest message's beginning, on one line)
        snprintf(preview, sizeof preview, c->last_sender[0] ? "%s: %s" : "%s%s", c->last_sender[0] ? c->last_sender : "", c->last_text);
        for (char *p = preview; *p; p++)
            if (*p == '\n' || *p == '\r' || *p == '\t') *p = ' ';
        gs_ui_text(ui, row, preview, s->small_px, s->muted, -1);
    }
    gs_ui_clip(ui, NULL);
    gs_ui_list_end(ui);
    if ((rows.activated || a->selected != before || (!a->open && !a->narrow)) && a->selected >= 0 && a->selected < a->nlisted) open_conversation(a, a->listed[a->selected], MESSAGES_FIRST);
}

// A message's row: a line saying who and when, the message it replies to if any, its text wrapped to
// the width, and the reactions it was given; a notice of the core's own is its text alone.
#define ROW_GAP 10
#define STACK_GAP 2          // between a message and the next, when that one stands under it as a line
#define STACK_WITHIN (5 * 60 * 1000)  // how soon after the same person's last a message joins it, in ms

static int day_of(int64_t ms);

// Whether, with compact turns, message i stands as a line under the one before: both text from the
// same person, the same day, less than STACK_WITHIN apart, with no marker of unread messages between
// and no reactions under the first. Whether it is sent, edited or changing does not count: those
// change after it is drawn, and a line that moved as they did would jump. (Others' rules, in research/message-grouping.md, agree
// on the sender and the day and mostly on five minutes.)
static bool stacked(const app *a, int i) {
    if (!a->compact || a->turns == TURNS_COLUMNS || i <= 0 || i >= a->nmessages) return false;
    const gsc_message *m = &a->messages[i], *before = &a->messages[i - 1];
    if (m->kind != GSC_TEXT || before->kind != GSC_TEXT || m->outgoing != before->outgoing || strcmp(m->sender, before->sender)) return false;
    if (m->sent_ms < before->sent_ms || m->sent_ms - before->sent_ms >= STACK_WITHIN || m->id == a->unread_from || before->reactions[0]) return false;
    return day_of(m->sent_ms) == day_of(before->sent_ms);
}

// The room under message i: less when the next stands under it as a line.
static float gap_below(const app *a, int i) { return stacked(a, i + 1) ? STACK_GAP : ROW_GAP; }
#define RESET_W 130
#define MENU_W 28
// A message's text as it is shown: with what its sender hid covered, until the user asks to see it.
static const char *body_of(const app *a, const gsc_message *m) { return m->hidden[0] && a->revealed == m->id ? m->hidden : m->text; }

// What is selected in a message's text, as a pair of byte offsets; 0, 0 when nothing is.
static void selection_of(const app *a, int64_t id, int *from, int *to) {
    *from = *to = 0;
    for (int i = 0; i < a->npicked; i++)
        if (a->picked[i].id == id) *from = a->picked[i].from, *to = a->picked[i].to;
}

static void select_in(app *a, int64_t id, int from, int to) {
    int i = 0;
    while (i < a->npicked && a->picked[i].id != id) i++;
    if (from == to) {
        if (i < a->npicked) a->picked[i] = a->picked[--a->npicked];
    } else if (i < a->npicked || a->npicked < MAX_MESSAGES) {
        if (i == a->npicked) a->npicked++;
        a->picked[i].id = id, a->picked[i].from = from, a->picked[i].to = to;
    }
}

// Who a message is from, as its row names them: in a conversation with one person, a sender
// without a name is that person.
static const char *sender_of(const app *a, const gsc_message *m) {
    if (m->outgoing) return "me";
    if (m->sender[0]) return m->sender;
    for (int i = 0; i < a->nconversations; i++)
        if (a->conversations[i].id == a->open && a->conversations[i].kind == GSC_DIRECT) return a->conversations[i].title;
    return "Unknown";
}

static const char *shown_text(const gsc_message *m) { return m->kind == GSC_DELETED ? "This message was deleted." : m->text; }

// Whether a file is a GIF, which plays in place where pictures go.
static bool gif(const held_file *f) { return !strcmp(f->type, "image/gif"); }

// Whether a file is shown as a picture: a kind the image cache or the GIF player decodes, here, with a known size.
static bool pictured(const held_file *f) {
    return f->state == GSC_ATTACHMENT_HERE && f->w > 0 && f->h > 0 && (!strcmp(f->type, "image/png") || !strcmp(f->type, "image/jpeg") || gif(f));
}

// Whether a file is shown as a video to play in its place: any video that is here.
static bool filmed(const held_file *f) { return f->state == GSC_ATTACHMENT_HERE && !strncmp(f->type, "video/", 6); }

// The size a picture or a video is shown at: its own, shrunk to fit the width and the longest side
// allowed. A video whose sender gave no size gets a wide frame, which it is fitted into.
static void picture_size(const held_file *f, float width, float *w, float *h) {
    float fw = f->w > 0 ? f->w : 320, fh = f->h > 0 ? f->h : 180;
    float scale = SDL_min(1, SDL_min(SDL_min(width, IMAGE_MAX) / fw, IMAGE_MAX / fh));
    *w = fw * scale, *h = fh * scale;
}

#define PICTURE_GAP 4.0f   // between two pictures side by side, and under a row of them

// The sizes two pictures side by side are shown at: one height for both, so that together they
// fill the width, but never taller than the tallest a picture is shown or than the smaller one's
// own height. With no second picture, the first alone, as picture_size has it.
static void picture_pair(const held_file *one, const held_file *two, float width, float *w1, float *w2, float *h) {
    if (!two) {
        picture_size(one, width, w1, h), *w2 = 0;
        return;
    }
    float r1 = (float)one->w / one->h, r2 = (float)two->w / two->h;
    *h = SDL_min(SDL_min((SDL_min(width, IMAGE_MAX * 2) - PICTURE_GAP) / (r1 + r2), IMAGE_MAX), (float)SDL_min(one->h, two->h));
    *w1 = *h * r1, *w2 = *h * r2;
}

// The next picture of a message's files after index i, or -1.
static int next_picture(const held_file *f, int n, int i) {
    while (++i < n && !pictured(&f[i])) {}
    return i < n ? i : -1;
}

// The height of a message's files: its pictures two to a row, and then each other file, a video
// with its frame, on a line naming it.
static float files_height(const app *a, gs_ui *ui, const gsc_message *m, float width) {
    int n;
    const held_file *f = files_of(a, m, &n);
    float total = 0, w, w2, h;
    for (int i = next_picture(f, n, -1); i >= 0; i = next_picture(f, n, i)) {
        int j = next_picture(f, n, i);
        picture_pair(&f[i], j >= 0 ? &f[j] : NULL, width, &w, &w2, &h), total += h + PICTURE_GAP;
        if (j >= 0) i = j;
    }
    for (int i = 0; i < n; i++) {
        if (pictured(&f[i])) continue;
        // (a video is its frame alone; its name and buttons are where it is opened)
        if (filmed(&f[i])) { picture_size(&f[i], width, &w, &h), total += h + 4; continue; }
        total += gs_ui_style_of(ui)->row;
    }
    return total;
}

#define BAR_W 1
#define BAR_GAP 8

// The colour of a message's writer: their own, or for the account's messages the accent.
static SDL_FColor person_colour(gs_ui *ui, const gsc_message *m) {
    return m->outgoing ? gs_ui_style_of(ui)->accent : m->person < 0 ? gs_ui_style_of(ui)->muted : numbered_colour(m->person);
}

// The width of the columns that hold the time and the name when messages are set out as a chat log.
static float columns_width(gs_ui *ui, float width) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    return gs_ui_text_width(ui, "00:00", s->small_px) + 10 + SDL_min(s->text_px * 8, width * 0.2f) + 12;
}

// The day a time falls on by the user's clock, as one number, and that day written out: "Today",
// "Yesterday", or the weekday and date, with the year when it is not this one.
static int day_of(int64_t ms) {
    SDL_DateTime t;
    return SDL_TimeToDateTime(SDL_MS_TO_NS(ms), &t, true) ? t.year * 10000 + t.month * 100 + t.day : 0;
}

static void day_text(int64_t ms, char *out, size_t size) {
    static const char *const days[] = { "Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday" };
    static const char *const months[] = { "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November", "December" };
    SDL_Time now;
    SDL_DateTime t, today;
    out[0] = 0;
    if (!SDL_GetCurrentTime(&now) || !SDL_TimeToDateTime(now, &today, true) || !SDL_TimeToDateTime(SDL_MS_TO_NS(ms), &t, true)) return;
    int ago = day_of(SDL_NS_TO_MS(now)) == day_of(ms) ? 0 : day_of(SDL_NS_TO_MS(now) - 86400000) == day_of(ms) ? 1 : 2;
    if (ago < 2) snprintf(out, size, ago ? "Yesterday" : "Today");
    else if (t.year == today.year) snprintf(out, size, "%s %d %s", days[t.day_of_week % 7], t.day, months[(t.month + 11) % 12]);
    else snprintf(out, size, "%s %d %s %d", days[t.day_of_week % 7], t.day, months[(t.month + 11) % 12], t.year);
}

// The room above a message for the day it was sent on, where that is not the day of the one before.
static float day_gap(const app *a, gs_ui *ui, int i) {
    bool first = i == 0 || day_of(a->messages[i].sent_ms) != day_of(a->messages[i - 1].sent_ms);
    return first ? gs_ui_line_height(ui, gs_ui_style_of(ui)->small_px) + 14 : 0;
}

// The room above the first message that was unread when the conversation was opened.
static float unread_gap(const app *a, gs_ui *ui, int i) {
    return a->unread_from && a->messages[i].id == a->unread_from ? gs_ui_line_height(ui, gs_ui_style_of(ui)->small_px) + 14 : 0;
}

// The width a message's text and files have, which the columns or a bar at its left take from.
static float text_width(const app *a, gs_ui *ui, float width) {
    // (in a chat log the menu's dots stand at the end of the text's first line, and the text keeps clear of them)
    return width - (a->turns == TURNS_COLUMNS ? columns_width(ui, width) + MENU_W : a->turns == TURNS_BARS ? BAR_W + BAR_GAP : 0);
}

// The height of the line of reactions under a message.
static float reactions_height(gs_ui *ui, const gsc_message *m, float width) {
    return m->reactions[0] ? gs_ui_line_height(ui, gs_ui_style_of(ui)->small_px) : 0;
}

#define MAX_SPANS 160      // the stretches of one style a message's text is drawn in

// The fonts a message's styled text is drawn in, by the indices text_spans gives: the chosen font,
// its bold, italic and bold italic, and a monospaced one for code. Returns how many.
enum { SPAN_PLAIN, SPAN_BOLD, SPAN_ITALIC, SPAN_BOTH, SPAN_CODE, SPAN_FONTS };
static int span_fonts(const app *a, gs_fontset **out) {
    bool mono = a->font == FONT_MONO;
    out[SPAN_PLAIN] = a->fonts[a->font], out[SPAN_CODE] = a->fonts[FONT_MONO];
    for (int k = 0; k < 3; k++) out[SPAN_BOLD + k] = a->styled[mono][k];
    return SPAN_FONTS;
}

// The stretches a message's text is drawn in: where its sender's styles and its links begin and end,
// each with its font, colour and background. link_of gives the link of each span, or -1. Returns how many.
static int text_spans(const app *a, gs_ui *ui, const gsc_message *m, const char *body, const gs_ui_text_range *links, int nlinks, gs_ui_text_span *out, int *link_of) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    struct { int style, start, end; } styled[64];
    int nstyled = 0, len = (int)strlen(body);
    // (the styles count characters, which are turned into bytes of the text shown)
    for (const char *line = m->styles; *line && nstyled < 64; ) {
        int style, from, to;
        if (sscanf(line, "%d %d %d", &style, &from, &to) == 3) {
            int byte = 0, at = 0, start = -1, end = len;
            for (; byte <= len; byte++) {
                if (byte < len && ((unsigned char)body[byte] & 0xC0) == 0x80) continue;
                if (at == from) start = byte;
                if (at == to) { end = byte; break; }
                at++;
            }
            if (start >= 0 && start < end) styled[nstyled].style = style, styled[nstyled].start = start, styled[nstyled].end = end, nstyled++;
        }
        const char *next = strchr(line, '\n');
        line = next ? next + 1 : line + strlen(line);
    }
    int edge[2 + 2 * 64 + 2 * 8], nedge = 0;
    edge[nedge++] = 0, edge[nedge++] = len;
    for (int i = 0; i < nstyled; i++) edge[nedge++] = styled[i].start, edge[nedge++] = styled[i].end;
    for (int i = 0; i < nlinks; i++) edge[nedge++] = links[i].start, edge[nedge++] = links[i].end;
    for (int i = 1; i < nedge; i++)
        for (int k = i; k > 0 && edge[k - 1] > edge[k]; k--) { int t = edge[k]; edge[k] = edge[k - 1], edge[k - 1] = t; }
    int n = 0;
    for (int i = 0; i + 1 < nedge && n < MAX_SPANS; i++) {
        int from = edge[i], to = edge[i + 1];
        if (from >= to) continue;
        bool bold = false, italic = false, struck = false, code = false;
        for (int k = 0; k < nstyled; k++)
            if (styled[k].start <= from && to <= styled[k].end) bold |= styled[k].style == 1, italic |= styled[k].style == 2, struck |= styled[k].style == 4, code |= styled[k].style == 5;
        int link = -1;
        for (int k = 0; k < nlinks && link < 0; k++)
            if (links[k].start <= from && to <= links[k].end) link = k;
        if (!bold && !italic && !struck && !code && link < 0) continue;
        gs_ui_text_span *span = &out[n];
        *span = (gs_ui_text_span){ .start = from, .end = to };
        span->font = code ? SPAN_CODE : bold && italic ? SPAN_BOTH : bold ? SPAN_BOLD : italic ? SPAN_ITALIC : SPAN_PLAIN;
        if (code) span->back = s->raised;
        // (a struck-through part is drawn faint, there being no line to strike it with)
        if (struck) span->color = s->muted;
        if (link >= 0) span->color = links[link].color, span->link = true;
        link_of[n++] = link;
    }
    return n;
}

// The room kept at the right of a joined line for its time, or what it says of its sending, and its
// menu's dots: its text keeps clear of them, the window's background being see-through.
static float stack_margin(gs_ui *ui) { return gs_ui_text_width(ui, "00:00:00", gs_ui_style_of(ui)->small_px) + 12 + MENU_W; }

static float message_height(const app *a, gs_ui *ui, const gsc_message *m, float width) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    float line = gs_ui_line_height(ui, s->small_px);
    if (m->kind == GSC_UNDECRYPTABLE) return SDL_max(s->row, gs_ui_text_height(ui, m->text, s->text_px, width - RESET_W - 8)) + ROW_GAP;
    if (m->kind != GSC_TEXT) return gs_ui_text_height(ui, shown_text(m), s->text_px, width) + ROW_GAP;
    float inner = text_width(a, ui, width) - (stacked(a, (int)(m - a->messages)) ? stack_margin(ui) : 0), text = 0;
    // (a text's height is kept by what it depends on: a message that arrives, or a receipt, has
    // every message held measured again, and wrapping them all again took most of a frame)
    static struct { Uint64 key; float height; } kept[4096];
    Uint64 key = 1469598103934665603ull;
    const char *shown = m->text[0] ? body_of(a, m) : "";
    for (const char *p = shown; *p; p++) key = (key ^ (unsigned char)*p) * 1099511628211ull;
    for (const char *p = m->styles; *p; p++) key = (key ^ (unsigned char)*p) * 1099511628211ull;
    float measures[] = { inner, s->text_px, s->line_height, gs_ui_scale(ui), (float)a->font };
    for (int k = 0; k < (int)SDL_arraysize(measures); k++) key = (key ^ (Uint64)(Sint64)(measures[k] * 1024)) * 1099511628211ull;
    key = (key ^ (Uint64)m->id) * 1099511628211ull;
    unsigned slot = (unsigned)(key ^ key >> 32) % SDL_arraysize(kept);
    if (!m->text[0]) {
    } else if (kept[slot].key == key) text = kept[slot].height;
    else if (m->styles[0]) {
        // (styled text is measured in its fonts, which may be wider than the plain one)
        static gs_ui_text_span spans[MAX_SPANS];
        int link_of[MAX_SPANS];
        gs_fontset *fonts[SPAN_FONTS];
        int nfonts = span_fonts(a, fonts), n = text_spans(a, ui, m, body_of(a, m), NULL, 0, spans, link_of);
        text = gs_ui_text_spans_height(ui, body_of(a, m), s->text_px, inner, fonts, nfonts, spans, n);
        kept[slot].key = key, kept[slot].height = text;
    } else {
        text = gs_ui_text_height(ui, body_of(a, m), s->text_px, inner);
        kept[slot].key = key, kept[slot].height = text;
    }
    float body = (m->reply_to ? line : 0) + files_height(a, ui, m, inner) + text + reactions_height(ui, m, inner);
    // (a chat log has no line of its own for the name, which stands beside the first line of the text)
    if (a->turns == TURNS_COLUMNS) return SDL_max(body, gs_ui_line_height(ui, s->text_px)) + 4;
    int i = (int)(m - a->messages);
    return (stacked(a, i) ? 0 : line) + body + gap_below(a, i);
}

// Empties the list of files that go with the draft. A picture that was pasted exists only as the
// file written for it, which has been stored with its message by now or is not wanted, and goes.
static void clear_attached(app *a) {
    for (int i = 0; i < a->nattach; i++)
        if (a->pasted[0] && !strncmp(a->attach[i], a->pasted, strlen(a->pasted))) SDL_RemovePath(a->attach[i]);
    a->nattach = 0;
    memset(a->attach_kind, 0, sizeof a->attach_kind);
}

// A number that changes when the draft does: its conversation, its text and its files.
static Uint32 draft_mark(const app *a) {
    Uint32 mark = 2166136261u ^ (Uint32)a->open;
    for (const char *c = a->draft; *c; c++) mark = (mark ^ (unsigned char)*c) * 16777619u;
    for (int i = 0; i < a->nattach; i++)
        for (const char *c = a->attach[i]; *c; c++) mark = (mark ^ (unsigned char)*c) * 16777619u + 1;
    return mark;
}

// Keeps the open conversation's draft in the database, when it is not as it was last kept.
static void save_draft(app *a) {
    static char files[MAX_ATTACH * 1025];
    if (!a->open || draft_mark(a) == a->draft_kept) return;
    size_t at = 0;
    for (int i = 0; i < a->nattach; i++) at += (size_t)snprintf(files + at, sizeof files - at, "%s%s", i ? "\n" : "", a->attach[i]);
    files[at] = 0;
    gsc_draft_set(a->core, a->open, a->draft, files);
    a->draft_kept = draft_mark(a);
}

// Brings back the draft kept for the open conversation, with those of its files that still exist.
static void load_draft(app *a) {
    const char *files = "", *text = gsc_draft(a->core, a->open, &files);
    snprintf(a->draft, sizeof a->draft, "%s", text);
    a->nattach = 0, memset(a->attach_kind, 0, sizeof a->attach_kind);
    for (const char *line = files; *line && a->nattach < MAX_ATTACH; ) {
        const char *end = strchr(line, '\n');
        size_t len = end ? (size_t)(end - line) : strlen(line);
        SDL_PathInfo info;
        if (len && len < sizeof a->attach[0]) {
            memcpy(a->attach[a->nattach], line, len), a->attach[a->nattach][len] = 0;
            if (SDL_GetPathInfo(a->attach[a->nattach], &info) && info.type == SDL_PATHTYPE_FILE) a->nattach++;
        }
        line += len + (end ? 1 : 0);
    }
    a->draft_kept = draft_mark(a);
}

// Takes one file off that list, and the others keep their order.
static void remove_attached(app *a, int i) {
    if (a->pasted[0] && !strncmp(a->attach[i], a->pasted, strlen(a->pasted))) SDL_RemovePath(a->attach[i]);
    a->nattach--;
    memmove(a->attach[i], a->attach[i + 1], (size_t)(a->nattach - i) * sizeof a->attach[0]);
    memmove(&a->attach_kind[i], &a->attach_kind[i + 1], (size_t)(a->nattach - i));
    a->attach_kind[a->nattach] = 0;
}

// The files that will go with the draft, each as a small square: a picture as itself, cut to the
// square, and any other file as its kind over its name. A cross on a square's corner takes that
// file off. They stand in rows above the draft, as many rows as they need.
#define TILE 64
#define TILE_GAP 6
static void attached_tiles(app *a, gs_ui *ui, gs_rect *r) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    if (!a->nattach) return;
    int across = SDL_max(1, (int)((r->w - 16 + TILE_GAP) / (TILE + TILE_GAP))), rows = (a->nattach + across - 1) / across, gone = -1;
    gs_rect area = gs_cut_bottom(r, rows * (TILE + TILE_GAP) + 2);
    for (int i = 0; i < a->nattach; i++) {
        gs_rect tile = { area.x + 8 + (i % across) * (TILE + TILE_GAP), area.y + (i / across) * (TILE + TILE_GAP), TILE, TILE };
        const char *path = a->attach[i], *name = strrchr(path, '/'), *other = strrchr(path, '\\');
        name = other > name ? other : name, name = name ? name + 1 : path;
        const char *dot = strrchr(name, '.');
        char key[1100], kind[8] = "file";
        snprintf(key, sizeof key, "draft:%s", path);
        if (dot && dot[1] && strlen(dot + 1) < sizeof kind)
            for (int k = 0; k == 0 || dot[k]; k++) kind[k] = (char)SDL_toupper(dot[k + 1]);
        // (a picture is read once, when its square is first drawn; a large one is left as a file)
        if (!a->attach_kind[i]) {
            a->attach_kind[i] = 2;
            SDL_PathInfo info;
            if ((!strcmp(kind, "PNG") || !strcmp(kind, "JPG") || !strcmp(kind, "JPEG")) && SDL_GetPathInfo(path, &info) && info.size <= (32 << 20)) {
                size_t len;
                void *bytes = SDL_LoadFile(path, &len);
                if (bytes && gs_images_put(a->images, key, bytes, len)) a->attach_kind[i] = 1;
                SDL_free(bytes);
            }
        }
        float scale = gs_ui_scale(ui);
        SDL_Texture *picture = a->attach_kind[i] == 1 ? gs_images_get(a->images, key, (int)(TILE * scale), (int)(TILE * scale)) : NULL;
        gs_ui_fill(ui, tile, s->raised);
        if (picture) gs_ui_texture(ui, tile, picture, NULL);
        else if (a->attach_kind[i] == 2) {
            gs_rect foot = { tile.x + 4, tile.y + tile.h - gs_ui_line_height(ui, s->small_px) - 2, tile.w - 8, gs_ui_line_height(ui, s->small_px) };
            gs_ui_text(ui, (gs_rect){ tile.x, tile.y, tile.w, foot.y - tile.y }, kind, s->text_px, s->text, 0);
            gs_ui_clip(ui, &foot), gs_ui_text(ui, foot, name, s->small_px, s->muted, -1), gs_ui_clip(ui, NULL);
        }
        gs_rect cross = { tile.x + tile.w - 18, tile.y, 18, 18 };
        gs_ui_fill(ui, cross, gs_ui_hovered(ui, cross) ? s->accent : s->panel);
        gs_ui_text(ui, cross, "x", s->small_px, gs_ui_hovered(ui, cross) ? s->accent_text : s->text, 0);
        if (gs_ui_clicked(ui, cross)) gone = i;
    }
    if (gone >= 0) remove_attached(a, gone), wake(NULL);
}

// Takes what the clipboard holds as files to go with the draft: the files copied in a file manager;
// or, when it holds neither those nor text, a picture, as a screenshot or a browser's "copy image"
// leaves one. Returns whether it took anything; if not, the paste is the text field's to handle.
static bool paste_files(app *a) {
    int before = a->nattach;
#ifdef _WIN32
    // (SDL does not offer the list of copied files on Windows, so it is asked of the system)
    if (IsClipboardFormatAvailable(CF_HDROP) && OpenClipboard(NULL)) {
        HDROP drop = (HDROP)GetClipboardData(CF_HDROP);
        UINT n = drop ? DragQueryFileW(drop, 0xFFFFFFFF, NULL, 0) : 0;
        for (UINT i = 0; i < n && a->nattach < MAX_ATTACH; i++) {
            WCHAR wide[1024];
            char *path = a->attach[a->nattach];
            SDL_PathInfo info;
            if (DragQueryFileW(drop, i, wide, 1024) && WideCharToMultiByte(CP_UTF8, 0, wide, -1, path, sizeof a->attach[0], NULL, NULL) && SDL_GetPathInfo(path, &info) && info.type == SDL_PATHTYPE_FILE) a->nattach++;
        }
        CloseClipboard();
    }
#else
    // Elsewhere they come as a list of addresses, one to a line, with bytes outside plain text written as %XX.
    size_t length = 0;
    char *list = SDL_GetClipboardData("text/uri-list", &length);
    for (char *line = list, *end; line && line < list + length && a->nattach < MAX_ATTACH; line = end + 1) {
        end = memchr(line, '\n', list + length - line);
        if (!end) end = list + length;
        char *path = a->attach[a->nattach];
        size_t n = 0;
        if (end - line > 7 && !strncmp(line, "file://", 7))
            for (const char *c = strchr(line + 7, '/'); c && c < end && *c != '\r' && n + 1 < sizeof a->attach[0]; c++) {
                unsigned byte;
                if (*c == '%' && c + 2 < end && sscanf(c + 1, "%2x", &byte) == 1) path[n++] = (char)byte, c += 2;
                else path[n++] = *c;
            }
        path[n] = 0;
        SDL_PathInfo info;
        if (n && SDL_GetPathInfo(path, &info) && info.type == SDL_PATHTYPE_FILE) a->nattach++;
        if (end >= list + length) break;
    }
    SDL_free(list);
#endif
    if (a->nattach > before) return true;
    if (!a->pasted[0] || a->nattach >= MAX_ATTACH) return false;
    // A picture goes before text: a browser's "copy image" leaves the picture's address as text
    // beside it, and what is wanted is the picture. The exception is what a spreadsheet or a word
    // processor copies, which is text with a picture of itself added, known by the forms only they add.
    static const char *const office[] = { "text/rtf", "text/richtext", "text/csv", "application/x-openoffice-embed-source-xml;windows_formatname=\"Star Embed Source (XML)\"" };
    bool written_text = false;
    for (int i = 0; i < (int)SDL_arraysize(office); i++) written_text |= SDL_HasClipboardData(office[i]);
#ifdef _WIN32
    static const char *const marks[] = { "Rich Text Format", "Csv", "Biff12", "XML Spreadsheet", "Star Embed Source (XML)" };
    for (int i = 0; i < (int)SDL_arraysize(marks); i++) written_text |= IsClipboardFormatAvailable(RegisterClipboardFormatA(marks[i])) != 0;
#endif
    bool text = SDL_HasClipboardText(), took = false;
    const char *seen = "none";
    // A picture is written to a file of its own, as the forms a message can carry; a bitmap, which
    // is what Windows keeps of a screenshot, is written as a PNG.
    static const char *const forms[][2] = { { "image/png", "png" }, { "image/jpeg", "jpg" }, { "image/gif", "gif" }, { "image/webp", "webp" }, { "image/bmp", "png" } };
    for (int i = 0; i < (int)SDL_arraysize(forms) && !took && !(text && written_text); i++) {
        size_t size = 0;
        void *data = SDL_HasClipboardData(forms[i][0]) ? SDL_GetClipboardData(forms[i][0], &size) : NULL;
        if (!data || !size) { SDL_free(data); continue; }
        seen = forms[i][0];
        char *path = a->attach[a->nattach];
        // (named by the time, so that one kept with a draft from an earlier run is not written over)
        SDL_Time now = 0;
        SDL_GetCurrentTime(&now);
        snprintf(path, sizeof a->attach[0], "%s%lld-%d.%s", a->pasted, (long long)SDL_NS_TO_MS(now), ++a->npasted, forms[i][1]);
        bool written;
        if (!strcmp(forms[i][0], "image/bmp")) {
            SDL_Surface *picture = SDL_LoadBMP_IO(SDL_IOFromConstMem(data, size), true);
            written = picture && SDL_SavePNG(picture, path);
            SDL_DestroySurface(picture);
        } else written = SDL_SaveFile(path, data, size);
        SDL_free(data);
        if (written) a->nattach++, took = true;
    }
    // What the paste found and did is written down, for looking at from outside when one did not
    // do what was meant: the kinds of thing on the clipboard, never what they held.
    char note[1100], said[200];
    snprintf(note, sizeof note, "%snote.txt", a->pasted);
    SDL_SaveFile(note, said, snprintf(said, sizeof said, "text %d\nwritten text %d\npicture %s\nattached %d\nerror %s\n", text, written_text, seen, took, took ? "" : SDL_GetError()));
    return took;
}

// Finds the web addresses in a text: what starts with "http://", "https://" or "www." at the start
// of a word and runs to the next space, less any closing punctuation. Returns how many, up to max.
static int find_links(const char *text, gs_ui_text_range *out, int max, SDL_FColor colour) {
    int n = 0;
    for (const char *p = text; *p && n < max; p++) {
        bool starts = p == text || SDL_isspace((unsigned char)p[-1]) || p[-1] == '(' || p[-1] == '<';
        if (!starts || (strncmp(p, "http://", 7) && strncmp(p, "https://", 8) && strncmp(p, "www.", 4))) continue;
        const char *end = p;
        while (*end && !SDL_isspace((unsigned char)*end) && *end != '<' && *end != '>') end++;
        while (end > p && strchr(".,;:!?)]}'\"", end[-1])) end--;
        if (end - p > 8) out[n++] = (gs_ui_text_range){ (int)(p - text), (int)(end - text), colour, true };
        p = end - 1;
    }
    return n;
}

// The room added above a message where the speaker changes, in the ways that mark turns by room.
static float turn_gap(const app *a, int i) {
    if (a->turns == TURNS_PLAIN || i == 0) return 0;
    const gsc_message *m = &a->messages[i], *before = &a->messages[i - 1];
    bool same = m->outgoing == before->outgoing && !strcmp(m->sender, before->sender) && m->kind == GSC_TEXT && before->kind == GSC_TEXT;
    return same ? 0 : a->turns == TURNS_COLUMNS ? 8 : 10;
}

typedef enum { ROW_NOTHING, ROW_RESET, ROW_MENU } row_asked;

// What file dialogs hand back, on whatever thread the system calls them: files to attach, and where
// to save the attachment asked about. The main loop takes them.
static struct {
    SDL_Mutex *lock;
    char attach[MAX_ATTACH][1024];
    int nattach;
    int64_t save;
    char save_to[1024];
} chosen;

static void SDLCALL chose_files(void *user, const char *const *paths, int filter) {
    SDL_LockMutex(chosen.lock);
    for (int i = 0; paths && paths[i] && chosen.nattach < MAX_ATTACH; i++) snprintf(chosen.attach[chosen.nattach++], sizeof chosen.attach[0], "%s", paths[i]);
    SDL_UnlockMutex(chosen.lock);
    wake(NULL);
}

static void SDLCALL chose_save(void *user, const char *const *paths, int filter) {
    SDL_LockMutex(chosen.lock);
    if (paths && paths[0]) chosen.save = (int64_t)(intptr_t)user, snprintf(chosen.save_to, sizeof chosen.save_to, "%s", paths[0]);
    SDL_UnlockMutex(chosen.lock);
    wake(NULL);
}

static void size_text(int64_t bytes, char *out, size_t size) {
    if (bytes < 1024) snprintf(out, size, "%lld B", (long long)bytes);
    else if (bytes < 1024 * 1024) snprintf(out, size, "%.0f KB", bytes / 1024.0);
    else snprintf(out, size, "%.1f MB", bytes / (1024.0 * 1024.0));
}

// Ends the sound that is playing or paused, if any.
static void stop_sound(app *a) {
    if (a->sound) gs_mix_remove(gs_sound_render, a->sound), gs_sound_free(a->sound);
    SDL_free(a->sound_bytes);
    a->sound = NULL, a->sound_bytes = NULL, a->sounding = 0;
}

// Sound is set up when it is first wanted: most runs play none. False when there is nothing to play on.
static bool mixing(app *a) { return a->mixing || (a->mixing = SDL_InitSubSystem(SDL_INIT_AUDIO) && gs_mix_open(48000)); }

// Closes a video, with its sound taken out of the mix first.
static void close_movie(app *a, int i) {
    if (a->movies[i].sounding) gs_mix_remove(gs_movie_render, a->movies[i].movie);
    if (a->movies[i].movie) gs_movie_free(a->movies[i].movie);
    SDL_free(a->movies[i].bytes);
    memset(&a->movies[i], 0, sizeof a->movies[i]);
}

static void close_movies(app *a) {
    for (int i = 0; i < MAX_MOVIES; i++) close_movie(a, i);
    for (int i = 0; i < MAX_GIFS; i++) {
        if (a->gifs[i].gif) gs_gif_free(a->gifs[i].gif);
        SDL_free(a->gifs[i].bytes);
        memset(&a->gifs[i], 0, sizeof a->gifs[i]);
    }
}

// The open GIF of an attachment, opened if it is not, in the place of the one drawn longest ago; and
// marked as drawn in this frame. NULL when its bytes are not a GIF that can be read.
static gs_gif *gif_of(app *a, int64_t id) {
    int oldest = 0;
    for (int i = 0; i < MAX_GIFS; i++) {
        if (a->gifs[i].id == id && a->gifs[i].gif) return a->gifs[i].drawn = a->frame, a->gifs[i].gif;
        if (a->gifs[i].drawn < a->gifs[oldest].drawn) oldest = i;
    }
    if (a->gifs[oldest].gif) gs_gif_free(a->gifs[oldest].gif);
    SDL_free(a->gifs[oldest].bytes);
    memset(&a->gifs[oldest], 0, sizeof a->gifs[oldest]);
    size_t len;
    const void *bytes = gsc_attachment_data(a->core, id, &len);
    void *copy = bytes ? SDL_malloc(len) : NULL;
    if (copy) memcpy(copy, bytes, len);
    gs_gif *g = copy ? gs_gif_open(copy, len, a->renderer) : NULL;
    if (!g) return SDL_free(copy), NULL;
    a->gifs[oldest].id = id, a->gifs[oldest].gif = g, a->gifs[oldest].bytes = copy, a->gifs[oldest].drawn = a->frame;
    return g;
}

// A GIF drawn whole in `at`, playing; a frame is asked for when its next is due.
static void gif_frame(app *a, gs_ui *ui, gs_gif *g, gs_rect at) {
    float scale = gs_ui_scale(ui);
    if (gs_gif_animated(g) && !gs_gif_playing(g)) gs_gif_play(g, true);
    gs_gif_draw(g, (SDL_FRect){ at.x * scale, at.y * scale, at.w * scale, at.h * scale });
    int due = gs_gif_due_ms(g);
    if (due >= 0) gs_ui_wake(ui, due);
}

// GIFs not drawn in this frame, scrolled away or closed, stop and let their decoders go.
static void rest_gifs(app *a) {
    for (int i = 0; i < MAX_GIFS; i++)
        if (a->gifs[i].gif && a->gifs[i].drawn != a->frame && gs_gif_playing(a->gifs[i].gif)) gs_gif_rest(a->gifs[i].gif);
}

// A video at rest: its sound out of the mix and its decoder let go, back at its first frame.
static void rest_movie(app *a, int i) {
    if (a->movies[i].sounding) gs_mix_remove(gs_movie_render, a->movies[i].movie), a->movies[i].sounding = false;
    gs_movie_rest(a->movies[i].movie);
}

// The open video of an attachment, opened if it is not: in the place of the one drawn longest ago
// when all the places are taken. -1 when its bytes are not held or are not a video gesso plays.
static int movie_of(app *a, const held_file *f) {
    int oldest = 0;
    for (int i = 0; i < MAX_MOVIES; i++) {
        if (a->movies[i].id == f->id) return i;
        if (a->movies[i].drawn < a->movies[oldest].drawn) oldest = i;
    }
    for (int i = 0; i < a->nno_movie; i++)
        if (a->no_movie[i] == f->id) return -1;
    close_movie(a, oldest);
    size_t len;
    const void *bytes = gsc_attachment_data(a->core, f->id, &len);
    void *copy = bytes ? SDL_malloc(len) : NULL;
    if (copy) memcpy(copy, bytes, len);
    gs_movie *movie = copy ? gs_movie_open(copy, len, a->renderer, mixing(a) ? gs_mix_rate() : 48000) : NULL;
    if (!movie) {
        SDL_free(copy);
        if (a->nno_movie < 64) a->no_movie[a->nno_movie++] = f->id;
        return -1;
    }
    if (f->looped) gs_movie_set_loop(movie, true), gs_movie_set_muted(movie, true);
    a->movies[oldest].id = f->id, a->movies[oldest].movie = movie, a->movies[oldest].bytes = copy;
    return oldest;
}

static void stop_sound(app *a);

// A video in its message: its first frame until it is clicked, then playing in place, with a bar
// along its foot that shows how far it has got and that a click moves. A click on the picture
// plays or pauses; one video plays at a time, and playing one stops a voice note.
// Hands a copy of a file to the program the system opens such files with.
static void open_file(app *a, int64_t id, const char *named) {
    if (!a->opened[0]) return;
    char path[1400], name[256];
    snprintf(name, sizeof name, "%s", named[0] ? named : "file");
    for (char *c = name; *c; c++)
        if (strchr("/\\:*?\"<>|", *c)) *c = '_';
    snprintf(path, sizeof path, "%s%lld-%s", a->opened, (long long)id, name);
    if (gsc_attachment_save(a->core, id, path)) SDL_OpenURL(path);
}

// A video in `at`: in a message (`big` false) its still, which a click opens whole; opened, the
// video itself, which a click plays or pauses and a click on the bar at its foot moves through.
static void movie_frame(app *a, gs_ui *ui, const held_file *f, gs_rect at, bool big) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    float scale = gs_ui_scale(ui);
    gs_ui_fill(ui, at, (SDL_FColor){ 0, 0, 0, 1 });
    int i = movie_of(a, f);
    if (i < 0) { gs_ui_text(ui, at, "This video cannot be played here; it can be saved", s->small_px, s->muted, 0); return; }
    gs_movie *movie = a->movies[i].movie;
    a->movies[i].drawn = SDL_max(SDL_GetTicks(), 1);
    // (fitted into its place whole, whatever shape the sender said it had)
    int mw, mh;
    gs_movie_size(movie, &mw, &mh);
    // (a video that came without its size is laid out again, now that it is known)
    if ((f->w != mw || f->h != mh) && mw > 0 && mh > 0) ((held_file *)f)->w = mw, ((held_file *)f)->h = mh, a->heights_width = 0, wake(NULL);
    float fit = SDL_min(at.w / SDL_max(mw, 1), at.h / SDL_max(mh, 1));
    gs_rect picture = { at.x + (at.w - mw * fit) / 2, at.y + (at.h - mh * fit) / 2, mw * fit, mh * fit };
    bool playing = gs_movie_playing(movie);
    gs_movie_draw(movie, (SDL_FRect){ picture.x * scale, picture.y * scale, picture.w * scale, picture.h * scale });
    // (frames are asked for while it plays, and while what is drawn is not yet the frame for its place:
    // before its first frame, and after its place was moved)
    if (playing || gs_movie_waiting(movie)) gs_ui_wake(ui, 16);
    // How it is being decoded is written down when that changes, for looking at from outside.
    const char *path = gs_movie_get_info(movie).path;
    if (a->renderer_path[0] && path && strcmp(path, a->video_path) && strcmp(path, "none")) {
        snprintf(a->video_path, sizeof a->video_path, "%s", path);
        char said[200];
        SDL_SaveFile(a->renderer_path, said, snprintf(said, sizeof said, "renderer %s\nvideo decoded by %s\n", SDL_GetRendererName(a->renderer), path));
    }
    if (gs_movie_ended(movie) && a->movies[i].sounding) gs_mix_remove(gs_movie_render, movie), a->movies[i].sounding = false;
    double length = gs_movie_length(movie), now = gs_movie_position(movie);
    gs_rect foot = { at.x, at.y + at.h - 14, at.w, 14 }, bar = { at.x, at.y + at.h - 4, at.w, 4 };
    if (playing || now > 0) {
        gs_ui_fill(ui, bar, (SDL_FColor){ 1, 1, 1, 0.25f });
        gs_ui_fill(ui, (gs_rect){ bar.x, bar.y, bar.w * (length > 0 ? (float)(now / length) : 0), bar.h }, s->accent);
    }
    if (!playing) {  // (a disc with a triangle in it says it can be played)
        float cx = at.x + at.w / 2, cy = at.y + at.h / 2, r = SDL_min(26, at.h / 3);
        gs_ui_circle(ui, cx, cy, r, (SDL_FColor){ 0, 0, 0, 0.55f });
        SDL_Vertex corners[3] = {
            { { (cx - r * 0.3f) * scale, (cy - r * 0.45f) * scale }, { 1, 1, 1, 1 }, { 0, 0 } },
            { { (cx - r * 0.3f) * scale, (cy + r * 0.45f) * scale }, { 1, 1, 1, 1 }, { 0, 0 } },
            { { (cx + r * 0.5f) * scale, cy * scale }, { 1, 1, 1, 1 }, { 0, 0 } },
        };
        SDL_RenderGeometry(a->renderer, NULL, corners, 3, NULL, 0);
    }
    // (opened, it plays at once)
    bool start = big && a->to_play;
    a->to_play = a->to_play && !big;
    if (!big && !a->menu_open && gs_ui_clicked(ui, at)) {
        a->viewing = f->id, a->to_play = true;
        snprintf(a->viewing_name, sizeof a->viewing_name, "%s", f->name);
        return;
    }
    if (!big || (!start && (a->menu_open || !gs_ui_clicked(ui, at)))) return;
    if (!start && (playing || now > 0) && gs_ui_hovered(ui, foot) && length > 0) { gs_movie_seek(movie, length * SDL_clamp((a->pointer_x - at.x) / at.w, 0, 1)); return; }
    if (playing && !start) { gs_movie_play(movie, false); return; }
    for (int k = 0; k < MAX_MOVIES; k++)
        if (k != i && a->movies[k].movie && gs_movie_playing(a->movies[k].movie)) rest_movie(a, k);
    stop_sound(a);
    if (!f->looped && gs_movie_has_sound(movie) && mixing(a) && !a->movies[i].sounding) gs_mix_add(gs_movie_render, movie), a->movies[i].sounding = true;
    gs_movie_play(movie, true);
}

// Plays an audio attachment from its start, in place of any other. False when its bytes are not held,
// are not a kind of sound gesso plays, or there is nothing to play it on.
static bool start_sound(app *a, int64_t attachment) {
    stop_sound(a);
    for (int i = 0; i < MAX_MOVIES; i++)
        if (a->movies[i].movie && gs_movie_playing(a->movies[i].movie)) rest_movie(a, i);
    if (!mixing(a)) return false;
    size_t len;
    const void *bytes = gsc_attachment_data(a->core, attachment, &len);
    if (!bytes || !(a->sound_bytes = SDL_malloc(len))) return false;
    memcpy(a->sound_bytes, bytes, len);
    if (!(a->sound = gs_sound_open(a->sound_bytes, len, gs_mix_rate()))) return stop_sound(a), false;
    gs_mix_add(gs_sound_render, a->sound);
    gs_sound_play(a->sound, true);
    a->sounding = attachment;
    return true;
}

// An audio attachment's line: a button that plays or pauses it, how far it has got as a bar that a
// click moves, and the time. *line is left with the room after them.
static void sound_controls(app *a, gs_ui *ui, const held_file *f, gs_rect *line) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    bool this = a->sounding == f->id && a->sound, playing = this && gs_sound_playing(a->sound);
    gs_rect button = gs_cut_left(line, 64);
    button.y += 3, button.h -= 6;
    gs_cut_left(line, 8);
    if (gs_ui_button(ui, button, playing ? "Pause" : "Play")) {
        if (this) gs_sound_play(a->sound, !playing);
        else if (!start_sound(a, f->id)) a->unplayable = f->id;
    }
    if (a->unplayable == f->id) { gs_cut_left(line, gs_ui_text(ui, *line, "cannot be played here", s->small_px, s->warning, -1) + 10); return; }
    if (!this) return;
    double at = gs_sound_position(a->sound), length = gs_sound_length(a->sound);
    gs_rect bar = gs_cut_left(line, 160);
    bar.y += bar.h / 2 - 3, bar.h = 6;
    gs_ui_progress(ui, bar, length > 0 ? (float)(at / length) : 0);
    gs_rect hit = { bar.x, bar.y - 8, bar.w, bar.h + 16 };
    if (gs_ui_clicked(ui, hit) && length > 0) gs_sound_seek(a->sound, length * SDL_clamp((a->pointer_x - bar.x) / bar.w, 0, 1));
    char time[32];
    snprintf(time, sizeof time, "%d:%02d / %d:%02d", (int)at / 60, (int)at % 60, (int)length / 60, (int)length % 60);
    gs_cut_left(line, 8);
    gs_cut_left(line, gs_ui_text(ui, *line, time, s->small_px, s->muted, -1) + 10);
    if (playing) gs_ui_wake(ui, 50);  // (the bar moves while it plays)
}

// Draws one line of text cut to its rectangle, inside a list whose own bounds then hold again for
// the rows that follow.
static void clipped_text(app *a, gs_ui *ui, gs_rect r, const char *text, float size, SDL_FColor colour) {
    float top = SDL_max(r.y, a->within.y), bottom = SDL_min(r.y + r.h, a->within.y + a->within.h);
    gs_rect part = { r.x, top, r.w, bottom - top };
    if (part.h > 0) gs_ui_clip(ui, &part), gs_ui_text(ui, r, text, size, colour, -1);
    gs_ui_clip(ui, &a->within);
}

// One picture of a message, in `at`; a click opens it whole.
static void picture_tile(app *a, gs_ui *ui, const held_file *f, gs_rect at) {
    char key[40];
    snprintf(key, sizeof key, "attachment-%lld", (long long)f->id);
    bool given = false;
    for (int k = 0; k < a->npictured && !given; k++) given = a->pictured[k] == f->id;
    if (!given && !gif(f) && a->npictured < MAX_FILES) {
        size_t len;
        const void *bytes = gsc_attachment_data(a->core, f->id, &len);
        if (bytes && gs_images_put(a->images, key, bytes, len)) a->pictured[a->npictured++] = f->id;
    }
    float scale = gs_ui_scale(ui);
    gs_gif *moving = gif(f) ? gif_of(a, f->id) : NULL;
    SDL_Texture *t = gif(f) ? NULL : gs_images_get(a->images, key, (int)(at.w * scale), (int)(at.h * scale));
    if (moving) gif_frame(a, ui, moving, at);
    else if (t) gs_ui_texture(ui, at, t, NULL);
    else gs_ui_fill(ui, at, gs_ui_style_of(ui)->raised);
    if (gs_ui_clicked(ui, at) && !a->menu_open) a->viewing = f->id, snprintf(a->viewing_name, sizeof a->viewing_name, "%s", f->name);
}

// A message's files, from the top of *row down: its pictures two to a row, with nothing under them
// (an opened picture is saved from where it is shown), then each other file, a video in its frame,
// on a line with the file's name and size and buttons to open and save it.
static void files_rows(app *a, gs_ui *ui, const gsc_message *m, gs_rect *row) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    int n;
    const held_file *all = files_of(a, m, &n), *f = all;
    for (int i = next_picture(all, n, -1); i >= 0; i = next_picture(all, n, i)) {
        int j = next_picture(all, n, i);
        float w, w2, h;
        picture_pair(&all[i], j >= 0 ? &all[j] : NULL, row->w, &w, &w2, &h);
        gs_rect line = gs_cut_top(row, h + PICTURE_GAP);
        picture_tile(a, ui, &all[i], (gs_rect){ line.x, line.y, w, h });
        if (j >= 0) picture_tile(a, ui, &all[j], (gs_rect){ line.x + w + PICTURE_GAP, line.y, w2, h }), i = j;
    }
    for (int i = 0; i < n; i++, f++) {
        if (pictured(f)) continue;
        if (filmed(f)) {
            float w, h;
            picture_size(f, row->w, &w, &h);
            gs_rect at = gs_cut_top(row, h + 4);
            at.w = w, at.h = h;
            movie_frame(a, ui, f, at, false);
            continue;
        }
        gs_rect line = gs_cut_top(row, s->row);
        char label[200], size[24];
        size_text(f->size, size, sizeof size);
        bool large = f->size > FETCHED_UNASKED, asked = false;
        for (int k = 0; k < a->nasked; k++) asked |= a->asked[k] == f->id;
        snprintf(label, sizeof label, "%s  %s%s", f->name[0] ? f->name : f->type, size,
                 f->state == GSC_ATTACHMENT_FAILED ? "  could not be fetched" : f->state == GSC_ATTACHMENT_HERE ? "" : large && !asked ? "  not fetched" : "  fetching");
        // One the core left alone, for its size or after failing, is fetched when the user asks.
        if ((f->state == GSC_ATTACHMENT_FAILED || (f->state == GSC_ATTACHMENT_WAITING && large)) && !asked) {
            gs_rect fetch = gs_cut_right(&line, 64);
            fetch.y += 3, fetch.h -= 6;
            if (gs_ui_button(ui, fetch, "Fetch") && a->nasked < MAX_ATTACH * 8) gsc_attachment_fetch(a->core, f->id), a->asked[a->nasked++] = f->id;
            gs_cut_right(&line, 8);
        }
        if (f->state == GSC_ATTACHMENT_HERE && !strncmp(f->type, "audio/", 6)) sound_controls(a, ui, f, &line);
        if (f->state == GSC_ATTACHMENT_HERE) {
            gs_rect save = gs_cut_right(&line, 64);
            save.y += 3, save.h -= 6;
            if (gs_ui_button(ui, save, "Save")) SDL_ShowSaveFileDialog(chose_save, (void *)(intptr_t)f->id, a->window, NULL, 0, f->name[0] ? f->name : NULL);
            gs_cut_right(&line, 8);
            // Open hands a copy to the program the system opens such files with.
            gs_rect open = gs_cut_right(&line, 64);
            open.y += 3, open.h -= 6;
            if (gs_ui_button(ui, open, "Open")) open_file(a, f->id, f->name);
            gs_cut_right(&line, 8);
        }
        clipped_text(a, ui, line, label, s->small_px, f->state == GSC_ATTACHMENT_FAILED ? s->warning : s->muted);
    }
}

// `unnamed` stands for a sender the core has no name for. Says whether the user asked, on a notice
// of a message that could not be decrypted, for a new session, or for a message's menu, which
// hangs from *anchor.
static row_asked message_row(app *a, gs_ui *ui, const gsc_message *m, gs_rect row, const char *unnamed, gs_rect *anchor) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    if (m->kind != GSC_TEXT) {
        bool reset = false;
        if (m->kind == GSC_UNDECRYPTABLE) {
            gs_rect button = gs_cut_right(&row, RESET_W);
            reset = gs_ui_button(ui, gs_cut_top(&button, s->row), "Reset session");
            gs_cut_right(&row, 8);
        }
        gs_ui_text_wrapped(ui, row, shown_text(m), s->text_px, m->kind == GSC_DELETED || m->kind == GSC_EVENT ? s->muted : s->warning);
        return reset ? ROW_RESET : ROW_NOTHING;
    }
    float line = gs_ui_line_height(ui, s->small_px);
    const char *name = m->outgoing ? "me" : m->sender[0] ? m->sender : unnamed;
    SDL_FColor colour = a->turns == TURNS_PLAIN ? (m->outgoing ? s->accent : s->muted) : person_colour(ui, m);
    // (a message not yet sent says so in place of how far it has got, and one given up says it in the warning colour)
    static const char *const statuses[] = { "held: safety number changed", "not sent", "sending", "sent", "delivered", "read" };
    const char *status = statuses[m->status - GSC_HELD];
    SDL_FColor status_colour = m->status <= GSC_NOT_SENT ? s->warning : s->muted;
    char when[16];
    clock_text(m->sent_ms, when, sizeof when);
    if (a->turns == TURNS_COLUMNS) {
        // The time and the name stand in columns beside the first line of the text, the name against
        // the text; the menu's dots are at the far end of that line.
        float first = gs_ui_line_height(ui, s->text_px);
        gs_rect left = gs_cut_left(&row, columns_width(ui, row.w)), top = { left.x, left.y, left.w - 12, first };
        when[5] = 0;  // (hours and minutes, which give way to a message's not being sent)
        gs_rect time = gs_cut_left(&top, gs_ui_text_width(ui, "00:00", s->small_px) + 10);
        if (m->outgoing && m->status < GSC_SENT) gs_ui_text(ui, time, m->status == GSC_HELD ? "held" : m->status == GSC_NOT_SENT ? "fail" : "wait", s->small_px, status_colour, -1);
        else gs_ui_text(ui, time, when, s->small_px, s->muted, -1);
        gs_ui_clip(ui, &(gs_rect){ top.x, SDL_max(top.y, a->within.y), top.w, SDL_max(0, SDL_min(top.y + top.h, a->within.y + a->within.h) - SDL_max(top.y, a->within.y)) });
        gs_ui_text(ui, top, name, s->text_px, colour, 1);
        gs_ui_clip(ui, &a->within);
        *anchor = (gs_rect){ row.x + row.w - MENU_W, row.y, MENU_W, first };
        row.w -= MENU_W;
        gs_ui_text(ui, *anchor, "...", s->small_px, gs_ui_hovered(ui, *anchor) ? s->accent : s->muted, 0);
    } else {
        if (a->turns == TURNS_BARS) gs_ui_fill(ui, gs_cut_left(&row, BAR_W), colour), gs_cut_left(&row, BAR_GAP);
        // A line under the same person's message before it has no name over it. At its top right,
        // over the text, it says while it matters that it is not yet sent, was edited or has a
        // change waiting, and shows its time and its menu's dots while the pointer is on it.
        if (stacked(a, (int)(m - a->messages))) {
            // (in a margin of its own beside the text: a short word for its sending, its time while
            // the pointer is on it, and its menu's dots)
            const char *note = m->outgoing && m->status == GSC_HELD ? "held" : m->outgoing && m->status < GSC_SENT ? status : m->changing ? (m->changing == 2 ? "not sent" : "waiting") : m->edited ? "edited" : NULL;
            SDL_FColor note_colour = m->outgoing && m->status < GSC_SENT ? status_colour : m->changing == 2 ? s->warning : s->muted;
            bool over = gs_ui_hovered(ui, row) || (a->menu_open && a->menu == m->id);
            gs_rect margin = gs_cut_right(&row, stack_margin(ui));
            margin.h = line;
            *anchor = gs_cut_right(&margin, MENU_W);
            gs_cut_left(&margin, 12);
            if (over) {
                gs_ui_text(ui, margin, when, s->small_px, s->muted, -1);
                gs_ui_text(ui, *anchor, "...", s->small_px, gs_ui_hovered(ui, *anchor) ? s->accent : s->muted, 0);
            } else if (note) gs_ui_text(ui, margin, note, s->small_px, note_colour, -1);
            goto body;
        }
        gs_rect head = gs_cut_top(&row, line);
        *anchor = gs_cut_right(&head, MENU_W);
        gs_ui_text(ui, *anchor, "...", s->small_px, gs_ui_hovered(ui, *anchor) ? s->accent : s->muted, 0);
        // The writer's picture stands before their name, as tall as that line and no taller, so that
        // it takes no room of its own; the account's own messages have none.
        if (!m->outgoing) {
            char known[64];
            snprintf(known, sizeof known, "writer-%d-%lld", m->person, (long long)m->picture_ms);
            gs_rect face = gs_cut_left(&head, head.h);
            picture_at(a, ui, face, s->background, m->picture_ms && m->person >= 0 ? known : NULL, gsc_writer_picture, m->id, name, colour);
            gs_cut_left(&head, 6);
        }
        gs_cut_left(&head, gs_ui_text(ui, head, name, s->small_px, colour, -1) + 10);
        gs_cut_left(&head, gs_ui_text(ui, head, when, s->small_px, s->muted, -1) + 10);
        // An outgoing message says how far it has got, and a disappearing one that its time is running.
        if (m->outgoing) gs_cut_left(&head, gs_ui_text(ui, head, status, s->small_px, status_colour, -1) + 10);
        if (m->edited) gs_cut_left(&head, gs_ui_text(ui, head, "edited", s->small_px, s->muted, -1) + 10);
        // (a reaction, edit or deletion asked of it shows once sent; until then it says that it waits, or that it failed)
        if (m->changing) gs_cut_left(&head, gs_ui_text(ui, head, m->changing == 2 ? "change not sent" : "change waiting", s->small_px, m->changing == 2 ? s->warning : s->muted, -1) + 10);
        if (m->expires_ms) gs_ui_text(ui, head, "disappears", s->small_px, s->warning, -1);
    }
body:
    if (m->reply_to) {
        char quoted[200];
        one_line(quoted, sizeof quoted, "Reply to %s: %s", m->reply_to[0] ? m->reply_to : "me", m->reply_text);
        gs_rect quote = gs_cut_top(&row, line);
        // (a click on it goes to the message it answers, reading further back when that is not held)
        bool to = m->reply_id && gs_ui_hovered(ui, quote);
        clipped_text(a, ui, quote, quoted, s->small_px, to ? s->accent : s->muted);
        if (m->reply_id && gs_ui_clicked(ui, quote) && !a->menu_open) {
            bool held = false;
            for (int i = 0; i < a->nmessages && !held; i++) held = a->messages[i].id == m->reply_id;
            int64_t target = m->reply_id;
            if (!held) a->wanted = MAX_MESSAGES, read_messages(a);
            a->found = target, a->show_found = true;
            return ROW_NOTHING;
        }
    }
    // Under the message, each reaction and how many gave it; who they were is shown while the
    // pointer rests on the line.
    if (m->reactions[0]) {
        gs_rect given = gs_cut_bottom(&row, reactions_height(ui, m, row.w));
        given.w = SDL_min(given.w, gs_ui_text(ui, given, m->reactions, s->small_px, s->text, -1));
        if (gs_ui_hovered(ui, given)) a->reacted = m->reacted, a->reacted_at = given;
    }
    files_rows(a, ui, m, &row);
    // The text can be selected in with the pointer, and a drag may run on across the texts of other
    // messages; a web address in it is a link that opens in the browser.
    if (m->text[0]) {
        gs_ui_text_range links[8];
        const char *body = body_of(a, m);
        int nlinks = find_links(body, links, 8, s->accent), clicked = -1;
        int from, to;
        selection_of(a, m->id, &from, &to);
        int was_from = from, was_to = to;
        char id[40];
        snprintf(id, sizeof id, "text-%lld", (long long)m->id);
        if (m->styles[0]) {
            // (the sender's bold, italic, struck-through and monospaced parts, in their own fonts)
            static gs_ui_text_span spans[MAX_SPANS];
            int link_of[MAX_SPANS];
            gs_fontset *fonts[SPAN_FONTS];
            int nfonts = span_fonts(a, fonts), n = text_spans(a, ui, m, body, links, nlinks, spans, link_of), span = -1;
            gs_ui_text_spans(ui, row, id, body, s->text_px, s->text, fonts, nfonts, spans, n, &from, &to, &span);
            clicked = span >= 0 && span < n ? link_of[span] : -1;
        } else gs_ui_text_select(ui, row, id, body, s->text_px, s->text, links, nlinks, &from, &to, &clicked);
        if (from != was_from || to != was_to) select_in(a, m->id, from, to);
        if (clicked >= 0 && clicked < nlinks) {
            char address[2100];
            const char *at = body + links[clicked].start;
            snprintf(address, sizeof address, "%s%.*s", strncmp(at, "www.", 4) ? "" : "https://", links[clicked].end - links[clicked].start, at);
            SDL_OpenURL(address);
        }
    }
    return gs_ui_clicked(ui, *anchor) ? ROW_MENU : ROW_NOTHING;
}

// Whether the open conversation is on an IRC account, which has no reactions, replies, edits,
// deletions, files or timers: plain lines of text.
static bool open_on_irc(const app *a) {
    for (int i = 0; i < a->nconversations; i++)
        if (a->conversations[i].id == a->open)
            for (int k = 0; k < a->naccounts; k++)
                if (a->accounts[k].id == a->conversations[i].account) return !strcmp(a->accounts[k].protocol, "irc");
    return false;
}

// The menu of the message the user asked about, drawn over the list beside the message's "...":
// six reactions in a row, then a line each to take the account's reaction back, reply, and for the
// account's own messages edit or delete for everyone. It opens below the "..." or, where the list
// ends too soon for that, above it, and a choice, a click elsewhere or Escape closes it.
static void message_menu(app *a, gs_ui *ui, gs_rect anchor, gs_rect list, const char *unnamed) {
    static const char *const emoji[] = { "\U0001F44D", "❤️", "\U0001F602", "\U0001F62E", "\U0001F622", "\U0001F64F" };
    const gs_ui_style *s = gs_ui_style_of(ui);
    const gsc_message *m = NULL;
    for (int i = 0; i < a->nmessages && !m; i++)
        if (a->messages[i].id == a->menu) m = &a->messages[i];
    if (!a->menu_open || !m || !anchor.w) { a->menu_open = false; return; }
    const char *lines[9];
    int nlines = 0, take_back = -1, reply = -1, copy = -1, edit = -1, delete = -1, resend = -1, reveal = -1, change_again = -1, change_drop = -1;
    // A message that no one has received can only be sent again or thrown away.
    bool unsent = m->outgoing && m->status < GSC_SENT;
    // (on IRC a message is only copied, or sent again: no reactions row either)
    bool irc = open_on_irc(a), plain = unsent || irc;
    if (m->reaction[0]) lines[take_back = nlines++] = "Remove my reaction";
    if (!plain) lines[reply = nlines++] = "Reply";
    if (m->text[0]) lines[copy = nlines++] = "Copy text";
    if (m->hidden[0]) lines[reveal = nlines++] = a->revealed == m->id ? "Cover the spoiler" : "Show the spoiler";
    if (m->status == GSC_NOT_SENT && m->outgoing) lines[resend = nlines++] = "Send again";
    if (m->status == GSC_HELD && m->outgoing) lines[resend = nlines++] = "Send anyway";
    if (m->changing == 2) lines[change_again = nlines++] = "Send the change again", lines[change_drop = nlines++] = "Drop the change";
    if (m->outgoing) {
        if (!plain) lines[edit = nlines++] = "Edit";
        if (unsent || !irc) lines[delete = nlines++] = unsent ? "Discard" : "Delete for everyone";
    }
    float cell = plain ? 0 : s->row + 4, width = plain ? 0 : cell * 6 + 8;
    for (int i = 0; i < nlines; i++) width = SDL_max(width, gs_ui_text_width(ui, lines[i], s->text_px) + 24);
    float height = (plain ? 4 : cell + 8) + nlines * s->row + 4;
    gs_rect menu = { anchor.x + anchor.w - width, anchor.y + anchor.h + 2, width, height };
    if (menu.y + menu.h > list.y + list.h) menu.y = SDL_max(list.y, anchor.y - 2 - height);
    bool chosen = gs_ui_key(ui, SDLK_ESCAPE, 0) || (gs_ui_clicked(ui, a->area) && !gs_ui_hovered(ui, menu));
    gs_ui_fill(ui, menu, s->panel), gs_ui_frame(ui, menu, s->border);
    gs_rect inner = gs_inset(menu, 4), top = gs_cut_top(&inner, cell);
    for (int i = 0; i < 6 && !plain; i++) {
        gs_rect at = gs_cut_left(&top, cell);
        bool mine = !strcmp(m->reaction, emoji[i]);
        if (mine || gs_ui_hovered(ui, at)) gs_ui_fill(ui, at, mine ? s->raised : s->border);
        gs_ui_text(ui, at, emoji[i], s->text_px * 1.3f, s->text, 0);
        // (the one already given is taken back by choosing it again)
        if (gs_ui_clicked(ui, at)) gsc_react(a->core, m->id, mine ? "" : emoji[i]), chosen = true;
    }
    if (!plain) gs_cut_top(&inner, 4);
    for (int i = 0; i < nlines; i++) {
        gs_rect at = gs_cut_top(&inner, s->row);
        if (gs_ui_hovered(ui, at)) gs_ui_fill(ui, at, s->border);
        gs_ui_text(ui, gs_inset(at, 8), lines[i], s->text_px, i == delete ? s->warning : s->text, -1);
        if (!gs_ui_clicked(ui, at)) continue;
        chosen = true;
        if (i == take_back) gsc_react(a->core, m->id, "");
        if (i == delete) gsc_delete(a->core, m->id);
        if (i == resend) gsc_resend(a->core, m->id);
        if (i == change_again || i == change_drop) gsc_changes_again(a->core, m->id, i == change_again);
        if (i == copy) SDL_SetClipboardText(body_of(a, m));
        if (i == reveal) a->revealed = a->revealed == m->id ? 0 : m->id, a->heights_width = 0;
        if (i == reply) {
            a->replying = m->id, a->editing = 0;
            one_line(a->answered, sizeof a->answered, "Replying to %s: %s", m->outgoing ? "me" : m->sender[0] ? m->sender : unnamed, m->text);
            gs_ui_focus(ui, "draft");
        }
        if (i == edit) {
            a->editing = m->id, a->replying = 0;
            snprintf(a->draft, sizeof a->draft, "%s", m->text);
            one_line(a->answered, sizeof a->answered, "Editing: %s%s", m->text, "");
            gs_ui_focus(ui, "draft");
        }
    }
    if (chosen) a->menu_open = false, wake(NULL);
}

// The open conversation: its messages, whole and scrolling, over a field that grows with what is typed.
static void message_view(app *a, gs_ui *ui, gs_rect r) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    // A picture that was clicked is shown whole, to zoom and move about, until it is closed.
    if (a->viewing) {
        gs_rect bar = gs_inset(gs_cut_top(&r, s->row + 16), 8);
        bool close = gs_ui_button(ui, gs_cut_right(&bar, 72), "Close") || gs_ui_key(ui, SDLK_ESCAPE, 0) || gs_ui_key(ui, SDLK_AC_BACK, 0);
        gs_cut_right(&bar, 8);
        if (gs_ui_button(ui, gs_cut_right(&bar, 72), "Save")) SDL_ShowSaveFileDialog(chose_save, (void *)(intptr_t)a->viewing, a->window, NULL, 0, a->viewing_name[0] ? a->viewing_name : NULL);
        gs_cut_right(&bar, 8);
        if (gs_ui_button(ui, gs_cut_right(&bar, 72), "Open")) open_file(a, a->viewing, a->viewing_name);
        // A video plays whole and fitted, as large as there is room for; closed, it stops.
        const held_file *film = NULL;
        for (int i = 0; i < a->nfiles && !film; i++)
            if (a->files[i].id == a->viewing && filmed(&a->files[i])) film = &a->files[i];
        if (film) {
            float fw = film->w > 0 ? film->w : 320, fh = film->h > 0 ? film->h : 180, fit = SDL_min(r.w / fw, r.h / fh);
            movie_frame(a, ui, film, (gs_rect){ r.x + (r.w - fw * fit) / 2, r.y + (r.h - fh * fit) / 2, fw * fit, fh * fit }, true);
            if (close) {
                for (int k = 0; k < MAX_MOVIES; k++)
                    if (a->movies[k].id == a->viewing && a->movies[k].movie) rest_movie(a, k);
                a->viewing = 0;
            }
            return;
        }
        char key[40], id[40], zoom[24];
        snprintf(key, sizeof key, "attachment-%lld", (long long)a->viewing);
        snprintf(id, sizeof id, "view-%lld", (long long)a->viewing);
        int w, h;
        // A GIF plays whole and fitted, not zoomed; a click on it or beside it closes it.
        gs_gif *moving = NULL;
        for (int i = 0; i < a->nfiles && !moving; i++)
            if (a->files[i].id == a->viewing && gif(&a->files[i])) moving = gif_of(a, a->viewing);
        if (moving) {
            gs_gif_size(moving, &w, &h);
            float fit = SDL_min(1, SDL_min(r.w / SDL_max(w, 1), r.h / SDL_max(h, 1)));
            gif_frame(a, ui, moving, (gs_rect){ r.x + (r.w - w * fit) / 2, r.y + (r.h - h * fit) / 2, w * fit, h * fit });
            close |= gs_ui_clicked(ui, r);
            snprintf(zoom, sizeof zoom, "%.0f%%", fit * 100);
            gs_ui_text(ui, bar, zoom, s->small_px, s->muted, -1);
            if (close) a->viewing = 0;
            return;
        }
        // (a click beside the picture closes it too)
        close |= gs_ui_image_view(ui, r, id, gs_images_full(a->images, key, &w, &h));
        snprintf(zoom, sizeof zoom, "%.0f%%", gs_ui_image_zoom(ui, id) * 100);
        gs_ui_text(ui, bar, zoom, s->small_px, s->muted, -1);
        if (close) a->viewing = 0;
        return;
    }
    // The conversation's name, and whether it may raise notifications.
    for (int i = 0; i < a->nconversations; i++) {
        const gsc_conversation *c = &a->conversations[i];
        if (c->id != a->open) continue;
        gs_rect bar = gs_cut_top(&r, s->row + 16);
        gs_ui_fill(ui, bar, s->panel);
        bar = gs_inset(bar, 8);
        // (alone in the window, it has a way back to the list)
        if (a->narrow && gs_ui_button(ui, gs_cut_left(&bar, 40), "<")) stop_typing(a), a->open = 0, gs_ui_focus(ui, NULL);
        if (a->narrow) gs_cut_left(&bar, 8);
        if (gs_ui_button(ui, gs_cut_right(&bar, 84), c->muted ? "Unmute" : "Mute")) gsc_mute(a->core, c->id, !c->muted);
        // The disappearing timer, for a conversation with one person: its button says what it is,
        // and opens the choices under it.
        // A group's members, listed under their button; one who administers the group may remove them.
        if (c->kind == GSC_GROUP) {
            gs_cut_right(&bar, 8);
            gs_rect button = gs_cut_right(&bar, 100);
            if (gs_ui_button(ui, button, "Members")) a->members_open = !a->members_open, a->removing = -1;
            a->members_button = button;
            float width = 320;
            // (with a line under them, for one who does not administer it, saying why they cannot be removed)
            a->members_panel = (gs_rect){ button.x + button.w - width, button.y + button.h + 4, width, (SDL_min(a->nmembers, 12) + !c->admin) * s->row + 8 };
        }
        if (c->kind == GSC_DIRECT && !open_on_irc(a)) {
            static const int choices[] = { 0, 300, 3600, 28800, 86400, 604800, 2419200 };
            static const char *const names[] = { "Off", "5 minutes", "1 hour", "8 hours", "1 day", "1 week", "4 weeks" };
            int now = gsc_timer(a->core, c->id), shown = -1;
            for (int k = 0; k < (int)SDL_arraysize(choices); k++)
                if (choices[k] == now) shown = k;
            char label[40];
            if (shown >= 0) snprintf(label, sizeof label, "Timer: %s", names[shown]);
            else snprintf(label, sizeof label, "Timer: %d s", now);
            gs_cut_right(&bar, 8);
            gs_rect button = gs_cut_right(&bar, 150);
            if (gs_ui_button(ui, button, label)) a->timer_open = !a->timer_open;
            if (a->timer_open) {
                gs_rect panel = { button.x, button.y + button.h + 4, button.w, SDL_arraysize(choices) * s->row + 8 };
                a->timer_panel = panel, a->timer_button = button;
                for (int k = 0; k < (int)SDL_arraysize(choices); k++) {
                    gs_rect at = { panel.x + 4, panel.y + 4 + k * s->row, panel.w - 8, s->row };
                    if (gs_ui_clicked(ui, at)) gsc_set_timer(a->core, c->id, choices[k]), a->timer_open = false;
                }
            }
        }
        gs_ui_text(ui, bar, c->title, s->text_px, s->text, -1);
        // Someone who wrote unasked is told nothing until accepted, and the user is told so.
        if (c->request) {
            gs_rect notice = gs_cut_top(&r, s->row + 12);
            gs_ui_fill(ui, notice, s->raised);
            notice = gs_inset(notice, 6);
            if (gs_ui_button(ui, gs_cut_right(&notice, 84), "Accept")) gsc_accept(a->core, c->id);
            gs_cut_right(&notice, 8);
            gs_ui_clip(ui, &notice);
            gs_ui_text_wrapped(ui, notice, "Not someone you have written to. Until you accept or reply, they are not told that you read this or are typing.", s->small_px, s->warning);
            gs_ui_clip(ui, NULL);
        }
    }
    bool files = !open_on_irc(a);  // (IRC carries text alone)
    float field_w = r.w - 16 - 72 - 8 - (files ? 72 + 8 : 0);
    gs_rect compose = gs_inset(gs_cut_bottom(&r, gs_ui_textarea_height(ui, a->draft, field_w, 6) + 16), 8);
    gs_rect button = gs_cut_right(&compose, 72);
    bool send = gs_ui_button_primary(ui, gs_cut_bottom(&button, s->row), "Send");
    gs_cut_right(&compose, 8);
    if (files) {
        gs_rect attach = gs_cut_left(&compose, 72);
        if (gs_ui_button(ui, gs_cut_bottom(&attach, s->row), "Attach")) SDL_ShowOpenFileDialog(chose_files, NULL, a->window, NULL, 0, NULL, true);
        gs_cut_left(&compose, 8);
    }
    // A conversation just opened, by any means, can be written in at once: from the frame after the
    // click that opened it, since in that one the field lets go of the keyboard for a press elsewhere.
    // (not where the keyboard is on the screen, a phone's: it would cover half the conversation unasked)
    if (a->to_draft && SDL_HasScreenKeyboardSupport()) a->to_draft = false;
    if (a->to_draft && (a->held || a->pressed)) wake(NULL);
    else if (a->to_draft) {
        // (with the caret after a draft kept from before, to go on with it)
        gs_ui_focus(ui, "draft"), gs_ui_textarea_select(ui, "draft", (int)strlen(a->draft), (int)strlen(a->draft));
        a->to_draft = false;
    }
    bool typed = gs_ui_textarea(ui, compose, "draft", a->draft, sizeof a->draft, "Message");
    send |= gs_ui_focused(ui, "draft") && (gs_ui_key(ui, SDLK_RETURN, 0) || gs_ui_key(ui, SDLK_KP_ENTER, 0));
    // The others are told while the draft keeps changing, and again when it is emptied; a message
    // sent ends it for them by itself.
    if (send && a->open && a->editing && a->draft[0]) {
        gsc_edit(a->core, a->editing, a->draft);
        a->draft[0] = 0, a->told_typing = 0, a->editing = 0;
    } else if (send && a->open && !a->editing && (a->draft[0] || a->nattach)) {
        const char *files[MAX_ATTACH];
        for (int i = 0; i < a->nattach; i++) files[i] = a->attach[i];
        // Each person named is found where their "@name" still stands in the text, one place each.
        gsc_mention mentions[MAX_NAMED];
        int nmentions = 0;
        for (int i = 0; i < a->nnamed; i++) {
            char at[100];
            int len = snprintf(at, sizeof at, "@%s", a->named[i].name);
            for (const char *found = strstr(a->draft, at); found; found = strstr(found + len, at)) {
                int start = (int)(found - a->draft);
                bool taken = false;
                for (int k = 0; k < nmentions; k++) taken |= start < mentions[k].start + mentions[k].length && mentions[k].start < start + len;
                if (!taken) { mentions[nmentions++] = (gsc_mention){ a->named[i].key, start, len }; break; }
            }
        }
        gsc_send(a->core, a->open, a->draft, a->replying, files, a->nattach, mentions, nmentions);
        a->draft[0] = 0, a->told_typing = 0, a->replying = 0, a->nnamed = 0;
        clear_attached(a);  // (the core has stored the files with the message)
        save_draft(a);  // (and there is no draft to bring back)
    } else if (!a->draft[0]) stop_typing(a);
    else if (a->open && typed && (!a->told_typing || SDL_GetTicks() - a->told_typing >= TYPING_RESEND_MS))
        gsc_typing(a->core, a->open, true), a->told_typing = SDL_max(SDL_GetTicks(), 1);

    // What the draft answers or replaces is named above it, until it is sent or given up.
    if (a->replying || a->editing) {
        gs_rect line = gs_inset(gs_cut_bottom(&r, s->row), 8);
        line.y -= 8, line.h += 16;
        bool cancel = gs_ui_button(ui, gs_cut_right(&line, 72), "Cancel") || gs_ui_key(ui, SDLK_ESCAPE, 0);
        gs_cut_right(&line, 8);
        gs_ui_clip(ui, &line), gs_ui_text(ui, line, a->answered, s->small_px, s->muted, -1), gs_ui_clip(ui, NULL);
        if (cancel) a->draft[a->editing ? 0 : strlen(a->draft)] = 0, a->replying = a->editing = 0;
    }
    // A draft that ends in "@" and the start of a member's name offers the members it could be; a
    // click on one writes the name out and has that person told they were mentioned.
    char *partial = strrchr(a->draft, '@');
    if (partial && a->nmembers && !a->editing && (partial == a->draft || partial[-1] == ' ' || partial[-1] == '\n') && !strchr(partial, '\n')) {
        gs_rect line = gs_inset(gs_cut_bottom(&r, s->row), 8);
        line.y -= 8, line.h += 16;
        size_t typed = strlen(partial + 1);
        for (int i = 0, shown = 0; i < a->nmembers && shown < 6; i++) {
            const char *name = a->members[i].name;
            if (!name[0] || SDL_strncasecmp(name, partial + 1, typed)) continue;
            char label[100];
            snprintf(label, sizeof label, "@%s", name);
            gs_rect button = gs_cut_left(&line, gs_ui_text_width(ui, label, s->text_px) + 24);
            gs_cut_left(&line, 8), shown++;
            if (!gs_ui_button(ui, button, label) || a->nnamed == MAX_NAMED) continue;
            snprintf(partial, sizeof a->draft - (partial - a->draft), "@%s ", name);
            a->named[a->nnamed] = a->members[i], a->nnamed++;
            gs_ui_focus(ui, "draft");
            // (an undo step of its own, with typing going on after the name)
            gs_ui_edited(ui, "draft", a->draft, (int)strlen(a->draft));
            break;
        }
    }
    attached_tiles(a, ui, &r);
    if (SDL_GetTicks() < a->typing_until) {
        char line[160];
        snprintf(line, sizeof line, "%s is typing", a->typing_name[0] ? a->typing_name : "Someone");
        gs_ui_text(ui, gs_inset(gs_cut_bottom(&r, s->row), 8), line, s->small_px, s->muted, -1);
    }
    r = gs_inset(r, 8);
    // (measured when the messages or the width change, not every frame: there may be thousands)
    if (a->heights_width != r.w) {
        a->counted = 0;  // (and the characters in view are counted again)
        for (int i = 0; i < a->nmessages; i++) a->heights[i] = message_height(a, ui, &a->messages[i], r.w) + turn_gap(a, i) + day_gap(a, ui, i) + unread_gap(a, ui, i);
        a->heights_width = r.w;
    }
    char id[40];  // (each conversation keeps its own place)
    snprintf(id, sizeof id, "messages-%lld", (long long)a->open);
    // A message gone to from a search or a reply is marked for a moment, long enough to be found by eye.
    if (a->show_found) a->found_until = SDL_GetTicks() + FOUND_MS;
    for (int i = 0; a->show_found && i < a->nmessages; i++)
        if (a->messages[i].id == a->found) gs_ui_flow_show(ui, id, i);
    for (int i = 0; a->show_unread && !a->show_found && i < a->nmessages; i++)
        if (a->messages[i].id == a->unread_from) gs_ui_flow_show(ui, id, i);
    a->show_found = a->show_unread = false;
    gs_ui_rows rows = gs_ui_flow(ui, r, id, a->nmessages, a->heights);
    gs_ui_select_group(ui, "bodies");  // (the messages' texts are selected across as one)
    gs_ui_clip(ui, &r);
    a->within = r;
    // (in a conversation with one person, a sender without a name is that person)
    const char *unnamed = "Unknown";
    for (int i = 0; i < a->nconversations; i++)
        if (a->conversations[i].id == a->open && a->conversations[i].kind == GSC_DIRECT) unnamed = a->conversations[i].title;
    float y = rows.y;
    gs_rect menu_at = { 0 };
    bool opened = false;
    gs_ui_disable(ui, a->menu_open || a->timer_open || a->members_open, false);  // (what is behind an open menu is drawn but takes no clicks)
    // The characters in view are counted line by line, as the text is wrapped here, when the list
    // has moved or been measured again, and not more than a few times a second while it scrolls.
    bool count = a->controls && (rows.y != a->counted_y || !a->counted) && SDL_GetTicks() - a->counted >= 150;
    int chars = 0;
    for (int i = rows.first; i < rows.end; y += a->heights[i++]) {
        const gsc_message *m = &a->messages[i];
        if (count && m->kind == GSC_TEXT && m->text[0]) {
            float scale = gs_ui_scale(ui), line = gs_ui_line_height(ui, s->text_px);
            gs_wrap *wrap = gs_fontset_wrap(a->fonts[a->font], s->text_px * scale, m->text, r.w * scale);
            // (the text is the last thing in its row but for a line of reactions)
            float top = y + a->heights[i] - gap_below(a, i) - reactions_height(ui, m, text_width(a, ui, r.w)) - (wrap ? wrap->count : 0) * line;
            for (int k = 0; wrap && k < wrap->count; k++) {
                float middle = top + (k + 0.5f) * line;
                if (middle < r.y || middle > r.y + r.h) continue;
                for (int b = wrap->lines[k].start; b < wrap->lines[k].end; b++) chars += ((unsigned char)m->text[b] & 0xC0) != 0x80;
            }
            gs_wrap_free(wrap);
        }
        gs_rect anchor = { 0 };
        // The day stands over the first message of each day, between two short rules, and where
        // the unread messages began, a line in the accent colour says so.
        float day = day_gap(a, ui, i), unread = unread_gap(a, ui, i);
        if (unread) {
            gs_rect line = { r.x, y + 4, r.w, unread - 14 };
            float w = gs_ui_text_width(ui, "New messages", s->small_px), mid = line.y + line.h / 2;
            gs_ui_fill(ui, (gs_rect){ r.x, mid, r.w - w - 12, 1 }, s->accent);
            gs_ui_text(ui, line, "New messages", s->small_px, s->accent, 1);
            y += unread;
        }
        if (a->messages[i].id == a->found && SDL_GetTicks() < a->found_until) {
            gs_ui_fill(ui, (gs_rect){ r.x - 4, y + day - 4, r.w + 8, a->heights[i] - day - unread - gap_below(a, i) + 8 }, s->raised);
            gs_ui_wake(ui, (int)(a->found_until - SDL_GetTicks()) + 1);  // (and drawn once more without it when the time is up)
        }
        if (day) {
            char said[48];
            day_text(a->messages[i].sent_ms, said, sizeof said);
            gs_rect line = { r.x, y + 4, r.w, day - 14 };
            float w = gs_ui_text_width(ui, said, s->small_px), mid = line.y + line.h / 2;
            gs_ui_text(ui, line, said, s->small_px, s->muted, 0);
            gs_ui_fill(ui, (gs_rect){ r.x + r.w / 2 - w / 2 - 48, mid, 36, 1 }, s->border), gs_ui_fill(ui, (gs_rect){ r.x + r.w / 2 + w / 2 + 12, mid, 36, 1 }, s->border);
        }
        float above = day + turn_gap(a, i), below = a->turns == TURNS_COLUMNS ? 4 : gap_below(a, i);
        row_asked asked = message_row(a, ui, &a->messages[i], (gs_rect){ r.x, y + above, r.w, a->heights[i] - unread - above - below }, unnamed, &anchor);
        y -= unread;
        if (asked == ROW_RESET) gsc_reset_session(a->core, a->open);
        if (asked == ROW_MENU && !a->menu_open) a->menu = a->messages[i].id, opened = true;
        if (a->messages[i].id == a->menu) menu_at = anchor;
    }
    gs_ui_select_group(ui, NULL);
    // A selection begun in a text ends the one before in messages not drawn, which gesso cannot
    // reach, and a press anywhere but on a message's text ends it everywhere.
    bool began = gs_ui_select_began(ui, "bodies");
    for (int k = a->npicked - 1; k >= 0 && (began || a->pressed); k--) {
        bool drawn = false;
        for (int i = rows.first; began && i < rows.end; i++) drawn |= a->messages[i].id == a->picked[k].id;
        if (!drawn) a->picked[k] = a->picked[--a->npicked];
    }
    gs_ui_clip(ui, NULL);
    gs_ui_list_end(ui);
    // (the count is shown in the status bar, which this frame has drawn already: another frame shows
    // it, and a view that moved too soon after the last count is counted when it has rested)
    if (count) a->shown_chars = chars, a->counted = SDL_max(SDL_GetTicks(), 1), a->counted_y = rows.y, wake(NULL);
    else if (a->controls && rows.y != a->counted_y) gs_ui_wake(ui, 150);
    gs_ui_disable(ui, false, false);
    // Who gave the reactions the pointer rests on, in a small panel beside them, where the line is
    // otherwise empty; many names wrap downward within the room there is.
    if (a->reacted && !a->menu_open) {
        float left = a->reacted_at.x + a->reacted_at.w + 10, room = SDL_max(120, r.x + r.w - left - 24);
        float width = SDL_min(gs_ui_text_width(ui, a->reacted, s->small_px), room), height = gs_ui_text_height(ui, a->reacted, s->small_px, width);
        gs_rect panel = { left, a->reacted_at.y, width + 16, height };  // (as tall as the line, so that it stands on no text above or below)
        if (panel.y + panel.h > r.y + r.h) panel.y = r.y + r.h - panel.h;
        gs_ui_fill(ui, panel, s->panel), gs_ui_frame(ui, panel, s->border);
        gs_ui_text_wrapped(ui, (gs_rect){ panel.x + 8, panel.y, width, height }, a->reacted, s->small_px, s->text);
    }
    a->reacted = NULL;
    message_menu(a, ui, menu_at, r, unnamed);
    // The timer's choices, over the messages; a click anywhere else or Escape puts them away.
    if (a->timer_open) {
        static const char *const names[] = { "Off", "5 minutes", "1 hour", "8 hours", "1 day", "1 week", "4 weeks" };
        gs_rect panel = a->timer_panel;
        gs_ui_fill(ui, panel, s->panel), gs_ui_frame(ui, panel, s->border);
        for (int k = 0; k < (int)SDL_arraysize(names); k++) {
            gs_rect at = { panel.x + 4, panel.y + 4 + k * s->row, panel.w - 8, s->row };
            if (gs_ui_hovered(ui, at)) gs_ui_fill(ui, at, s->border);
            gs_ui_text(ui, gs_inset(at, 8), names[k], s->text_px, s->text, -1);
        }
        if (gs_ui_key(ui, SDLK_ESCAPE, 0) || (gs_ui_clicked(ui, a->area) && !gs_ui_hovered(ui, panel) && !gs_ui_hovered(ui, a->timer_button))) a->timer_open = false;
    }
    // The members, over the messages; a click anywhere else or Escape puts them away. Removing one
    // is asked about first, since it is done for everyone in the group.
    if (a->members_open) {
        const gsc_conversation *c = NULL;
        for (int i = 0; i < a->nconversations; i++)
            if (a->conversations[i].id == a->open) c = &a->conversations[i];
        gs_rect panel = a->members_panel;
        gs_ui_fill(ui, panel, s->panel), gs_ui_frame(ui, panel, s->border);
        for (int k = 0; k < SDL_min(a->nmembers, 12); k++) {
            gs_rect at = { panel.x + 4, panel.y + 4 + k * s->row, panel.w - 8, s->row };
            if (c && c->admin) {
                gs_rect remove = gs_cut_right(&at, 76);
                remove.y += 3, remove.h -= 6;
                if (gs_ui_button(ui, remove, "Remove")) a->removing = k;
            }
            gs_ui_text(ui, gs_inset(at, 8), a->members[k].name[0] ? a->members[k].name : "Someone without a name", s->text_px, s->text, -1);
        }
        if (c && !c->admin) {
            gs_rect line = { panel.x + 4, panel.y + 4 + SDL_min(a->nmembers, 12) * s->row, panel.w - 8, s->row };
            gs_ui_text(ui, gs_inset(line, 8), "Only the group's admins can remove members.", s->small_px, s->muted, -1);
        }
        if (a->nmembers > 12) {
            char more[40];
            snprintf(more, sizeof more, "and %d more", a->nmembers - 12);
            gs_ui_text(ui, (gs_rect){ panel.x + 12, panel.y + panel.h, panel.w, s->row }, more, s->small_px, s->muted, -1);
        }
        if (a->removing < 0 && (gs_ui_key(ui, SDLK_ESCAPE, 0) || (gs_ui_clicked(ui, a->area) && !gs_ui_hovered(ui, panel) && !gs_ui_hovered(ui, a->members_button)))) a->members_open = false;
    }
    if (a->removing >= 0 && a->removing < a->nmembers) {
        char question[200];
        snprintf(question, sizeof question, "Remove %s from the group? Everyone in it is told.", a->members[a->removing].name[0] ? a->members[a->removing].name : "this person");
        int answer = gs_ui_confirm(ui, question, "Remove", "Keep", true);
        if (answer > 0) gsc_remove_member(a->core, a->open, a->members[a->removing].key), a->members_open = false;
        if (answer) a->removing = -1;
    }
    if (opened) a->menu_open = true, wake(NULL);  // (from the next frame, so that the click that opened it does not close it)
    // Near the oldest message held, older ones are read and put in front, and the list is told, so
    // that it keeps its place. Few at a time, since each is measured: a frame must not wait for it.
    if (rows.above < r.h && a->nmessages && a->nmessages < MAX_MESSAGES && !a->oldest_held) {
        static gsc_message older[MESSAGES_STEP];
        int n = gsc_messages(a->core, a->open, a->messages[0].id, older, SDL_min(MESSAGES_STEP, MAX_MESSAGES - a->nmessages));
        a->oldest_held = n < MESSAGES_STEP;
        memmove(a->messages + n, a->messages, a->nmessages * sizeof *a->messages);
        memmove(a->heights + n, a->heights, a->nmessages * sizeof *a->heights);
        memcpy(a->messages, older, n * sizeof *older);
        // (as many are asked for from now on as are held; never fewer than before, or a conversation
        // shorter than a first read would be cut down to what it had when it was opened)
        a->nmessages += n, a->wanted = SDL_max(a->wanted, a->nmessages);
        a->nfiles = 0, read_files(a, 0, a->nmessages);
        for (int i = 0; i <= n && i < a->nmessages; i++) a->heights[i] = message_height(a, ui, &a->messages[i], r.w) + turn_gap(a, i) + day_gap(a, ui, i) + unread_gap(a, ui, i);
        if (n) gs_ui_flow_prepended(ui, id, n);
    }
}

static void frame(app *a, gs_ui *ui) {
    const gs_ui_style *s = gs_ui_style_of(ui);
    a->frame++;
    gs_rect r = a->area = gs_ui_begin(ui);
    gs_ui_font(ui, a->fonts[a->font]);  // (a frame starts with the fonts the interface was made with)
    health(a, ui, &r);
    const gsc_account *waiting = NULL;
    for (int i = 0; i < a->naccounts; i++)
        if (a->accounts[i].state == GSC_ACCOUNT_SIGNING_IN && a->accounts[i].id == a->code_account) waiting = &a->accounts[i];
    if (!waiting) a->code_since = 0;  // (and with no code shown, nothing counts down)
    // While an account waits to be linked or receives messages, the app stays online behind other apps:
    // on a phone the code is scanned with Signal in front. (asked while it is in front, as Android wants)
    const char *keep = waiting ? "Waiting to be linked" : a->naccounts ? "Receiving messages" : NULL;
    if (keep != a->kept) {
        if (keep) a->kept = gs_background_keep("gink", keep) ? keep : NULL;
        else gs_background_release(), a->kept = NULL;
    }
    if (!a->naccounts) {
        gs_rect button = { r.x + (r.w - 220) / 2, r.y + (r.h - s->row) / 2, 220, s->row };
        if (gs_ui_button(ui, button, "Link a Signal account")) gsc_account_add(a->core, "signal", "gink");
    } else if (waiting) {
        sign_in(a, ui, r);
    } else if ((a->narrow = r.w < NARROW)) {
        // (the system's back gesture goes from a picture to its conversation, and from it to the list)
        // (back at the list, the message box lets go of the keyboard, and the screen's keyboard goes)
        if (a->open && !a->viewing && gs_ui_key(ui, SDLK_AC_BACK, 0)) stop_typing(a), a->open = 0, gs_ui_focus(ui, NULL);
#ifdef __ANDROID__
        else if (!a->open && gs_ui_key(ui, SDLK_AC_BACK, 0)) SDL_SendAndroidBackButton();
#endif
        if (a->open) message_view(a, ui, r);
        else conversation_list(a, ui, r);
    } else {
        // The edge between the list and the conversation is dragged to make the list wider or
        // narrower. Short of LIST_MIN it goes to its pictures alone, or stays at LIST_MIN,
        // whichever the pointer is nearer; and it catches at LIST_W, where it started.
        float icons = s->row * 1.6f, most = SDL_max(LIST_MIN, SDL_min(520, r.w - 320));
        gs_rect whole = r, edge = { r.x + (a->list_w == LIST_ICONS ? icons : SDL_min(a->list_w, most)) - 3, r.y, 7, r.h };
        bool over = gs_ui_hovered(ui, edge) && !a->menu_open && !a->viewing;
        if (over && a->pressed) a->sizing_list = true;
        if (!a->held) {
            if (a->sizing_list) apply_settings(a, ui, true);  // (kept for the next run once it is let go)
            a->sizing_list = false;
        }
        if (a->sizing_list) {
            float want = a->pointer_x - whole.x;
            a->list_w = want < (icons + LIST_MIN) / 2 ? LIST_ICONS : want < LIST_MIN ? LIST_MIN : SDL_fabsf(want - LIST_W) < 4 ? LIST_W : SDL_min(want, most);
            a->heights_width = 0;
        }
        // (the pointer's shape is set when it comes to the edge and when it leaves, and otherwise left to gesso, which gives it a hand over a link and a beam over text)
        if ((over || a->sizing_list) != a->at_edge) a->at_edge = over || a->sizing_list, SDL_SetCursor(a->at_edge ? a->across : a->arrow);
        gs_rect list = gs_cut_left(&r, a->list_w == LIST_ICONS ? icons : SDL_min(a->list_w, most));
        gs_ui_fill(ui, gs_cut_left(&r, 1), over || a->sizing_list ? s->accent : s->border);
        // (while the edge is dragged, nothing under the pointer takes it for a click or a selection)
        gs_ui_disable(ui, a->sizing_list, false);
        conversation_list(a, ui, list);
        message_view(a, ui, r);
        gs_ui_disable(ui, false, false);
    }
    rest_gifs(a);
    // A test build asks once, on its first run, before it looks for any update.
    if (a->update_asked) {
        char question[700];
        snprintf(question, sizeof question,
                 "This is a test build of gink (build %s). It can keep itself up to date: it looks for a new build every minute at updates.waka.mx, "
                 "puts it in place, and restarts when you have left it alone for two minutes, keeping what you were writing. Each look also sends a random "
                 "number for this installation, the build, and whether the window is hidden, open or in use; never your account, contacts or messages. "
                 "You can turn this off in the status bar. Keep it up to date?", gsc_build());
        int answer = gs_ui_confirm(ui, question, "Keep it up to date", "Not now", false);
        if (answer) a->update_asked = false, a->update_on = answer > 0, set_update_consent(a->update_path, a->update_on), gsc_update_enable(a->update_on);
    }
    a->pressed = false;
    gs_ui_end(ui);
}

// The user's answer about updating, from update.txt in the data folder: 1 yes, 0 no, -1 not asked.
static int update_consent(const char *path) {
    char *text = SDL_LoadFile(path, NULL);
    int said = !text ? -1 : strstr(text, "consent yes") ? 1 : strstr(text, "consent no") ? 0 : -1;
    SDL_free(text);
    return said;
}

// Keeps the user's answer in update.txt, with the lines that name builds not to take again.
static void set_update_consent(const char *path, bool yes) {
    char *text = SDL_LoadFile(path, NULL), out[4096];
    size_t n = (size_t)snprintf(out, sizeof out, "consent %s\n", yes ? "yes" : "no");
    for (char *line = text; line && *line && n < sizeof out; ) {
        char *end = strchr(line, '\n');
        size_t len = end ? (size_t)(end - line) : strlen(line);
        if (!strncmp(line, "skip ", 5)) n += (size_t)snprintf(out + n, sizeof out - n, "%.*s\n", (int)len, line);
        line += len + (end ? 1 : 0);
    }
    SDL_SaveFile(path, out, SDL_min(n, sizeof out));
    SDL_free(text);
}

#define UPDATED 75  // the exit code with which the program says it has been updated and wants to start again
#define UPDATED_HIDDEN 76  // the same, from a window that was hidden behind its tray icon

// Runs the program proper as a child of this one and starts it again when it ends by a fault: a
// crash would otherwise leave no window and no notifications, with nothing said. The log gets a
// line each time. Three faults each within a minute of starting end the watch, since a program
// that cannot stay up is not helped by being started for ever. Returns the child's exit code, or
// -1 when no child could be started and the caller should carry on by itself.
static int watch_over(int argc, char **argv, const char *dir) {
    const char *args[72];
    int n = 0, soon = 0;
    for (int i = 0; i < argc && n < 70; i++) args[n++] = argv[i];
    args[n++] = "--watched", args[n] = NULL;
    gsc_log_start(dir);
    for (;;) {
        Uint64 began = SDL_GetTicks();
        SDL_Process *child = SDL_CreateProcess(args, false);
        int code = 0;
        if (!child) return -1;
        SDL_WaitProcess(child, true, &code);
        SDL_DestroyProcess(child);
        // (a fault ends a Windows program with an error status, which reads as a negative code, and
        // elsewhere with one of the signals a fault raises; being told to stop is not a fault)
#ifdef _WIN32
        bool fault = code < 0;
#else
        bool fault = code == -4 || code == -5 || code == -6 || code == -7 || code == -8 || code == -11;
#endif
        // The program put a newer build in place of its files: a new first copy of the new build
        // takes over, and this one, still the old build, ends.
        // (it comes back as it was: hidden when it was hidden, and otherwise shown without taking
        // the keyboard from whatever the user is doing)
        if (code == UPDATED || code == UPDATED_HIDDEN) {
            const char *again[72];
            int n = 0;
            for (int i = 0; i < argc && n < 70; i++)
                if (strcmp(argv[i], "--hidden") && strcmp(argv[i], "--restarted")) again[n++] = argv[i];
            again[n++] = code == UPDATED_HIDDEN ? "--hidden" : "--restarted";
            again[n] = NULL;
            SDL_Process *next = SDL_CreateProcess(again, false);
            char said[300];
            snprintf(said, sizeof said, next ? "the new build was started" : "the new build could not be started: %s", SDL_GetError());
            gsc_log(said);
            return next ? 0 : 1;
        }
        if (!fault) return code;
        soon = SDL_GetTicks() - began < 60000 ? soon + 1 : 0;
        // A build put in place by an update that fails within a minute of starting is undone: its
        // files go back to those it replaced, kept aside as .old for that, it is named as one not to
        // take again, and the build before it starts in its place and says so.
        char folder[1024], old[1100];
        snprintf(folder, sizeof folder, "%s", SDL_GetBasePath() ? SDL_GetBasePath() : "");
        const char *self = SDL_strrchr(argv[0], '/') ? SDL_strrchr(argv[0], '/') + 1 : argv[0];
        if (SDL_strrchr(self, '\\')) self = SDL_strrchr(self, '\\') + 1;
        snprintf(old, sizeof old, "%s%s.old", folder, self);
        SDL_PathInfo info;
        if (soon && folder[0] && SDL_GetPathInfo(old, &info)) {
            int count = 0;
            char **names = SDL_GlobDirectory(folder, "*.old", 0, &count);
            for (int i = 0; names && i < count; i++) {
                char from[1100], to[1100], bad[1100];
                snprintf(from, sizeof from, "%s%s", folder, names[i]);
                snprintf(to, sizeof to, "%s%.*s", folder, (int)strlen(names[i]) - 4, names[i]);
                snprintf(bad, sizeof bad, "%s.bad", to);
                SDL_RemovePath(bad);
                SDL_RenamePath(to, bad), SDL_RenamePath(from, to);
            }
            SDL_free(names);
            char path[1100], skip[200];
            snprintf(path, sizeof path, "%supdate.txt", dir);
            char *text = SDL_LoadFile(path, NULL);
            int len = snprintf(skip, sizeof skip, "skip %s\n", gsc_build());
            SDL_IOStream *io = SDL_IOFromFile(path, "w");
            if (io) {
                if (text) SDL_WriteIO(io, text, strlen(text));
                SDL_WriteIO(io, skip, (size_t)len), SDL_CloseIO(io);
            }
            SDL_free(text);
            snprintf(path, sizeof path, "%sundone.txt", dir);
            SDL_SaveFile(path, gsc_build(), strlen(gsc_build()));
            const char *again[72];
            int m = 0;
            for (int i = 0; i < argc && m < 70; i++)
                if (strcmp(argv[i], "--restarted")) again[m++] = argv[i];
            again[m++] = "--restarted", again[m] = NULL;
            SDL_Process *next = SDL_CreateProcess(again, false);
            char said[200];
            snprintf(said, sizeof said, "build %s failed %d s after starting; it was undone, and the build before it %s", gsc_build(), (int)((SDL_GetTicks() - began) / 1000), next ? "was started" : "could not be started");
            gsc_log(said);
            return next ? 0 : 1;
        }
        char said[160];
        snprintf(said, sizeof said, "the window's program ended by a fault, code %d (0x%08X), after %d s; %s", code, (unsigned)code, (int)((SDL_GetTicks() - began) / 1000), soon >= 3 ? "that is three in a row soon after starting, so it is left stopped" : "starting it again");
        gsc_log(said);
        if (soon >= 3) return code;
        SDL_Delay(1000);
    }
}

#ifdef _WIN32
// Has Windows start gink when the user signs in, hidden behind its tray icon, or stops it doing so.
static void start_at_login(bool on) {
    HKEY run;
    if (RegOpenKeyExW(HKEY_CURRENT_USER, L"Software\\Microsoft\\Windows\\CurrentVersion\\Run", 0, KEY_SET_VALUE, &run)) return;
    WCHAR path[1024], command[1100];
    DWORD len = GetModuleFileNameW(NULL, path, 1024);
    if (on && len && len < 1024) {
        int chars = swprintf(command, 1100, L"\"%ls\" --hidden", path);
        if (chars > 0) RegSetValueExW(run, L"gink", 0, REG_SZ, (const BYTE *)command, (DWORD)(chars + 1) * sizeof(WCHAR));
    } else if (!on) RegDeleteValueW(run, L"gink");
    RegDeleteValueW(run, L"gsignal");  // (what it was started by when it was called gsignal)
    RegCloseKey(run);
}
#else
static void start_at_login(bool on) {}
#endif

// Whether a restart now would cut something off: a sound or a video playing, a draft not yet kept,
// or a message of the open conversation still being sent.
static bool busy(const app *a) {
    if ((a->sound && gs_sound_playing(a->sound)) || a->draft_changed) return true;
    for (int i = 0; i < MAX_MOVIES; i++)
        if (a->movies[i].movie && gs_movie_playing(a->movies[i].movie)) return true;
    for (int i = 0; i < a->nmessages; i++)
        if (a->messages[i].outgoing && a->messages[i].status == GSC_WAITING) return true;
    return false;
}

int main(int argc, char **argv) {
    static app a;
    const char *data = NULL, *shot = NULL, *open = NULL, *irc = NULL;
    double after = 5;
    int scroll_frames = 0, width = 900, height = 600;
    bool offscreen = false, watched = false, hidden = false;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--data") && i + 1 < argc) data = argv[++i];
        else if (!strcmp(argv[i], "--shot") && i + 1 < argc) shot = argv[++i];
        else if (!strcmp(argv[i], "--open") && i + 1 < argc) open = argv[++i];
        else if (!strcmp(argv[i], "--scroll-test") && i + 1 < argc) scroll_frames = SDL_atoi(argv[++i]);
        else if (!strcmp(argv[i], "--after") && i + 1 < argc) after = SDL_atof(argv[++i]);
        else if (!strcmp(argv[i], "--offscreen")) offscreen = true;
        else if (!strcmp(argv[i], "--add-irc") && i + 1 < argc) irc = argv[++i];
        else if (!strcmp(argv[i], "--watched")) watched = true;
        else if (!strcmp(argv[i], "--hidden")) hidden = true;
        else if (!strcmp(argv[i], "--restarted")) SDL_SetHint(SDL_HINT_WINDOW_ACTIVATE_WHEN_SHOWN, "0");
        else if (!strcmp(argv[i], "--size") && i + 2 < argc) width = SDL_atoi(argv[i + 1]), height = SDL_atoi(argv[i + 2]), i += 2;
        else return fprintf(stderr, "usage: gink [--data DIR] [--open TITLE] [--shot F.png [--after S]] [--scroll-test N] [--offscreen] [--size W H]\n"), 2;
    }
    if (offscreen) SDL_SetHint(SDL_HINT_VIDEO_DRIVER, "offscreen"), SDL_SetHint(SDL_HINT_RENDER_DRIVER, "software");
    // The click that brings the window forward also does what it was aimed at; SDL would otherwise
    // drop it, and the message box would take a second click.
    SDL_SetHint(SDL_HINT_MOUSE_FOCUS_CLICKTHROUGH, "1");
    // (Android's back gesture comes as a key, which the narrow layout answers; at the list it leaves as usual)
    SDL_SetHint(SDL_HINT_ANDROID_TRAP_BACK_BUTTON, "1");
    if (!SDL_Init(SDL_INIT_VIDEO)) return SDL_Log("SDL_Init: %s", SDL_GetError()), 1;

    char dir[1024], *error = NULL;
    uint8_t key[GSC_KEY_LEN];
    data_dir(data, dir, sizeof dir);
#ifdef _WIN32
    // An installed copy that was called gsignal moves to its new name: it copies itself to gink.exe
    // beside it and starts that in its place, which removes gsignal.exe once it has ended. (Copies
    // that update themselves start again as the program they were started as, gsignal.exe, so the
    // first build called gink arrives under both names.)
    {
        const char *base = SDL_GetBasePath();
        const char *self = SDL_strrchr(argv[0], '\\') ? SDL_strrchr(argv[0], '\\') + 1 : argv[0];
        char was[1100], now[1100];
        snprintf(was, sizeof was, "%sgsignal.exe", base ? base : "");
        snprintf(now, sizeof now, "%sgink.exe", base ? base : "");
        // (GINK_UPDATE_FROM lets a copy on data of its own try it, as it lets one try updating)
        if (base && (!data || SDL_getenv("GINK_UPDATE_FROM")) && !watched && !SDL_strcasecmp(self, "gsignal.exe") && SDL_CopyFile(was, now)) {
            const char *again[72];
            int n = 0;
            again[n++] = now;
            for (int i = 1; i < argc && n < 71; i++) again[n++] = argv[i];
            again[n] = NULL;
            SDL_Process *next = SDL_CreateProcess(again, false);
            if (next) return 0;
        }
        if (base && SDL_strcasecmp(self, "gsignal.exe")) {
            for (int i = 0; i < 50 && SDL_GetPathInfo(was, NULL); i++) SDL_RemovePath(was) ? (void)0 : SDL_Delay(100);
        }
    }
#endif
    // An ordinary run is watched over by a first copy of the program, which starts it again after
    // a fault. (GINK_UNWATCHED=1 runs it alone, for running it under a debugger.)
    if (!offscreen && !shot && !scroll_frames && !watched && !SDL_getenv("GINK_UNWATCHED")) {
        int code = watch_over(argc, argv, dir);
        if (code != -1) return code;
    }
    // One program per data folder: started again, it brings the first one's window forward. (A run that
    // only takes a picture or a timing is left to be given a folder nothing else has open.)
    bool plain = !offscreen && !shot && !scroll_frames;
    if (plain && !gs_instance_acquire(dir, started_again)) return gs_instance_forward(dir, argc, argv) ? 0 : 1;
    // (a test of undoing a failed update: a fault.txt in the data folder makes the watched window
    // fail at once, the one time, as a new build that does not work would)
    if (watched && SDL_getenv("GINK_UPDATE_FROM")) {
        char fault[1100];
        snprintf(fault, sizeof fault, "%sfault.txt", dir);
        SDL_PathInfo info;
        if (SDL_GetPathInfo(fault, &info) && SDL_RemovePath(fault)) SDL_TriggerBreakpoint();
    }
    if (!database_key(dir, key)) return SDL_Log("no database key could be made or kept in %s", dir), 1;
    if (!(a.core = gsc_open(dir, key, wake, NULL, &error))) return SDL_Log("%s", error), 1;
    // An IRC bouncer is added once, from --add-irc ADDRESS (see gsc_account_add).
    if (irc) {
        gsc_account known[MAX_ACCOUNTS];
        int n = SDL_min(gsc_accounts(a.core, known, MAX_ACCOUNTS), MAX_ACCOUNTS);
        bool has = false;
        for (int i = 0; i < n; i++) has |= !strcmp(known[i].protocol, "irc");
        if (!has) gsc_account_add(a.core, "irc", irc);
    }
    a.naccounts = SDL_min(gsc_accounts(a.core, a.accounts, MAX_ACCOUNTS), MAX_ACCOUNTS);
    a.wanted = MESSAGES_FIRST;
    read_conversations(&a);
    // It opens the conversation named, or else the one open when it last ended, as when it is
    // started again after an update; failing both, the list opens its first.
    int64_t last = 0;
    if (plain) {
        snprintf(a.open_path, sizeof a.open_path, "%sopen.txt", dir);
        char *kept = SDL_LoadFile(a.open_path, NULL);
        if (kept) last = SDL_strtoll(kept, NULL, 10), SDL_free(kept);
    }
    for (int i = 0; i < a.nconversations; i++)
        if (open ? !strcmp(a.conversations[i].title, open) : last && a.conversations[i].id == last) open_conversation(&a, i, MESSAGES_FIRST);

    // (a window that brings its own graphics context when offscreen: otherwise SDL gives a macOS
    // window OpenGL, which the offscreen driver there cannot load)
    // Videos are decoded by the graphics card where the system offers it, which has to be arranged
    // before the window is made and may ask for a renderer of its own; gesso falls back to software
    // when the card's frames cannot be shown. "hardware_video 0" in the settings file keeps to
    // software from the start, as does a run without a display.
    snprintf(a.settings_path, sizeof a.settings_path, "%ssettings.txt", dir);
    char *settings = SDL_LoadFile(a.settings_path, NULL);
    a.hardware_video = plain && !(settings && strstr(settings, "hardware_video 0"));
    const char *renderer = a.hardware_video ? gs_video_prepare(true) : NULL;
    SDL_PropertiesID p = SDL_CreateProperties();
    // (a copy that updates itself is a test build, and its title says so)
    char title[80];
    bool updating = plain && watched && (!data || SDL_getenv("GINK_UPDATE_FROM"));
    snprintf(title, sizeof title, updating ? "gink test build %s" : "gink", gsc_build());
    SDL_SetStringProperty(p, SDL_PROP_WINDOW_CREATE_TITLE_STRING, title);
    SDL_SetNumberProperty(p, SDL_PROP_WINDOW_CREATE_WIDTH_NUMBER, width);
    SDL_SetNumberProperty(p, SDL_PROP_WINDOW_CREATE_HEIGHT_NUMBER, height);
    // (made hidden, and shown once gesso has put it where it was, so it does not jump)
    SDL_SetNumberProperty(p, SDL_PROP_WINDOW_CREATE_FLAGS_NUMBER, SDL_WINDOW_RESIZABLE | SDL_WINDOW_HIGH_PIXEL_DENSITY | (offscreen ? 0 : SDL_WINDOW_HIDDEN));
    SDL_SetBooleanProperty(p, SDL_PROP_WINDOW_CREATE_EXTERNAL_GRAPHICS_CONTEXT_BOOLEAN, offscreen);
    // (on Windows the window's icon is the program's own, resource 1 from assets/icon/gink.rc)
    SDL_SetHint(SDL_HINT_WINDOWS_INTRESOURCE_ICON, "1");
    SDL_SetHint(SDL_HINT_WINDOWS_INTRESOURCE_ICON_SMALL, "1");
    SDL_Window *win = SDL_CreateWindowWithProperties(p);
    SDL_DestroyProperties(p);
    // gesso brings the window back where it was and as large, from a file beside the databases.
    char placed_path[1100] = "";
    if (plain) snprintf(placed_path, sizeof placed_path, "%swindow.txt", dir);
    if (win) SDL_SetStringProperty(SDL_GetWindowProperties(win), GS_UI_PLACE, placed_path);
    SDL_Renderer *ren = win ? SDL_CreateRenderer(win, renderer) : NULL;
    if (win && !ren && renderer) ren = SDL_CreateRenderer(win, NULL);  // (the one asked for is not to be had: the system's own)
    if (!ren) return SDL_Log("window: %s", SDL_GetError()), 1;
    if (plain) {  // (what draws, and what was asked for, written down for looking at from outside)
        char said[160];
        snprintf(a.renderer_path, sizeof a.renderer_path, "%srenderer.txt", dir);
        snprintf(a.pasted, sizeof a.pasted, "%spasted-", dir);
        // (a file opened in another program is a copy outside the encrypted database: such copies
        // are kept no longer than until the next start)
        snprintf(a.opened, sizeof a.opened, "%sopened/", dir);
        int nold = 0;
        char **old = SDL_GlobDirectory(a.opened, "*", 0, &nold);
        for (int i = 0; old && i < nold; i++) {
            char path[1400];
            snprintf(path, sizeof path, "%s%s", a.opened, old[i]);
            SDL_RemovePath(path);
        }
        SDL_free(old);
        SDL_CreateDirectory(a.opened);
        SDL_SaveFile(a.renderer_path, said, snprintf(said, sizeof said, "renderer %s\nasked for %s\n", SDL_GetRendererName(ren), renderer ? renderer : "the default"));
        snprintf(said, sizeof said, "window opened, renderer %s, %s", SDL_GetRendererName(ren), SDL_GetPlatform());
        gsc_log(said);
    }
    // Each frame waits for the display's refresh, so a frame drawn right after another replaces it on
    // screen; without this macOS can keep showing the earlier of two frames presented in one refresh.
    SDL_SetRenderVSync(ren, 1);
    a.window = win, a.renderer = ren;
    const char *size = settings ? strstr(settings, "font_size ") : NULL;
    a.removing = -1;
    a.square_pictures = !(settings && strstr(settings, "square_pictures 0"));  // (square unless round was chosen)
    const char *font = settings ? strstr(settings, "\nfont ") : NULL;
    // (a file from before there were three fonts says only whether the monospaced one, now the default, was chosen)
    a.font = font ? SDL_clamp(SDL_atoi(font + 6), 0, FONTS - 1) : settings && strstr(settings, "mono 0") ? FONT_SYSTEM : FONT_MONO;
    a.font_size = size ? SDL_atoi(size + 10) : FONT_SIZE;
    const char *extra = settings ? strstr(settings, "line_extra ") : NULL;
    a.line_extra = extra ? SDL_atoi(extra + 11) : 0;
    a.soft = !(settings && strstr(settings, "soft 0"));  // (soft unless sharp was chosen)
    a.controls = settings && strstr(settings, "controls 1");
    const char *turns = settings ? strstr(settings, "turns ") : NULL;
    a.turns = turns ? SDL_clamp(SDL_atoi(turns + 6), 0, TURNS_WAYS - 1) : TURNS_NAMES;
    a.start_at_login = !(settings && strstr(settings, "start_at_login 0"));  // (unless the user said not to)
    a.compact = settings && strstr(settings, "compact 1");
    a.report_use = !(settings && strstr(settings, "report_use 0"));
    gsc_report_use(a.report_use);
    const char *list_width = settings ? strstr(settings, "list_width ") : NULL;
    a.list_w = list_width ? SDL_atoi(list_width + 11) : LIST_W;  // (0 is its pictures alone)
    if (a.list_w != LIST_ICONS) a.list_w = SDL_max(a.list_w, LIST_MIN);
    a.across = SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_EW_RESIZE), a.arrow = SDL_CreateSystemCursor(SDL_SYSTEM_CURSOR_DEFAULT);
    SDL_free(settings);
    gs_jobs *jobs = gs_jobs_new(1);
    a.images = gs_images_new(ren, jobs, 64 << 20);
    chosen.lock = SDL_CreateMutex();
    // (a run that only takes a picture or a timing tells nobody)
    if (plain) gs_notify_init("gink", "com.wakamex.gink", NULL, notification_clicked);
    // With a tray icon, closing the window leaves the program receiving and notifying; the icon's
    // menu brings the window back or ends the program. Without one (a Linux desktop that lacks the
    // library for it), closing the window ends the program as before.
    SDL_Surface *icon = plain ? tray_icon(false) : NULL;
    SDL_Tray *tray = icon ? SDL_CreateTray(icon, "gink") : NULL;
    int tray_unread = 0;
    if (tray) {
        SDL_TrayMenu *menu = SDL_CreateTrayMenu(tray);
        SDL_SetTrayEntryCallback(SDL_InsertTrayEntryAt(menu, -1, "Show gink", SDL_TRAYENTRY_BUTTON), tray_show, NULL);
        SDL_SetTrayEntryCallback(SDL_InsertTrayEntryAt(menu, -1, "Quit", SDL_TRAYENTRY_BUTTON), tray_quit, NULL);
        SDL_SetHint(SDL_HINT_QUIT_ON_LAST_WINDOW_CLOSE, "0");
    }
    SDL_DestroySurface(icon);
    // (the user's own profile, not one named for a trial, decides whether it starts at sign-in)
    if (plain && !data) start_at_login(a.start_at_login);
    // A copy installed as the user's own, watched over and on its own data, keeps itself up to date.
    // (GINK_UPDATE_FROM names another server, for trying it out on any data)
    // It looks only once the user has agreed, which it asks on the first run; the status bar turns
    // it on and off. After an update, or after a failed one was undone, it says so once.
    if (updating && SDL_GetBasePath()) {
        a.updating = true;
        snprintf(a.update_path, sizeof a.update_path, "%supdate.txt", dir);
        int consent = update_consent(a.update_path);
        a.update_on = consent == 1, a.update_asked = consent < 0;
        gsc_update_enable(a.update_on);
        gsc_update_start(a.core, SDL_GetBasePath());
        char path[1100];
        snprintf(path, sizeof path, "%supdated.txt", dir);
        char *built = SDL_LoadFile(path, NULL);
        if (built) snprintf(a.notice, sizeof a.notice, "Updated to build %s", built);
        SDL_free(built), SDL_RemovePath(path);
        snprintf(path, sizeof path, "%sundone.txt", dir);
        char *undone = SDL_LoadFile(path, NULL);
        if (undone) snprintf(a.notice, sizeof a.notice, "Build %s failed to start and was undone", undone);
        SDL_free(undone), SDL_RemovePath(path);
    }
    // Each choice is the system's set of fonts behind the chosen font's file, with the emoji font
    // shipped beside the program first of all but on macOS: Windows' own has no glyphs for flags or
    // families, and a Linux system may have none it can draw, while macOS's own has them all.
    for (int i = 0; i < FONTS; i++) {
        char shipped[1100];
        a.fonts[i] = gs_fontset_system();
        if (font_files[i]) snprintf(shipped, sizeof shipped, "%s%s", SDL_GetBasePath(), font_files[i]), gs_fontset_prepend(a.fonts[i], shipped);
#ifndef __APPLE__
        snprintf(shipped, sizeof shipped, "%sClankerTwemoji.ttf", SDL_GetBasePath());
        gs_fontset_prepend(a.fonts[i], shipped);
#endif
    }
    for (int mono = 0; mono < 2; mono++) {
        a.styled[mono][0] = mono ? gs_fontset_system_mono(700) : gs_fontset_system_weight(700);
        a.styled[mono][1] = gs_fontset_system_italic(400, mono);
        a.styled[mono][2] = gs_fontset_system_italic(700, mono);
#ifndef __APPLE__
        char shipped[1100];
        snprintf(shipped, sizeof shipped, "%sClankerTwemoji.ttf", SDL_GetBasePath());
        for (int k = 0; k < 3; k++) gs_fontset_prepend(a.styled[mono][k], shipped);
#endif
    }
    gs_ui *ui = gs_ui_new(win, ren, a.fonts[FONT_SYSTEM]);
    // Started at sign-in, it waits behind its tray icon until it is wanted.
    // (without a tray icon to bring it back from, a window started hidden is shown after all)
    if (!offscreen && (!hidden || !tray)) SDL_ShowWindow(win);
    a.sharp = *gs_ui_style_of(ui);
    apply_settings(&a, ui, false);

    // Drawn when something happens and otherwise asleep: the core wakes it with a user event, and
    // gesso asks for the frame after one that drew text it could not show yet.
    int code = 0, scrolled = 0, slow = 0;
    double scroll_total = 0, scroll_worst = 0;
    Uint64 started = SDL_GetTicks();
    for (bool running = true, first = true; running; first = false) {
        SDL_Event e;
        int step = 0;  // (conversations to move up or down the list, asked for with the keyboard)
        int wait = first ? 0 : gs_ui_wait_ms(ui);
        Uint64 now = SDL_GetTicks();
        if (scroll_frames && now - started >= 2000) {  // (after two seconds for the window to settle)
            int w, h;
            SDL_GetWindowSize(win, &w, &h);
            SDL_Event wheel = { .wheel = { .type = SDL_EVENT_MOUSE_WHEEL, .windowID = SDL_GetWindowID(win), .y = 1, .integer_y = 1, .mouse_x = w * 0.6f, .mouse_y = h * 0.5f } };
            SDL_Event move = { .motion = { .type = SDL_EVENT_MOUSE_MOTION, .windowID = wheel.wheel.windowID, .x = wheel.wheel.mouse_x, .y = wheel.wheel.mouse_y } };
            SDL_PushEvent(&move), SDL_PushEvent(&wheel);
            wait = 0;
        }
        if (a.typing_until > now) {  // (the typing notice is taken down when its time runs out)
            int left = (int)(a.typing_until - now) + 1;
            wait = wait < 0 || left < wait ? left : wait;
        }
        // What is being written is kept once it has rested most of a second, so that a restart loses a word at most.
        Uint32 mark = draft_mark(&a);
        if (mark != a.draft_seen) a.draft_seen = mark, a.draft_changed = SDL_max(SDL_GetTicks(), 1);
        if (a.draft_changed && SDL_GetTicks() - a.draft_changed >= 700) save_draft(&a), a.draft_changed = 0;
        if (a.draft_changed) wait = wait < 0 || wait > 700 ? 700 : wait;
        if (a.updated) wait = wait < 0 || wait > 10000 ? 10000 : wait;  // (to see whether the user has left it be)
        if (a.code_since) wait = wait < 0 || wait > 1000 ? 1000 : wait;  // (the code's time left counts down)
        if (shot) {  // (a shot wakes on its own when its time comes)
            int left = (int)SDL_max(0, (Sint64)(after * 1000) - (Sint64)(now - started));
            wait = wait < 0 || left < wait ? left : wait;
        }
        if (wait < 0 ? SDL_WaitEvent(&e) : SDL_WaitEventTimeout(&e, wait)) {
            do {
                if (e.type == SDL_EVENT_QUIT) running = false;
                else if (e.type == SDL_EVENT_WINDOW_CLOSE_REQUESTED && tray) SDL_HideWindow(win), a.focused = false;
                else if (e.type == SDL_EVENT_WINDOW_CLOSE_REQUESTED) running = false;
                else if (e.type == SDL_EVENT_WINDOW_FOCUS_GAINED) a.focused = true, a.open ? gsc_mark_read(a.core, a.open) : (void)0;
                else if (e.type == SDL_EVENT_WINDOW_FOCUS_LOST) a.focused = false;
                else if (e.type == SDL_EVENT_MOUSE_MOTION || e.type == SDL_EVENT_MOUSE_BUTTON_DOWN) a.pointer_x = e.type == SDL_EVENT_MOUSE_MOTION ? e.motion.x : e.button.x;
                if (e.type == SDL_EVENT_MOUSE_BUTTON_DOWN && e.button.button == SDL_BUTTON_LEFT) a.pressed = a.held = true;
                if (e.type == SDL_EVENT_KEY_DOWN || e.type == SDL_EVENT_MOUSE_BUTTON_DOWN || e.type == SDL_EVENT_MOUSE_WHEEL) a.input = true, a.last_input = SDL_GetTicks();
                if (e.type == SDL_EVENT_MOUSE_BUTTON_UP && e.button.button == SDL_BUTTON_LEFT) a.held = false;
                else if (e.type == SDL_EVENT_DROP_FILE && a.nattach < MAX_ATTACH) snprintf(a.attach[a.nattach++], sizeof a.attach[0], "%s", e.drop.data);
                // Alt with Up or Down goes to the conversation above or below in the list, wherever
                // the keyboard is; the interface is not shown the key, which a text field would take
                // for moving its caret.
                // Copy takes what is selected in the messages, when something is; otherwise it is the
                // text field's. Several messages' parts go in order, each under who wrote it and when.
                // Ctrl+F goes to the search, with the list widened to show it if it held pictures alone.
                if (e.type == SDL_EVENT_KEY_DOWN && e.key.key == SDLK_F && (e.key.mod & (SDL_KMOD_CTRL | SDL_KMOD_GUI)) && !(e.key.mod & SDL_KMOD_ALT)) {
                    if (a.list_w == LIST_ICONS) a.list_w = LIST_W, a.heights_width = 0;
                    a.viewing = 0, a.to_search = true;
                    continue;
                }
                if (e.type == SDL_EVENT_KEY_DOWN && e.key.key == SDLK_C && (e.key.mod & (SDL_KMOD_CTRL | SDL_KMOD_GUI)) && a.npicked) {
                    size_t size = 1, used = 0;
                    for (int i = 0; i < a.nmessages; i++) size += strlen(body_of(&a, &a.messages[i])) + 200;
                    char *copied = SDL_malloc(size);
                    for (int i = 0; copied && i < a.nmessages; i++) {
                        int from, to;
                        selection_of(&a, a.messages[i].id, &from, &to);
                        if (from == to) continue;
                        const char *body = body_of(&a, &a.messages[i]);
                        int length = (int)strlen(body), lo = SDL_min(SDL_min(from, to), length), hi = SDL_min(SDL_max(from, to), length);
                        char when[32];
                        clock_text(a.messages[i].sent_ms, when, sizeof when);
                        if (a.npicked > 1) used += SDL_snprintf(copied + used, size - used, "%s%s, %s\n", used ? "\n\n" : "", sender_of(&a, &a.messages[i]), when);
                        used += SDL_snprintf(copied + used, size - used, "%.*s", hi - lo, body + lo);
                    }
                    if (copied) SDL_SetClipboardText(copied), SDL_free(copied);
                    continue;
                }
                // A paste of files or of a picture attaches them to the draft; a paste of text is the field's.
                if (e.type == SDL_EVENT_KEY_DOWN && e.key.key == SDLK_V && (e.key.mod & (SDL_KMOD_CTRL | SDL_KMOD_GUI)) && !(e.key.mod & SDL_KMOD_ALT) && a.open && !a.viewing && paste_files(&a)) continue;
                if (e.type == SDL_EVENT_KEY_DOWN && (e.key.mod & SDL_KMOD_ALT) && (e.key.key == SDLK_UP || e.key.key == SDLK_DOWN)) {
                    step += e.key.key == SDLK_UP ? -1 : 1;
                    continue;
                }
                gs_ui_event(ui, &e);
            } while (SDL_PollEvent(&e));
        }
        if (!running) break;
        // (saved once it has rested half a second, not at every step of a drag)
        Uint64 frame_start = SDL_GetPerformanceCounter();
        SDL_LockMutex(chosen.lock);
        for (int i = 0; i < chosen.nattach && a.nattach < MAX_ATTACH; i++) memcpy(a.attach[a.nattach++], chosen.attach[i], sizeof a.attach[0]);
        if (chosen.save) gsc_attachment_save(a.core, chosen.save, chosen.save_to);
        chosen.nattach = 0, chosen.save = 0;
        SDL_UnlockMutex(chosen.lock);
        drain_events(&a);
        // A click on a notification brings the window forward on that conversation.
        for (int64_t wanted = SDL_SetAtomicInt(&notified, 0), i = 0; wanted && i < a.nconversations; i++)
            if (a.conversations[i].id == wanted) open_conversation(&a, (int)i, MESSAGES_FIRST), SDL_SetAtomicInt(&wants_window, 1);
        if (SDL_SetAtomicInt(&wants_quit, 0)) break;
        // A new build in place starts when the user asks, or else once they have left the program
        // alone two minutes and it is busy with nothing, and this run has lasted a quarter hour.
        if (a.updated && (a.restart_now || (SDL_GetTicks() - a.last_input >= 120000 && SDL_GetTicks() >= 15 * 60000 && !busy(&a)))) {
            code = (SDL_GetWindowFlags(win) & SDL_WINDOW_HIDDEN) ? UPDATED_HIDDEN : UPDATED;
            break;
        }
        // (the window's state, for the use reported with each look for updates)
        SDL_WindowFlags flags = SDL_GetWindowFlags(win);
        gsc_window_state((flags & (SDL_WINDOW_HIDDEN | SDL_WINDOW_MINIMIZED)) ? 0 : a.focused ? 2 : 1, a.input);
        a.input = false;
        if (step && a.nlisted && !a.query[0]) {
            int row = SDL_clamp((a.open ? a.selected : -1) + step, 0, a.nlisted - 1);
            open_conversation(&a, a.listed[row], MESSAGES_FIRST);
        }
        if (SDL_SetAtomicInt(&wants_window, 0)) SDL_ShowWindow(win), SDL_RestoreWindow(win), gs_window_activate(win);
        // The tray icon says whether anything is unread, and how much.
        int unread = 0;
        for (int i = 0; i < a.nconversations; i++) unread += a.conversations[i].muted ? 0 : a.conversations[i].unread;
        if (tray && unread != tray_unread) {
            char tip[64];
            snprintf(tip, sizeof tip, unread ? "gink: %d unread" : "gink", unread);
            SDL_SetTrayTooltip(tray, tip);
            if (!unread != !tray_unread && (icon = tray_icon(unread > 0))) SDL_SetTrayIcon(tray, icon), SDL_DestroySurface(icon);
            tray_unread = unread;
        }
        const gs_ui_style *s = gs_ui_style_of(ui);
        SDL_SetRenderDrawColorFloat(ren, s->background.r, s->background.g, s->background.b, 1);
        SDL_RenderClear(ren);
        frame(&a, ui);
        if (gs_images_end_frame(a.images)) wake(NULL);  // (a picture finished loading: draw it)
        // (a shot is of a frame that shows all its text)
        if (shot && SDL_GetTicks() - started >= (Uint64)(after * 1000) && gs_ui_wait_ms(ui) != 0) code = gs_draw_save_png(ren, shot) ? 0 : 1, running = false;
        if (scroll_frames && now - started >= 2000) {
            // (the time to draw the frame, up to handing it to the display, which then waits for its refresh)
            SDL_FlushRenderer(ren);
            double ms = (SDL_GetPerformanceCounter() - frame_start) * 1000.0 / SDL_GetPerformanceFrequency();
            scroll_total += ms, scroll_worst = SDL_max(scroll_worst, ms), slow += ms > 1000.0 / 60;
            if (++scrolled == scroll_frames) {
                printf("scrolled %d frames over %d messages: mean %.2f ms, worst %.2f ms, %d over 16.7 ms\n", scrolled, a.nmessages, scroll_total / scrolled, scroll_worst, slow);
                running = false;
            }
        }
        SDL_RenderPresent(ren);
    }

    // Notifications go with the program: a click on one left behind would reach nobody.
    for (int i = 0; i < a.nconversations; i++) {
        char tag[24];
        snprintf(tag, sizeof tag, "%lld", (long long)a.conversations[i].id);
        gs_notify_close(tag);
    }
    save_draft(&a);
    stop_sound(&a);
    close_movies(&a);
    if (a.mixing) gs_mix_close();
    if (tray) SDL_DestroyTray(tray);
    gs_ui_free(ui);
    for (int i = 0; i < FONTS; i++) gs_fontset_free(a.fonts[i]);
    for (int i = 0; i < 6; i++) gs_fontset_free(a.styled[i / 3][i % 3]);
    gs_images_free(a.images), gs_jobs_free(jobs);
    SDL_DestroyRenderer(ren), SDL_DestroyWindow(win);
    gsc_close(a.core);
    SDL_Quit();
    return code;
}
