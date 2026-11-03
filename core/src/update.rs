//! Keeps an installed copy of the program up to date by itself. Every minute it reads a manifest
//! from the update server: the build it describes and the name, SHA-256 and size of each file of
//! that build, signed with the publisher's key. When the build is not this one and the signature
//! holds, each file that differs is downloaded, checked against its hash, and put in place of the
//! old one, which is renamed aside (a running program on Windows can be renamed but not
//! overwritten). Then the app is told, and starts again as the new build when the user is not
//! using it; that build's number is left in `updated.txt` in the data folder for it to say so.
//!
//! It looks only while the app has it on (`enable`), which it is once the user has agreed. A build
//! that failed soon after starting, and was undone by the window's watcher, is named on a "skip"
//! line of `update.txt` in the data folder, and is not taken again. What was renamed aside stays
//! for two minutes after a start, so that a new build that fails at once can be undone.
//!
//! Each look also tells the server, in its address, what the owner agreed to report: a random
//! number naming this installation, the build, and whether the window is hidden, shown, in front,
//! or in use (a key or the pointer within the last minute). Nothing of the account or its messages
//! goes. "report_use 0" in the settings file leaves the three out.
//!
//! The server is reached over plain HTTP: the signature, made with a key that never leaves the
//! publisher's machine, is what keeps a changed file out, whoever answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicI64, Ordering};
use std::time::Duration;

use ed25519_dalek::{Signature, VerifyingKey};
use sha2::{Digest, Sha256};

use crate::shared::{Event, Shared};

/// Where updates are published, unless GINK_UPDATE_FROM names another (for trying it out).
const SERVER: &str = "http://updates.waka.mx/gink";
/// The publisher's key; tools/publish-update signs with its private half.
const PUBLISHER: [u8; 32] = [
    0x88, 0x86, 0x33, 0xc5, 0x93, 0xe4, 0xe9, 0x4f, 0x55, 0x74, 0x02, 0x7f, 0x29, 0x9a, 0xd4, 0x08, 0x98, 0xaf, 0x5f, 0x41, 0x42, 0x1a, 0x10, 0x17, 0x64, 0xb5, 0x39, 0x66, 0xf3, 0x84, 0x9b, 0x0a,
];
const EVERY: Duration = Duration::from_secs(60);

/// What the window last said of itself: 0 hidden, 1 shown behind others, 2 in front.
static WINDOW: AtomicI32 = AtomicI32::new(0);
/// When the user last pressed a key or used the pointer in it, in Unix milliseconds.
static INPUT_MS: AtomicI64 = AtomicI64::new(0);
static REPORT: AtomicBool = AtomicBool::new(true);
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether to look for updates; off until the user agrees.
pub fn enable(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// The builds not to take again, from the "skip" lines of `update.txt` in the data folder.
fn skipped(data: &Path) -> Vec<String> {
    let text = std::fs::read_to_string(data.join("update.txt")).unwrap_or_default();
    text.lines().filter_map(|l| l.strip_prefix("skip ")).map(|b| b.trim().to_owned()).collect()
}

/// Notes the window's state, for the report that goes with each look.
pub fn window(state: i32, input: bool) {
    WINDOW.store(state, Ordering::Relaxed);
    if input {
        INPUT_MS.store(crate::shared::now_ms(), Ordering::Relaxed);
    }
}

/// Whether each look reports the installation's use.
pub fn report(on: bool) {
    REPORT.store(on, Ordering::Relaxed);
}

/// What goes in the manifest's address: the installation, the build, and the window's state, with
/// 3 for in front and used within the last minute; or nothing, when the owner said not to.
fn usage(install: &str) -> String {
    if !REPORT.load(Ordering::Relaxed) {
        return String::new();
    }
    let used = crate::shared::now_ms() - INPUT_MS.load(Ordering::Relaxed) < 60_000;
    let state = match WINDOW.load(Ordering::Relaxed) {
        2 if used => 3,
        state => state,
    };
    format!("?install={install}&build={}&state={state}", build())
}

/// The random number that names this installation, kept in `install.txt` in the data folder.
fn install_id(data: &Path) -> String {
    let path = data.join("install.txt");
    if let Ok(id) = std::fs::read_to_string(&path)
        && id.trim().len() == 16
    {
        return id.trim().to_owned();
    }
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    let id = hex::encode(bytes);
    let _ = std::fs::write(&path, &id);
    id
}

/// The build this program is.
pub fn build() -> &'static str {
    option_env!("GINK_BUILD").unwrap_or("unknown")
}

