# gink

gink, formerly gsignal, is a native desktop client for [Signal](https://signal.org/): one program of about 14 MB with no browser inside. Its window is drawn with [gesso](https://github.com/wakamex/gesso), a C library on [Simple DirectMedia Layer 3 (SDL3)](https://www.libsdl.org/), and its protocol core is a Rust library on [libsignal-service-rs](https://github.com/whisperfish/libsignal-service-rs). It links to an existing Signal account as a second device, beside the phone.

It is in daily use by its author on Windows and has no release yet, only test builds. The Signal trademark belongs to Signal Messenger; this project is not affiliated with it.

## Install

A test build for Windows is offered at <https://updates.waka.mx/gink/>: unpack `gink-test-windows.zip` into a folder of its own and run `gink.exe`, then scan the code it shows with your phone, under Signal's settings for linked devices. Windows may warn about a program it does not know the first time; "More info" and "Run anyway" start it. On macOS and Linux, build it from source as below.

A test build can keep itself up to date, and asks on its first run whether it may. If you agree, it looks for a new build every minute, puts it in place, and restarts when you have left it alone for two minutes and nothing is playing or being sent, at most once a quarter hour, coming back as it was. A build that fails to start is undone and the one before it runs again. Each look also sends a random number naming the installation, the build, and whether the window is hidden, open or in use; never an account, contacts or messages. "updates on" in the status bar turns it off, and `report_use 0` in the settings file leaves the report out. A copy started with `--data` never updates itself.

## How gink differs from other Signal clients

### From Signal Desktop

Signal's own desktop program is built on [Electron](https://www.electronjs.org/), which ships a browser engine with it. gink is one executable under 30 MB for each of Windows, macOS and Linux, with nothing to install beside it, and it draws its own window.

- It shows a message sooner. Both programs record when they store each message, and `tools/arrival-lag` pairs the records on one Windows machine. Over 35 messages that arrived on 2026-10-13 while both were connected, gink had every one first, by a median of 1.6 s. One second of that is fixed: Signal Desktop stores a message 1.02 s after taking it off its connection.
- A changed profile picture shows on its owner's next message. Signal Desktop looks when a conversation is opened, and otherwise every few days.
- A photo is sent without its location, camera and time, and with every pixel as it was. Signal Desktop removes the same things by encoding the picture again, scaled down.
- The same window holds [Internet Relay Chat (IRC)](https://ircv3.net/) networks and [Matrix](https://matrix.org/) accounts beside Signal, in one list of conversations.

### From the other unofficial clients

Most unofficial Signal clients are built on one Rust library, [presage](https://github.com/whisperfish/presage). gink started from presage's code and wrote what presage leaves out. The table sets gink against what three of those clients say of themselves in their own documentation and issue lists, as read on 2026-10-11: [gurk](https://github.com/boxdot/gurk-rs), which runs in a terminal, [Flare](https://gitlab.com/schmiddi-on-mobile/flare), a GTK program for Linux, and [Whisperfish](https://gitlab.com/whisperfish/whisperfish), for Sailfish OS phones. The evidence for each row is in [research/client-limitations.md](research/client-limitations.md).

| | gink | The others |
|---|---|---|
| Where it runs | A window of its own on Windows, macOS and Linux | gurk in a terminal on Linux, macOS and FreeBSD; Flare on Linux; Whisperfish on Sailfish OS. None offers a graphical program for Windows or macOS |
| Groups | Creates and leaves groups, adds and removes members, accepts and declines invitations | presage has no call for any of these ([#298](https://github.com/whisperfish/presage/issues/298), [#313](https://github.com/whisperfish/presage/issues/313)); Flare lists them as blocked by presage, and Whisperfish lists invitations as not implemented |
| Blocking and muting | Both, kept in step with the account's other devices | Not in presage; Flare lists both as blocked by it |
| A message that cannot be decrypted | Said in the conversation, with a session reset offered; the sender's request to send again is answered | presage drops that request, which leaves the sender in a loop ([#399](https://github.com/whisperfish/presage/issues/399)). Whisperfish's FAQ has the user delete the contact's stored identity by hand; Flare repairs such conversations itself since 0.22.1 |
| Whether it is still receiving | The status bar says whether it is connected and when it last received anything | Receiving that stops without a sign is the most-discussed report in gurk's tracker ([#494](https://github.com/boxdot/gurk-rs/issues/494), [#471](https://github.com/boxdot/gurk-rs/issues/471)) |
| After the computer sleeps, or the phone unlinks the device | Reconnects; returns to the code to scan | Flare has open reports of staying offline ([#297](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/297)) and of not returning to linking ([#294](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/294)) |
| Text longer than 2,048 bytes | Sent and received whole | Flare has an open report of messages cut off there ([#323](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/323)) |

### What it does not do

gink has no voice or video calls, stories, stickers or polls, and it links to an account as a second device, so the phone is still needed to register. Signal does not support third-party clients and can change its service without notice. Its own programs expire three months after release, and after that the service is free to drop what they relied on, so an older release of gink may go on working or may stop without warning. The code has had none of the outside security review that Signal's own programs have.

### Limits of Signal's own programs

Some limits belong to Signal's own programs and not to the protocol: another program can send what they cannot show. gink shows what arrives in full, and sends only what Signal's programs can show, since most people it writes to use them. The list grows as such limits are met; each was checked in [Signal Desktop's source](https://github.com/signalapp/Signal-Desktop), and the phone programs have not been checked.

| Limit | Where it is | What gink does |
|---|---|---|
| Of a message's files, only the first is shown unless they are pictures or videos: a message with three sound files shows one player | `isAudio` in `ts/util/Attachment.std.ts` looks at the first file alone, and `Message.dom.tsx` draws that one | Shows every file of a message it receives. Sends pictures and videos together with the text, and each other file as a message of its own |
| A message being written takes at most 32 files | `ts/state/ducks/composer.preload.ts` stops adding at 32 | Takes at most 32 as well |

## IRC

gink is also a client for an [IRC](https://en.wikipedia.org/wiki/IRC) bouncer such as [soju](https://soju.im/), a server that stays on the IRC networks and keeps their history while gink is closed, so a phone and a computer can both be the same person there. Add one with Add IRC in the status bar, or `--add-irc ADDRESS`, giving the bouncer's address as `ircs://USER:PASSWORD@HOST:PORT?sha256=FINGERPRINT`. The fingerprint is the SHA-256 of the bouncer's certificate, `openssl x509 -in cert.pem -outform DER | sha256sum`, which gink takes in place of a certificate authority's, as a bouncer on a home network has none. `irc://` without TLS is for a bouncer on the same machine.

gink speaks only standard [IRCv3](https://ircv3.net/): SASL to sign in, the list of networks the bouncer holds (`soju.im/bouncer-networks`), the history of what came while it was closed (`draft/chathistory`), and what was read on another client (`draft/read-marker`). A channel is a group conversation and a person a direct one. A line that starts with `/me`, `/join`, `/part`, `/msg` or `/nick` is that command, and `//` starts a line with a slash. IRC carries text alone, so its conversations have no files, reactions, replies, edits or timers.

## Build and run

Building needs [Zig](https://ziglang.org/) 0.17, [Cargo](https://doc.rust-lang.org/cargo/) and the [Protocol Buffers](https://protobuf.dev/) compiler `protoc` on the path, or named by `PROTOC`. The Windows build also needs Rust's `x86_64-pc-windows-gnu` target (`rustup target add x86_64-pc-windows-gnu`) and `x86_64-w64-mingw32-dlltool` from [MinGW-w64](https://www.mingw-w64.org/)'s binutils, packaged as `binutils-mingw-w64-x86-64` on Debian and Ubuntu and `mingw64-binutils` on Fedora.

```sh
zig build run                                                              # build and run
zig build -Doptimize=ReleaseSmall -p zig-out/release                       # a release build
zig build -Doptimize=ReleaseSmall -Dtarget=x86_64-windows-gnu -p zig-out/windows
zig build -Doptimize=ReleaseSmall -Dstrip=false -p zig-out/release-symbols # with symbols, for reading a crash
```

An Android build is at an early stage: `tools/android-apk` builds it for phones (arm64-v8a) and the emulator (x86_64) and packs `zig-out/android/gink.apk`, signed with the [Android SDK](https://developer.android.com/studio)'s debug key. It needs the SDK, with its build tools, a platform and the Native Development Kit (NDK), named by `ANDROID_HOME`, and Rust's `aarch64-linux-android` and `x86_64-linux-android` targets.

The first run shows a code to scan with the phone, under Signal's settings for linked devices. The program keeps its data, encrypted, in the user's application data folder; `--data DIR` names another.

## Tests

```sh
cd core && cargo test --locked -p signal-client -p signal-store -p gsignal_core
GINK_LIVE=1 python3 -m unittest -v tests.test_signal_live
```

The live checks send real messages between two spare accounts, each served by a [signal-cli](https://github.com/AsamK/signal-cli) daemon, and are described at the top of `tests/test_signal_live.py`. signal-cli is a separate implementation of the protocol, so a check that passes says more than gink talking to itself.

Maintaining the update server, the tools for a Windows test machine and the arrival-time tracker are described in [RELEASES.md](RELEASES.md).

## Working principles

These are the rules the work on gink is held to. Each came from something that went wrong, or right, while building it.

### Safety before features

1. Store before sending, and ask before trusting. Whatever can lose a message or share something the user did not mean to share is fixed before the next visible feature.
2. A feature that is taken over in part defaults to the safer half. A view-once picture that is shown like any other is worse than one that is not shown.

### Evidence

3. Test from where a user starts. The standard checks include a profile with nothing in it, a conversation of one message and a long one: a fault that struck every conversation under 200 messages went unseen because every test conversation was longer.
4. A control is said to work after it has been clicked in a real window. Offscreen pictures show layout; they do not show a key lost, a button that moves from under the pointer, or a caret that stays put.
5. Drawing is timed again after any change to it, and a slowdown is traced to one layer before anything is changed to cure it.
6. A check that fails now and then is a fault to find, not a note to keep. The one that was noted and left turned out to be a race that broke decryption.

### On the user's machine

7. Nothing takes the keyboard or brings a window forward unasked, and the running program is restarted no more often than a change needs.
8. A copy of a linked profile stays off the network unless being on it is the test: two programs on one linked device corrupt its sessions.
9. What was not seen on the user's own screen is said so, as a line of its own, every time.

### Code

10. A schema step that has run is never edited; a change is a new step.
11. A choice the window offers lives in the one settings file, with the chosen default shipped and the alternatives kept reachable there.
12. The window's code is divided before it grows further, and formatting and the tests run on every commit by machine, while the repository is small enough for that to be cheap.

### Working with gesso

13. A fault in gesso is reported with a way to reproduce it and a guess marked as a guess, and the request asks for the fix and a look at the places like it.
14. Before asking gesso for something, see whether gink can draw it with what gesso already offers. What only one program wants stays in that program.

### Decisions

15. A question about what could be done gets options, what each costs, and a recommendation, and nothing is built until one is chosen. A description of a particular design is a request to build it.

### On the shared host

16. A test opens windows and sends input only on a display it started itself, with `tools/test-display`, and stops when that display does not start. A display number written by hand once belonged to another project's streamed screen, and a test window went out on its stream.

## Licence

gink is under the [GNU Affero General Public License, version 3](LICENSE), which libsignal-service-rs requires of a program that links it and which the code taken from presage carries. What is built into it or shipped beside it keeps its own licence:

- `core/signal-client` and `core/signal-store` began as copies of the `presage` and `presage-store-sqlite` crates of [presage](https://github.com/whisperfish/presage) at commit `b44d0129348fffde7455214bc081859dcf2fd242`, under presage's AGPL-3.0; each crate's README says so.
- The fonts shipped beside the program: their licences are in [assets/NOTICE.md](assets/NOTICE.md).
- [gesso](https://github.com/wakamex/gesso) is MIT, and its README lists the libraries it compiles in: SDL3 (zlib), libopus (BSD), stb (public domain or MIT), kb_text_shape (zlib), and FFmpeg's video decoders, linked statically under the GNU Lesser General Public License 2.1 or later.
- The Rust crates of the core keep their licences, listed by `cargo about` or in each crate's `Cargo.toml`.
