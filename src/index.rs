//! Per-folder index: what we believe every path looks like, plus scanning the disk.
//!
//! Conflict rule: every entry carries a `stamp` (wall-clock nanoseconds of the change).
//! The entry with the higher (stamp, hash) wins; deletions are tombstones that compete
//! the same way, so a deleted file is not resurrected by a stale peer.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

pub const CHUNK: usize = 128 * 1024;
pub const TMP_PREFIX: &str = ".fsync-tmp-";
/// Largest file we sync: its chunk list must fit comfortably in one index message.
pub const MAX_CHUNKS: usize = 131_072;
pub const MAX_FILE_SIZE: u64 = MAX_CHUNKS as u64 * CHUNK as u64;
const MAX_PATH_BYTES: usize = 4096;
const MAX_NAME_BYTES: usize = 255;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Entry {
    /// Path relative to the folder root, '/' separated.
    pub path: String,
    pub kind: Kind,
    pub deleted: bool,
    pub mode: u32,
    pub size: u64,
    /// Local observation cache: size + mtime unchanged means "don't re-hash".
    pub mtime_ns: u64,
    /// Logical change time used for conflict resolution.
    pub stamp: u64,
    /// blake3 of the whole file (hex).
    pub hash: String,
    /// blake3 of each CHUNK-sized block (hex).
    pub chunks: Vec<String>,
}

impl Entry {
    pub fn key(&self) -> (u64, &str) {
        (self.stamp, self.hash.as_str())
    }
    pub fn is_live_file(&self) -> bool {
        !self.deleted && self.kind == Kind::File
    }
}

pub fn remote_wins(local: Option<&Entry>, remote: &Entry) -> bool {
    match local {
        None => true,
        Some(l) => remote.key() > l.key(),
    }
}

pub fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn next_stamp(old: Option<&Entry>) -> u64 {
    let now = now_ns();
    match old {
        // saturating: a bogus huge stamp must never wrap around to a small one
        Some(o) => now.max(o.stamp.saturating_add(1)),
        None => now,
    }
}

/// Reject anything a peer could use to escape the folder or touch our temp files: absolute
/// paths, empty / `.` / `..` components, NUL, backslashes, control characters, over-long
/// names, and our own temp-file names.
pub fn safe_rel(p: &str) -> bool {
    !p.is_empty()
        && p.len() <= MAX_PATH_BYTES
        && !p.starts_with('/')
        && !p.chars().any(|c| c.is_control() || c == '\\')
        && p.split('/').all(|c| {
            !c.is_empty() && c != "." && c != ".." && c.len() <= MAX_NAME_BYTES && !c.starts_with(TMP_PREFIX)
        })
}

fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|c| c.is_ascii_hexdigit())
}

/// Permissions we are willing to set because a peer asked: never setuid/setgid/sticky, never
/// group- or world-writable, and always usable by us (owner read/write, plus search for dirs).
pub fn safe_mode(kind: Kind, mode: u32) -> u32 {
    let m = mode & 0o755;
    match kind {
        Kind::Dir => m | 0o700,
        Kind::File => m | 0o600,
    }
}

