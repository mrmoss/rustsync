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

use crate::index::CHUNK;
use crate::net::valid_hex;
use crate::sync::{Msg, KIND_BLOB, KIND_RECORD};
use anyhow::Result;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;
use tokio::io::AsyncWriteExt;

const MAX_RECORD: usize = 12 * 1024 * 1024;
const MAX_DELETE: usize = 1000; // blobs removed per request
const MAX_BLOB_PAGE: usize = 20_000; // blobs listed per request
static TMP: AtomicU64 = AtomicU64::new(0);

fn ok_id(s: &str) -> bool {
    valid_hex(s, 64)
}

pub const MARKER: &str = ".rustsync-backup";

pub struct Depot {
    dir: PathBuf,
    /// slot -> stamp of the record we hold
    records: Mutex<HashMap<String, u64>>,
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
        let mut used = files_size(&dir.join("records"));
        if let Ok(rd) = std::fs::read_dir(dir.join("blobs")) {
            for sub in rd.flatten() {
                used = used.saturating_add(files_size(&sub.path()));
            }
        }
        let mut map: HashMap<String, u64> = HashMap::new();
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

    async fn put_blob(&self, id: &str, data: Vec<u8>) -> bool {
        let path = self.blob_path(id);
        if tokio::fs::try_exists(&path).await.unwrap_or(false) {
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

    /// (slot, stamp) of every record we hold.
    pub fn list(&self) -> Vec<(String, u64)> {
        self.records.lock().unwrap().iter().map(|(s, st)| (s.clone(), *st)).collect()
    }

    /// Blobs older than `min_age` seconds whose id sorts after `after`, in id order, at most one
    /// page. (Used by a client to clean up chunks nothing refers to any more.)
    async fn old_blobs(&self, after: String, min_age: u64) -> Vec<(String, u64)> {
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
            let sz = Self::file_len(&path).await;
            if tokio::fs::remove_file(&path).await.is_ok() {
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
                        if data.len() > CHUNK + 64 {
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
            Msg::DepotList { .. } => bincode::serialize(&self.list()).ok(),
            Msg::DepotHave { ids, .. } => {
                let mut missing: Vec<String> = Vec::new();
                for id in ids {
                    if !ok_id(&id) || !tokio::fs::try_exists(self.blob_path(&id)).await.unwrap_or(false) {
                        missing.push(id);
                    }
                }
                bincode::serialize(&missing).ok()
            }
            Msg::DepotStat { .. } => bincode::serialize(&(self.used(), self.quota())).ok(),
            Msg::DepotBlobs { after, min_age, .. } => {
                if after.len() > 64 || !after.chars().all(|c| c.is_ascii_hexdigit()) {
                    return None;
                }
                bincode::serialize(&self.old_blobs(after.to_lowercase(), min_age).await).ok()
            }
            Msg::DepotDelete { ids, .. } => {
                if ids.len() > MAX_DELETE {
                    return None;
                }
                bincode::serialize(&self.delete_blobs(ids).await).ok()
            }
            _ => None,
        }
    }
}
