// gink-core: the core driven from a terminal, for development and tests. It opens the core on a
// data folder, adds a Signal account if there is none, and prints each event as one line.
//   gink-core --data DIR [--seconds N] [--send TITLE TEXT] [--typing TITLE] [--reset TITLE] [--read TITLE]
// --send sends TEXT to the conversation titled TITLE once the account is online, --typing says the
// user is typing there, --reset starts a new session with the person it is with, --read marks that conversation read whenever a message arrives in it, and
// --seconds ends the run after N seconds (default: until interrupted). Each line on standard input
// is one more thing to do, its parts separated by " | "; MESSAGE is the whole text of one of the
// conversation's newest 200 messages:
//   send TITLE | TEXT
//   reply TITLE | MESSAGE | TEXT
//   react TITLE | MESSAGE | EMOJI        (no EMOJI takes the reaction away)
//   edit TITLE | MESSAGE | TEXT
//   delete TITLE | MESSAGE              (one not yet sent is removed)
//   resend TITLE | MESSAGE              (tries one that was not sent again, and sends one held for a changed safety number)
//   mention TITLE | NAME | TEXT          (sends "@NAME TEXT" to a group, naming its member NAME)
//   file TITLE | PATH | TEXT             (sends the file, with TEXT if any)
//   files TITLE | PATH,PATH... | TEXT    (sends the files as one would attach them together)
//   save TITLE | MESSAGE | PATH          (writes the message's first attachment to PATH)
//   accept TITLE                         (accepts a conversation held apart as a request)
//   timer TITLE | SECONDS                (sets a conversation's disappearing timer; 0 turns it off)
//   remove TITLE | KEY                   (removes a member, by key, from a group the account administers)
//   picture TITLE | PATH                 (writes the conversation's picture to PATH, and prints "picture TITLE | BYTES", 0 for none)
// Lines printed:
//   account ID STATE LABEL undecryptable=N error=...
//   sign-in-code LINK
//   conversations N
//   listed group|direct|request CONVERSATION     (one per conversation, at the start and after each of the above)
//   message in|out CONVERSATION | SENDER | TEXT
//   reply CONVERSATION | WHOSE | QUOTED  (after the line of a message that is a reply; WHOSE is empty for the account's own)
//   typing CONVERSATION | SENDER | started|stopped
//   attachment CONVERSATION | NAME | TYPE | BYTES | WIDTHxHEIGHT | waiting|here|failed   (one per file of the message a line above or below is about)
//   saved PATH | not saved PATH
//   conversation CONVERSATION unread=N messages=N newest=held|waiting|not-sent|sent|delivered|read|received timer=yes|no
//   newest CONVERSATION | TEXT | text|deleted|notice edited=yes|no mine=EMOJI reactions=EMOJI... who=EMOJI NAME, NAME... change=none|waiting|given-up   (after each of the above)
#include <SDL3/SDL.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "gsignal_core.h"
#include "key.h"

static SDL_Semaphore *woken;
static void wake(void *user) { SDL_SignalSemaphore(woken); }

static const char *const states[] = { "signing-in", "connecting", "catching-up", "online", "offline" };

static gsc_conversation conversations[1024];
static int nconversations;

static const gsc_conversation *conversation_by_id(int64_t id) {
    for (int i = 0; i < nconversations; i++)
        if (conversations[i].id == id) return &conversations[i];
    return NULL;
}

// Lines read from standard input by a thread of their own, for the main loop to take.
static char said[16][4096];
static int nsaid;
static SDL_Mutex *said_lock;

static int listen(void *user) {
    char line[4096];
    while (fgets(line, sizeof line, stdin)) {
        line[strcspn(line, "\r\n")] = 0;
        SDL_LockMutex(said_lock);
        if (nsaid < 16) strcpy(said[nsaid++], line);
        SDL_UnlockMutex(said_lock);
        SDL_SignalSemaphore(woken);
    }
    return 0;
}