/// Check and normalise an entry received from another device before it can touch anything.
/// `now` is the current time and `max_skew` how far into the future a change time may be
/// (ns): without that bound a peer with a wrong clock, or a hostile one, could stamp entries
/// so far ahead that every real edit afterwards would lose the conflict rule.
pub fn sanitize_remote(mut e: Entry, now: u64, max_skew: u64) -> Result<Entry, &'static str> {
    if !safe_rel(&e.path) {
        return Err("unsafe path");
    }
    if e.stamp > now.saturating_add(max_skew) {
        return Err("change time too far in the future (check the clocks)");
    }
    if e.deleted {
        if e.size != 0 || !e.chunks.is_empty() || !e.hash.is_empty() {
            return Err("malformed deletion");
        }
    } else {
        match e.kind {
            Kind::Dir => {
                if e.size != 0 || !e.chunks.is_empty() || !e.hash.is_empty() {
                    return Err("malformed directory");
                }
            }
            Kind::File => {
                if e.chunks.len() > MAX_CHUNKS || e.size > MAX_FILE_SIZE {
                    return Err("file too large");
                }
                if e.chunks.len() as u64 != e.size.div_ceil(CHUNK as u64) {
                    return Err("size does not match the number of chunks");
                }
                if !is_hex64(&e.hash) || !e.chunks.iter().all(|c| is_hex64(c)) {
                    return Err("malformed hash");
                }
            }
        }
    }
    e.mode = safe_mode(e.kind, e.mode);
    e.mtime_ns = e.mtime_ns.min(now.saturating_add(max_skew));
    Ok(e)
}

/// Open a file for reading without following a symlink in the last path component, and
/// confirm (through the open descriptor, so it cannot be fooled by a directory swapped for a
/// symlink after we looked) that it really lives inside `root`.
pub fn open_nofollow(path: &Path, root: &Path) -> std::io::Result<fs::File> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let f = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)?;
    if !fd_inside(f.as_raw_fd(), root) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "path leaves the library folder",
        ));
    }
    Ok(f)
}

/// Is the file behind this open descriptor located under `root`? This is the check that catches a
/// folder being swapped for a symlink between our checks and the open. Linux, Android and macOS can tell us
/// where an open descriptor really is; on the BSDs there is no portable way, so we rely on the
/// other safeguards there (O_NOFOLLOW and the ancestor checks).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn fd_inside(fd: std::os::fd::RawFd, root: &Path) -> bool {
    let proc_fd = Path::new("/proc/self/fd");
    if !proc_fd.is_dir() {
        return true;
    }
    match fs::read_link(proc_fd.join(fd.to_string())) {
        Ok(real) => {
            let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
            real.starts_with(&root)
        }
        Err(_) => false,
    }
}

#[cfg(target_os = "macos")]
pub fn fd_inside(fd: std::os::fd::RawFd, root: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;
    let mut buf = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes a NUL-terminated path of at most PATH_MAX bytes into the buffer.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr()) } == -1 {
        return false;
    }
    let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let real = Path::new(std::ffi::OsStr::from_bytes(&buf[..len]));
    let root = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    real.starts_with(&root)
}

#[cfg(not(any(target_os = "linux", target_os = "android", target_os = "macos")))]
pub fn fd_inside(_fd: std::os::fd::RawFd, _root: &Path) -> bool {
    true
}

pub fn under(p: &str, root: &str) -> bool {
    root.is_empty() || p == root || (p.starts_with(root) && p.as_bytes().get(root.len()) == Some(&b'/'))
}

pub fn mtime_of(md: &fs::Metadata) -> u64 {
    (md.mtime().max(0) as u64) * 1_000_000_000 + md.mtime_nsec().max(0) as u64
}

/// Does the file/dir on disk still look like what this index entry recorded?
pub fn disk_matches(abs: &Path, e: &Entry) -> bool {
    match fs::symlink_metadata(abs) {
        Ok(md) => match e.kind {
            Kind::File => md.is_file() && md.len() == e.size && mtime_of(&md) == e.mtime_ns,
            Kind::Dir => md.is_dir(),
        },
        Err(_) => false,
    }
}

pub struct Folder {
    pub id: String,
    pub root: PathBuf,
    state_path: PathBuf,
    pub index: Mutex<HashMap<String, Entry>>,
    /// How far ahead of our clock a peer's change times may be (ns).
    pub max_skew_ns: u64,
    /// Held while scanning and while committing remote changes, so the two never interleave.
    pub fs_lock: tokio::sync::Mutex<()>,
}

