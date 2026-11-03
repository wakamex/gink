//! A log of what went wrong, kept beside the databases as gsignal.log: the account's changes of
//! state and their errors, the warnings and errors of the libraries beneath, a panic's last words,
//! and what the app adds. It is for explaining afterwards what happened on a machine nobody was
//! watching. It holds no message text; a line is cut at 400 characters, and the file starts again
//! when it passes a megabyte, with the one before kept as gsignal.log.1.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

static FILE: OnceLock<Mutex<PathBuf>> = OnceLock::new();
const FULL: u64 = 1 << 20;
const LINE: usize = 400;

/// Starts the log in `dir` and has panics and the libraries' warnings written to it.
pub fn start(dir: &Path) {
    if FILE.set(Mutex::new(dir.join("gsignal.log"))).is_err() {
        return;
    }
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        line(&format!("PANIC {panic}"));
        before(panic);
    }));
    let _ = tracing::subscriber::set_global_default(Warnings);
    line(&format!("started, build {}", crate::update::build()));
}

/// Adds a line, with the time in UTC before it.
pub fn line(text: &str) {
    let Some(file) = FILE.get() else { return };
    let path = file.lock().unwrap_or_else(|e| e.into_inner());
    if std::fs::metadata(&*path).is_ok_and(|m| m.len() > FULL) {
        let _ = std::fs::rename(&*path, path.with_extension("log.1"));
    }
    let mut cut: String = text.chars().take(LINE).map(|c| if c.is_control() { ' ' } else { c }).collect();
    if text.chars().count() > LINE {
        cut.push('…');
    }
    if let Ok(mut out) = std::fs::OpenOptions::new().create(true).append(true).open(&*path) {
        let _ = writeln!(out, "{} {cut}", stamp());
    }
}

/// The time now as 2026-10-13 21:50:03.123, in UTC.
fn stamp() -> String {
    let ms = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let (days, rest) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // (days since 1970 to a date: the civil-from-days reckoning, in years that begin in March)
    let z = days + 719_468;
    let (era, day_of_era) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted = (5 * day_of_year + 2) / 153;
    let (day, month) = (day_of_year - (153 * shifted + 2) / 5 + 1, if shifted < 10 { shifted + 3 } else { shifted - 9 });
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}.{:03}", rest / 3_600_000, rest / 60_000 % 60, rest / 1000 % 60, rest % 1000)
}

/// Takes the warnings and errors the libraries report through `tracing`, and nothing quieter.
struct Warnings;

impl tracing::Subscriber for Warnings {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        *metadata.level() <= tracing::Level::WARN
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        struct Fields(String);
        impl tracing::field::Visit for Fields {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                let _ = if field.name() == "message" { write!(self.0, "{value:?}") } else { write!(self.0, " {}={value:?}", field.name()) };
            }
        }
        let mut fields = Fields(String::new());
        event.record(&mut fields);
        line(&format!("{} {}: {}", event.metadata().level(), event.metadata().target(), fields.0));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_time_is_written_as_a_date() {
        // (the stamp is of now; its shape and a year in this century are what can be checked)
        let stamp = super::stamp();
        assert_eq!(stamp.len(), 23);
        assert!(stamp.starts_with("20") && &stamp[4..5] == "-" && &stamp[10..11] == " ");
        let (month, day): (u32, u32) = (stamp[5..7].parse().unwrap(), stamp[8..10].parse().unwrap());
        assert!((1..=12).contains(&month) && (1..=31).contains(&day));
    }
}
