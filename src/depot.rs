//! One `Depot` is one library's encrypted backup, in a folder the owner of this machine picked.
//! It has no key and cannot read anything it stores. (Which devices may use a library is
//! decided by the node before a request reaches this module.)
//!
//! Layout: blobs/<aa>/<id>              immutable encrypted chunk
//!         records/<slot>.<stamp>       newest encrypted index record for one (hidden) path
//!         .rustsync-backup             marker
//!
//! A size limit (`quota`, 0 = none) caps what the folder may hold; `used` counts the bytes of
//! all blobs and records.

use crate::index::MAX_CHUNK;
use crate::net::valid_hex;
use crate::sync::{Msg, KIND_BLOB, KIND_RECORD};
use anyhow::Result;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;

const MAX_RECORD: usize = 12 * 1024 * 1024;
const MAX_DELETE: usize = 1000; // blobs removed per request
/// A chunk younger than this is never deleted or listed for deletion, whatever a client asks:
/// the backup itself enforces it, so a stolen laptop or a buggy client cannot empty it at once.
const MIN_DELETE_AGE: u64 = 7 * 24 * 3600;
const MAX_BLOB_PAGE: usize = 20_000; // blobs listed per request
pub const MAX_LIST_PAGE: usize = 20_000; // records listed per request
/// A record's version number is the change time (ns since 1970) of the entry it holds. A number
/// further ahead than this could never be replaced by a real edit, which would freeze that file's
/// backup, so it is refused. (Not enforced while this machine's own clock is clearly unset.)
const MAX_STAMP_AHEAD_NS: u64 = 3 * 24 * 3600 * 1_000_000_000;
const CLOCK_SET_AFTER_NS: u64 = 1_704_067_200 * 1_000_000_000; // 2024-01-01

fn stamp_too_far_ahead(stamp: u64) -> bool {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    now >= CLOCK_SET_AFTER_NS && stamp > now.saturating_add(MAX_STAMP_AHEAD_NS)
}

/// A chunk reported present is marked as used again unless that already happened this recently.
const TOUCH_EVERY: u64 = 3600;
static TMP: AtomicU64 = AtomicU64::new(0);

/// Ids of records and chunks: 64 lowercase hex characters. One spelling only, so that the order
/// of the stored names is the order every page of a list follows.
fn ok_id(s: &str) -> bool {
    valid_hex(s, 64) && !s.bytes().any(|c| c.is_ascii_uppercase())
}

/// Where a list goes on from: empty, or (the start of) an id in the one spelling ids have.
fn ok_cursor(s: &str) -> bool {
    s.len() <= 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'))
}

pub const MARKER: &str = ".rustsync-backup";

pub struct Depot {
    dir: PathBuf,
    /// slot -> stamp of the record we hold
    records: Mutex<BTreeMap<String, u64>>,
    /// Bytes held (blobs + records).
    used: AtomicU64,
    /// Most bytes we may hold; 0 = no limit.
    quota: AtomicU64,
}

/// Total size of the files directly inside `dir` (not recursive).
fn files_size(dir: &Path) -> u64 {
    let mut total = 0u64;
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            if let Ok(m) = e.metadata() {
                if m.is_file() {
                    total = total.saturating_add(m.len());
                }
            }
        }
    }
    total
}

