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
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use crate::ignore::{Ignore, IGNORE_FILE, MAX_FILE_LEN};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tracing::{debug, warn};

/// Files are cut into equal pieces ("chunks"); the size of the pieces depends only on the size of
/// the file, so every device cuts a file the same way: small pieces for small files (little is
/// sent again after a change), large pieces for large files (far fewer requests and hashes).
pub const SMALL_CHUNK: usize = 128 * 1024;
pub const MEDIUM_CHUNK: usize = 1024 * 1024;
pub const MAX_CHUNK: usize = 4 * 1024 * 1024;
const SMALL_FILE: u64 = 64 * 1024 * 1024;
const MEDIUM_FILE: u64 = 16 * 1024 * 1024 * 1024;
pub const TMP_PREFIX: &str = ".fsync-tmp-";
/// Files another device deleted wait here for a few minutes (in the top folder of the library;
/// never synced, like every temp file): if the same content turns up under another name, the
/// file was renamed or moved, and it is put back under the new name instead of fetched again.
const STASH_DIR: &str = ".fsync-tmp-stash";
pub const STASH_KEEP: Duration = Duration::from_secs(300);
const MAX_STASH: usize = 20_000;

struct Stashed {
    path: PathBuf,
    size: u64,
    at: Instant,
}
/// Largest file we sync: its chunk list must fit comfortably in one index message.
pub const MAX_CHUNKS: usize = 131_072;
pub const MAX_FILE_SIZE: u64 = MAX_CHUNKS as u64 * MAX_CHUNK as u64;

/// The size of the pieces a file of this size is cut into.
pub fn chunk_size(file_size: u64) -> usize {
    if file_size <= SMALL_FILE {
        SMALL_CHUNK
    } else if file_size <= MEDIUM_FILE {
        MEDIUM_CHUNK
    } else {
        MAX_CHUNK
    }
}

/// How many pieces a file of this size has.
pub fn chunk_count(file_size: u64) -> u64 {
    file_size.div_ceil(chunk_size(file_size) as u64)
}
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
    /// blake3 of each piece (hex); see `chunk_size`.
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

/// The version of something a scan finds for the first time: its change time on disk (never
/// later than now). A file this machine only lost track of (it was received, but the index was
/// not saved before the program was stopped, for example) is then older than whatever happened to
/// it elsewhere since, such as being deleted, instead of looking like a brand-new change.
fn first_stamp(mtime_ns: u64) -> u64 {
    mtime_ns.clamp(1, now_ns().max(1))
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
    // Lowercase only: every hash we compute ourselves is lowercase, so an uppercase one could
    // never verify (and would be fetched again and again).
    s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
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
        // A deleted file may carry the hash of what was deleted (so a copy that is still the same
        // can be recognised); nothing else.
        let bad_hash = !e.hash.is_empty() && (e.kind == Kind::Dir || !is_hex64(&e.hash));
        if e.size != 0 || !e.chunks.is_empty() || bad_hash {
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
                if e.chunks.len() as u64 != chunk_count(e.size) {
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
            "cannot confirm the file is inside the library folder (or it is not; on Linux this needs /proc mounted)",
        ));
    }
    Ok(f)
}