static void print_attachments(gsc_core *core, const char *title, int64_t message) {
    static const char *const states[] = { "waiting", "here", "failed" };
    gsc_attachment files[8];
    int n = gsc_attachments(core, message, files, 8);
    for (int i = 0; i < n && i < 8; i++)
        printf("attachment %s | %s | %s | %lld | %dx%d | %s\n", title, files[i].name, files[i].type, (long long)files[i].size, files[i].width, files[i].height, states[files[i].state]);
}

// Does what one line of standard input asks.
static void obey(gsc_core *core, char *line) {
    static gsc_message recent[200];
    char *part[4] = { 0 };
    int nparts = 0;
    char *verb = line, *rest = strchr(line, ' ');
    if (!rest) return;
    *rest++ = 0;
    for (char *at = rest; at && nparts < 4; ) {
        part[nparts++] = at;
        if ((at = strstr(at, " | "))) *at = 0, at += 3;
    }
    nconversations = gsc_conversations(core, conversations, 1024);
    const gsc_conversation *c = NULL;
    for (int i = 0; i < nconversations && i < 1024 && !c; i++)
        if (!strcmp(conversations[i].title, part[0])) c = &conversations[i];
    if (!c) { printf("no conversation is titled %s\n", part[0]); return; }
    if (!strcmp(verb, "accept")) { gsc_accept(core, c->id); return; }
    if (!strcmp(verb, "timer") && nparts == 2) { gsc_set_timer(core, c->id, SDL_atoi(part[1])); return; }
    if (!strcmp(verb, "remove") && nparts == 2) { gsc_remove_member(core, c->id, part[1]); return; }
    if (!strcmp(verb, "picture") && nparts == 2) {
        size_t len = 0;
        const void *bytes = gsc_avatar(core, c->id, &len);
        FILE *f = bytes ? fopen(part[1], "wb") : NULL;
        if (f) fwrite(bytes, 1, len, f), fclose(f);
        printf("picture %s | %zu\n", part[0], f ? len : 0);
        return;
    }
    if (!strcmp(verb, "send") && nparts == 2) { gsc_send(core, c->id, part[1], 0, NULL, 0, NULL, 0); return; }
    if (!strcmp(verb, "mention") && nparts == 3) {
        static gsc_member members[256];
        char text[4096];
        int n = SDL_min(gsc_members(core, c->id, members, 256), 256), at = snprintf(text, sizeof text, "@%s", part[1]);
        snprintf(text + at, sizeof text - at, " %s", part[2]);
        for (int i = 0; i < n; i++)
            if (!strcmp(members[i].name, part[1])) { gsc_send(core, c->id, text, 0, NULL, 0, &(gsc_mention){ members[i].key, 0, at }, 1); return; }
        printf("no member there is named %s\n", part[1]);
        return;
    }
    if (!strcmp(verb, "files") && nparts >= 2) {
        const char *paths[32];
        int n = 0;
        for (char *at = part[1], *comma; at && n < 32; at = comma ? comma + 1 : NULL) {
            if ((comma = strchr(at, ','))) *comma = 0;
            paths[n++] = at;
        }
        gsc_send(core, c->id, nparts == 3 ? part[2] : "", 0, paths, n, NULL, 0);
        return;
    }
    if (!strcmp(verb, "file") && nparts >= 2) { gsc_send(core, c->id, nparts == 3 ? part[2] : "", 0, (const char *const *)&part[1], 1, NULL, 0); return; }
    int64_t message = 0;
    int n = nparts >= 2 ? gsc_messages(core, c->id, 0, recent, 200) : 0;
    for (int i = n - 1; i >= 0 && !message; i--)
        if (!strcmp(recent[i].text, part[1])) message = recent[i].id;
    if (!message) printf("no message there reads %s\n", nparts >= 2 ? part[1] : "");
    else if (!strcmp(verb, "reply") && nparts == 3) gsc_send(core, c->id, part[2], message, NULL, 0, NULL, 0);
    else if (!strcmp(verb, "react")) gsc_react(core, message, nparts == 3 ? part[2] : "");
    else if (!strcmp(verb, "edit") && nparts == 3) gsc_edit(core, message, part[2]);
    else if (!strcmp(verb, "delete")) gsc_delete(core, message);
    else if (!strcmp(verb, "resend")) gsc_resend(core, message);
    else if (!strcmp(verb, "save") && nparts == 3) {
        gsc_attachment first;
        bool saved = gsc_attachments(core, message, &first, 1) > 0 && gsc_attachment_save(core, first.id, part[2]);
        printf("%s %s\n", saved ? "saved" : "not saved", part[2]);
    }
}

