//! Applying a peer's index entries to the local disk.
//!
//! Order: deletions (deepest first), then directories (shallowest first), then files.
//! Files are rebuilt in a temp file: chunks that already match the local copy are reused,
//! only the differing chunks are fetched, and the result is verified against the whole-file
//! hash before an atomic rename.

use crate::index::{
    chunk_size, disk_matches, fd_inside, hash_file, mtime_of, now_ns, open_nofollow, remote_wins, safe_rel,
    sanitize_remote, Entry, Folder, Kind, TMP_PREFIX,
};
use crate::sync::Remote;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tokio::sync::OwnedSemaphorePermit;
use tracing::{debug, info, warn};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn depth(p: &str) -> usize {
    p.matches('/').count()
}

fn record(folder: &Folder, e: Entry) {
    folder.index.lock().unwrap().insert(e.path.clone(), e);
}

/// What applying a batch did.
#[derive(Default)]
pub struct Applied {
    /// Changes made here (forwarded to the other devices that sync the library).
    pub changes: Vec<Entry>,
    /// Files the other device deleted that are kept here because they are different: newer
    /// versions that go to every device, the one that deleted them included.
    pub kept: Vec<Entry>,
    /// Changes that could not be applied (they are asked for again later).
    pub failed: usize,
}

impl Applied {
    pub fn is_empty(&self) -> bool {
        self.changes.is_empty() && self.kept.is_empty()
    }
    pub fn len(&self) -> usize {
        self.changes.len() + self.kept.len()
    }
}

pub async fn apply_entries(folder: &Arc<Folder>, remote: &Remote, entries: Vec<Entry>) -> Result<Applied> {
    // Nothing from another device is trusted: every entry is validated first (path, sizes,
    // hashes, change time, permissions).
    let now = now_ns();
    let mut rejected = 0usize;
    let ign = folder.ignores();
    let todo: Vec<Entry> = {
        let idx = folder.index.lock().unwrap();
        entries
            .into_iter()
            .filter_map(|e| match sanitize_remote(e, now, folder.max_skew_ns) {
                Ok(e) => {
                    // Ignored here: neither fetched nor deleted.
                    if ign.is_ignored(&e.path, e.kind == Kind::Dir) {
                        None
                    } else if remote_wins(idx.get(&e.path), &e) {
                        Some(e)
                    } else {
                        None
                    }
                }
                Err(why) => {
                    debug!("ignoring an entry: {}", why);
                    rejected += 1;
                    None
                }
            })
            .collect()
    };
    if rejected > 0 {
        warn!("ignored {} invalid or out-of-range entries from another device", rejected);
    }
    if todo.is_empty() {
        return Ok(Applied::default());
    }

    let mut deletes: Vec<Entry> = todo.iter().filter(|e| e.deleted).cloned().collect();
    deletes.sort_by_key(|e| std::cmp::Reverse(depth(&e.path)));
    let mut dirs: Vec<Entry> = todo.iter().filter(|e| !e.deleted && e.kind == Kind::Dir).cloned().collect();
    dirs.sort_by_key(|e| depth(&e.path));
    let files: Vec<Entry> = todo.iter().filter(|e| !e.deleted && e.kind == Kind::File).cloned().collect();

    let mut applied = Vec::new();
    let mut kept = Vec::new();
    let mut failed = 0usize;

    for e in deletes.into_iter().chain(dirs) {
        let res = {
            let _guard = folder.fs_lock.lock().await;
            let (f, e2) = (folder.clone(), e.clone());
            tokio::task::spawn_blocking(move || apply_meta(&f, &e2)).await?
        };
        match res {
            Ok(Some(done)) if e.deleted && !done.deleted => kept.push(done),
            Ok(Some(done)) => applied.push(done),
            Ok(None) => {}
            Err(err) => {
                failed += 1;
                warn!("{}: {:#}", e.path, err)
            }
        }
    }

    // Several files at once where that helps (an SSD), one after another where it hurts (a hard
    // disk); never more than the device allows in total.
    let at_once = folder.files_at_once.load(Ordering::Relaxed).clamp(1, 16);
    // Content that is already here under another name (a file that was copied, or renamed with
    // the deletion still to come) is copied on this disk instead of fetched: hash -> path.
    let sources: HashMap<String, String> = {
        let wanted: HashSet<&str> = files.iter().filter(|e| e.size > 0).map(|e| e.hash.as_str()).collect();
        if wanted.is_empty() {
            HashMap::new()
        } else {
            let idx = folder.index.lock().unwrap();
            idx.values()
                .filter(|l| l.is_live_file() && l.size > 0 && wanted.contains(l.hash.as_str()))
                .map(|l| (l.hash.clone(), l.path.clone()))
                .collect()
        }
    };
    let (folder2, remote2) = (folder.clone(), remote.clone());
    let jobs = futures_util::stream::iter(files)
        .map(move |e| {
            let (folder, remote) = (folder2.clone(), remote2.clone());
            let source = sources.get(&e.hash).filter(|p| **p != e.path).cloned();
            async move {
                let _slot = remote.file_slot().await;
                let res = apply_file(&folder, &remote, &e, source).await;
                // Arrived, skipped or given up: it no longer counts as "left".
                remote.file_done(&folder.id, &e.path, e.stamp);
                (e, res)
            }
        })
        .buffer_unordered(at_once);
    futures_util::pin_mut!(jobs);
    // What was received is written down now and then, not only at the end: if the program is
    // stopped half way through a large batch, it still knows which files it got.
    let mut saved = Instant::now();
    while let Some((e, res)) = jobs.next().await {
        if saved.elapsed() > Duration::from_secs(30) {
            let f = folder.clone();
            let _ = tokio::task::spawn_blocking(move || f.persist_now()).await;
            saved = Instant::now();
        }
        match res {
            Ok(true) => {
                debug!("synced {}", e.path);
                applied.push(e);
            }
            Ok(false) => {}
            Err(err) => {
                failed += 1;
                warn!("{}: {:#}", e.path, err)
            }
        }
    }

    let f = folder.clone();
    let _ = tokio::task::spawn_blocking(move || f.persist_now()).await;
    Ok(Applied { changes: applied, kept, failed })
}

