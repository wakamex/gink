# Maintainer notes

## Test builds

gink publishes self-updating test builds, not releases, under the owner's release policy's section for such builds. An installed copy reads a manifest from the update server every minute: the build it describes and each file's name, SHA-256 and size, signed with an [Ed25519](https://ed25519.cr.yp.to/) key that stays on the maintainer's machine; the program carries the public half. When it describes another build, the files that differ are fetched, checked and put in place, the old ones kept aside two minutes after the next start so the window's watcher can undo a build that fails within a minute.

`tools/publish-update` builds the current commit for Windows, refuses an uncommitted tree or a commit not on the public `main`, signs the manifest and uploads the files by hash, then the signature and the manifest, with `gink-test-windows.zip` and the page that offers it. The program was called gsignal: copies installed then are offered the program under that name too until all have moved (`FORMERLY` in the script), and the server answers at its old address as well.

`tools/usage` reads the installations' reports from the update server's logs: when each copy runs, and how long it is in front and in use.

## A Windows test machine

`tools/windows-grab` takes a picture of the window on a Windows machine and `tools/windows-update` puts a new build in place of the running one, both through deskrun, which runs a script in the signed-in desktop session without taking the keyboard. `tools/test-windows` runs a build on that machine's desktop and copies back a picture of its window.

## Arrival times

`tools/arrival-lag report` says how much sooner or later gink receives each message than Signal's own desktop program on the same Windows machine. gink writes the time it stores each message from someone else to `arrivals.txt` in its data folder, and `tools/arrival-lag collect`, run hourly by a systemd user timer, pairs those with the times in Signal Desktop's log before that log is rotated away. Both files hold times alone.

## Names kept from gsignal

The program was called gsignal. To keep the data and updates of copies installed under that name, a few internal names keep it on purpose: the data folder's files `gsignal.db` and `gsignal.log`, the core's crate and C interface `gsignal_core` (`gsc_` calls), and the update server's old address, which serves the same folder as the new one. The data folder itself moved to `gink`, and a program started as `gsignal.exe` moves itself to `gink.exe`.

## Publishing

`tools/publish-update` reads where to publish from `~/.config/gink/publish-to` and signs with `~/.config/gink/update-signing.key`, both outside the repository. It publishes only a commit that is on the public `main` branch, and the page that offers the build links that commit's source, as the AGPL asks; the Rust crates' sources are those `core/Cargo.lock` pins.