impl Folder {
    pub fn open(id: String, path: &str, state_dir: &Path, max_skew_ns: u64) -> Result<Folder> {
        fs::create_dir_all(path)?;
        let root = fs::canonicalize(path)?;
        fs::create_dir_all(state_dir)?;
        // One index file per library, named after the library id (see Node::remove_library).
        let state_path = state_dir.join(format!("folder-{}.json", id));
        let mut index = HashMap::new();
        if let Ok(bytes) = fs::read(&state_path) {
            match serde_json::from_slice::<Vec<Entry>>(&bytes) {
                Ok(list) => {
                    for e in list {
                        index.insert(e.path.clone(), e);
                    }
                }
                Err(e) => warn!("ignoring unreadable state file {}: {}", state_path.display(), e),
            }
        }
        Ok(Folder {
            id,
            root,
            state_path,
            index: Mutex::new(index),
            max_skew_ns,
            fs_lock: tokio::sync::Mutex::new(()),
        })
    }

    pub fn abs(&self, rel: &str) -> PathBuf {
        if rel.is_empty() {
            self.root.clone()
        } else {
            self.root.join(rel)
        }
    }

    pub fn get(&self, rel: &str) -> Option<Entry> {
        self.index.lock().unwrap().get(rel).cloned()
    }

    /// All entries, sorted so parents come before children.
    pub fn snapshot(&self) -> Vec<Entry> {
        let mut v: Vec<Entry> = self.index.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.path.cmp(&b.path));
        v
    }

    pub fn persist(&self, idx: &HashMap<String, Entry>) -> Result<()> {
        let tmp = self.state_path.with_extension("tmp");
        let list: Vec<&Entry> = idx.values().collect();
        fs::write(&tmp, serde_json::to_vec(&list)?)?;
        fs::rename(&tmp, &self.state_path)?;
        Ok(())
    }

    pub fn persist_now(&self) {
        let idx = self.index.lock().unwrap();
        if let Err(e) = self.persist(&idx) {
            warn!("cannot save state: {}", e);
        }
    }
}

/// Remove temp files left behind by a crash.
pub fn clean_tmp(dir: &Path) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            clean_tmp(&e.path());
        } else if ft.is_file() && e.file_name().to_string_lossy().starts_with(TMP_PREFIX) {
            let _ = fs::remove_file(e.path());
        }
    }
}

fn read_full<R: Read>(r: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        let k = r.read(&mut buf[n..])?;
        if k == 0 {
            break;
        }
        n += k;
    }
    Ok(n)
}

/// Returns (whole-file hash, per-chunk hashes, size).
pub fn hash_file(path: &Path, root: &Path) -> std::io::Result<(String, Vec<String>, u64)> {
    let mut f = open_nofollow(path, root)?;
    let mut whole = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
        chunks.push(blake3::hash(&buf[..n]).to_hex().to_string());
        total += n as u64;
        if n < CHUNK {
            break;
        }
    }
    Ok((whole.finalize().to_hex().to_string(), chunks, total))
}