/// Refuse to write through a symlinked or non-directory ancestor.
fn check_ancestors(folder: &Folder, rel: &str) -> Result<()> {
    let parts: Vec<&str> = rel.split('/').collect();
    let mut cur = folder.root.clone();
    for p in &parts[..parts.len() - 1] {
        cur.push(p);
        match fs::symlink_metadata(&cur) {
            Ok(md) => {
                if md.file_type().is_symlink() {
                    bail!("refusing to write through symlink {}", cur.display());
                }
                if !md.is_dir() {
                    bail!("{} is not a directory", cur.display());
                }
            }
            Err(_) => break,
        }
    }
    Ok(())
}

/// Deletions and directories. Caller holds fs_lock. Returns what was recorded (None: nothing).
fn apply_meta(folder: &Folder, e: &Entry) -> Result<Option<Entry>> {
    check_ancestors(folder, &e.path)?;
    let abs = folder.abs(&e.path);
    let local = folder.get(&e.path);
    if !remote_wins(local.as_ref(), e) {
        return Ok(None);
    }
    let existing = fs::symlink_metadata(&abs).ok();

    if e.deleted {
        if let Some(md) = existing {
            if md.is_dir() {
                // Only a directory we know of, and only once it is empty (anything still in it
                // is not ours to remove).
                if !matches!(&local, Some(l) if !l.deleted && disk_matches(&abs, l)) {
                    warn!("not deleting {}: it has local changes", e.path);
                    return Ok(None);
                }
                if let Err(err) = fs::remove_dir(&abs) {
                    // Files that are still being fetched into it are given up (they were
                    // deleted too): it goes when they are cleaned up.
                    if !folder.leftovers_only(&e.path) {
                        warn!("cannot remove directory {}: {}", e.path, err);
                        return Ok(None);
                    }
                }
            } else {
                let indexed = matches!(&local, Some(l) if l.is_live_file() && disk_matches(&abs, l));
                // Is what is here the same as what the other device deleted? (Older deletions
                // do not say what was deleted: then it has to be what we indexed.)
                let same = if e.hash.is_empty() {
                    indexed
                } else if indexed {
                    local.as_ref().is_some_and(|l| l.hash == e.hash)
                } else {
                    md.is_file() && hash_file(&abs, &folder.root).is_ok_and(|(h, _, _)| h == e.hash)
                };
                if !same {
                    if let (true, Some(l)) = (indexed, &local) {
                        // A different version is here (changed here, or one the other device never
                        // had): it is kept, and becomes the newest version so it reaches the other
                        // devices again. A change wins over a deletion.
                        let mut keep = l.clone();
                        keep.stamp = now_ns().max(e.stamp.saturating_add(1));
                        record(folder, keep.clone());
                        info!("kept {}: it was deleted on another device, but the version here is different", e.path);
                        return Ok(Some(keep));
                    }
                    warn!("not deleting {}: it has local changes", e.path);
                    return Ok(None);
                }
                // Kept aside for a few minutes rather than deleted at once: if the same content
                // is announced under another name, this was a rename, and the file is put there.
                if !folder.stash_file(&abs, &e.hash, md.len()) {
                    fs::remove_file(&abs)?;
                }
            }
        }
    } else {
        match existing {
            Some(md) if md.is_dir() => {}
            Some(md) => {
                let safe = md.is_file() && matches!(&local, Some(l) if l.is_live_file() && disk_matches(&abs, l));
                if !safe {
                    warn!("not replacing {} with a directory: it has local changes", e.path);
                    return Ok(None);
                }
                fs::remove_file(&abs)?;
                fs::create_dir(&abs)?;
            }
            None => fs::create_dir_all(&abs)?,
        }
        if folder.perms {
            fs::set_permissions(&abs, fs::Permissions::from_mode(e.mode))?;
        }
    }
    record(folder, e.clone());
    Ok(Some(e.clone()))
}

