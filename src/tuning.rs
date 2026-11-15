//! How hard this machine works when it transfers and scans.
//!
//! Two simple choices cover it:
//!
//! - the **profile** of the device (laptop, desktop, server, phone): how much data is asked for at
//!   once from another device, and how many files are written at the same time in total;
//! - the **storage** of each library (SSD or hard disk): on a hard disk files are written and read
//!   one after another, because working on several at once only makes the heads jump around.
//!
//! `custom` uses the numbers from the `[performance]` section of the config file. Speed limits are
//! separate and apply to every profile.
//!
//! None of this is sent to other devices. Each device decides for itself how much it asks for, so
//! a laptop syncing with a server is never pushed harder than its own profile allows.

use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Requests for data are counted in units of this many bytes.
pub const UNIT: usize = 64 * 1024;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Profile {
    Laptop,
    Desktop,
    Server,
    Phone,
    Custom,
}

impl Profile {
    pub const ALL: [Profile; 5] = [Profile::Laptop, Profile::Desktop, Profile::Server, Profile::Phone, Profile::Custom];

    pub fn parse(s: &str) -> Option<Profile> {
        match s.trim().to_lowercase().as_str() {
            "laptop" => Some(Profile::Laptop),
            "desktop" => Some(Profile::Desktop),
            "server" => Some(Profile::Server),
            "phone" => Some(Profile::Phone),
            "custom" => Some(Profile::Custom),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Profile::Laptop => "laptop",
            Profile::Desktop => "desktop",
            Profile::Server => "server",
            Profile::Phone => "phone",
            Profile::Custom => "custom",
        }
    }

    /// What a machine uses when its config file does not say.
    pub fn default_here() -> Profile {
        if cfg!(target_os = "android") {
            Profile::Phone
        } else {
            Profile::Desktop
        }
    }
}

/// What a library's folder is stored on.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Storage {
    #[default]
    Ssd,
    Hdd,
}

impl Storage {
    pub fn parse(s: &str) -> Option<Storage> {
        match s.trim().to_lowercase().replace([' ', '-', '_'], "").as_str() {
            "ssd" | "flash" | "nvme" => Some(Storage::Ssd),
            "hdd" | "harddisk" | "harddrive" | "disk" => Some(Storage::Hdd),
            _ => None,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Storage::Ssd => "ssd",
            Storage::Hdd => "hdd",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Storage::Ssd => "SSD",
            Storage::Hdd => "hard disk",
        }
    }
}

/// A first guess for a new library: is this folder on a spinning disk? Only Linux can tell, and
/// only for ordinary disks (not for network folders and the like); everything else counts as an
/// SSD. The owner can always say otherwise.
#[cfg(target_os = "linux")]
pub fn guess_storage(folder: &std::path::Path) -> Storage {
    use std::os::unix::fs::MetadataExt;
    let Ok(md) = std::fs::metadata(folder) else { return Storage::Ssd };
    let dev = md.dev();
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    // The device itself, or (for a partition) the disk it is part of.
    let Ok(sys) = std::fs::canonicalize(format!("/sys/dev/block/{}:{}", major, minor)) else { return Storage::Ssd };
    for dir in [Some(sys.as_path()), sys.parent()].into_iter().flatten() {
        if let Ok(text) = std::fs::read_to_string(dir.join("queue/rotational")) {
            return if text.trim() == "1" { Storage::Hdd } else { Storage::Ssd };
        }
    }
    Storage::Ssd
}

#[cfg(not(target_os = "linux"))]
pub fn guess_storage(_folder: &std::path::Path) -> Storage {
    Storage::Ssd
}

/// The numbers a profile stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tuning {
    /// Bytes asked for from one device and not yet written (the "window").
    pub window: u64,
    /// Files written at the same time, over all libraries.
    pub files: usize,
    /// Files read and hashed at the same time when a library on an SSD is scanned.
    pub hash_threads: usize,
}

const MIB: u64 = 1024 * 1024;
/// The window is never smaller than the largest piece of a file, or larger than this.
pub const MIN_WINDOW: u64 = 4 * MIB;
pub const MAX_WINDOW: u64 = 256 * MIB;
pub const MAX_FILES: usize = 16;