/// Compare the disk against the index for the given roots ("" = whole folder),
/// update the index and return the entries that changed locally.
pub fn scan(folder: &Folder, roots: &[String], deep: bool) -> Vec<Entry> {
    let mut sorted = roots.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut kept: Vec<String> = Vec::new();
    for r in sorted {
        if !kept.iter().any(|k| under(&r, k)) {
            kept.push(r);
        }
    }

    let old: HashMap<String, Entry> = {
        let idx = folder.index.lock().unwrap();
        idx.iter()
            .filter(|(p, _)| kept.iter().any(|r| under(p.as_str(), r.as_str())))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut updates: Vec<Entry> = Vec::new();
    let mut changed: Vec<Entry> = Vec::new();
    for r in &kept {
        walk(folder, r, &old, deep, &mut seen, &mut updates, &mut changed);
    }

    for (p, o) in &old {
        if !o.deleted && !seen.contains(p) {
            let t = Entry {
                path: p.clone(),
                kind: o.kind,
                deleted: true,
                mode: o.mode,
                size: 0,
                mtime_ns: 0,
                stamp: next_stamp(Some(o)),
                hash: String::new(),
                chunks: Vec::new(),
            };
            updates.push(t.clone());
            changed.push(t);
        }
    }

    if !updates.is_empty() {
        let mut idx = folder.index.lock().unwrap();
        for e in updates {
            idx.insert(e.path.clone(), e);
        }
        if let Err(e) = folder.persist(&idx) {
            warn!("cannot save state: {}", e);
        }
    }
    changed
}

fn walk(
    folder: &Folder,
    rel: &str,
    old: &HashMap<String, Entry>,
    deep: bool,
    seen: &mut HashSet<String>,
    updates: &mut Vec<Entry>,
    changed: &mut Vec<Entry>,
) {
    if rel.split('/').any(|c| c.starts_with(TMP_PREFIX)) {
        return;
    }
    if !rel.is_empty() && !safe_rel(rel) {
        warn!("skipping {:?}: the name cannot be synced (control characters, backslash or too long)", rel);
        return;
    }
    let abs = folder.abs(rel);
    let md = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(_) => return, // gone: handled as a deletion by the caller
    };
    if md.file_type().is_symlink() {
        debug!("skipping symlink {}", rel);
        return;
    }
    let mode = md.mode() & 0o777;
    let prev = old.get(rel);

    if md.is_dir() {
        if !rel.is_empty() {
            seen.insert(rel.to_string());
            let same = matches!(prev, Some(o) if !o.deleted && o.kind == Kind::Dir && o.mode == mode);
            if !same {
                let e = Entry {
                    path: rel.to_string(),
                    kind: Kind::Dir,
                    deleted: false,
                    mode,
                    size: 0,
                    mtime_ns: 0,
                    stamp: next_stamp(prev),
                    hash: String::new(),
                    chunks: Vec::new(),
                };
                updates.push(e.clone());
                changed.push(e);
            }
        }
        let rd = match fs::read_dir(&abs) {
            Ok(r) => r,
            Err(e) => {
                warn!("cannot read dir {}: {}", rel, e);
                return;
            }
        };
        let mut names: Vec<String> = rd
            .filter_map(|e| e.ok())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        names.sort();
        for n in names {
            let child = if rel.is_empty() { n } else { format!("{}/{}", rel, n) };
            walk(folder, &child, old, deep, seen, updates, changed);
        }
    } else if md.is_file() {
        seen.insert(rel.to_string());
        if md.len() > MAX_FILE_SIZE {
            warn!("skipping {}: files over {} GiB are not synced", rel, MAX_FILE_SIZE >> 30);
            return;
        }
        let mtime = mtime_of(&md);

        // Cheap path: size + mtime unchanged.
        if let Some(o) = prev {
            if o.is_live_file() && !deep && o.size == md.len() && o.mtime_ns == mtime {
                if o.mode != mode {
                    let mut e = o.clone();
                    e.mode = mode;
                    e.stamp = next_stamp(prev);
                    updates.push(e.clone());
                    changed.push(e);
                }
                return;
            }
        }

        match hash_file(&abs, &folder.root) {
            Ok((hash, chunks, size)) => {
                if let Some(o) = prev {
                    if o.is_live_file() && o.hash == hash && o.mode == mode {
                        // Same content (e.g. only mtime moved): refresh the cache silently.
                        if o.mtime_ns != mtime || o.size != size {
                            let mut e = o.clone();
                            e.mtime_ns = mtime;
                            e.size = size;
                            updates.push(e);
                        }
                        return;
                    }
                }
                let e = Entry {
                    path: rel.to_string(),
                    kind: Kind::File,
                    deleted: false,
                    mode,
                    size,
                    mtime_ns: mtime,
                    stamp: next_stamp(prev),
                    hash,
                    chunks,
                };
                updates.push(e.clone());
                changed.push(e);
            }
            Err(e) => warn!("cannot read {}: {}", rel, e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_entry(size: u64) -> Entry {
        let n = size.div_ceil(CHUNK as u64) as usize;
        Entry {
            path: "a/b.txt".into(),
            kind: Kind::File,
            deleted: false,
            mode: 0o666,
            size,
            mtime_ns: 1,
            stamp: 1_000,
            hash: "0".repeat(64),
            chunks: vec!["1".repeat(64); n],
        }
    }

    #[test]
    fn safe_rel_rejects_escapes() {
        for bad in [
            "", "/", "/etc/passwd", "..", "../x", "a/../b", "a/./b", "./a", "a//b", "a/", "a\\b", "a\0b",
            "a\nb", "a/\u{7f}", ".fsync-tmp-1", "d/.fsync-tmp-1/x",
        ] {
            assert!(!safe_rel(bad), "{:?} must be rejected", bad);
        }
        assert!(!safe_rel(&"a".repeat(256)), "over-long name");
        assert!(!safe_rel(&format!("{}/x", "a/".repeat(2100))), "over-long path");
        for good in ["a", "a/b", "dir/file.txt", "ünï/cødé", "a b/c d", ".hidden", "..a", "a.."] {
            assert!(safe_rel(good), "{:?} must be accepted", good);
        }
    }

    #[test]
    fn modes_never_gain_dangerous_bits() {
        for mode in [0o000, 0o777, 0o666, 0o4755, 0o2755, 0o1777, 0o7777, 0o022] {
            let f = safe_mode(Kind::File, mode);
            let d = safe_mode(Kind::Dir, mode);
            assert_eq!(f & !0o755, 0);
            assert_eq!(d & !0o755, 0);
            assert_eq!(f & 0o600, 0o600, "owner can always read and write");
            assert_eq!(d & 0o700, 0o700, "owner can always enter a directory");
        }
    }

    #[test]
    fn sanitize_accepts_good_and_rejects_bad() {
        let now = 10_000_000_000u64;
        let skew = 1_000_000_000u64;
        assert!(sanitize_remote(file_entry(0), now, skew).is_ok());
        assert!(sanitize_remote(file_entry(1), now, skew).is_ok());
        assert!(sanitize_remote(file_entry(CHUNK as u64), now, skew).is_ok());
        assert!(sanitize_remote(file_entry(CHUNK as u64 + 1), now, skew).is_ok());

        let mut e = file_entry(1);
        e.chunks.clear(); // size says one chunk
        assert!(sanitize_remote(e, now, skew).is_err());

        let mut e = file_entry(1);
        e.chunks[0] = "zz".into();
        assert!(sanitize_remote(e, now, skew).is_err());

        let mut e = file_entry(1);
        e.path = "../x".into();
        assert!(sanitize_remote(e, now, skew).is_err());

        let mut e = file_entry(1);
        e.stamp = u64::MAX;
        assert!(sanitize_remote(e, now, skew).is_err(), "far-future stamp");
        let mut e = file_entry(1);
        e.stamp = now + skew + 1;
        assert!(sanitize_remote(e, now, skew).is_err());
        let mut e = file_entry(1);
        e.stamp = now + skew;
        assert!(sanitize_remote(e, now, skew).is_ok());

        let mut e = file_entry(0);
        e.size = MAX_FILE_SIZE + 1;
        assert!(sanitize_remote(e, now, skew).is_err());

        let mut e = file_entry(1);
        e.mode = 0o7777;
        assert_eq!(sanitize_remote(e, now, skew).unwrap().mode, 0o755);
    }

    #[test]
    fn stamps_cannot_overflow() {
        let mut old = file_entry(1);
        old.stamp = u64::MAX;
        assert_eq!(next_stamp(Some(&old)), u64::MAX);
    }
}
