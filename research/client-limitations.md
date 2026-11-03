# Limitations of existing Signal clients

gink is a native desktop client for [Signal](https://signal.org/). Most unofficial clients share one Rust library, [presage](https://github.com/whisperfish/presage), and gink's Signal code starts from presage's source. This page asks what the existing unofficial clients cannot do, and which of those limits gink started with.

Short result: every graphical client built on presage lacks group management, calls and history transfer, and each has open reports of receiving stopping after a Signal server change or an identity mismatch. The limits come from presage and from Signal's lack of support for third-party clients, so gink starts with the first kind until it writes the missing parts and keeps the second kind for good. No project offers a native graphical client for Windows or macOS.

How this was checked: by reading each project's own README, wiki, changelog and open issue list as of 2026-10-11, and presage's manifests and public interface at its commit of 2026-10-10. No client was installed or run. The copies read are in [sources/](sources/INDEX.md).

## Summary by client

The table compares what each project is, what it is built on, and the limits it documents. "Presage clients" share the limits listed under presage.

| Client | Kind and platforms | Built on | State | Documented limits |
|---|---|---|---|---|
| [signal-cli](https://github.com/AsamK/signal-cli) | Command line and daemon; Linux, macOS, Windows | Java, on a patched copy of the Android app's service library | v0.14.9 released 2026-10-07 | No interface of its own. No calls. Its feature matrix lists no sending of link previews, stickers or view-once media. Releases older than three months "may not work correctly" |
| [gurk](https://github.com/boxdot/gurk-rs) | Terminal; Linux, macOS, FreeBSD | Rust, presage | v0.10.1 released 2026-07-26 | No Windows build. No text formatting. No images shown in the terminal. Open reports of receiving stopping, of linking failing with HTTP 409, and of messages sent from gurk not reaching the account's other devices |
| [Flare](https://gitlab.com/schmiddi-on-mobile/flare) | Graphical, GTK; Linux only | Rust, presage | 0.22.3 released 2026-10-10 | No calls, group management, blocking, search, forwarding, sent edits, sent typing indicators or history transfer. No handling of a changed safety number. Stuck offline after resume from suspend. Messages cut off at 2,048 characters |
| [Whisperfish](https://gitlab.com/whisperfish/whisperfish) | Graphical, Qt; Sailfish OS phones only | Rust, [libsignal-service-rs](https://github.com/whisperfish/libsignal-service-rs) | Beta; status page lists current breakage | No group invitations. Primary registration flaky. Group reception can stall on a stale identity. Its README recommends keeping Signal Desktop linked alongside |
| [Axolotl](https://github.com/axolotl-chat/axolotl) | Graphical in a web view (Vue.js in Tauri); Ubuntu Touch and Linux | Rust, presage | Last commit 2025-07-04 | Contact discovery broken. Groups "mostly". Linking "partially" |
| [scli](https://github.com/isamert/scli) | Terminal; Linux | Python, drives signal-cli | Last commit 2024-11-30 | No quoting or mentions when sending, no sent read receipts, no view-once or expiring messages |
| [purple-presage](https://github.com/hoehermann/purple-presage) | Plug-in for Pidgin; Linux, Windows | Rust, presage | Last commit 2026-08-09 | Sends plain text and attachments only. Contact list currently empty. Cannot mark messages read. No typing indicators or receipts |
| [mautrix-signal](https://github.com/mautrix/signal) | Bridge to [Matrix](https://matrix.org/); a server | Go, on libsignal's C interface | Commits 2026-10-10 | Needs a Matrix server and client. Cannot accept group invitations or register as primary. No payment messages |
| [Molly](https://github.com/mollyim/mollyim-android) | Fork of the official app; Android only | Signal's Android code | Updated every two weeks | Not a desktop client |

## Presage and libsignal-service-rs

Presage's code is where gink's Signal client layer starts, and libsignal-service-rs is the library under both, so presage's limits are gink's until the core fills them.

Missing from its public interface (`presage/src/manager/registered.rs`):

- Creating a group, leaving one, changing members, and accepting an invitation. Requests [#298](https://github.com/whisperfish/presage/issues/298) and [#313](https://github.com/whisperfish/presage/issues/313) are open. A linked device is in a group only after someone adds the account.
- Blocking and muting contacts.
- Transfer of message history from the phone at linking ([#396](https://github.com/whisperfish/presage/issues/396)).

Left to the client:

- Removing disappearing messages when their timer ends ([#168](https://github.com/whisperfish/presage/issues/168)). Gurk added this itself in 0.9.2.
- Applying receipts to stored messages ([#141](https://github.com/whisperfish/presage/issues/141)) and counting unread ones ([#86](https://github.com/whisperfish/presage/issues/86)).
- Storing reactions against their message ([#123](https://github.com/whisperfish/presage/issues/123)) and keeping the revisions of an edited message ([#195](https://github.com/whisperfish/presage/issues/195)). Presage does not attach either to the message it refers to.
- Building link previews ([#356](https://github.com/whisperfish/presage/issues/356)).
- Answering the captcha challenge Signal can demand when sending ([#176](https://github.com/whisperfish/presage/issues/176)).

Open defects that reach every client built on it:

- Sending a first message to someone who is not yet a contact ([#187](https://github.com/whisperfish/presage/issues/187)), and decrypting the first message from a new sender in some cases ([#394](https://github.com/whisperfish/presage/issues/394)).
- A sender left in a loop of failed decryption when the receive loop drops a retry request ([#399](https://github.com/whisperfish/presage/issues/399), and [#449](https://github.com/whisperfish/libsignal-service-rs/issues/449) in libsignal-service-rs).
- A device registered as primary cannot send or receive ([#439](https://github.com/whisperfish/presage/issues/439), opened 2026-10-09).
- Signal's server is moving requests from REST endpoints to the WebSocket, and the remaining REST calls in libsignal-service-rs "may stop working soon" ([#365](https://github.com/whisperfish/libsignal-service-rs/issues/365), open since June 2025).

Build facts:

- libsignal-service-rs turns contact discovery on by default, which brings in libsignal's network code and [BoringSSL](https://boringssl.googlesource.com/boringssl/). Presage depends on it with defaults off, so a presage build has no BoringSSL unless the `cdsi` feature is asked for. Without it, connections use [rustls](https://github.com/rustls/rustls).
- purple-presage's README reports that BoringSSL's Rust bindings do not build with the GNU toolchain on Windows, that the default SQLCipher build needs OpenSSL there, and that a build takes 12 GB of disk.
- Gurk avoids OpenSSL by building SQLCipher with a cryptography provider written in Rust (`bundled-sqlcipher-custom-crypto`), with presage's default features off. Its release archives are 9 to 12 MiB compressed.
- libsignal-service-rs needs a patched `curve25519-dalek` and Rust 1.89 or newer.

## signal-cli

- It is the most complete unofficial implementation: its wiki's feature matrix covers groups (create, join, leave, update), blocking, reactions, receipts, typing, remote delete, device management and registration.
- The matrix marks call messages and payment notifications as not implemented, and sending link previews, stickers and view-once media as unsupported. The page is community-edited and may trail the code.
- It runs on Java 25, or as an experimental native Linux build made with GraalVM.
- Its README states the constraint every client shares: "The official Signal clients expire after three months and then the Signal-Server can make incompatible changes. So signal-cli releases older than three months may not work correctly."
- Its wiki notes that positions of mentions and text formatting are counted in UTF-16 code units, as the protocol defines them, so an emoji counts as two.

## gurk

- Most-discussed open reports: receiving stopped ([#494](https://github.com/boxdot/gurk-rs/issues/494), [#471](https://github.com/boxdot/gurk-rs/issues/471)), sending failed with a protocol error ([#462](https://github.com/boxdot/gurk-rs/issues/462)), linking failed with HTTP 409 ([#556](https://github.com/boxdot/gurk-rs/issues/556), [#512](https://github.com/boxdot/gurk-rs/issues/512)).
- Messages sent from gurk did not appear on the account's phone or desktop ([#242](https://github.com/boxdot/gurk-rs/issues/242)), and messages sent from the phone did not appear in gurk ([#514](https://github.com/boxdot/gurk-rs/issues/514)).
- A burst of system notifications at startup while it catches up on queued messages ([#196](https://github.com/boxdot/gurk-rs/issues/196)).
- No way to start a conversation with someone new ([#274](https://github.com/boxdot/gurk-rs/issues/274)), no text formatting ([#237](https://github.com/boxdot/gurk-rs/issues/237)), no Windows release ([#309](https://github.com/boxdot/gurk-rs/issues/309)).
- Until 0.10.0 it loaded the whole message history into memory at startup; it now reads a window around the selection from the database.
- It disables SQLCipher's cleanup at exit, which crashed when other threads were still running.

## Flare

- Its [feature roadmap](https://gitlab.com/schmiddi-on-mobile/flare/-/wikis/Feature-roadmap) lists as not planned: calls, identity verification, sending stickers, stories and most settings. As blocked by presage: read receipts, disappearing messages, group invitations and management, contact discovery, blocking and muting, an all-media view and message import. The changelog shows a message dialog listing receipts in 0.22.0, so the roadmap page trails the code on that row.
- Open reports: no handling of a changed safety number ([#95](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/95), since 2023), stuck offline after resume from suspend ([#297](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/297)), repeated 403 errors after the phone was replaced ([#305](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/305)), no return to the linking screen after being unlinked ([#294](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/294)), messages cut off at 2,048 characters ([#323](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/323)), pictures sent from Flare that do not open for the receiver ([#236](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/236)), notifications shown while its window is active ([#230](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/230)), drafts not kept ([#173](https://gitlab.com/schmiddi-on-mobile/flare/-/issues/173)).
- Version 0.22.1 added automatic repair of conversations whose sessions had broken, and 0.21.0 was an emergency fix for linking.
- Its README warns that the app "will probably worsen the security compared to official Signal products".

## Whisperfish

- Its [status page](https://whisperfish.rubdos.be/) lists dated breakage: linking failed from 2026-07-01 to 2026-07-09 after Signal stopped sending a key during linking, and from 2026-08-30 group messages from a contact with an out-of-date stored identity stopped decrypting until a one-to-one exchange repaired it.
- Its FAQ describes contacts whose messages stop arriving with an "Untrusted identity" error after they link a new device, fixed by deleting that contact's stored identity by hand.
- On Signal's stance, the FAQ says the Signal developers appear to tolerate third-party clients, ignore outreach, and can ban them at any time. Whisperfish identifies itself to the servers with its own User-Agent.
- Its database is SQLCipher with the key derived from a user password by scrypt.
- A static link once put two libraries with clashing symbol names in one program (the embedded browser and SQLCipher), which crashed registration.
- Search is by sentence embeddings and needs a 400 MB model on the device.

## Axolotl, scli, purple-presage, mautrix-signal, Molly

- Axolotl draws its interface in a web view, so it is no model for gink's front end. Its README lists contact discovery as broken.
- scli depends on signal-cli and has been quiet since November 2024.
- purple-presage is the only presage client built for Windows, which makes its build notes the best available evidence for that platform.
- mautrix-signal implements its own client logic in Go on libsignal's C interface and supports group creation, membership changes and permissions. It is the working reference for group management outside Signal's own apps.
- Molly keeps pace with Signal by merging upstream every two weeks, which is possible because it is a fork of the official code.

## Signal's terms and Signal Desktop's own gaps

Signal's [Terms of Service](https://signal.org/legal/) do not mention third-party clients. Three clauses bear on one:

- Automated or bulk messaging is not allowed, and accounts may not be created "through unauthorized or automated means".
- Signal's trademarks and logos may not be used without written permission.
- Signal may end anyone's access "anytime for any reason".

A community-kept [feature comparison](https://community.signalusers.org/t/feature-comparison-android-ios-desktop/12003) lists what Signal Desktop lacks against the phone apps: registering as a primary device, sharing a location, a screen lock, proxy and censorship-circumvention support, scheduled messages, and an encrypted store for the database key. It also shows what Desktop has that a replacement would be measured against: polls, pinned messages, chat folders, usernames, view-once media, sticker pack creation and GIF search.

## Consequences for gink

Each item names the evidence above that it follows from.

1. Build configuration: libsignal-service-rs with contact discovery off, as presage sets it, and SQLCipher with the Rust cryptography provider, as gurk does. This keeps BoringSSL and OpenSSL out of the build, which is what makes a Windows build from a Linux machine plausible. Gurk's 9 to 12 MiB archives suggest a program under 30 MB is reachable.
2. Nobody ships a presage client for Windows built with the GNU toolchain. gink's first build is the first test of it.
3. gink loads the system's libcurl at run time on Linux and macOS through `gs_http`, and links SQLCipher statically. Whisperfish's symbol clash shows the risk in mixing two cryptography-bearing libraries in one process, so an early test should make one HTTPS request and open the encrypted database in the same run.
4. The core needs its own message tables. Presage does not apply reactions, edits or receipts to the messages they refer to, and gurk had to move from loading everything to windowed reads.
5. Session and identity failures are the common serious defect across gurk, Flare and Whisperfish. The core needs to surface a decryption failure per contact, offer a session reset, and show a changed safety number.
6. Being unlinked from the phone must return the app to the linking screen, and waking from suspend must reconnect.
7. Text over 2,048 bytes is sent as a text attachment in Signal's protocol. The core has to split it on send and join it on receive.
8. Mention and formatting ranges are in UTF-16 code units while gesso works in UTF-8. The core converts at the boundary.
9. Messages queued while the app was closed arrive in a burst. Notifications are suppressed until the queue has drained.
10. Whether a message sent from gink appears on the account's other devices, and the reverse, needs its own test, since gurk has open reports of both failing.
11. Starting a conversation with someone who is not yet a contact is unreliable in presage. It stays on the phone until gink has group management.
12. Group management is absent from presage and present in mautrix-signal, so it is feasible. libsignal-service-rs can already build the actions that add and remove members and encrypt a new group's state, with nothing above them that submits a change. mautrix-signal's Go code and signal-cli are the references for building it.
13. A release has no fixed life: it is likeliest to work in its first three months and may stop at any time, by signal-cli's statement of the servers' behaviour: the service can make incompatible changes once the official clients of that age have expired, and does not have to.
14. The public name and icon cannot use Signal's trademarks.
15. gink is a personal client for a person's own conversations. Automated or bulk sending is outside Signal's terms and stays out of scope.