/// Put a finished temp file in place (or give it up), holding the lock that keeps scans away.
async fn commit(folder: &Arc<Folder>, e: &Entry, tmp: &Path, planned: &Option<Entry>) -> Result<bool> {
    let _guard = folder.fs_lock.lock().await;
    let (f, e2, t2, l2) = (folder.clone(), e.clone(), tmp.to_path_buf(), planned.clone());
    tokio::task::spawn_blocking(move || {
        let res = commit_file(&f, &e2, &t2, l2.as_ref());
        if !matches!(res, Ok(true)) {
            let _ = fs::remove_file(&t2);
            tidy_parents(&f, &e2.path);
        }
        res
    })
    .await?
}

/// Make `tmp` a copy of a local file that should have the content `hash`. False if it turned
/// out to be something else (the caller removes `tmp`).
fn copy_local(src: &Path, root: &Path, tmp: &Path, hash: &str) -> Result<bool> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    let mut from = open_nofollow(src, root)?;
    let mut to = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(tmp)?;
    if !fd_inside(to.as_raw_fd(), root) {
        bail!("{} would be written outside the library folder", tmp.display());
    }
    let mut whole = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = from.read(&mut buf)?;
        if n == 0 {
            break;
        }
        whole.update(&buf[..n]);
        to.write_all(&buf[..n])?;
    }
    to.flush()?;
    Ok(whole.finalize().to_hex().as_str() == hash)
}

