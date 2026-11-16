//! What is going to and from each device: bytes sent and received, the current speeds, and how
//! much of a library is still to be fetched (here) or still to be fetched by the other side
//! (as that side tells us). Only for showing in the window and `rustsync status`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

/// One file that was announced by a device and has not arrived yet: its version, and how many
/// bytes of it are still to come.
struct Pending {
    stamp: u64,
    bytes: u64,
}

#[derive(Default)]
pub struct Traffic {
    sent: AtomicU64,
    received: AtomicU64,
    /// Bytes per second (smoothed over a few seconds); `tick` updates them once a second.
    up_rate: AtomicU64,
    down_rate: AtomicU64,
    last: Mutex<(u64, u64)>,
    /// library id -> path -> the files we are still to fetch from this device. Kept per file, so
    /// that a file leaves the count the moment it arrived, was deleted or was replaced, whatever
    /// else is still waiting.
    todo: Mutex<HashMap<String, HashMap<String, Pending>>>,
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

    /// A file this device announced is going to be fetched. Returns false if exactly this
    /// version was counted already (announced again before it arrived).
    pub fn queue_file(&self, lib: &str, path: &str, stamp: u64, bytes: u64) -> bool {
        let mut m = self.todo.lock().unwrap();
        let files = m.entry(lib.to_string()).or_default();
        if files.get(path).is_some_and(|p| p.stamp == stamp) {
            return false;
        }
        files.insert(path.to_string(), Pending { stamp, bytes });
        true
    }

    /// Some data of a file arrived.
    pub fn got(&self, lib: &str, path: &str, n: usize) {
        if let Some(p) = self.todo.lock().unwrap().get_mut(lib).and_then(|files| files.get_mut(path)) {
            p.bytes = p.bytes.saturating_sub(n as u64);
        }
    }

    /// This version of a file is finished: it arrived, was skipped or was given up.
    pub fn file_done(&self, lib: &str, path: &str, stamp: u64) {
        self.drop_if(lib, path, |p| p.stamp == stamp);
    }

    /// A newer change of the path arrived (it was deleted, say): what was waiting is not fetched.
    pub fn superseded(&self, lib: &str, path: &str, stamp: u64) {
        self.drop_if(lib, path, |p| p.stamp < stamp);
    }

    /// What is to be fetched from this device for a library, as the puller worked it out:
    /// (path, version, size). Files already counted keep what arrived of them; the rest is
    /// replaced.
    pub fn set_todo(&self, lib: &str, files: Vec<(String, u64, u64)>) {
        let mut m = self.todo.lock().unwrap();
        if files.is_empty() {
            m.remove(lib);
            return;
        }
        let old = m.remove(lib).unwrap_or_default();
        let mut new: HashMap<String, Pending> = HashMap::with_capacity(files.len());
        for (path, stamp, bytes) in files {
            let p = match old.get(&path) {
                Some(o) if o.stamp == stamp => Pending { stamp, bytes: o.bytes.min(bytes) },
                _ => Pending { stamp, bytes },
            };
            new.insert(path, p);
        }
        m.insert(lib.to_string(), new);
    }

    fn drop_if(&self, lib: &str, path: &str, test: impl Fn(&Pending) -> bool) {
        let mut m = self.todo.lock().unwrap();
        if let Some(files) = m.get_mut(lib) {
            if files.get(path).is_some_and(test) {
                files.remove(path);
            }
            if files.is_empty() {
                m.remove(lib);
            }
        }
    }

    fn sum(files: &HashMap<String, Pending>) -> (u64, u64) {
        (files.len() as u64, files.values().fold(0u64, |b, p| b.saturating_add(p.bytes)))
    }

    /// (files, bytes) of a library still to be fetched from this device.
    pub fn left(&self, lib: &str) -> (u64, u64) {
        self.todo.lock().unwrap().get(lib).map(Self::sum).unwrap_or((0, 0))
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
            m.iter().map(|(k, files)| (k.clone(), Self::sum(files))).collect()
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
        assert!(t.queue_file("lib", "a", 1, 1000));
        assert!(t.queue_file("lib", "b", 1, 1000));
        assert!(t.queue_file("lib", "c", 1, 1000));
        assert_eq!(t.left("lib"), (3, 3000));
        t.got("lib", "a", 600);
        assert_eq!(t.left("lib"), (3, 2400));
        t.file_done("lib", "a", 1);
        assert_eq!(t.left("lib"), (2, 2000));
        assert_eq!(t.report(), vec![("lib".to_string(), 2, 2000)]);
        assert!(t.report().is_empty());
        t.file_done("lib", "b", 1);
        t.file_done("lib", "c", 1);
        assert_eq!(t.left("lib"), (0, 0));
        assert_eq!(t.report(), vec![("lib".to_string(), 0, 0)]);
        assert!(t.report().is_empty());
        // data that arrives with nothing queued (a backup pull) changes nothing
        t.got("lib", "x", 500);
        t.file_done("lib", "x", 1);
        assert_eq!(t.left("lib"), (0, 0));
    }

    #[test]
    fn a_file_is_counted_once_and_leaves_when_it_is_deleted_or_replaced() {
        let t = Traffic::default();
        assert!(t.queue_file("lib", "big", 5, 7_000));
        // announced again before it arrived: not counted twice
        assert!(!t.queue_file("lib", "big", 5, 7_000));
        assert_eq!(t.left("lib"), (1, 7_000));
        for i in 0..1200 {
            t.queue_file("lib", &format!("dir/f{}", i), 6, 10);
        }
        assert_eq!(t.left("lib"), (1201, 19_000));
        // the folder is deleted while the big file is still on its way
        for i in 0..1200 {
            t.superseded("lib", &format!("dir/f{}", i), 9);
        }
        assert_eq!(t.left("lib"), (1, 7_000));
        // an older deletion does not remove a newer version that is waiting
        t.superseded("lib", "big", 4);
        assert_eq!(t.left("lib"), (1, 7_000));
        // a newer version replaces the one that was waiting; finishing the old one changes nothing
        assert!(t.queue_file("lib", "big", 8, 9_000));
        t.file_done("lib", "big", 5);
        assert_eq!(t.left("lib"), (1, 9_000));
        t.file_done("lib", "big", 8);
        assert_eq!(t.left("lib"), (0, 0));
    }

    #[test]
    fn the_puller_sets_what_is_left() {
        let t = Traffic::default();
        t.set_todo("lib", vec![("a".into(), 1, 1000), ("b".into(), 1, 500)]);
        t.got("lib", "a", 400);
        assert_eq!(t.left("lib"), (2, 1100));
        // worked out again: what arrived of a stays, b is gone (deleted), c is new
        t.set_todo("lib", vec![("a".into(), 1, 1000), ("c".into(), 2, 10)]);
        assert_eq!(t.left("lib"), (2, 610));
        // a newer version of a starts from the beginning
        t.set_todo("lib", vec![("a".into(), 3, 2000)]);
        assert_eq!(t.left("lib"), (1, 2000));
        t.set_todo("lib", vec![]);
        assert_eq!(t.left("lib"), (0, 0));
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