int main(int argc, char **argv) {
    const char *data = NULL, *to = NULL, *text = NULL, *read = NULL, *typing = NULL, *reset = NULL;
    double seconds = -1;
    for (int i = 1; i < argc; i++) {
        if (!strcmp(argv[i], "--data") && i + 1 < argc) data = argv[++i];
        else if (!strcmp(argv[i], "--seconds") && i + 1 < argc) seconds = SDL_atof(argv[++i]);
        else if (!strcmp(argv[i], "--send") && i + 2 < argc) to = argv[i + 1], text = argv[i + 2], i += 2;
        else if (!strcmp(argv[i], "--read") && i + 1 < argc) read = argv[++i];
        else if (!strcmp(argv[i], "--typing") && i + 1 < argc) typing = argv[++i];
        else if (!strcmp(argv[i], "--reset") && i + 1 < argc) reset = argv[++i];
        else data = NULL, i = argc;
    }
    if (!data) return fprintf(stderr, "usage: gink-core --data DIR [--seconds N] [--send TITLE TEXT] [--typing TITLE] [--reset TITLE] [--read TITLE]\n"), 2;

    char dir[1024], *error = NULL;
    uint8_t key[GSC_KEY_LEN];
    data_dir(data, dir, sizeof dir);
    if (!database_key(dir, key)) return fprintf(stderr, "no database key could be made or kept in %s\n", dir), 1;
    woken = SDL_CreateSemaphore(0);
    said_lock = SDL_CreateMutex();
    gsc_core *core = gsc_open(dir, key, wake, NULL, &error);
    if (!core) return fprintf(stderr, "%s\n", error), 1;
    gsc_account accounts[8];
    if (gsc_accounts(core, accounts, 8) == 0) gsc_account_add(core, "signal", "gink");

    nconversations = gsc_conversations(core, conversations, 1024);
    for (int c = 0; c < nconversations && c < 1024; c++) printf("listed %s %s\n", conversations[c].request ? "request" : conversations[c].kind == GSC_GROUP ? "group" : "direct", conversations[c].title);
    fflush(stdout);

    SDL_DetachThread(SDL_CreateThread(listen, "input", NULL));
    Uint64 started = SDL_GetTicks();
    if (typing) to = typing;
    if (reset) to = reset;
    bool sent = !to;
    for (;;) {
        gsc_event e;
        while (gsc_event_next(core, &e)) {
            if (e.type == GSC_EVENT_ACCOUNT) {
                int n = gsc_accounts(core, accounts, 8);
                for (int i = 0; i < n && i < 8; i++) {
                    const gsc_account *a = &accounts[i];
                    if (a->id != e.account) continue;
                    printf("account %lld %s %s undecryptable=%d error=%s\n", (long long)a->id, states[a->state], a->label, a->undecryptable, a->error);
                    if (!sent && a->state == GSC_ACCOUNT_ONLINE) {
                        nconversations = gsc_conversations(core, conversations, 1024);
                        for (int c = 0; c < nconversations && c < 1024 && !sent; c++)
                            if (!strcmp(conversations[c].title, to)) {
                                int64_t id = conversations[c].id;
                                reset ? gsc_reset_session(core, id) : typing ? gsc_typing(core, id, true) : gsc_send(core, id, text, 0, NULL, 0, NULL, 0);
                                sent = true;
                            }
                        if (!sent) printf("no conversation is titled %s\n", to), sent = true;
                    }
                }
            } else if (e.type == GSC_EVENT_SIGN_IN_CODE) {
                printf("sign-in-code %s\n", e.text);
            } else if (e.type == GSC_EVENT_CONVERSATIONS) {
                nconversations = gsc_conversations(core, conversations, 1024);
                printf("conversations %d\n", nconversations);
                for (int c = 0; c < nconversations && c < 1024; c++) printf("listed %s %s\n", conversations[c].request ? "request" : conversations[c].kind == GSC_GROUP ? "group" : "direct", conversations[c].title);
            } else if (e.type == GSC_EVENT_MESSAGE) {
                // (it is among the newest, though a notice placed just before it may have been stored after)
                gsc_message last[8], m = { 0 };
                nconversations = gsc_conversations(core, conversations, 1024);
                const gsc_conversation *c = conversation_by_id(e.conversation);
                for (int i = gsc_messages(core, e.conversation, 0, last, 8) - 1; i >= 0; i--)
                    if (last[i].id == e.message) m = last[i];
                if (m.id) printf("message %s %s | %s | %s\n", m.outgoing ? "out" : "in", c ? c->title : "?", m.sender, m.text);
                if (m.id) print_attachments(core, c ? c->title : "?", m.id);
                if (m.id && m.reply_to) printf("reply %s | %s | %s\n", c ? c->title : "?", m.reply_to, m.reply_text);
                if (read && c && !strcmp(c->title, read)) gsc_mark_read(core, e.conversation);
            } else if (e.type == GSC_EVENT_TYPING) {
                nconversations = gsc_conversations(core, conversations, 1024);
                const gsc_conversation *c = conversation_by_id(e.conversation);
                printf("typing %s | %s | %s\n", c ? c->title : "?", e.text, e.typing ? "started" : "stopped");
            } else if (e.type == GSC_EVENT_CONVERSATION || e.type == GSC_EVENT_MESSAGE_CHANGED || e.type == GSC_EVENT_LISTED) {
                static const char *const statuses[] = { "held", "not-sent", "waiting", "sent", "delivered", "read" }, *const *status = statuses - GSC_HELD;
                static gsc_message recent[200];
                nconversations = gsc_conversations(core, conversations, 1024);
                const gsc_conversation *c = conversation_by_id(e.conversation);
                int n = gsc_messages(core, e.conversation, 0, recent, 200);
                const gsc_message *newest = n ? &recent[n - 1] : NULL;
                printf("conversation %s unread=%d messages=%d newest=%s timer=%s\n", c ? c->title : "?", c ? c->unread : 0, n,
                       !newest ? "none" : newest->outgoing ? status[newest->status] : "received", newest && newest->expires_ms ? "yes" : "no");
                if (newest)
                    printf("newest %s | %s | %s edited=%s mine=%s reactions=%s who=%s change=%s\n", c ? c->title : "?", newest->text, newest->kind == GSC_TEXT ? "text" : newest->kind == GSC_DELETED ? "deleted" : "notice",
                           newest->edited ? "yes" : "no", newest->reaction, newest->reactions, newest->reacted, (const char *[]){ "none", "waiting", "given-up" }[SDL_clamp(newest->changing, 0, 2)]);
                if (newest) print_attachments(core, c ? c->title : "?", newest->id);
            }
            fflush(stdout);
        }
        SDL_LockMutex(said_lock);
        for (int i = 0; i < nsaid; i++) obey(core, said[i]);
        nsaid = 0;
        SDL_UnlockMutex(said_lock);
        fflush(stdout);
        Sint64 left = seconds < 0 ? -1 : (Sint64)(seconds * 1000) - (Sint64)(SDL_GetTicks() - started);
        if (seconds >= 0 && left <= 0) break;
        SDL_WaitSemaphoreTimeout(woken, left < 0 ? -1 : (Sint32)left);
    }
    gsc_close(core);
    // (leaving through exit would wait for the input thread, which holds standard input while it reads)
    fflush(stdout);
    _Exit(0);
}
