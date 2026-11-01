//! A short, size-capped history of what rustsync did, kept in the state directory as plain
//! text so it is as easy to read with `tail` and `grep` on a server as it is in a window.
//!
//! One line per event:
//!
//! ```text
//! 2026-11-01T09:12:03Z INFO device="laptop" library="photos" received 12 change(s)
//! ```
//!
//! `device="..."` and `library="..."` appear when the event is about one. The file is
//! `activity.log`; when it grows past half of the size limit it becomes `activity.log.1` (the
//! older one is dropped), so the two together never pass the limit. `daemon.log` (what the
//! program prints) is cut back to the same limit at the same moments.

use crate::config::Config;
use std::fmt::Write as _;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::Context;
use tracing_subscriber::prelude::*;
use tracing_subscriber::Layer;

pub const FILE: &str = "activity.log";
pub const DEFAULT_MAX: u64 = 2_000_000;
pub const MIN_MAX: u64 = 100_000;
pub const MAX_MAX: u64 = 500_000_000;
/// The most that is ever read back from one file (the newest part).
const READ_LIMIT: u64 = 8 * 1024 * 1024;
const LINE_LIMIT: usize = 600;

static LOG: OnceLock<Arc<ActivityLog>> = OnceLock::new();

pub struct ActivityLog {
    path: PathBuf,
    old: PathBuf,
    daemon_log: Option<PathBuf>,
    max: AtomicU64,
    file: Mutex<(Option<File>, u64)>,
}

impl ActivityLog {
    fn new(dir: &Path, max: u64, daemon_log: Option<PathBuf>) -> ActivityLog {
        ActivityLog {
            path: dir.join(FILE),
            old: dir.join(format!("{}.1", FILE)),
            daemon_log,
            max: AtomicU64::new(clamp(max)),
            file: Mutex::new((None, 0)),
        }
    }

    fn append(&self, line: &str) {
        let mut guard = match self.file.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        if guard.0.is_none() {
            match OpenOptions::new().create(true).append(true).mode(0o600).open(&self.path) {
                Ok(f) => {
                    guard.1 = f.metadata().map(|m| m.len()).unwrap_or(0);
                    guard.0 = Some(f);
                }
                Err(_) => return,
            }
        }
        let mut bytes = line.as_bytes().to_vec();
        bytes.push(b'\n');
        if let Some(f) = guard.0.as_mut() {
            if f.write_all(&bytes).is_err() {
                guard.0 = None;
                return;
            }
        }
        guard.1 += bytes.len() as u64;
        let max = self.max.load(Ordering::Relaxed);
        if guard.1 > max / 2 {
            guard.0 = None;
            guard.1 = 0;
            let _ = std::fs::rename(&self.path, &self.old);
            if let Some(d) = &self.daemon_log {
                trim_tail(d, max);
            }
        }
    }
}

fn clamp(bytes: u64) -> u64 {
    bytes.clamp(MIN_MAX, MAX_MAX)
}

/// Keep only the newest part of a file that has grown past `max` bytes (starting at a line).
fn trim_tail(path: &Path, max: u64) {
    let Ok(mut f) = File::open(path) else { return };
    let Ok(len) = f.metadata().map(|m| m.len()) else { return };
    if len <= max {
        return;
    }
    let keep = max / 2;
    if f.seek(SeekFrom::Start(len - keep)).is_err() {
        return;
    }
    let mut tail = Vec::new();
    if f.read_to_end(&mut tail).is_err() {
        return;
    }
    if let Some(nl) = tail.iter().position(|b| *b == b'\n') {
        tail.drain(..=nl);
    }
    // The program that started us keeps its own append-mode handle on this file, so rewriting
    // it in place is safe: its next write simply lands after what we leave.
    if let Ok(mut w) = OpenOptions::new().write(true).open(path) {
        if w.set_len(0).is_ok() {
            let _ = w.write_all(&tail);
        }
    }
}

/// Start logging: to the screen (when `console`) and to the activity log.
pub fn init(cfg: &Config, level: Level, console: bool) {
    let log = Arc::new(ActivityLog::new(&cfg.dir, cfg.logging.max_bytes(), Some(cfg.log_path())));
    let _ = LOG.set(log.clone());
    trim_tail(&cfg.log_path(), clamp(cfg.logging.max_bytes()));
    let filter = LevelFilter::from_level(level);
    let registry = tracing_subscriber::registry().with(ActivityLayer { log }.with_filter(filter));
    if console {
        let _ = registry.with(tracing_subscriber::fmt::layer().with_filter(filter)).try_init();
    } else {
        let _ = registry.try_init();
    }
}

/// Change the size limit of the running program.
pub fn set_max(bytes: u64) {
    if let Some(l) = LOG.get() {
        l.max.store(clamp(bytes), Ordering::Relaxed);
    }
}

pub fn max() -> u64 {
    LOG.get().map(|l| l.max.load(Ordering::Relaxed)).unwrap_or(DEFAULT_MAX)
}

/// A name as it is written in a log line: one line, no quotes.
pub fn tag(name: &str) -> String {
    name.chars()
        .map(|c| if c == '"' { '\'' } else if c.is_control() { ' ' } else { c })
        .take(60)
        .collect()
}

// ---------------------------------------------------------------- the tracing layer

struct ActivityLayer {
    log: Arc<ActivityLog>,
}