/// Is the file behind this open descriptor located under `root`? This is the check that catches a
/// folder being swapped for a symlink between our checks and the open. Linux, Android and macOS can tell us
/// where an open descriptor really is (and say "no" when they cannot, e.g. no /proc); on the BSDs there is no portable way, so we rely on the
/// other safeguards there (O_NOFOLLOW and the ancestor checks).
#[cfg(any(target_os = "linux", target_os = "android"))]
pub fn fd_inside(fd: std::os::fd::RawFd, root: &Path) -> bool {
    let proc_fd = Path::new("/proc/self/fd");
    // Without /proc we cannot tell where the descriptor points, so we cannot vouch for it:
    // refuse rather than assume it is fine (a container without /proc mounted will see errors).
    if !proc_fd.is_dir() {
        return false;
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
    /// How many files of this library are written at the same time (1 on a hard disk).
    pub files_at_once: AtomicUsize,
    /// How many files are read and hashed at the same time when scanning (1 on a hard disk).
    pub hash_threads: AtomicUsize,
    /// Can files here keep their permission bits? Not on Android's shared storage, FAT/exFAT
    /// drives and many network shares. There permissions are neither applied nor compared, so a
    /// permission the disk cannot hold never looks like a change of the file.
    pub perms: bool,
    /// The ignore patterns, and the (change time, size) of the file they were read from.
    ignore: Mutex<(Option<(u64, u64)>, Arc<Ignore>)>,
    /// Deleted files that are kept for a moment, by the hash of their content (see STASH_DIR).
    stash: Mutex<HashMap<String, Vec<Stashed>>>,
    stash_seq: AtomicUsize,
}

/// Try whether permission bits set on a file in `root` stay as they were set.
fn holds_permissions(root: &Path) -> bool {
    use std::os::unix::fs::OpenOptionsExt;
    let probe = root.join(format!("{}perm-{}", TMP_PREFIX, std::process::id()));
    let _ = fs::remove_file(&probe);
    let made = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&probe);
    if made.is_err() {
        return true; // nothing can be written here anyway
    }
    let ok = [0o600u32, 0o640, 0o604].iter().all(|&m| {
        fs::set_permissions(&probe, fs::Permissions::from_mode(m)).is_ok()
            && fs::symlink_metadata(&probe).map(|md| md.mode() & 0o777 == m).unwrap_or(false)
    });
    let _ = fs::remove_file(&probe);
    ok
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
                    for mut e in list {
                        // Indexed by a version that cut files differently: forget what we saw
                        // of the file, so the next scan reads it again.
                        if e.is_live_file() && e.chunks.len() as u64 != chunk_count(e.size) {
                            e.mtime_ns = 0;
                        }
                        index.insert(e.path.clone(), e);
                    }
                }
                Err(e) => warn!("ignoring unreadable state file {}: {}", state_path.display(), e),
            }
        }
        let perms = holds_permissions(&root);
        Ok(Folder {
            id,
            root,
            state_path,
            index: Mutex::new(index),
            max_skew_ns,
            fs_lock: tokio::sync::Mutex::new(()),
            files_at_once: AtomicUsize::new(2),
            hash_threads: AtomicUsize::new(1),
            perms,
            ignore: Mutex::new((None, Arc::new(Ignore::default()))),
            stash: Mutex::new(HashMap::new()),
            stash_seq: AtomicUsize::new(0),
        })
    }

    /// Instead of deleting a file right away, keep it aside for a few minutes under the hash of
    /// its content. False if that is not possible (the caller then deletes it).
    pub fn stash_file(&self, abs: &Path, hash: &str, size: u64) -> bool {
        if hash.is_empty() || size == 0 {
            return false;
        }
        let dir = self.root.join(STASH_DIR);
        match fs::symlink_metadata(&dir) {
            Ok(md) if md.is_dir() => {}
            Ok(_) => return false,
            Err(_) => {
                use std::os::unix::fs::DirBuilderExt;
                if fs::DirBuilder::new().mode(0o700).create(&dir).is_err() {
                    return false;
                }
            }
        }
        let mut stash = self.stash.lock().unwrap();
        if stash.values().map(|v| v.len()).sum::<usize>() >= MAX_STASH {
            return false;
        }
        let n = self.stash_seq.fetch_add(1, Ordering::Relaxed);
        let to = dir.join(format!("{}{}-{}", TMP_PREFIX, std::process::id(), n));
        if fs::rename(abs, &to).is_err() {
            return false;
        }
        stash.entry(hash.to_string()).or_default().push(Stashed { path: to, size, at: Instant::now() });
        true
    }

    /// A file with this content that was deleted a moment ago, if there is one: it is the
    /// caller's now (to move into place, or to remove).
    pub fn take_stashed(&self, hash: &str, size: u64) -> Option<PathBuf> {
        if hash.is_empty() || size == 0 {
            return None;
        }
        let found = {
            let mut stash = self.stash.lock().unwrap();
            let mut list = stash.remove(hash)?;
            let found = list.iter().position(|s| s.size == size).map(|i| list.swap_remove(i));
            if !list.is_empty() {
                stash.insert(hash.to_string(), list);
            }
            found?
        };
        match fs::symlink_metadata(&found.path) {
            Ok(md) if md.is_file() && md.len() == size => Some(found.path),
            _ => None,
        }
    }

    /// Really delete what was kept aside for longer than `keep`.
    pub fn purge_stash(&self, keep: Duration) {
        let mut stash = self.stash.lock().unwrap();
        if stash.is_empty() {
            return;
        }
        let now = Instant::now();
        for list in stash.values_mut() {
            list.retain(|s| {
                let old = now.duration_since(s.at) >= keep;
                if old {
                    let _ = fs::remove_file(&s.path);
                }
                !old
            });
        }
        stash.retain(|_, list| !list.is_empty());
        if stash.is_empty() {
            let _ = fs::remove_dir(self.root.join(STASH_DIR));
        }
    }

    /// The ignore patterns of this library (`.rustsyncignore` in its top folder), read again
    /// whenever the file changed.
    pub fn ignores(&self) -> Arc<Ignore> {
        let path = self.root.join(IGNORE_FILE);
        let stamp = fs::symlink_metadata(&path)
            .ok()
            .filter(|md| md.is_file() && md.len() <= MAX_FILE_LEN)
            .map(|md| (mtime_of(&md), md.len()));
        let mut cached = self.ignore.lock().unwrap();
        if cached.0 == stamp && (stamp.is_some() || cached.1.is_empty()) {
            return cached.1.clone();
        }
        let text = match stamp {
            None => String::new(),
            Some(_) => open_nofollow(&path, &self.root)
                .and_then(|mut f| {
                    let mut s = String::new();
                    std::io::Read::read_to_string(&mut f, &mut s).map(|_| s)
                })
                .unwrap_or_default(),
        };
        *cached = (stamp, Arc::new(Ignore::parse(&text)));
        cached.1.clone()
    }

    /// Is this folder, deleted in our index, kept only by downloads that are being given up
    /// (our temp files, or folders like it)? Then it is as good as deleted: it goes as soon as
    /// they are cleaned up.
    pub fn leftovers_only(&self, rel: &str) -> bool {
        fn check(f: &Folder, rel: &str, depth: usize) -> bool {
            if depth > 64 {
                return false;
            }
            let Ok(rd) = fs::read_dir(f.abs(rel)) else { return false };
            let mut any = false;
            for ent in rd {
                let Ok(ent) = ent else { return false };
                let Ok(name) = ent.file_name().into_string() else { return false };
                any = true;
                if name.starts_with(TMP_PREFIX) {
                    continue;
                }
                let child = if rel.is_empty() { name } else { format!("{}/{}", rel, name) };
                let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let deleted = matches!(f.get(&child), Some(e) if e.deleted);
                if !(is_dir && deleted && check(f, &child, depth + 1)) {
                    return false;
                }
            }
            any
        }
        !rel.is_empty() && check(self, rel, 0)
    }

    /// The version (stamp and hash) recorded for a path, without copying the whole entry.
    pub fn key_of(&self, rel: &str) -> Option<(u64, String)> {
        self.index.lock().unwrap().get(rel).map(|e| (e.stamp, e.hash.clone()))
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
    // The size decides how the file is cut, so it has to be known before reading.
    let chunk = chunk_size(f.metadata()?.len());
    let mut whole = blake3::Hasher::new();
    let mut chunks = Vec::new();
    let mut buf = vec![0u8; chunk];
    let mut total = 0u64;
    loop {
        let n = read_full(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
        chunks.push(blake3::hash(&buf[..n]).to_hex().to_string());
        total += n as u64;
        if n < chunk {
            break;
        }
    }
    if chunk_size(total) != chunk || chunks.len() as u64 != chunk_count(total) {
        // It grew or shrank while we read it: the next scan reads it again.
        return Err(std::io::Error::new(std::io::ErrorKind::Other, "the file changed while it was read"));
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
    let mut to_hash: Vec<ToHash> = Vec::new();
    folder.purge_stash(STASH_KEEP);
    let ign = folder.ignores();
    for r in &kept {
        walk(folder, &ign, r, &old, deep, &mut seen, &mut updates, &mut changed, &mut to_hash);
    }

    // Read and hash the files that look new or changed: several at once on an SSD, one after
    // another on a hard disk (where reading several files at once only makes the heads jump).
    let threads = folder.hash_threads.load(Ordering::Relaxed).clamp(1, 16);
    let hashes = hash_all(folder, &to_hash, threads);
    for (item, hashed) in to_hash.iter().zip(hashes) {
        let prev = old.get(&item.rel);
        match hashed {
            Ok((hash, chunks, size)) => {
                if let Some(o) = prev {
                    if o.is_live_file() && o.hash == hash && o.mode == item.mode && o.chunks == chunks {
                        // Same content (e.g. only mtime moved): refresh the cache silently.
                        if o.mtime_ns != item.mtime || o.size != size {
                            let mut e = o.clone();
                            e.mtime_ns = item.mtime;
                            e.size = size;
                            updates.push(e);
                        }
                        continue;
                    }
                }
                let e = Entry {
                    path: item.rel.clone(),
                    kind: Kind::File,
                    deleted: false,
                    mode: item.mode,
                    size,
                    mtime_ns: item.mtime,
                    stamp: if prev.is_none() { first_stamp(item.mtime) } else { next_stamp(prev) },
                    hash,
                    chunks,
                };
                updates.push(e.clone());
                changed.push(e);
            }
            Err(e) => warn!("cannot read {}: {}", item.rel, e),
        }
    }

    // Ignored now: forgotten here, without telling anyone it is gone (their copies stay).
    let forget: Vec<String> = old.iter().filter(|(p, o)| ign.is_ignored(p, o.kind == Kind::Dir)).map(|(p, _)| p.clone()).collect();
    for (p, o) in &old {
        if !o.deleted && !seen.contains(p) && !ign.is_ignored(p, o.kind == Kind::Dir) {
            let t = Entry {
                path: p.clone(),
                kind: o.kind,
                deleted: true,
                mode: o.mode,
                size: 0,
                mtime_ns: 0,
                stamp: next_stamp(Some(o)),
                // What was deleted: another device deletes its copy only if it is the same.
                hash: if o.kind == Kind::File { o.hash.clone() } else { String::new() },
                chunks: Vec::new(),
            };
            updates.push(t.clone());
            changed.push(t);
        }
    }

    if !updates.is_empty() || !forget.is_empty() {
        let mut idx = folder.index.lock().unwrap();
        for p in &forget {
            idx.remove(p);
        }
        for e in updates {
            idx.insert(e.path.clone(), e);
        }
        if let Err(e) = folder.persist(&idx) {
            warn!("cannot save state: {}", e);
        }
    }
    changed
}

/// A file the scan has to read: it is new, or its size or change time moved.
struct ToHash {
    rel: String,
    abs: PathBuf,
    mode: u32,
    mtime: u64,
}

type Hashed = std::io::Result<(String, Vec<String>, u64)>;

/// Hash the files, `threads` at a time; the results are in the order of `items`.
fn hash_all(folder: &Folder, items: &[ToHash], threads: usize) -> Vec<Hashed> {
    if threads <= 1 || items.len() <= 1 {
        return items.iter().map(|i| hash_file(&i.abs, &folder.root)).collect();
    }
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, Hashed)>> = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|s| {
        for _ in 0..threads.min(items.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(item) = items.get(i) else { break };
                let r = hash_file(&item.abs, &folder.root);
                done.lock().unwrap().push((i, r));
            });
        }
    });
    let mut done = done.into_inner().unwrap();
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