fn cores() -> usize {
    std::thread::available_parallelism().map(|n| n.get()).unwrap_or(2)
}

impl Tuning {
    /// The numbers for a profile on a machine with this many cores. `window` and `files` are
    /// only looked at for `custom` (missing ones are taken from `desktop`).
    pub fn of(profile: Profile, cores: usize, window: Option<u64>, files: Option<usize>) -> Tuning {
        let cores = cores.max(1);
        let t = match profile {
            Profile::Laptop => Tuning { window: 8 * MIB, files: 2, hash_threads: 1 },
            Profile::Desktop => Tuning { window: 16 * MIB, files: 3, hash_threads: (cores / 2).clamp(1, 4) },
            Profile::Server => Tuning { window: 64 * MIB, files: 8, hash_threads: cores.clamp(1, 8) },
            Profile::Phone => Tuning { window: 4 * MIB, files: 1, hash_threads: 1 },
            Profile::Custom => {
                let d = Tuning::of(Profile::Desktop, cores, None, None);
                Tuning { window: window.unwrap_or(d.window), files: files.unwrap_or(d.files), hash_threads: d.hash_threads }
            }
        };
        Tuning {
            window: t.window.clamp(MIN_WINDOW, MAX_WINDOW),
            files: t.files.clamp(1, MAX_FILES),
            hash_threads: t.hash_threads.clamp(1, 16),
        }
    }

    pub fn here(profile: Profile, window: Option<u64>, files: Option<usize>) -> Tuning {
        Tuning::of(profile, cores(), window, files)
    }

    /// The window in units (see `UNIT`).
    pub fn window_units(&self) -> u32 {
        (self.window / UNIT as u64).clamp(1, u32::MAX as u64 >> 4) as u32
    }

    /// (files written at once, files hashed at once) for a library on this kind of storage.
    pub fn for_library(&self, storage: Storage) -> (usize, usize) {
        match storage {
            Storage::Ssd => (self.files.min(4), self.hash_threads),
            Storage::Hdd => (1, 1),
        }
    }
}

/// How many units a request for `len` bytes takes of a window of `window_units`.
pub fn units(len: u64, window_units: u32) -> u32 {
    (len.div_ceil(UNIT as u64).max(1).min(u32::MAX as u64) as u32).min(window_units.max(1))
}

/// A speed limit: `take(n)` waits until `n` more bytes may pass. 0 = no limit.
pub struct Limiter {
    /// Bytes per second.
    rate: AtomicU64,
    /// When the next bytes may pass.
    next: Mutex<Instant>,
}

impl Limiter {
    pub fn new(rate: u64) -> Limiter {
        Limiter { rate: AtomicU64::new(rate), next: Mutex::new(Instant::now()) }
    }

    pub fn set(&self, rate: u64) {
        self.rate.store(rate, Ordering::Relaxed);
        *self.next.lock().unwrap() = Instant::now();
    }

    pub fn rate(&self) -> u64 {
        self.rate.load(Ordering::Relaxed)
    }

    /// How long to wait before `n` bytes may pass (and count them as passed).
    fn delay(&self, n: usize, now: Instant) -> Duration {
        let rate = self.rate();
        if rate == 0 || n == 0 {
            return Duration::ZERO;
        }
        let mut next = self.next.lock().unwrap();
        // Time that was not used is not saved up for more than a moment (no long bursts).
        let start = (*next).max(now.checked_sub(Duration::from_millis(100)).unwrap_or(now));
        *next = start + Duration::from_secs_f64(n as f64 / rate as f64);
        start.saturating_duration_since(now)
    }