/// A file of a build: its name beside the program, its SHA-256 in hex, and its size.
#[derive(Debug, PartialEq)]
struct File {
    name: String,
    hash: String,
    size: u64,
}

/// Reads a manifest, given its signature in hex: the build and its files, or why it was refused.
fn manifest(text: &str, signature: &str, key: &[u8; 32]) -> Result<(String, Vec<File>), String> {
    let key = VerifyingKey::from_bytes(key).map_err(|e| e.to_string())?;
    let signature: [u8; 64] = hex::decode(signature.trim()).ok().and_then(|s| s.try_into().ok()).ok_or("the signature is not 64 bytes in hex")?;
    key.verify_strict(text.as_bytes(), &Signature::from_bytes(&signature)).map_err(|_| "the signature does not match the manifest")?;
    let (mut build, mut files) = (None, vec![]);
    for line in text.lines() {
        let part: Vec<&str> = line.split(' ').collect();
        match part[..] {
            ["build", id] => build = Some(id.to_owned()),
            ["file", name, hash, size] => {
                // (a name is a file beside the program, never a path elsewhere)
                if name.is_empty() || name.contains(['/', '\\', ':']) || name.starts_with('.') || hash.len() != 64 {
                    return Err(format!("the manifest names a file it may not: {name}"));
                }
                files.push(File { name: name.to_owned(), hash: hash.to_lowercase(), size: size.parse().map_err(|_| "a size that is not a number")? });
            }
            _ => {}
        }
    }
    Ok((build.ok_or("the manifest names no build")?, files))
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Starts looking for updates in the background, for the program installed in `folder`.
pub fn start(shared: Arc<Shared>, folder: PathBuf) {
    let install = install_id(&shared.dir);
    let server = std::env::var("GINK_UPDATE_FROM").unwrap_or_else(|_| SERVER.to_owned());
    // (what was renamed aside by the last update goes once this build has run two minutes without
    // failing, and what a failed build left when it was undone goes at once)
    let clean = |folder: &Path, end: &str| {
        for entry in std::fs::read_dir(folder).into_iter().flatten().flatten() {
            if entry.file_name().to_string_lossy().ends_with(end) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    };
    clean(&folder, ".bad");
    let kept = folder.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(120));
        clean(&kept, ".old");
    });
    let data = shared.dir.clone();
    std::thread::spawn(move || {
        let Ok(runtime) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return };
        // (the client is built with TLS, for which a provider must be chosen before any client is
        // made, as the Signal connection chooses one; the server itself is reached without it)
        if rustls::crypto::CryptoProvider::get_default().is_none() {
            let _ = rustls::crypto::ring::default_provider().install_default();
        }
        let client = reqwest::Client::builder().timeout(Duration::from_secs(120)).build().unwrap_or_default();
        let mut said = String::new();
        loop {
            if !ENABLED.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(2));
                continue;
            }
            match runtime.block_on(look(&client, &server, &folder, &usage(&install), &skipped(&data))) {
                Ok(Some(build)) => {
                    crate::log::line(&format!("updated to build {build}; to start again when the user is not using it"));
                    let _ = std::fs::write(data.join("updated.txt"), &build);
                    shared.push(Event::Updated);
                    return;
                }
                Ok(None) => said.clear(),
                // (a failure is written to the log when it first happens, not every minute)
                Err(error) if error != said => {
                    crate::log::line(&format!("looking for an update: {error}"));
                    said = error;
                }
                Err(_) => {}
            }
            std::thread::sleep(EVERY);
        }
    });
}