/// `source`: the path of another file here that has the same content, if there is one.
async fn apply_file(folder: &Arc<Folder>, remote: &Remote, e: &Entry, source: Option<String>) -> Result<bool> {
    let abs = folder.abs(&e.path);
    let local = folder.get(&e.path);
    if !remote_wins(local.as_ref(), e) {
        return Ok(false);
    }
    check_ancestors(folder, &e.path)?;

    // The same content is here already (only its version or change time differs): take over the
    // version without copying the file.
    if let Some(l) = &local {
        if l.is_live_file() && l.hash == e.hash && l.chunks == e.chunks && disk_matches(&abs, l) {
            let _guard = folder.fs_lock.lock().await;
            let (f, e2, l2) = (folder.clone(), e.clone(), l.clone());
            return tokio::task::spawn_blocking(move || adopt_same(&f, &e2, &l2)).await?;
        }
    }

    // Chunks we already have, if the local file is exactly what we indexed.
    let local_chunks: Vec<String> = match &local {
        Some(l) if l.is_live_file() && disk_matches(&abs, l) => l.chunks.clone(),
        _ => Vec::new(),
    };
    let local_size = local.as_ref().map(|l| l.size).unwrap_or(0);

    let parent = abs.parent().ok_or_else(|| anyhow!("bad path"))?;
    tokio::fs::create_dir_all(parent).await?;

    // Renamed or moved on the other device: a file with exactly this content was deleted here a
    // moment ago and is still kept aside. Put it back under the new name; nothing is fetched.
    if let Some(stashed) = folder.take_stashed(&e.hash, e.size) {
        debug!("{} is a file that was renamed or moved", e.path);
        return commit(folder, e, &stashed, &local).await;
    }

    let tmp: PathBuf = parent.join(format!(
        "{}{}-{}",
        TMP_PREFIX,
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    // The same content is here under another name: copy it on this disk instead of fetching it.
    if let Some(src) = source {
        let from = folder.abs(&src);
        let usable = matches!(folder.get(&src), Some(l) if l.is_live_file() && l.hash == e.hash && l.size == e.size && disk_matches(&from, &l));
        if usable && safe_rel(&src) {
            let (root, t2, hash) = (folder.root.clone(), tmp.clone(), e.hash.clone());
            let copied = tokio::task::spawn_blocking(move || copy_local(&from, &root, &t2, &hash)).await?;
            if matches!(copied, Ok(true)) {
                debug!("{} copied from {} (same content)", e.path, src);
                return commit(folder, e, &tmp, &local).await;
            }
            let _ = tokio::fs::remove_file(&tmp).await;
        }
    }

    let built = build_temp(
        folder.clone(),
        local.as_ref().map(|l| l.key()).map(|(s, h)| (s, h.to_string())),
        remote.clone(),
        e.clone(),
        abs.clone(),
        local_chunks,
        local_size,
        tmp.clone(),
    )
    .await;
    if let Err(err) = built {
        let _guard = folder.fs_lock.lock().await;
        let _ = tokio::fs::remove_file(&tmp).await;
        let (f, p) = (folder.clone(), e.path.clone());
        let _ = tokio::task::spawn_blocking(move || tidy_parents(&f, &p)).await;
        if err.downcast_ref::<Superseded>().is_some() {
            return Ok(false); // a newer change (a deletion, say) arrived meanwhile
        }
        return Err(err);
    }

    commit(folder, e, &tmp, &local).await
}

/// The file being fetched was changed or deleted meanwhile: the fetch stops.
#[derive(Debug)]
struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a newer change arrived meanwhile")
    }
}

impl std::error::Error for Superseded {}

/// Folders above `rel` that were created for a file that then did not arrive, and that are
/// deleted in our index: removed again if empty, so they do not come back as new folders.
/// Caller holds fs_lock.
fn tidy_parents(folder: &Folder, rel: &str) {
    let mut path = rel;
    while let Some(i) = path.rfind('/') {
        path = &path[..i];
        let gone = matches!(folder.get(path), Some(e) if e.deleted);
        if !gone || fs::remove_dir(folder.abs(path)).is_err() {
            break;
        }
    }
}

