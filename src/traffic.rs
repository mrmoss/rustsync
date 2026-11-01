//! What is going to and from each device: bytes sent and received, the current speeds, and how
//! much of a library is still to be fetched (here) or still to be fetched by the other side
//! (as that side tells us). Only for showing in the window and `rustsync status`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// A library's changes that were announced by one device and are still being fetched from it.
#[derive(Default)]
struct Left {
    files: u64,
    bytes: u64,
    files_done: u64,
    bytes_got: u64,
}

#[derive(Default)]
pub struct Traffic {
    sent: AtomicU64,
    received: AtomicU64,
    /// Bytes per second (smoothed over a few seconds); `tick` updates them once a second.
    up_rate: AtomicU64,
    down_rate: AtomicU64,
    last: Mutex<(u64, u64)>,
    /// library id -> what we are still fetching from this device
    todo: Mutex<HashMap<String, Left>>,
    /// library id -> what this device says it is still fetching from us (files, bytes)
    remote: Mutex<HashMap<String, (u64, u64)>>,
    /// library id -> what we last told this device about our own `todo`
    reported: Mutex<HashMap<String, (u64, u64)>>,
}

impl Traffic {
    pub fn add_sent(&self, n: usize) {
        self.sent.fetch_add(n as u64, Ordering::Relaxed);
    }

    pub fn add_received(&self, n: usize) {
        self.received.fetch_add(n as u64, Ordering::Relaxed);
    }

    /// (sent, received) in total since the program started.
    pub fn totals(&self) -> (u64, u64) {
        (self.sent.load(Ordering::Relaxed), self.received.load(Ordering::Relaxed))
    }

    /// (upload, download) in bytes per second.
    pub fn rates(&self) -> (u64, u64) {
        (self.up_rate.load(Ordering::Relaxed), self.down_rate.load(Ordering::Relaxed))
    }

    /// Call once a second.
    pub fn tick(&self) {
        let (up, down) = self.totals();
        let mut last = self.last.lock().unwrap();
        let (du, dd) = (up.saturating_sub(last.0), down.saturating_sub(last.1));
        *last = (up, down);
        let smooth = |old: &AtomicU64, new: u64| old.store((old.load(Ordering::Relaxed) + new) / 2, Ordering::Relaxed);
        smooth(&self.up_rate, du);
        smooth(&self.down_rate, dd);
    }

    // ---- what we are fetching from this device

    /// A batch of changes from this device is waiting to be applied.
    pub fn queue(&self, lib: &str, files: u64, bytes: u64) {
        if files == 0 && bytes == 0 {
            return;
        }
        let mut m = self.todo.lock().unwrap();
        let l = m.entry(lib.to_string()).or_default();
        l.files = l.files.saturating_add(files);
        l.bytes = l.bytes.saturating_add(bytes);
    }

    /// Some data of a file arrived.
    pub fn got(&self, lib: &str, n: usize) {
        if let Some(l) = self.todo.lock().unwrap().get_mut(lib) {
            if l.bytes > 0 {
                l.bytes_got = l.bytes_got.saturating_add(n as u64);
            }
        }
    }

    /// One file of the batch is finished (applied or not).
    pub fn file_done(&self, lib: &str) {
        if let Some(l) = self.todo.lock().unwrap().get_mut(lib) {
            if l.files > 0 {
                l.files_done += 1;
            }
        }
    }

    /// The batch that was queued with these numbers is finished.
    pub fn finish(&self, lib: &str, files: u64, bytes: u64) {
        let mut m = self.todo.lock().unwrap();
        if let Some(l) = m.get_mut(lib) {
            l.files = l.files.saturating_sub(files);
            l.bytes = l.bytes.saturating_sub(bytes);
            l.files_done = 0;
            l.bytes_got = 0;
            if l.files == 0 && l.bytes == 0 {
                m.remove(lib);
            }
        }
    }

    /// (files, bytes) of a library still to be fetched from this device.
    pub fn left(&self, lib: &str) -> (u64, u64) {
        match self.todo.lock().unwrap().get(lib) {
            Some(l) => (l.files.saturating_sub(l.files_done), l.bytes.saturating_sub(l.bytes_got)),
            None => (0, 0),
        }
    }

    // ---- what the device says it is fetching from us

    pub fn set_remote(&self, lib: &str, files: u64, bytes: u64) {
        let mut m = self.remote.lock().unwrap();
        if files == 0 && bytes == 0 {
            m.remove(lib);
        } else {
            m.insert(lib.to_string(), (files, bytes));
        }
    }

    pub fn remote_left(&self, lib: &str) -> (u64, u64) {
        self.remote.lock().unwrap().get(lib).copied().unwrap_or((0, 0))
    }

    /// What changed in `left` since it was last told to the device: (library, files, bytes).
    /// A library that is finished is reported once with zeros.
    pub fn report(&self) -> Vec<(String, u64, u64)> {
        let now: HashMap<String, (u64, u64)> = {
            let m = self.todo.lock().unwrap();
            m.iter()
                .map(|(k, l)| (k.clone(), (l.files.saturating_sub(l.files_done), l.bytes.saturating_sub(l.bytes_got))))
                .collect()
        };
        let mut reported = self.reported.lock().unwrap();
        let mut out = Vec::new();
        for (lib, v) in &now {
            if reported.get(lib) != Some(v) {
                out.push((lib.clone(), v.0, v.1));
                reported.insert(lib.clone(), *v);
            }
        }
        let gone: Vec<String> = reported.keys().filter(|k| !now.contains_key(*k)).cloned().collect();
        for lib in gone {
            reported.remove(&lib);
            out.push((lib, 0, 0));
        }
        out
    }

    /// A new connection to the device: forget what the old one said.
    pub fn new_session(&self) {
        self.remote.lock().unwrap().clear();
        self.reported.lock().unwrap().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_counts_down_and_is_reported_once() {
        let t = Traffic::default();
        t.queue("lib", 3, 3000);
        assert_eq!(t.left("lib"), (3, 3000));
        t.got("lib", 1000);
        t.file_done("lib");
        assert_eq!(t.left("lib"), (2, 2000));
        assert_eq!(t.report(), vec![("lib".to_string(), 2, 2000)]);
        assert!(t.report().is_empty());
        t.finish("lib", 3, 3000);
        assert_eq!(t.left("lib"), (0, 0));
        assert_eq!(t.report(), vec![("lib".to_string(), 0, 0)]);
        assert!(t.report().is_empty());
        // data that arrives with nothing queued (a backup pull) changes nothing
        t.got("lib", 500);
        t.file_done("lib");
        t.queue("lib", 1, 100);
        assert_eq!(t.left("lib"), (1, 100));
    }

    #[test]
    fn rates_follow_the_traffic_and_fade() {
        let t = Traffic::default();
        t.add_sent(4000);
        t.add_received(1000);
        t.tick();
        assert_eq!(t.rates(), (2000, 500));
        for _ in 0..20 {
            t.tick();
        }
        assert_eq!(t.rates(), (0, 0));
        assert_eq!(t.totals(), (4000, 1000));
    }

    #[test]
    fn remote_progress_is_kept_and_cleared() {
        let t = Traffic::default();
        t.set_remote("lib", 2, 50);
        assert_eq!(t.remote_left("lib"), (2, 50));
        t.set_remote("lib", 0, 0);
        assert_eq!(t.remote_left("lib"), (0, 0));
        t.set_remote("lib", 1, 1);
        t.new_session();
        assert_eq!(t.remote_left("lib"), (0, 0));
    }
}