#[allow(clippy::too_many_arguments)]
fn walk(
    folder: &Folder,
    ign: &Ignore,
    rel: &str,
    old: &HashMap<String, Entry>,
    deep: bool,
    seen: &mut HashSet<String>,
    updates: &mut Vec<Entry>,
    changed: &mut Vec<Entry>,
    to_hash: &mut Vec<ToHash>,
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
    if ign.is_ignored(rel, md.is_dir()) {
        return;
    }
    let prev = old.get(rel);
    let kind = if md.is_dir() { Kind::Dir } else { Kind::File };
    // Where the disk cannot keep permissions, they are whatever the last version said (or the
    // usual ones for something new): what the disk reports is not a change.
    let mode = if folder.perms {
        md.mode() & 0o777
    } else {
        prev.filter(|o| !o.deleted && o.kind == kind)
            .map(|o| o.mode)
            .unwrap_or(if kind == Kind::Dir { 0o755 } else { 0o644 })
    };

    if md.is_dir() {
        if matches!(prev, Some(o) if o.deleted) && folder.leftovers_only(rel) {
            return; // deleted, and only kept by downloads that are being given up
        }
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
                    stamp: if prev.is_none() { first_stamp(mtime_of(&md)) } else { next_stamp(prev) },
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
            walk(folder, ign, &child, old, deep, seen, updates, changed, to_hash);
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

        to_hash.push(ToHash { rel: rel.to_string(), abs, mode, mtime });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashes_must_be_lowercase_hex() {
        assert!(is_hex64(&"a".repeat(64)));
        assert!(is_hex64(&"0123456789abcdef".repeat(4)));
        assert!(!is_hex64(&"A".repeat(64)));
        assert!(!is_hex64(&"a".repeat(63)));
        assert!(!is_hex64(&format!("{}g", "a".repeat(63))));
    }

    fn file_entry(size: u64) -> Entry {
        let n = chunk_count(size) as usize;
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
        assert!(sanitize_remote(file_entry(SMALL_CHUNK as u64), now, skew).is_ok());
        assert!(sanitize_remote(file_entry(SMALL_CHUNK as u64 + 1), now, skew).is_ok());
        // a large file has large pieces; a list cut the old way is refused
        assert!(sanitize_remote(file_entry(SMALL_FILE + 1), now, skew).is_ok());
        let mut e = file_entry(SMALL_FILE + 1);
        e.chunks = vec!["1".repeat(64); (SMALL_FILE + 1).div_ceil(SMALL_CHUNK as u64) as usize];
        assert!(sanitize_remote(e, now, skew).is_err());

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
    fn files_are_cut_by_their_size() {
        assert_eq!(chunk_size(0), SMALL_CHUNK);
        assert_eq!(chunk_size(SMALL_FILE), SMALL_CHUNK);
        assert_eq!(chunk_size(SMALL_FILE + 1), MEDIUM_CHUNK);
        assert_eq!(chunk_size(MEDIUM_FILE), MEDIUM_CHUNK);
        assert_eq!(chunk_size(MEDIUM_FILE + 1), MAX_CHUNK);
        assert_eq!(chunk_count(0), 0);
        assert_eq!(chunk_count(1), 1);
        assert_eq!(chunk_count(SMALL_FILE), 512);
        assert_eq!(chunk_count(SMALL_FILE + 1), 65);
        // the largest file still fits the list of pieces
        assert!(chunk_count(MAX_FILE_SIZE) <= MAX_CHUNKS as u64);
    }

    #[test]
    fn scanning_with_several_threads_finds_the_same_files() {
        let dir = std::env::temp_dir().join(format!("rustsync-scan-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (data, state) = (dir.join("data"), dir.join("state"));
        fs::create_dir_all(data.join("sub")).unwrap();
        for i in 0..20u8 {
            fs::write(data.join(format!("f{}", i)), vec![i; 1000 + i as usize]).unwrap();
        }
        fs::write(data.join("sub/big"), vec![7u8; SMALL_CHUNK * 2 + 5]).unwrap();
        let hashes = |threads: usize, id: &str| {
            let folder = Folder::open(id.to_string(), data.to_str().unwrap(), &state, 0).unwrap();
            folder.hash_threads.store(threads, Ordering::Relaxed);
            let mut found: Vec<(String, String, usize)> = scan(&folder, &[String::new()], false)
                .into_iter()
                .filter(|e| e.kind == Kind::File)
                .map(|e| (e.path, e.hash, e.chunks.len()))
                .collect();
            found.sort();
            found
        };
        let one = hashes(1, "one");
        assert_eq!(one.len(), 21);
        assert_eq!(one, hashes(4, "four"));
        assert_eq!(one.iter().find(|e| e.0 == "sub/big").unwrap().2, 3);
        let _ = fs::remove_dir_all(&dir);
    }

    fn test_folder(name: &str) -> (PathBuf, PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("rustsync-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let (data, state) = (dir.join("data"), dir.join("state"));
        fs::create_dir_all(&data).unwrap();
        (dir, data, state)
    }

    #[test]
    fn ignored_files_are_not_announced_and_not_deleted_elsewhere() {
        let (dir, data, state) = test_folder("ignore");
        fs::create_dir_all(data.join("target/debug")).unwrap();
        fs::write(data.join("target/debug/big"), b"build output").unwrap();
        fs::write(data.join("src.rs"), b"fn main() {}").unwrap();
        let folder = Folder::open("ignore".into(), data.to_str().unwrap(), &state, 0).unwrap();
        let first = scan(&folder, &[String::new()], false);
        assert!(first.iter().any(|e| e.path == "target/debug/big"));
        // now ignore it: it is forgotten here, but no deletion goes out
        fs::write(data.join(IGNORE_FILE), "target/\n").unwrap();
        let second = scan(&folder, &[String::new()], false);
        assert!(second.iter().all(|e| !e.path.starts_with("target")), "{:?}", second);
        assert!(second.iter().any(|e| e.path == IGNORE_FILE));
        assert!(folder.get("target/debug/big").is_none() && folder.get("target").is_none());
        assert!(data.join("target/debug/big").exists());
        // and later changes in it are not seen
        fs::write(data.join("target/new"), b"x").unwrap();
        assert!(scan(&folder, &[String::new()], false).is_empty());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleted_files_wait_a_moment_and_can_be_taken_back() {
        let (dir, data, state) = test_folder("stash");
        fs::write(data.join("a"), b"hello").unwrap();
        fs::write(data.join("b"), b"other").unwrap();
        let folder = Folder::open("stash".into(), data.to_str().unwrap(), &state, 0).unwrap();
        assert!(folder.stash_file(&data.join("a"), "h1", 5));
        assert!(!data.join("a").exists());
        // not seen by a scan, in either direction
        let seen = scan(&folder, &[String::new()], false);
        assert_eq!(seen.iter().map(|e| e.path.as_str()).collect::<Vec<_>>(), ["b"]);
        // taken back by content (and size), once
        assert!(folder.take_stashed("h1", 6).is_none());
        let back = folder.take_stashed("h1", 5).unwrap();
        assert_eq!(fs::read(&back).unwrap(), b"hello");
        assert!(folder.take_stashed("h1", 5).is_none());
        fs::remove_file(&back).unwrap();
        // what nobody takes back is deleted after a while
        assert!(folder.stash_file(&data.join("b"), "h2", 5));
        folder.purge_stash(Duration::from_secs(3600));
        fs::remove_file(folder.take_stashed("h2", 5).unwrap()).unwrap();
        fs::write(data.join("c"), b"third").unwrap();
        assert!(folder.stash_file(&data.join("c"), "h3", 5));
        folder.purge_stash(Duration::ZERO);
        assert!(folder.take_stashed("h3", 5).is_none());
        assert!(!data.join(STASH_DIR).exists());
        // empty files and unknown content are not kept
        fs::write(data.join("e"), b"").unwrap();
        assert!(!folder.stash_file(&data.join("e"), "h4", 0));
        assert!(!folder.stash_file(&data.join("e"), "", 3));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn deletions_say_what_was_deleted() {
        let (dir, data, state) = test_folder("tomb");
        fs::write(data.join("a"), b"hello").unwrap();
        fs::create_dir_all(data.join("d")).unwrap();
        let folder = Folder::open("tomb".into(), data.to_str().unwrap(), &state, 0).unwrap();
        let first = scan(&folder, &[String::new()], false);
        let hash = first.iter().find(|e| e.path == "a").unwrap().hash.clone();
        fs::remove_file(data.join("a")).unwrap();
        fs::remove_dir(data.join("d")).unwrap();
        let gone = scan(&folder, &[String::new()], false);
        let a = gone.iter().find(|e| e.path == "a").unwrap();
        assert!(a.deleted && a.hash == hash);
        let d = gone.iter().find(|e| e.path == "d").unwrap();
        assert!(d.deleted && d.hash.is_empty());
        // and such deletions are accepted from other devices
        let now = now_ns();
        assert!(sanitize_remote(a.clone(), now, 0).is_ok());
        let mut bad = d.clone();
        bad.hash = hash.clone();
        assert!(sanitize_remote(bad, now, 0).is_err());
        let mut bad = a.clone();
        bad.hash = "X".repeat(64);
        assert!(sanitize_remote(bad, now, 0).is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_found_for_the_first_time_is_as_old_as_its_change_time() {
        let (dir, data, state) = test_folder("first");
        let old = SystemTime::now() - Duration::from_secs(3600 * 24 * 30);
        let f = fs::File::create(data.join("old")).unwrap();
        f.set_modified(old).unwrap();
        drop(f);
        fs::write(data.join("new"), b"x").unwrap();
        let folder = Folder::open("first".into(), data.to_str().unwrap(), &state, 0).unwrap();
        let found = scan(&folder, &[String::new()], false);
        let o = found.iter().find(|e| e.path == "old").unwrap();
        assert_eq!(o.stamp, o.mtime_ns);
        assert!(o.stamp < now_ns() - 1_000_000_000 * 3600);
        // a later change of a known file is a new version, newer than anything before
        fs::write(data.join("old"), b"changed").unwrap();
        let changed = scan(&folder, &[String::new()], false);
        let o2 = changed.iter().find(|e| e.path == "old").unwrap();
        assert!(o2.stamp > o.stamp && o2.stamp >= now_ns() - 60 * 1_000_000_000);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn permissions_the_disk_cannot_keep_are_not_changes() {
        let (dir, data, state) = test_folder("perms");
        fs::write(data.join("f"), b"x").unwrap();
        fs::set_permissions(data.join("f"), fs::Permissions::from_mode(0o644)).unwrap();
        let mut folder = Folder::open("perms".into(), data.to_str().unwrap(), &state, 0).unwrap();
        assert!(folder.perms, "a normal temp folder keeps permissions");
        scan(&folder, &[String::new()], false);
        fs::set_permissions(data.join("f"), fs::Permissions::from_mode(0o660)).unwrap();
        folder.perms = false;
        assert!(scan(&folder, &[String::new()], false).is_empty());
        assert!(scan(&folder, &[String::new()], true).is_empty());
        folder.perms = true;
        let changed = scan(&folder, &[String::new()], false);
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].mode, 0o660);
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn stamps_cannot_overflow() {
        let mut old = file_entry(1);
        old.stamp = u64::MAX;
        assert_eq!(next_stamp(Some(&old)), u64::MAX);
    }
}