/// Looks once. Returns the build put in place, or None when this is the newest.
async fn look(client: &reqwest::Client, server: &str, folder: &Path, usage: &str, skip: &[String]) -> Result<Option<String>, String> {
    let get = |path: String| async move {
        let response = client.get(format!("{server}/{path}")).send().await.map_err(|e| e.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{path}: {}", response.status()));
        }
        response.bytes().await.map_err(|e| e.to_string())
    };
    let text = String::from_utf8(get(format!("manifest.txt{usage}")).await?.to_vec()).map_err(|_| "the manifest is not text")?;
    let signature = String::from_utf8(get("manifest.sig".into()).await?.to_vec()).map_err(|_| "the signature is not text")?;
    let (build, files) = manifest(&text, &signature, &PUBLISHER)?;
    if build == self::build() || skip.contains(&build) {
        return Ok(None);
    }
    // Every file that differs is fetched and checked before any is put in place.
    let mut fetched = vec![];
    for file in &files {
        let here = folder.join(&file.name);
        // (the program's former name is offered to copies that still run under it, and fetched only by them)
        if file.name == "gsignal.exe" && !here.exists() {
            continue;
        }
        if std::fs::read(&here).is_ok_and(|bytes| sha256(&bytes) == file.hash) {
            continue;
        }
        let bytes = get(format!("files/{}", file.hash)).await?;
        if bytes.len() as u64 != file.size || sha256(&bytes) != file.hash {
            return Err(format!("{} did not arrive as the manifest describes it", file.name));
        }
        let new = folder.join(format!("{}.new", file.name));
        std::fs::write(&new, &bytes).map_err(|e| format!("{}: {e}", new.display()))?;
        // (with the old file's permissions, so that the program stays one that can be run)
        if let Ok(meta) = std::fs::metadata(&here) {
            let _ = std::fs::set_permissions(&new, meta.permissions());
        }
        fetched.push((here, new));
    }
    // (files that all match already are this build, whatever it calls itself: nothing to restart for)
    if fetched.is_empty() {
        return Ok(None);
    }
    for (here, new) in fetched {
        let old = here.with_file_name(format!("{}.old", here.file_name().unwrap_or_default().to_string_lossy()));
        let _ = std::fs::remove_file(&old);
        if here.exists() {
            std::fs::rename(&here, &old).map_err(|e| format!("{}: {e}", here.display()))?;
        }
        std::fs::rename(&new, &here).map_err(|e| format!("{}: {e}", here.display()))?;
    }
    Ok(Some(build))
}

#[cfg(test)]
mod tests {
    use ed25519_dalek::{Signer, SigningKey};

    use super::{File, manifest};

    #[test]
    fn a_manifest_is_taken_only_with_its_publishers_signature() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let public = key.verifying_key().to_bytes();
        let text = format!("build abc1234\nfile gsignal.exe {} 12\n", "ab".repeat(32));
        let signed = hex::encode(key.sign(text.as_bytes()).to_bytes());
        let (build, files) = manifest(&text, &signed, &public).unwrap();
        assert_eq!(build, "abc1234");
        assert_eq!(files, [File { name: "gsignal.exe".into(), hash: "ab".repeat(32), size: 12 }]);
        // Changed by one byte, or signed by another key, it is refused.
        assert!(manifest(&text.replace("12", "13"), &signed, &public).is_err());
        let other = hex::encode(SigningKey::from_bytes(&[8; 32]).sign(text.as_bytes()).to_bytes());
        assert!(manifest(&text, &other, &public).is_err());
        // A name that would reach outside the program's folder is refused even when signed.
        let outside = format!("build x\nfile ../evil.exe {} 1\n", "ab".repeat(32));
        assert!(manifest(&outside, &hex::encode(key.sign(outside.as_bytes()).to_bytes()), &public).is_err());
    }
}