#[derive(Default)]
struct Fields {
    message: String,
    device: Option<String>,
    library: Option<String>,
}

impl Visit for Fields {
    fn record_str(&mut self, f: &Field, v: &str) {
        match f.name() {
            "message" => self.message = v.to_string(),
            "device" => self.device = Some(v.to_string()),
            "library" => self.library = Some(v.to_string()),
            _ => {}
        }
    }
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        match f.name() {
            "message" => self.message = format!("{:?}", v),
            "device" => self.device = Some(format!("{:?}", v)),
            "library" => self.library = Some(format!("{:?}", v)),
            _ => {}
        }
    }
}

impl<S: Subscriber> Layer<S> for ActivityLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        // Only what rustsync itself says (not the libraries it uses).
        if !meta.target().starts_with("rustsync") {
            return;
        }
        let mut fields = Fields::default();
        event.record(&mut fields);
        let mut line = format!("{} {:<5}", timestamp(unix_now()), meta.level().as_str());
        if let Some(d) = &fields.device {
            let _ = write!(line, " device=\"{}\"", tag(d));
        }
        if let Some(l) = &fields.library {
            let _ = write!(line, " library=\"{}\"", tag(l));
        }
        line.push(' ');
        let msg: String = fields.message.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
        line.push_str(&msg);
        if line.len() > LINE_LIMIT {
            let mut cut = LINE_LIMIT;
            while !line.is_char_boundary(cut) {
                cut -= 1;
            }
            line.truncate(cut);
        }
        self.log.append(&line);
    }
}

// ---------------------------------------------------------------- reading it back

/// The newest `lines` lines about a device and/or library (both given: about that library *and*
/// that device), oldest first. With neither, everything.
pub fn read(dir: &Path, device: Option<&str>, library: Option<&str>, lines: usize) -> String {
    let want_dev = device.map(|d| format!("device=\"{}\"", tag(d).to_lowercase()));
    let want_lib = library.map(|l| format!("library=\"{}\"", tag(l).to_lowercase()));
    let mut out: Vec<String> = Vec::new();
    for name in [format!("{}.1", FILE), FILE.to_string()] {
        for line in read_tail(&dir.join(name)).lines() {
            if want_dev.is_some() || want_lib.is_some() {
                let lower = line.to_lowercase();
                if want_dev.as_ref().map_or(false, |w| !lower.contains(w.as_str()))
                    || want_lib.as_ref().map_or(false, |w| !lower.contains(w.as_str()))
                {
                    continue;
                }
            }
            out.push(line.to_string());
        }
    }
    let skip = out.len().saturating_sub(lines.max(1));
    out[skip..].join("\n")
}

fn read_tail(path: &Path) -> String {
    let Ok(mut f) = File::open(path) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(READ_LIMIT);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    let _ = f.read_to_end(&mut buf);
    let text = String::from_utf8_lossy(&buf).into_owned();
    if start > 0 {
        // began in the middle of a line
        text.split_once('\n').map(|(_, rest)| rest.to_string()).unwrap_or_default()
    } else {
        text
    }
}

// ---------------------------------------------------------------- time

/// "5 minutes ago", for a time in seconds since 1970 (`then` of 0: never).
pub fn ago(now: u64, then: u64) -> String {
    if then == 0 {
        return "never".to_string();
    }
    let s = now.saturating_sub(then);
    let (n, unit) = match s {
        0..=59 => return "just now".to_string(),
        60..=3599 => (s / 60, "minute"),
        3600..=86_399 => (s / 3600, "hour"),
        _ => (s / 86_400, "day"),
    };
    format!("{} {}{} ago", n, unit, if n == 1 { "" } else { "s" })
}

pub fn unix_now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Seconds since 1970 as `2026-11-01T09:12:03Z` (UTC).
pub fn timestamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // days since 1970-01-01 -> civil date (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(0), "1970-01-01T00:00:00Z");
        assert_eq!(timestamp(1_782_897_123), "2026-07-01T09:12:03Z");
        assert_eq!(timestamp(951_782_400), "2000-02-29T00:00:00Z");
    }

    #[test]
    fn tags_are_one_line_without_quotes() {
        assert_eq!(tag("my \"lap\ntop\""), "my 'lap top'");
    }

    #[test]
    fn filtering_and_rotation() {
        let dir = std::env::temp_dir().join(format!("rs-activity-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = ActivityLog::new(&dir, MIN_MAX, None);
        log.append("2026-01-01T00:00:00Z INFO  device=\"Laptop\" library=\"photos\" one");
        log.append("2026-01-01T00:00:01Z INFO  device=\"desk\" two");
        log.append("2026-01-01T00:00:02Z WARN  library=\"photos\" three");
        let all = read(&dir, None, None, 100);
        assert_eq!(all.lines().count(), 3);
        assert_eq!(read(&dir, Some("laptop"), None, 100).lines().count(), 1);
        assert_eq!(read(&dir, None, Some("photos"), 100).lines().count(), 2);
        assert_eq!(read(&dir, Some("laptop"), Some("photos"), 100).lines().count(), 1);
        assert_eq!(read(&dir, None, None, 2).lines().count(), 2);
        // fill past half the limit: the file is moved aside, nothing is lost yet
        let filler = "x".repeat(1000);
        for _ in 0..100 {
            log.append(&filler);
        }
        assert!(dir.join(format!("{}.1", FILE)).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
