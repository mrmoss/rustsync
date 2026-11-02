//! Applying a peer's index entries to the local disk.
//!
//! Order: deletions (deepest first), then directories (shallowest first), then files.
//! Files are rebuilt in a temp file: chunks that already match the local copy are reused,
//! only the differing chunks are fetched, and the result is verified against the whole-file
//! hash before an atomic rename.

use crate::index::{
    disk_matches, fd_inside, mtime_of, now_ns, open_nofollow, remote_wins, sanitize_remote, Entry, Folder, Kind,
    CHUNK, TMP_PREFIX,
};
use crate::sync::Remote;
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use std::fs;
use std::io::SeekFrom;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tracing::{debug, warn};

static TMP_COUNTER: AtomicU64 = AtomicU64::new(0);

fn depth(p: &str) -> usize {
    p.matches('/').count()
}

fn record(folder: &Folder, e: Entry) {
    folder.index.lock().unwrap().insert(e.path.clone(), e);
}

/// Returns the entries that were actually applied (so they can be forwarded to other peers).
pub async fn apply_entries(folder: &Arc<Folder>, remote: &Remote, entries: Vec<Entry>) -> Result<Vec<Entry>> {
    // Nothing from another device is trusted: every entry is validated first (path, sizes,
    // hashes, change time, permissions).
    let now = now_ns();
    let mut rejected = 0usize;
    let todo: Vec<Entry> = {
        let idx = folder.index.lock().unwrap();
        entries
            .into_iter()
            .filter_map(|e| match sanitize_remote(e, now, folder.max_skew_ns) {
                Ok(e) => {
                    if remote_wins(idx.get(&e.path), &e) {
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
        return Ok(Vec::new());
    }

    let mut deletes: Vec<Entry> = todo.iter().filter(|e| e.deleted).cloned().collect();
    deletes.sort_by_key(|e| std::cmp::Reverse(depth(&e.path)));
    let mut dirs: Vec<Entry> = todo.iter().filter(|e| !e.deleted && e.kind == Kind::Dir).cloned().collect();
    dirs.sort_by_key(|e| depth(&e.path));
    let files: Vec<Entry> = todo.iter().filter(|e| !e.deleted && e.kind == Kind::File).cloned().collect();

    let mut applied = Vec::new();

    for e in deletes.into_iter().chain(dirs) {
        let res = {
            let _guard = folder.fs_lock.lock().await;
            let (f, e2) = (folder.clone(), e.clone());
            tokio::task::spawn_blocking(move || apply_meta(&f, &e2)).await?
        };
        match res {
            Ok(true) => applied.push(e),
            Ok(false) => {}
            Err(err) => warn!("{}: {:#}", e.path, err),
        }
    }

    for e in files {
        let res = apply_file(folder, remote, &e).await;
        remote.file_done(&folder.id);
        match res {
            Ok(true) => {
                debug!("synced {}", e.path);
                applied.push(e);
            }
            Ok(false) => {}
            Err(err) => warn!("{}: {:#}", e.path, err),
        }
    }

    let f = folder.clone();
    let _ = tokio::task::spawn_blocking(move || f.persist_now()).await;
    Ok(applied)
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

/// Deletions and directories. Caller holds fs_lock.
fn apply_meta(folder: &Folder, e: &Entry) -> Result<bool> {
    check_ancestors(folder, &e.path)?;
    let abs = folder.abs(&e.path);
    let local = folder.get(&e.path);
    if !remote_wins(local.as_ref(), e) {
        return Ok(false);
    }
    let existing = fs::symlink_metadata(&abs).ok();

    if e.deleted {
        if let Some(md) = existing {
            // Only delete what we have indexed and that is unchanged; never destroy unscanned local work.
            let safe = matches!(&local, Some(l) if !l.deleted && disk_matches(&abs, l));
            if !safe {
                warn!("not deleting {}: it has local changes", e.path);
                return Ok(false);
            }
            if md.is_dir() {
                if let Err(err) = fs::remove_dir(&abs) {
                    warn!("cannot remove directory {}: {}", e.path, err);
                    return Ok(false);
                }
            } else {
                fs::remove_file(&abs)?;
            }
        }
    } else {
        match existing {
            Some(md) if md.is_dir() => {}
            Some(md) => {
                let safe = md.is_file() && matches!(&local, Some(l) if l.is_live_file() && disk_matches(&abs, l));
                if !safe {
                    warn!("not replacing {} with a directory: it has local changes", e.path);
                    return Ok(false);
                }
                fs::remove_file(&abs)?;
                fs::create_dir(&abs)?;
            }
            None => fs::create_dir_all(&abs)?,
        }
        fs::set_permissions(&abs, fs::Permissions::from_mode(e.mode))?;
    }
    record(folder, e.clone());
    Ok(true)
}

async fn apply_file(folder: &Arc<Folder>, remote: &Remote, e: &Entry) -> Result<bool> {
    let abs = folder.abs(&e.path);
    let local = folder.get(&e.path);
    if !remote_wins(local.as_ref(), e) {
        return Ok(false);
    }
    check_ancestors(folder, &e.path)?;

    // Chunks we already have, if the local file is exactly what we indexed.
    let local_chunks: Vec<String> = match &local {
        Some(l) if l.is_live_file() && disk_matches(&abs, l) => l.chunks.clone(),
        _ => Vec::new(),
    };
    let local_size = local.as_ref().map(|l| l.size).unwrap_or(0);

    let parent = abs.parent().ok_or_else(|| anyhow!("bad path"))?;
    tokio::fs::create_dir_all(parent).await?;
    let tmp: PathBuf = parent.join(format!(
        "{}{}-{}",
        TMP_PREFIX,
        std::process::id(),
        TMP_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));

    let built = build_temp(
        folder.id.clone(),
        folder.root.clone(),
        remote.clone(),
        e.clone(),
        abs.clone(),
        local_chunks,
        local_size,
        tmp.clone(),
    )
    .await;
    if let Err(err) = built {
        let _ = tokio::fs::remove_file(&tmp).await;
        return Err(err);
    }

    let _guard = folder.fs_lock.lock().await;
    let (f, e2, t2, l2) = (folder.clone(), e.clone(), tmp.clone(), local.clone());
    let res = tokio::task::spawn_blocking(move || commit_file(&f, &e2, &t2, l2.as_ref())).await?;
    if !matches!(res, Ok(true)) {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    res
}

// Takes everything by value: borrowed data inside the buffered stream makes the future
// fail Rust's "Send is not general enough" check when spawned on tokio.
async fn build_temp(
    folder_id: String,
    root: PathBuf,
    remote: Remote,
    e: Entry,
    abs: PathBuf,
    local_chunks: Vec<String>,
    local_size: u64,
    tmp: PathBuf,
) -> Result<()> {
    // New file, never following a symlink, and checked to really be inside the folder.
    let mut out = {
        use std::os::fd::AsRawFd;
        let mut opts = tokio::fs::OpenOptions::new();
        opts.write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW);
        let f = opts.open(&tmp).await?;
        if !fd_inside(f.as_raw_fd(), &root) {
            drop(f);
            let _ = tokio::fs::remove_file(&tmp).await;
            bail!("{} would be written outside the library folder (or that could not be confirmed: on Linux this needs /proc mounted)", e.path);
        }
        f
    };
    let mut old = if local_chunks.is_empty() {
        None
    } else {
        Some(tokio::fs::File::from_std(open_nofollow(&abs, &root)?))
    };
    let mut whole = blake3::Hasher::new();

    // (index, wanted hash, can we reuse the local chunk?)
    let items: Vec<(usize, String, bool)> = e
        .chunks
        .iter()
        .enumerate()
        .map(|(i, want)| (i, want.clone(), local_chunks.get(i) == Some(want)))
        .collect();
    let path = e.path.clone();
    // The sender chooses the chunks, so check their sizes against what the entry declares: every
    // chunk is CHUNK bytes except the last, and together they are exactly `size`.
    let (declared_size, chunk_count) = (e.size, e.chunks.len());
    let expected_len = move |i: usize| -> u64 {
        if i + 1 < chunk_count {
            CHUNK as u64
        } else {
            declared_size.saturating_sub((chunk_count.saturating_sub(1) as u64) * CHUNK as u64)
        }
    };

    // Missing chunks are fetched concurrently (8 in flight); results are consumed in order.
    let fetches = futures_util::stream::iter(items)
        .map(move |(i, want, reuse)| {
            let remote = remote.clone();
            let path = path.clone();
            let folder_id = folder_id.clone();
            async move {
                if reuse {
                    return Ok::<Option<Vec<u8>>, anyhow::Error>(None);
                }
                let data = remote.get_chunk(folder_id, path, i as u32, want.clone()).await?;
                if data.len() as u64 != expected_len(i) {
                    bail!("chunk {} has the wrong size", i);
                }
                if blake3::hash(&data).to_hex().as_str() != want {
                    bail!("chunk {} failed verification", i);
                }
                Ok(Some(data))
            }
        })
        .buffered(8);
    futures_util::pin_mut!(fetches);

    let mut i = 0usize;
    let mut total = 0u64;
    while let Some(res) = fetches.next().await {
        let data = match res? {
            Some(d) => d,
            None => {
                let off = i as u64 * CHUNK as u64;
                let len = local_size.saturating_sub(off).min(CHUNK as u64) as usize;
                let f = old.as_mut().ok_or_else(|| anyhow!("local file missing"))?;
                f.seek(SeekFrom::Start(off)).await?;
                let mut buf = vec![0u8; len];
                f.read_exact(&mut buf).await?;
                buf
            }
        };
        total += data.len() as u64;
        if total > e.size {
            bail!("the file is larger than it says");
        }
        whole.update(&data);
        out.write_all(&data).await?;
        i += 1;
    }
    out.flush().await?;
    if total != e.size {
        bail!("the file is smaller than it says");
    }
    if whole.finalize().to_hex().as_str() != e.hash {
        bail!("file hash mismatch after assembly");
    }
    Ok(())
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
    fs::set_permissions(tmp, fs::Permissions::from_mode(e.mode))?;
    fs::rename(tmp, &abs)?;

    let md = fs::symlink_metadata(&abs)?;
    let mut rec = e.clone();
    rec.mtime_ns = mtime_of(&md);
    rec.size = md.len();
    record(folder, rec);
    Ok(true)
}