impl Depot {
    /// Open (creating if needed) the folder that holds one library's encrypted backup. The
    /// folder gets a marker file so it is never mistaken for a normal synced folder.
    ///
    /// `create` is true only when the backup is first set up. Afterwards a folder that has lost
    /// its marker (an unmounted drive, a wiped disk) is refused instead of silently starting
    /// empty, so clients never think the backup is gone.
    pub fn open(dir: &Path, lib: &str, create: bool, quota: u64) -> Result<Depot> {
        let marker = dir.join(MARKER);
        if !create && !marker.exists() {
            anyhow::bail!("{} is missing or is not the backup folder it used to be (is the drive mounted?)", dir.display());
        }
        std::fs::create_dir_all(dir.join("blobs"))?;
        std::fs::create_dir_all(dir.join("records"))?;
        if !marker.exists() {
            std::fs::write(&marker, format!("rustsync encrypted backup\nlibrary {}\nThis folder holds scrambled data that this machine cannot read. Do not edit it.\n", lib))?;
        }
        // Remove temp files left by a crash.
        for e in std::fs::read_dir(dir)?.flatten() {
            if e.file_name().to_string_lossy().starts_with("tmp-") {
                let _ = std::fs::remove_file(e.path());
            }
        }
        // A crash while a chunk was being removed leaves it as `del-<id>`: put it back (a chunk
        // that really is unused is removed by the next cleanup, once it is old enough).
        if let Ok(rd) = std::fs::read_dir(dir.join("blobs")) {
            for sub in rd.flatten() {
                let Ok(files) = std::fs::read_dir(sub.path()) else { continue };
                for f in files.flatten() {
                    let name = f.file_name().to_string_lossy().to_string();
                    let Some(id) = name.strip_prefix("del-") else { continue };
                    if !ok_id(id) {
                        continue;
                    }
                    let home = f.path().with_file_name(id);
                    if home.exists() {
                        let _ = std::fs::remove_file(f.path());
                    } else {
                        let _ = std::fs::rename(f.path(), home);
                    }
                }
            }
        }
        let mut used = files_size(&dir.join("records"));
        if let Ok(rd) = std::fs::read_dir(dir.join("blobs")) {
            for sub in rd.flatten() {
                used = used.saturating_add(files_size(&sub.path()));
            }
        }
        let mut map: BTreeMap<String, u64> = BTreeMap::new();
        if let Ok(files) = std::fs::read_dir(dir.join("records")) {
            for f in files.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                let Some((slot, stamp)) = name.split_once('.') else { continue };
                let Ok(stamp) = stamp.parse::<u64>() else { continue };
                if !ok_id(slot) {
                    continue;
                }
                let cur = map.entry(slot.to_string()).or_insert(0);
                if stamp > *cur {
                    *cur = stamp;
                }
            }
        }
        Ok(Depot {
            dir: dir.to_path_buf(),
            records: Mutex::new(map),
            used: AtomicU64::new(used),
            quota: AtomicU64::new(quota),
        })
    }

    pub fn used(&self) -> u64 {
        self.used.load(Ordering::Relaxed)
    }

    pub fn quota(&self) -> u64 {
        self.quota.load(Ordering::Relaxed)
    }

    pub fn set_quota(&self, bytes: u64) {
        self.quota.store(bytes, Ordering::Relaxed);
    }

    /// Reserve `len` bytes. `credit` is space that is about to be freed by the same operation
    /// (an older record being replaced). False if that would go over the quota.
    #[allow(deprecated)] // `try_update` is newer than our minimum Rust version
    fn try_add(&self, len: u64, credit: u64) -> bool {
        self.used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| {
                let q = self.quota.load(Ordering::Relaxed);
                if q != 0 && u.saturating_add(len) > q.saturating_add(credit) {
                    None
                } else {
                    Some(u.saturating_add(len))
                }
            })
            .is_ok()
    }

    #[allow(deprecated)]
    fn release(&self, n: u64) {
        let _ = self
            .used
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |u| Some(u.saturating_sub(n)));
    }

    fn blob_path(&self, id: &str) -> PathBuf {
        self.dir.join("blobs").join(&id[..2]).join(id)
    }

    fn record_path(&self, slot: &str, stamp: u64) -> PathBuf {
        self.dir.join("records").join(format!("{}.{}", slot, stamp))
    }

    async fn write_atomic(&self, path: &Path, data: &[u8]) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        // An unpredictable name, created exclusively (fails if anything is already there, a
        // symlink included), never through a symlink, readable by us only.
        let tmp = self.dir.join(format!(
            "tmp-{}-{}-{:016x}",
            std::process::id(),
            TMP.fetch_add(1, Ordering::Relaxed),
            rand::random::<u64>()
        ));
        let written = async {
            let mut f = tokio::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)
                .await?;
            f.write_all(data).await?;
            f.flush().await
        }
        .await;
        if let Err(e) = written {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        if let Err(e) = tokio::fs::rename(&tmp, path).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            return Err(e);
        }
        Ok(())
    }

    async fn file_len(path: &Path) -> u64 {
        tokio::fs::metadata(path).await.map(|m| m.len()).unwrap_or(0)
    }

    /// Say "this chunk is in use now": its age starts again, so a cleanup that started before
    /// the new reference arrived cannot remove it. False if the chunk is not there.
    async fn touch_blob(&self, id: &str) -> bool {
        let path = self.blob_path(id);
        tokio::task::spawn_blocking(move || {
            let Ok(md) = std::fs::metadata(&path) else { return false };
            let age = md
                .modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(u64::MAX);
            if age < TOUCH_EVERY {
                return true; // touched recently enough: far from the 7 day limit
            }
            match std::fs::OpenOptions::new().write(true).open(&path) {
                Ok(f) => f.set_modified(SystemTime::now()).is_ok(),
                Err(_) => false,
            }
        })
        .await
        .unwrap_or(false)
    }

    async fn put_blob(&self, id: &str, data: Vec<u8>) -> bool {
        let path = self.blob_path(id);
        if self.touch_blob(id).await {
            return true;
        }
        let len = data.len() as u64;
        if !self.try_add(len, 0) {
            return false; // over the quota
        }
        if self.write_atomic(&path, &data).await.is_err() {
            self.release(len);
            return false;
        }
        true
    }

    /// Keep only the newest record per slot (same last-writer-wins rule the clients use).
    async fn put_record(&self, slot: &str, stamp: u64, data: Vec<u8>) -> bool {
        if stamp_too_far_ahead(stamp) {
            tracing::warn!("a backup record was refused: its change time is far in the future (check the clocks of your devices)");
            return false;
        }
        let prev = self.records.lock().unwrap().get(slot).copied();
        if matches!(prev, Some(p) if p >= stamp) {
            return true;
        }
        let old_len = match prev {
            Some(p) => Self::file_len(&self.record_path(slot, p)).await,
            None => 0,
        };
        let len = data.len() as u64;
        if !self.try_add(len, old_len) {
            return false; // over the quota
        }
        let path = self.record_path(slot, stamp);
        if self.write_atomic(&path, &data).await.is_err() {
            self.release(len);
            return false;
        }
        // Re-check under the lock in case another client raced us.
        let (remove_ours, remove_old) = {
            let mut m = self.records.lock().unwrap();
            let cur = m.get(slot).copied();
            match cur {
                Some(p) if p > stamp => (true, None),
                Some(p) if p == stamp => (false, None),
                other => {
                    m.insert(slot.to_string(), stamp);
                    (false, other)
                }
            }
        };
        if remove_ours {
            let _ = tokio::fs::remove_file(&path).await;
            self.release(len);
        }
        if let Some(p) = remove_old {
            let old = self.record_path(slot, p);
            let sz = Self::file_len(&old).await;
            if tokio::fs::remove_file(&old).await.is_ok() {
                self.release(sz);
            }
        }
        true
    }

    async fn get_record(&self, slot: &str) -> Option<Vec<u8>> {
        let stamp = self.records.lock().unwrap().get(slot).copied()?;
        tokio::fs::read(self.record_path(slot, stamp)).await.ok()
    }

    /// (slot, stamp) of the records we hold whose slot sorts after `after`, in slot order, at most
    /// one page (a big library has far more than fits in one message).
    pub fn list_page(&self, after: &str) -> Vec<(String, u64)> {
        use std::ops::Bound::{Excluded, Unbounded};
        let m = self.records.lock().unwrap();
        m.range::<str, _>((Excluded(after), Unbounded))
            .take(MAX_LIST_PAGE)
            .map(|(s, st)| (s.clone(), *st))
            .collect()
    }

    /// Blobs older than `min_age` seconds whose id sorts after `after`, in id order, at most one
    /// page. (Used by a client to clean up chunks nothing refers to any more.)
    async fn old_blobs(&self, after: String, min_age: u64) -> Vec<(String, u64)> {
        let min_age = min_age.max(MIN_DELETE_AGE);
        let root = self.dir.join("blobs");
        tokio::task::spawn_blocking(move || {
            let now = SystemTime::now();
            let mut subs: Vec<String> = std::fs::read_dir(&root)
                .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect())
                .unwrap_or_default();
            subs.sort();
            let mut out: Vec<(String, u64)> = Vec::new();
            for sub in subs {
                if sub.len() != 2 || sub.as_str() < after.get(..2).unwrap_or("") {
                    continue;
                }
                let mut here: Vec<(String, u64)> = Vec::new();
                if let Ok(rd) = std::fs::read_dir(root.join(&sub)) {
                    for e in rd.flatten() {
                        let name = e.file_name().to_string_lossy().to_string();
                        if !ok_id(&name) || name <= after {
                            continue;
                        }
                        let Ok(m) = e.metadata() else { continue };
                        let age = m
                            .modified()
                            .ok()
                            .and_then(|t| now.duration_since(t).ok())
                            .map(|d| d.as_secs())
                            .unwrap_or(0);
                        if age >= min_age {
                            here.push((name, m.len()));
                        }
                    }
                }
                here.sort();
                out.extend(here);
                if out.len() >= MAX_BLOB_PAGE {
                    out.truncate(MAX_BLOB_PAGE);
                    break;
                }
            }
            out
        })
        .await
        .unwrap_or_default()
    }

    async fn delete_blobs(&self, ids: Vec<String>) -> u64 {
        let mut n = 0u64;
        for id in ids {
            if !ok_id(&id) {
                continue;
            }
            let path = self.blob_path(&id);
            // Move it aside first, then look at its age. A client that says "we have it" from now
            // on is told it is missing and uploads it again; one that said so before the move
            // has already refreshed the age, which we see here, and the chunk goes back.
            let aside = path.with_file_name(format!("del-{}", id));
            if tokio::fs::rename(&path, &aside).await.is_err() {
                continue;
            }
            let Ok(md) = tokio::fs::metadata(&aside).await else { continue };
            let age = md
                .modified()
                .ok()
                .and_then(|t| SystemTime::now().duration_since(t).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            let sz = md.len();
            if age < MIN_DELETE_AGE {
                if tokio::fs::try_exists(&path).await.unwrap_or(false) {
                    // uploaded again meanwhile: one copy is enough
                    if tokio::fs::remove_file(&aside).await.is_ok() {
                        self.release(sz);
                    }
                } else if tokio::fs::rename(&aside, &path).await.is_err() {
                    let _ = tokio::fs::remove_file(&aside).await;
                    self.release(sz);
                }
                continue;
            }
            if tokio::fs::remove_file(&aside).await.is_ok() {
                self.release(sz);
                n += 1;
            }
        }
        n
    }

    /// Answer one request for this library. None = refused or not found; Some(bytes) = success
    /// or payload. (The node has already checked that the sender may use this backup.)
    pub async fn serve(&self, msg: Msg) -> Option<Vec<u8>> {
        match msg {
            Msg::DepotPut { kind, id, stamp, data, .. } => {
                if !ok_id(&id) {
                    return None;
                }
                match kind {
                    KIND_BLOB => {
                        if data.len() > MAX_CHUNK + 64 {
                            return None;
                        }
                        self.put_blob(&id, data).await.then(Vec::new)
                    }
                    KIND_RECORD => {
                        if data.len() > MAX_RECORD {
                            return None;
                        }
                        self.put_record(&id, stamp, data).await.then(Vec::new)
                    }
                    _ => None,
                }
            }
            Msg::DepotGet { kind, id, .. } => {
                if !ok_id(&id) {
                    return None;
                }
                match kind {
                    KIND_BLOB => tokio::fs::read(self.blob_path(&id)).await.ok(),
                    KIND_RECORD => self.get_record(&id).await,
                    _ => None,
                }
            }
            Msg::DepotList { after, .. } => {
                if !ok_cursor(&after) {
                    return None;
                }
                postcard::to_allocvec(&self.list_page(&after)).ok()
            }
            Msg::DepotHave { ids, .. } => {
                let mut missing: Vec<String> = Vec::new();
                for id in ids {
                    // Answering "we have it" also marks it as in use again (see touch_blob).
                    if !ok_id(&id) || !self.touch_blob(&id).await {
                        missing.push(id);
                    }
                }
                postcard::to_allocvec(&missing).ok()
            }
            Msg::DepotStat { .. } => postcard::to_allocvec(&(self.used(), self.quota())).ok(),
            Msg::DepotBlobs { after, min_age, .. } => {
                if !ok_cursor(&after) {
                    return None;
                }
                postcard::to_allocvec(&self.old_blobs(after, min_age).await).ok()
            }
            Msg::DepotDelete { ids, .. } => {
                if ids.len() > MAX_DELETE {
                    return None;
                }
                postcard::to_allocvec(&self.delete_blobs(ids).await).ok()
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(f)
    }

    fn age_blob(depot: &Depot, id: &str, days: u64) {
        let f = std::fs::OpenOptions::new().write(true).open(depot.blob_path(id)).unwrap();
        f.set_modified(SystemTime::now() - std::time::Duration::from_secs(days * 24 * 3600)).unwrap();
    }

    #[test]
    fn ids_have_one_spelling() {
        assert!(ok_id(&"0a".repeat(32)));
        assert!(!ok_id(&"0A".repeat(32)));
        assert!(!ok_id(&"0a".repeat(31)));
        assert!(!ok_id(&"zz".repeat(32)));
        assert!(ok_cursor("") && ok_cursor("0af") && ok_cursor(&"f".repeat(64)));
        assert!(!ok_cursor("0AF") && !ok_cursor(&"f".repeat(65)) && !ok_cursor("../x"));
        let dir = std::env::temp_dir().join(format!("rustsync-depotcase-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let depot = Depot::open(&dir, "lib", true, 0).unwrap();
        run(async {
            let upper = Msg::DepotPut { req: 1, kind: KIND_RECORD, lib: "lib".into(), id: "AB".repeat(32), stamp: 1, data: vec![0; 4] };
            assert!(depot.serve(upper).await.is_none());
            let lower = Msg::DepotPut { req: 2, kind: KIND_RECORD, lib: "lib".into(), id: "ab".repeat(32), stamp: 1, data: vec![0; 4] };
            assert!(depot.serve(lower).await.is_some());
            assert!(depot.serve(Msg::DepotList { req: 3, lib: "lib".into(), after: "AB".into() }).await.is_none());
            assert!(depot.serve(Msg::DepotList { req: 4, lib: "lib".into(), after: String::new() }).await.is_some());
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_stamps_far_in_the_future_are_refused() {
        let now = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_nanos() as u64;
        assert!(!stamp_too_far_ahead(now));
        assert!(!stamp_too_far_ahead(now + 3600 * 1_000_000_000));
        assert!(stamp_too_far_ahead(now + 10 * 24 * 3600 * 1_000_000_000));
        assert!(stamp_too_far_ahead(u64::MAX));
        let dir = std::env::temp_dir().join(format!("rustsync-depotstamp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let depot = Depot::open(&dir, "lib", true, 0).unwrap();
        run(async {
            assert!(!depot.put_record(&"a".repeat(64), u64::MAX, vec![0; 4]).await);
            assert!(depot.put_record(&"a".repeat(64), now, vec![0; 4]).await);
        });
        assert_eq!(depot.list_page("").len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_record_list_comes_in_ordered_pages() {
        let dir = std::env::temp_dir().join(format!("rustsync-depotlist-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let depot = Depot::open(&dir, "lib", true, 0).unwrap();
        let slots: Vec<String> = ["c", "a", "e", "b"].iter().map(|c| c.repeat(64)).collect();
        run(async {
            for (i, s) in slots.iter().enumerate() {
                assert!(depot.put_record(s, 10 + i as u64, vec![0; 8]).await);
            }
        });
        let all = depot.list_page("");
        let names: Vec<&str> = all.iter().map(|(s, _)| &s[..1]).collect();
        assert_eq!(names, ["a", "b", "c", "e"]);
        let rest = depot.list_page(&"b".repeat(64));
        assert_eq!(rest.iter().map(|(s, _)| &s[..1]).collect::<Vec<_>>(), ["c", "e"]);
        assert!(depot.list_page(&"e".repeat(64)).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_chunk_that_is_used_again_is_not_removed() {
        let dir = std::env::temp_dir().join(format!("rustsync-depot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let depot = Depot::open(&dir, "lib", true, 0).unwrap();
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        run(async {
            assert!(depot.put_blob(&a, vec![1; 100]).await);
            assert!(depot.put_blob(&b, vec![2; 100]).await);
            // both are old, so both could be cleaned up
            age_blob(&depot, &a, 10);
            age_blob(&depot, &b, 10);
            // a client reports `a` as already present (it re-uses it), and re-uploads `b`
            assert!(depot.touch_blob(&a).await);
            assert!(depot.put_blob(&b, vec![2; 100]).await);
            // a cleanup that listed them earlier now asks for both to be deleted: nothing goes
            assert_eq!(depot.delete_blobs(vec![a.clone(), b.clone()]).await, 0);
            assert!(depot.blob_path(&a).exists() && depot.blob_path(&b).exists());
            assert_eq!(depot.used(), 200);
            // an old, unused chunk is still removed
            age_blob(&depot, &a, 10);
            assert_eq!(depot.delete_blobs(vec![a.clone()]).await, 1);
            assert!(!depot.blob_path(&a).exists());
            assert_eq!(depot.used(), 100);
            // a chunk that is not there is reported missing
            assert!(!depot.touch_blob(&a).await);
        });
        // a half-finished removal is put back when the folder is opened again
        std::fs::rename(depot.blob_path(&b), depot.blob_path(&b).with_file_name(format!("del-{}", b))).unwrap();
        drop(depot);
        let depot = Depot::open(&dir, "lib", false, 0).unwrap();
        assert!(depot.blob_path(&b).exists());
        assert_eq!(depot.used(), 100);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