    pub async fn take(&self, n: usize) {
        let wait = self.delay(n, Instant::now());
        if !wait.is_zero() {
            tokio::time::sleep(wait.min(Duration::from_secs(300))).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_have_names_and_sane_numbers() {
        for p in Profile::ALL {
            assert_eq!(Profile::parse(p.word()), Some(p));
            for cores in [1, 2, 4, 8, 64] {
                let t = Tuning::of(p, cores, None, None);
                assert!((MIN_WINDOW..=MAX_WINDOW).contains(&t.window), "{:?}", t);
                assert!((1..=MAX_FILES).contains(&t.files) && (1..=16).contains(&t.hash_threads), "{:?}", t);
                // one piece of the largest size always fits the window
                assert!(t.window_units() as u64 * UNIT as u64 >= crate::index::MAX_CHUNK as u64);
            }
        }
        assert_eq!(Profile::parse(" Server "), Some(Profile::Server));
        assert_eq!(Profile::parse("fast"), None);
        // a phone works less hard than a laptop, a laptop less than a server
        let (phone, laptop, server) = (
            Tuning::of(Profile::Phone, 8, None, None),
            Tuning::of(Profile::Laptop, 8, None, None),
            Tuning::of(Profile::Server, 8, None, None),
        );
        assert!(phone.window <= laptop.window && laptop.window < server.window);
        assert!(phone.files <= laptop.files && laptop.files < server.files);
    }

    #[test]
    fn custom_uses_the_given_numbers_within_bounds() {
        let t = Tuning::of(Profile::Custom, 4, Some(32 * MIB), Some(6));
        assert_eq!((t.window, t.files), (32 * MIB, 6));
        let t = Tuning::of(Profile::Custom, 4, Some(1), Some(0));
        assert_eq!((t.window, t.files), (MIN_WINDOW, 1));
        let t = Tuning::of(Profile::Custom, 4, Some(u64::MAX), Some(1000));
        assert_eq!((t.window, t.files), (MAX_WINDOW, MAX_FILES));
        let d = Tuning::of(Profile::Desktop, 4, None, None);
        assert_eq!(Tuning::of(Profile::Custom, 4, None, None), d);
    }

    #[test]
    fn a_hard_disk_gets_one_file_at_a_time() {
        let server = Tuning::of(Profile::Server, 16, None, None);
        assert_eq!(server.for_library(Storage::Hdd), (1, 1));
        let (files, threads) = server.for_library(Storage::Ssd);
        assert!(files > 1 && threads > 1);
        assert_eq!(Tuning::of(Profile::Phone, 8, None, None).for_library(Storage::Ssd), (1, 1));
        assert_eq!(Storage::parse("Hard disk"), Some(Storage::Hdd));
        assert_eq!(Storage::parse("SSD"), Some(Storage::Ssd));
        assert_eq!(Storage::parse("tape"), None);
        assert_eq!(Storage::default(), Storage::Ssd);
    }

    #[test]
    fn requests_are_counted_in_units() {
        assert_eq!(units(0, 100), 1);
        assert_eq!(units(1, 100), 1);
        assert_eq!(units(UNIT as u64, 100), 1);
        assert_eq!(units(UNIT as u64 + 1, 100), 2);
        assert_eq!(units(4 * MIB, 1000), 64);
        // never more than the whole window (or the request could never be made)
        assert_eq!(units(4 * MIB, 16), 16);
    }

    #[test]
    fn the_speed_limit_spaces_the_bytes_out() {
        let free = Limiter::new(0);
        let l = Limiter::new(1000); // 1000 bytes a second
        let now = Instant::now();
        assert_eq!(free.delay(1_000_000, now), Duration::ZERO);
        assert_eq!(l.delay(500, now), Duration::ZERO); // the first bytes pass at once
        let second = l.delay(500, now);
        assert!(second >= Duration::from_millis(350) && second <= Duration::from_millis(500), "{:?}", second);
        let third = l.delay(1, now);
        assert!(third >= Duration::from_millis(850) && third <= Duration::from_millis(1000), "{:?}", third);
        // after a pause it does not burst
        let later = now + Duration::from_secs(60);
        assert_eq!(l.delay(1000, later), Duration::ZERO);
        assert!(l.delay(1, later) >= Duration::from_millis(850));
        l.set(0);
        assert_eq!(l.delay(1_000_000, later), Duration::ZERO);
    }
}