// Takes everything by value: borrowed data inside the buffered stream makes the future
// fail Rust's "Send is not general enough" check when spawned on tokio.
async fn build_temp(
    folder: Arc<Folder>,
    planned: Option<(u64, String)>,
    remote: Remote,
    e: Entry,
    abs: PathBuf,
    local_chunks: Vec<String>,
    local_size: u64,
    tmp: PathBuf,
) -> Result<()> {
    let (folder_id, root) = (folder.id.clone(), folder.root.clone());
    let watch = folder;
    // New file, never following a symlink, and checked to really be inside the folder.
    let out = {
        use std::os::fd::AsRawFd;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let f = opts.open(&tmp).await?;
        if !fd_inside(f.as_raw_fd(), &root) {
            drop(f);
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!("{} would be written outside the library folder (or that could not be confirmed: on Linux this needs /proc mounted)", e.path);
        }
        f.into_std().await
    };
    // Pieces of the local copy can be reused if both versions are cut the same way.
    let chunk = chunk_size(e.size);
    let same_cut = chunk_size(local_size) == chunk;
    let old = if local_chunks.is_empty() || !same_cut { None } else { Some(open_nofollow(&abs, &root)?) };

    // (index, wanted hash, can we reuse the local chunk?)
    let items: Vec<(usize, String, bool)> = e
        .chunks
        .iter()
        .enumerate()
        .map(|(i, want)| (i, want.clone(), same_cut && local_chunks.get(i) == Some(want)))
        .collect();
    let path = e.path.clone();
    // The sender chooses the chunks, so check their sizes against what the entry declares: every
    // chunk has the size that goes with the file's size, except the last, and together they are
    // exactly `size`.
    let (declared_size, chunk_count) = (e.size, e.chunks.len());
    let expected_len = move |i: usize| -> u64 {
        if i + 1 < chunk_count {
            chunk as u64
        } else {
            declared_size.saturating_sub((chunk_count.saturating_sub(1) as u64) * chunk as u64)
        }
    };

    // Writing and hashing the whole file happen on their own thread, in order; this side only
    // fetches. The small queue between the two holds the fetching back when the disk is slower.
    let (piece_tx, piece_rx) = tokio::sync::mpsc::channel::<Piece>(2);
    let writer = tokio::task::spawn_blocking(move || write_pieces(out, old, piece_rx, chunk, local_size, declared_size));

    // Missing chunks are fetched several at a time (as many as the device's window allows);
    // results are consumed in order.
    let ahead = remote.chunks_ahead(chunk);
    let fetches = futures_util::stream::iter(items)
        .map(move |(i, want, reuse)| {
            let remote = remote.clone();
            let path = path.clone();
            let folder_id = folder_id.clone();
            let watch = watch.clone();
            let planned = planned.clone();
            async move {
                if reuse {
                    return Ok::<Piece, anyhow::Error>(Piece::Local(i));
                }
                // Changed or deleted meanwhile (by a later message): stop fetching it.
                if watch.key_of(&path) != planned {
                    return Err(Superseded.into());
                }
                let len = expected_len(i);
                let (data, window) = remote.get_chunk(folder_id, path, i as u32, want.clone(), len).await?;
                if data.len() as u64 != len {
                    bail!("chunk {} has the wrong size", i);
                }
                // Hashing is kept off the threads that serve the network.
                let data = tokio::task::spawn_blocking(move || {
                    (blake3::hash(&data).to_hex().as_str() == want).then_some(data)
                })
                .await?;
                match data {
                    Some(data) => Ok(Piece::Fetched(data, window)),
                    None => bail!("chunk {} failed verification", i),
                }
            }
        })
        .buffered(ahead);
    futures_util::pin_mut!(fetches);

    let mut failed = None;
    while let Some(res) = fetches.next().await {
        match res {
            Ok(piece) => {
                if piece_tx.send(piece).await.is_err() {
                    break; // the writer stopped: its error is reported below
                }
            }
            Err(err) => {
                failed = Some(err);
                break;
            }
        }
    }
    drop(piece_tx);
    let written = writer.await?;
    if let Some(err) = failed {
        return Err(err);
    }
    if written? != e.hash {
        bail!("file hash mismatch after assembly");
    }
    Ok(())
}

/// One piece of a file being put together.
enum Piece {
    /// Fetched from the other device. The window permit is never looked at: it is held so that
    /// the room is given back only when the piece is dropped, after it was written.
    Fetched(Vec<u8>, #[allow(dead_code)] Option<OwnedSemaphorePermit>),
    /// Unchanged: taken from the local copy.
    Local(usize),
}

/// Write the pieces in the order they arrive and return the hash of the whole file. Runs on a
/// thread of its own (it blocks on the disk).
fn write_pieces(
    mut out: fs::File,
    old: Option<fs::File>,
    mut pieces: tokio::sync::mpsc::Receiver<Piece>,
    chunk: usize,
    local_size: u64,
    size: u64,
) -> Result<String> {
    use std::io::Write;
    use std::os::unix::fs::FileExt;
    let mut whole = blake3::Hasher::new();
    let mut total = 0u64;
    let mut buf: Vec<u8> = Vec::new();
    while let Some(piece) = pieces.blocking_recv() {
        let data: &[u8] = match &piece {
            Piece::Fetched(data, _) => data,
            Piece::Local(i) => {
                let off = *i as u64 * chunk as u64;
                let len = local_size.saturating_sub(off).min(chunk as u64) as usize;
                let f = old.as_ref().ok_or_else(|| anyhow!("local file missing"))?;
                buf.resize(len, 0);
                f.read_exact_at(&mut buf, off)?;
                &buf
            }
        };
        total += data.len() as u64;
        if total > size {
            bail!("the file is larger than it says");
        }
        whole.update(data);
        out.write_all(data)?;
    }
    out.flush()?;
    if total != size {
        bail!("the file is smaller than it says");
    }
    Ok(whole.finalize().to_hex().to_string())
}

/// The file here already has the content of `e`: give it `e`'s change time and permissions and
/// record `e`. Caller holds fs_lock.
fn adopt_same(folder: &Folder, e: &Entry, planned: &Entry) -> Result<bool> {
    let abs = folder.abs(&e.path);
    let now_local = folder.get(&e.path);
    if now_local.as_ref().map(|l| l.key()) != Some(planned.key()) || !disk_matches(&abs, planned) {
        return Ok(false); // something changed meanwhile; the next exchange tries again
    }
    {
        use std::os::unix::fs::OpenOptionsExt;
        let f = fs::OpenOptions::new().write(true).custom_flags(libc::O_NOFOLLOW).open(&abs)?;
        use std::os::fd::AsRawFd;
        if !fd_inside(f.as_raw_fd(), &folder.root) {
            bail!("{} is outside the library folder", e.path);
        }
        f.set_modified(UNIX_EPOCH + Duration::from_nanos(e.mtime_ns))?;
    }
    if folder.perms {
        fs::set_permissions(&abs, fs::Permissions::from_mode(e.mode))?;
    }
    let md = fs::symlink_metadata(&abs)?;
    let mut rec = e.clone();
    rec.mtime_ns = mtime_of(&md);
    rec.size = md.len();
    record(folder, rec);
    Ok(true)
}

/// Atomically move the finished temp file into place. Caller holds fs_lock.
fn commit_file(folder: &Folder, e: &Entry, tmp: &Path, planned: Option<&Entry>) -> Result<bool> {
    let abs = folder.abs(&e.path);
    let now_local = folder.get(&e.path);

    // Our view of this path must not have changed while we were downloading.
    let same = match (planned, &now_local) {
        (None, None) => true,
        (Some(a), Some(b)) => a.key() == b.key(),
        _ => false,
    };
    if !same || !remote_wins(now_local.as_ref(), e) {
        return Ok(false);
    }

    match fs::symlink_metadata(&abs) {
        Ok(md) if md.is_dir() => {
            fs::remove_dir(&abs).context("a directory is in the way")?;
        }
        Ok(md) => {
            let ok = md.is_file() && matches!(&now_local, Some(l) if l.is_live_file() && disk_matches(&abs, l));
            if !ok {
                warn!("{} changed locally during sync; will retry on next exchange", e.path);
                return Ok(false);
            }
        }
        Err(_) => {}
    }

    // mtime first: a read-only mode would block reopening for write.
    let t = UNIX_EPOCH + Duration::from_nanos(e.mtime_ns);
    {
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(tmp)?
            .set_modified(t)?;
    }
    if folder.perms {
        fs::set_permissions(tmp, fs::Permissions::from_mode(e.mode))?;
    }
    fs::rename(tmp, &abs)?;

    let md = fs::symlink_metadata(&abs)?;
    let mut rec = e.clone();
    rec.mtime_ns = mtime_of(&md);
    rec.size = md.len();
    record(folder, rec);
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::scan;

    fn folder(name: &str) -> (PathBuf, PathBuf, Folder) {
        let dir = std::env::temp_dir().join(format!("rustsync-apply-{}-{}", name, std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let data = dir.join("data");
        fs::create_dir_all(&data).unwrap();
        fs::write(data.join("f"), b"version one").unwrap();
        let f = Folder::open(name.into(), data.to_str().unwrap(), &dir.join("state"), 0).unwrap();
        scan(&f, &[String::new()], false);
        (dir, data, f)
    }

    fn deletion(f: &Folder, hash: &str) -> Entry {
        let mut e = f.get("f").unwrap();
        e.deleted = true;
        e.size = 0;
        e.mtime_ns = 0;
        e.chunks.clear();
        e.hash = hash.to_string();
        e.stamp += 1_000;
        e
    }

    #[test]
    fn a_deletion_removes_the_same_file() {
        let (dir, data, f) = folder("same");
        let hash = f.get("f").unwrap().hash;
        let done = apply_meta(&f, &deletion(&f, &hash)).unwrap().unwrap();
        assert!(done.deleted && !data.join("f").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deletion_removes_the_same_file_even_if_the_index_is_behind() {
        let (dir, data, f) = folder("behind");
        let hash = f.get("f").unwrap().hash;
        // same content, but the change time no longer matches what was indexed
        let t = fs::File::options().write(true).open(data.join("f")).unwrap();
        t.set_modified(UNIX_EPOCH + Duration::from_secs(1_000_000)).unwrap();
        drop(t);
        assert!(apply_meta(&f, &deletion(&f, &hash)).unwrap().is_some());
        assert!(!data.join("f").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_deleted_file_is_kept_aside_and_a_copy_is_made_from_local_content() {
        let (dir, data, f) = folder("rename");
        let entry = f.get("f").unwrap();
        // deleted by another device: gone from its place, but still to be had by its content
        assert!(apply_meta(&f, &deletion(&f, &entry.hash)).unwrap().is_some());
        assert!(!data.join("f").exists());
        let back = f.take_stashed(&entry.hash, entry.size).expect("kept aside");
        assert_eq!(fs::read(&back).unwrap(), b"version one");
        // put under a new name, as a rename does
        let mut renamed = entry.clone();
        renamed.path = "g".to_string();
        assert!(commit_file(&f, &renamed, &back, None).unwrap());
        assert_eq!(fs::read(data.join("g")).unwrap(), b"version one");
        assert_eq!(f.get("g").unwrap().hash, entry.hash);
        // a copy of local content is checked against the hash it should have
        let tmp = data.join(format!("{}copy", TMP_PREFIX));
        assert!(copy_local(&data.join("g"), &f.root, &tmp, &entry.hash).unwrap());
        assert_eq!(fs::read(&tmp).unwrap(), b"version one");
        fs::remove_file(&tmp).unwrap();
        assert!(!copy_local(&data.join("g"), &f.root, &tmp, &"0".repeat(64)).unwrap());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_different_version_wins_over_a_deletion() {
        let (dir, data, f) = folder("differs");
        let gone = deletion(&f, &"0".repeat(64));
        let kept = apply_meta(&f, &gone).unwrap().unwrap();
        assert!(!kept.deleted && kept.stamp > gone.stamp);
        assert!(data.join("f").exists());
        assert_eq!(f.get("f").unwrap().stamp, kept.stamp);
        // an unindexed change is left alone (the next scan picks it up)
        fs::write(data.join("f"), b"changed here").unwrap();
        let mut again = gone.clone();
        again.stamp = kept.stamp + 1;
        assert!(apply_meta(&f, &again).unwrap().is_none());
        assert!(data.join("f").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}
