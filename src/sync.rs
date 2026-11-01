//! The daemon: devices, libraries, sessions, pairing and the local control socket.
//!
//! Concepts
//!   device   a machine you paired with (identified by its public key; every pair of devices
//!            shares its own secret, mixed into the encrypted handshake)
//!   library  a named folder with an id and a vault key. You share a library with a paired
//!            device, and choose per library what that device does with it:
//!              Peer   syncs the plain files (it accepts the offer and picks its own folder)
//!              Store  only keeps an encrypted copy (a "backup": it never gets the vault key,
//!                     the library name or any plaintext, so it can't read what it stores)
//!            One server can therefore sync some libraries normally and back up others.

use crate::apply;
use crate::config::Config;
use crate::depot::Depot;
use crate::index::{self, Entry, Folder, CHUNK};
use crate::net::{self, Keys, Reassembler};
use crate::proto::{fmt_size, BackupUsage, DeviceInfo, LibraryInfo, OfferInfo, Reply, Request, StatusInfo};
use crate::state::{Device, LibMode, Library, Member, Offer, Role, State};
use crate::vault::{self, Vault};
use anyhow::{anyhow, bail, Context, Result};
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio::sync::{mpsc, oneshot, OwnedSemaphorePermit, Semaphore};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

// Limits: a buggy or hostile device must not be able to exhaust our memory, CPU or disk.
const MAX_BATCH_ENTRIES: usize = 500; // entries per index message we send
const MAX_BATCH_BYTES: usize = 4 * 1024 * 1024; // ... or roughly this many bytes
const MAX_RECV_ENTRIES: usize = 5_000; // entries per index message we accept
const MAX_INDEX_MSG: usize = 16 * 1024 * 1024; // bytes per index message we accept
const MAX_MEMBERSHIP: usize = 1_000; // items in a library list
const MAX_OFFERS: usize = 256; // offers waiting for `accept`
const MAX_BACKUP_LIBRARIES: usize = 10_000; // libraries we keep encrypted backups of
const PRUNE_MIN_AGE_SECS: u64 = 7 * 24 * 3600; // only chunks this old are ever deleted
const AUTO_PRUNE_EVERY: Duration = Duration::from_secs(3600);
const MAX_BACKUP_HINTS: usize = 64; // new backup locations learned from one device at once
const OUT_QUEUE: usize = 256; // messages waiting to be sent to one device
const FRAME_QUEUE: usize = 64; // received frames waiting to be processed
const MAX_QUEUED_APPLY: usize = 64 * 1024 * 1024; // bytes of index updates waiting to be applied
const MAX_SERVE: usize = 16; // requests from one device answered at the same time
const WRITE_TIMEOUT: Duration = Duration::from_secs(60); // a device that stops reading is dropped
const MAX_HANDSHAKES: usize = 32; // connections that have not proven who they are yet
const MAX_HANDSHAKES_PER_IP: u32 = 4;
pub const KIND_BLOB: u8 = 0;
pub const KIND_RECORD: u8 = 1;
const INVITE_SECS: u64 = 600;
/// How long each side may take to compare the pairing codes and decide.
const APPROVE_SECS: u64 = 300;

// ---------------------------------------------------------------- wire messages

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OfferMsg {
    pub id: String,
    /// What the receiver is asked to do with the library.
    pub role: Role,
    /// Empty for an encrypted-backup offer (the receiver never learns library names).
    pub name: String,
    /// Empty for an encrypted-backup offer (the receiver never gets the vault key).
    pub vault: String,
    /// (Encrypted-backup offer) proof that we know the library's key; see `vault::access_token`.
    pub access: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum Msg {
    Hello { name: String },
    /// Libraries I have confirmed with you (and the role I took), libraries I am offering you,
    /// and (only to devices that sync the library) which device keeps an encrypted backup of it.
    Libraries { shared: Vec<(String, Role)>, offers: Vec<OfferMsg>, backups: Vec<(String, String)> },
    /// A backup I keep for you has new data: fetch it now.
    DepotChanged { lib: String },
    Index { lib: String, entries: Vec<Entry> },
    GetChunk { req: u64, lib: String, path: String, index: u32, hash: String },
    /// Reply to every request (GetChunk and all Depot* calls). None = refused / not found.
    Chunk { req: u64, data: Option<Vec<u8>> },
    Ping,
    DepotPut { req: u64, kind: u8, lib: String, id: String, stamp: u64, data: Vec<u8> },
    DepotGet { req: u64, kind: u8, lib: String, id: String },
    DepotList { req: u64, lib: String },
    DepotHave { req: u64, lib: String, ids: Vec<String> },
    /// -> (bytes used, size limit)
    DepotStat { req: u64, lib: String },
    /// Chunks older than `min_age` seconds, in id order, after id `after` (one page).
    DepotBlobs { req: u64, lib: String, after: String, min_age: u64 },
    DepotDelete { req: u64, lib: String, ids: Vec<String> },
    /// Please send me your library list again (I just paired with someone new).
    Resync,
}

impl Msg {
    /// (request id, library) of a depot request.
    fn depot_req(&self) -> Option<(u64, String)> {
        match self {
            Msg::DepotPut { req, lib, .. }
            | Msg::DepotGet { req, lib, .. }
            | Msg::DepotList { req, lib }
            | Msg::DepotHave { req, lib, .. }
            | Msg::DepotStat { req, lib }
            | Msg::DepotBlobs { req, lib, .. }
            | Msg::DepotDelete { req, lib, .. } => Some((*req, lib.clone())),
            _ => None,
        }
    }
}

// Pairing, after the handshake that proves both sides know the one-time code:
//   joiner  -> inviter   PairHello    its device name
//   inviter -> joiner    PairInfo     its device name
//   both screens show the same verification code; each person compares and decides
//   joiner  -> inviter   PairConfirm  what the person at the joiner decided
//   inviter -> joiner    PairAccept   only if both said yes: the secret for this pair

/// Sent by a joining device over the one-time-code connection.
#[derive(Serialize, Deserialize)]
struct PairHello {
    name: String,
}

#[derive(Serialize, Deserialize)]
struct PairInfo {
    name: String,
}

#[derive(Serialize, Deserialize)]
struct PairConfirm {
    ok: bool,
}

/// The inviting device's final answer. `psk` is the secret this pair of devices will use from
/// now on (empty when `ok` is false).
#[derive(Serialize, Deserialize)]
struct PairAccept {
    ok: bool,
    psk: Vec<u8>,
    name: String,
}

/// What a person is asked to compare and approve while pairing.
pub struct PairPrompt {
    pub name: String,
    pub fingerprint: String,
    pub code: String,
    pub decide: oneshot::Sender<bool>,
}

/// What the inviting side's command hears about a pairing attempt.
enum InviteEvent {
    Verify(PairPrompt),
    Paired(String),
    Failed(String),
}

/// A short code derived from the one-time secret and both public keys. Both devices compute it
/// on their own; it matches only when they agree on who is pairing.
fn pairing_code(secret: &[u8], joiner: &[u8], inviter: &[u8]) -> String {
    let mut h = blake3::Hasher::new_derive_key("rustsync pairing code v1");
    h.update(&(secret.len() as u64).to_le_bytes());
    h.update(secret);
    h.update(&(joiner.len() as u64).to_le_bytes());
    h.update(joiner);
    h.update(&(inviter.len() as u64).to_le_bytes());
    h.update(inviter);
    let mut out = [0u8; 8];
    h.finalize_xof().fill(&mut out);
    let n = u64::from_le_bytes(out) % 100_000_000;
    format!("{:04} {:04}", n / 10_000, n % 10_000)
}

/// A readable fingerprint of a public key, e.g. "ab12 cd34 ef56 7890 1234".
fn fingerprint(key: &[u8]) -> String {
    let h = blake3::hash(key);
    let hex = net::hex(&h.as_bytes()[..10]);
    hex.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join(" ")
}

enum DepotJob {
    /// Pull what the depot has and push what it lacks, for every library shared with it.
    SyncAll,
    Push(String, Vec<Entry>),
    /// Delete chunks in the backup that no record refers to any more.
    Prune(String),
}

type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Option<Vec<u8>>>>>>;

/// Handle used by apply/depot tasks to ask the other side for data.
#[derive(Clone)]
pub struct Remote {
    tx: mpsc::Sender<Msg>,
    pending: Pending,
    next: Arc<AtomicU64>,
    vault: Option<Arc<Vault>>,
}

impl Remote {
    fn with_vault(&self, vault: Arc<Vault>) -> Remote {
        let mut r = self.clone();
        r.vault = Some(vault);
        r
    }

    async fn call(&self, make: impl FnOnce(u64) -> Msg) -> Result<Vec<u8>> {
        let req = self.next.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(req, tx);
        if self.tx.send(make(req)).await.is_err() {
            self.pending.lock().unwrap().remove(&req);
            bail!("session closed");
        }
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Some(data))) => Ok(data),
            Ok(Ok(None)) => bail!("the other side could not or would not serve the request"),
            Ok(Err(_)) => bail!("session closed"),
            Err(_) => {
                self.pending.lock().unwrap().remove(&req);
                bail!("timed out waiting for a reply")
            }
        }
    }

    /// Fetch one plaintext chunk: from a peer directly, or from the depot (then decrypted here).
    pub async fn get_chunk(&self, lib: String, path: String, index: u32, hash: String) -> Result<Vec<u8>> {
        if let Some(vault) = &self.vault {
            let id = vault.blob_id(&hash);
            let id2 = id.clone();
            let sealed = self
                .call(move |req| Msg::DepotGet { req, kind: KIND_BLOB, lib, id: id2 })
                .await?;
            return vault.open_blob(&id, &sealed);
        }
        self.call(move |req| Msg::GetChunk { req, lib, path, index, hash }).await
    }
}

// ---------------------------------------------------------------- node

struct SessionHandle {
    id: u64,
    tx: mpsc::Sender<Msg>,
    jobs: mpsc::UnboundedSender<DepotJob>,
    cancel: CancellationToken,
}

impl SessionHandle {
    /// Queue a message without ever blocking the caller. If the queue is full (the other side
    /// is slow) the message waits in its own task, which ends together with the session.
    fn push(&self, msg: Msg) {
        match self.tx.try_send(msg) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(msg)) => {
                let tx = self.tx.clone();
                let cancel = self.cancel.clone();
                tokio::spawn(async move {
                    tokio::select! {
                        _ = cancel.cancelled() => {}
                        _ = tx.send(msg) => {}
                    }
                });
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {}
        }
    }
}

/// A connection that has not proven who it is yet. Counts against the limits until dropped.
struct PreAuth {
    node: Arc<Node>,
    ip: IpAddr,
    _permit: OwnedSemaphorePermit,
}

impl Drop for PreAuth {
    fn drop(&mut self) {
        let mut m = self.node.ip_conns.lock().unwrap();
        let remove = match m.get_mut(&self.ip) {
            Some(c) => {
                *c = c.saturating_sub(1);
                *c == 0
            }
            None => false,
        };
        if remove {
            m.remove(&self.ip);
        }
    }
}

/// A running library: its folder, vault and background tasks.
pub struct LibRuntime {
    pub folder: Arc<Folder>,
    pub vault: Arc<Vault>,
    cancel: CancellationToken,
}

struct Invite {
    secret: Vec<u8>,
    expires: Instant,
    events: mpsc::UnboundedSender<InviteEvent>,
}

enum Cand {
    Device(String),
    Invite(Vec<u8>),
}

pub struct Node {
    pub cfg: Config,
    keys: Keys,
    name: String,
    /// Encrypted backups kept here for other devices, by library id.
    depots: Mutex<HashMap<String, Arc<Depot>>>,
    /// (library, backup device) -> (bytes used, size limit) as that device last reported.
    usage: Mutex<HashMap<(String, String), (u64, u64)>>,
    state: Mutex<State>,
    libs: Mutex<HashMap<String, Arc<LibRuntime>>>,
    sessions: Mutex<HashMap<String, SessionHandle>>,
    invites: Mutex<Vec<Invite>>,
    dialers: Mutex<HashSet<String>>,
    handshakes: Arc<Semaphore>,
    ip_conns: Mutex<HashMap<IpAddr, u32>>,
    next_session: AtomicU64,
    pub shutdown: CancellationToken,
}

fn entry_cost(e: &Entry) -> usize {
    128 + e.path.len() + e.hash.len() + e.chunks.len() * 72
}

/// Split entries into messages of limited size (a few huge files can be as big as many small ones).
fn split_batches(entries: &[Entry]) -> Vec<Vec<Entry>> {
    let mut out: Vec<Vec<Entry>> = Vec::new();
    let mut cur: Vec<Entry> = Vec::new();
    let mut bytes = 0usize;
    for e in entries {
        let c = entry_cost(e);
        if !cur.is_empty() && (cur.len() >= MAX_BATCH_ENTRIES || bytes + c > MAX_BATCH_BYTES) {
            out.push(std::mem::take(&mut cur));
            bytes = 0;
        }
        bytes += c;
        cur.push(e.clone());
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Does the folder hold anything besides our own marker file?
fn canon_has_files(dir: &str) -> bool {
    match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().any(|e| e.file_name().to_string_lossy() != crate::depot::MARKER),
        Err(_) => false,
    }
}

/// A library name as a folder name: no separators, no leading dot.
fn safe_dir_name(name: &str) -> String {
    let t: String = clean_name(name)
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '_' } else { c })
        .collect();
    let t = t.trim_start_matches('.').trim().to_string();
    if t.is_empty() {
        "library".to_string()
    } else {
        t
    }
}

fn short(k: &str) -> &str {
    &k[..8.min(k.len())]
}

/// Names come from other machines: drop control characters, limit the length.
fn clean_name(s: &str) -> String {
    let t: String = s.chars().filter(|c| !c.is_control()).take(64).collect();
    let t = t.trim().to_string();
    if t.is_empty() {
        "unnamed".to_string()
    } else {
        t
    }
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut v);
    v
}

impl Node {
    fn save_state(&self) {
        let st = self.state.lock().unwrap();
        if let Err(e) = st.save(&self.cfg.state_file()) {
            warn!("cannot save state: {:#}", e);
        }
    }

    fn is_connected(&self, key: &str) -> bool {
        self.sessions
            .lock()
            .unwrap()
            .get(key)
            .map(|s| !s.cancel.is_cancelled())
            .unwrap_or(false)
    }

    fn lib_rt(&self, id: &str) -> Option<Arc<LibRuntime>> {
        self.libs.lock().unwrap().get(id).cloned()
    }

    /// Does this device sync the plain files of this library with us?
    fn is_peer(&self, lib: &str, dev: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.libraries.iter().any(|l| l.id == lib && l.has(dev, Role::Peer))
    }

    /// Does this device keep an encrypted backup of this library for us?
    fn is_store(&self, lib: &str, dev: &str) -> bool {
        let st = self.state.lock().unwrap();
        st.libraries.iter().any(|l| l.id == lib && l.has(dev, Role::Store))
    }

    /// The encrypted backup of this library, if we keep one for that device.
    fn depot_for(&self, dev: &str, lib: &str) -> Option<Arc<Depot>> {
        let allowed = {
            let st = self.state.lock().unwrap();
            st.libraries
                .iter()
                .any(|l| l.id == lib && l.mode == LibMode::Backup && l.has(dev, Role::Client))
        };
        if !allowed {
            return None;
        }
        self.depots.lock().unwrap().get(lib).cloned()
    }

    /// Tell the other devices that back up this library here that there is new data.
    fn notify_depot_changed(&self, lib: &str, except: &str) {
        let others: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.libraries
                .iter()
                .find(|l| l.id == lib && l.mode == LibMode::Backup)
                .map(|l| l.devices(Role::Client))
                .unwrap_or_default()
        };
        let sessions = self.sessions.lock().unwrap();
        for d in others.iter().filter(|d| d.as_str() != except) {
            if let Some(s) = sessions.get(d) {
                s.push(Msg::DepotChanged { lib: lib.to_string() });
            }
        }
    }

    /// Libraries (that are running here) that this device has confirmed in the given role.
    fn libs_with(&self, dev: &str, role: Role) -> Vec<(String, Arc<LibRuntime>)> {
        let ids: Vec<String> = {
            let st = self.state.lock().unwrap();
            st.libraries
                .iter()
                .filter(|l| l.has(dev, role))
                .map(|l| l.id.clone())
                .collect()
        };
        let libs = self.libs.lock().unwrap();
        ids.into_iter()
            .filter_map(|id| libs.get(&id).map(|rt| (id.clone(), rt.clone())))
            .collect()
    }

    /// Send local changes to every device that has the library (except `except`). Syncing
    /// devices get the plain index; a device that only keeps a backup gets the entries through
    /// its worker, which encrypts them first.
    pub fn broadcast(&self, lib: &str, entries: &[Entry], except: Option<&str>) {
        if entries.is_empty() {
            return;
        }
        let members: Vec<Member> = {
            let st = self.state.lock().unwrap();
            st.libraries
                .iter()
                .find(|l| l.id == lib)
                .map(|l| l.members.iter().filter(|m| m.accepted).cloned().collect())
                .unwrap_or_default()
        };
        let sessions = self.sessions.lock().unwrap();
        for m in members {
            if except == Some(m.device.as_str()) {
                continue;
            }
            let Some(s) = sessions.get(&m.device) else { continue };
            match m.role {
                Role::Peer => {
                    for batch in split_batches(entries) {
                        s.push(Msg::Index { lib: lib.to_string(), entries: batch });
                    }
                }
                Role::Store => {
                    let _ = s.jobs.send(DepotJob::Push(lib.to_string(), entries.to_vec()));
                }
                Role::Client => {}
            }
        }
    }

    /// Send the whole index of a library to one connected syncing device.
    fn push_index(&self, dev: &str, lib: &str) {
        if !self.is_peer(lib, dev) {
            return;
        }
        let Some(rt) = self.lib_rt(lib) else { return };
        let snap = rt.folder.snapshot();
        let (tx, cancel) = {
            let sessions = self.sessions.lock().unwrap();
            let Some(s) = sessions.get(dev) else { return };
            (s.tx.clone(), s.cancel.clone())
        };
        let lib = lib.to_string();
        // Sent from its own task so a big library never blocks us (or overflows the queue).
        tokio::spawn(async move {
            for batch in split_batches(&snap) {
                let msg = Msg::Index { lib: lib.clone(), entries: batch };
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    r = tx.send(msg) => {
                        if r.is_err() {
                            return;
                        }
                    }
                }
            }
        });
    }

    // ------------------------------------------------------------ library membership

    fn libraries_msg_for(&self, dev: &str) -> Msg {
        let st = self.state.lock().unwrap();
        let mut shared: Vec<(String, Role)> = Vec::new();
        let mut offers: Vec<OfferMsg> = Vec::new();
        let mut backups: Vec<(String, String)> = Vec::new();
        for l in &st.libraries {
            if l.mode == LibMode::Backup {
                // We keep this library's encrypted backup for that device: confirm it.
                if l.has(dev, Role::Client) {
                    shared.push((l.id.clone(), Role::Store));
                }
                continue;
            }
            let access = net::parse_hex32(&l.vault, "a vault key")
                .map(|k| vault::access_token(&k))
                .unwrap_or_default();
            let dev_is_peer = l.has(dev, Role::Peer);
            for m in l.members.iter().filter(|m| m.device == dev) {
                match (m.accepted, m.role) {
                    (true, Role::Peer) => shared.push((l.id.clone(), Role::Peer)),
                    (false, Role::Peer) => offers.push(OfferMsg {
                        id: l.id.clone(),
                        role: Role::Peer,
                        name: l.name.clone(),
                        vault: l.vault.clone(),
                        access: String::new(),
                    }),
                    // Never the name or the key: all the other side sees is an opaque id and a
                    // proof that we hold the key.
                    (false, Role::Store) => offers.push(OfferMsg {
                        id: l.id.clone(),
                        role: Role::Store,
                        name: String::new(),
                        vault: String::new(),
                        access: access.clone(),
                    }),
                    // They keep a backup for us: tell them we still use it (if we stop, they notice).
                    (true, Role::Store) => shared.push((l.id.clone(), Role::Client)),
                    (_, Role::Client) => {}
                }
            }
            // Devices that sync this library learn where its encrypted backups are, so they can
            // use them too (this is how a machine that was off catches up from the server).
            if dev_is_peer {
                for m in l.members.iter().filter(|m| m.role == Role::Store && m.accepted) {
                    backups.push((l.id.clone(), m.device.clone()));
                }
            }
        }
        Msg::Libraries { shared, offers, backups }
    }

    /// Tell connected devices (all, or just `only`) which libraries we share / offer.
    fn notify_libraries(&self, only: Option<&str>) {
        let devs: Vec<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        for dev in devs {
            if let Some(o) = only {
                if o != dev {
                    continue;
                }
            }
            let msg = self.libraries_msg_for(&dev);
            let sessions = self.sessions.lock().unwrap();
            if let Some(h) = sessions.get(&dev) {
                h.push(msg);
            }
        }
    }

    /// Apply a device's `Libraries` message. Returns (offers of ours it just confirmed, with the
    /// role it took; whether anything changed).
    fn process_libraries(
        &self,
        from: &str,
        shared: Vec<(String, Role)>,
        offers: Vec<OfferMsg>,
        backups: Vec<(String, String)>,
    ) -> (Vec<(String, Role)>, bool) {
        let mut newly: Vec<(String, Role)> = Vec::new();
        let mut changed = false;
        let mut announce: Vec<(String, String)> = Vec::new();
        let mut hints: Vec<String> = Vec::new();
        let mut gone: Vec<(String, String)> = Vec::new();
        {
            let mut st = self.state.lock().unwrap();
            let from_name = st.device_name(from);
            for (id, role) in &shared {
                if let Some(lib) = st.libraries.iter_mut().find(|l| &l.id == id) {
                    if let Some(m) = lib
                        .members
                        .iter_mut()
                        .find(|m| m.device == from && m.role == *role && !m.accepted)
                    {
                        m.accepted = true;
                        newly.push((id.clone(), *role));
                        changed = true;
                    }
                }
            }
            // A device that no longer lists a library it had confirmed has dropped it (removed the
            // library, or unshared it from us): forget it as a member here too. A library it is
            // still offering counts as listed (the confirmation may be on its way).
            for lib in st.libraries.iter_mut() {
                let id = lib.id.clone();
                let still_offered = offers.iter().any(|o| o.id == id);
                let before = lib.members.len();
                lib.members.retain(|m| {
                    m.device != from || !m.accepted || still_offered || shared.iter().any(|(i, r)| *i == id && *r == m.role)
                });
                if lib.members.len() != before {
                    changed = true;
                    gone.push((lib.name.clone(), from_name.clone()));
                }
            }
            for o in offers {
                if !net::valid_hex(&o.id, 32) {
                    continue;
                }
                match o.role {
                    Role::Store => {
                        // A paired device wants to keep an encrypted backup here.
                        if !self.cfg.accept_backups || !net::valid_hex(&o.access, 64) {
                            continue;
                        }
                        // (backup mode?, key proof matches?, owner unshared this device before?)
                        let found = st
                            .libraries
                            .iter()
                            .find(|l| l.id == o.id)
                            .map(|l| (l.mode == LibMode::Backup, l.access == o.access, l.excluded.iter().any(|d| d == from)));
                        match found {
                            // Already a backup we keep: a device that proves it knows the key
                            // (another of your machines, or a replacement after a loss) joins
                            // without asking again.
                            Some((true, true, false)) => {
                                if let Some(lib) = st.libraries.iter_mut().find(|l| l.id == o.id) {
                                    if !lib.has(from, Role::Client) && lib.members.len() < MAX_MEMBERSHIP {
                                        lib.members.retain(|m| m.device != from);
                                        lib.members.push(Member { device: from.to_string(), role: Role::Client, accepted: true });
                                        changed = true;
                                        info!("{} now uses the encrypted backup kept here", from_name);
                                    }
                                }
                                continue;
                            }
                            // We stopped serving this device: ask the owner again.
                            Some((true, true, true)) | None => {}
                            // A library of ours that is not such a backup, or a wrong key proof.
                            Some(_) => continue,
                        }
                        if st.offers.iter().any(|x| x.id == o.id && x.from == from) {
                            continue;
                        }
                        if st.offers.len() >= MAX_OFFERS || st.libraries.len() >= MAX_BACKUP_LIBRARIES {
                            continue;
                        }
                        let name = format!("backup-{}", &o.id[..8]);
                        announce.push((from_name.clone(), name.clone()));
                        st.offers.push(Offer {
                            from: from.to_string(),
                            id: o.id,
                            name,
                            vault: String::new(),
                            backup: true,
                            access: o.access,
                        });
                        changed = true;
                    }
                    Role::Peer => {
                        if !net::valid_hex(&o.vault, 64) {
                            continue;
                        }
                        if st.libraries.iter().any(|l| l.id == o.id) {
                            continue;
                        }
                        if st.offers.iter().any(|x| x.id == o.id && x.from == from) {
                            continue;
                        }
                        if st.offers.len() >= MAX_OFFERS {
                            continue; // accept or decline some of the waiting offers first
                        }
                        let name = clean_name(&o.name);
                        announce.push((from_name.clone(), name.clone()));
                        st.offers.push(Offer {
                            from: from.to_string(),
                            id: o.id,
                            name,
                            vault: o.vault,
                            backup: false,
                            access: String::new(),
                        });
                        changed = true;
                    }
                    Role::Client => {}
                }
            }

            // A device that syncs a library with us says where its encrypted backups are. If we
            // are paired with that device too, we use the backup as well.
            let mut learned = 0usize;
            for (id, store) in backups {
                if learned >= MAX_BACKUP_HINTS || !net::valid_hex(&id, 32) || !net::valid_hex(&store, 64) {
                    continue;
                }
                if store == net::hex(&self.keys.public) {
                    continue;
                }
                let paired = st.device(&store).is_some();
                let store_name = st.device_name(&store);
                let Some(lib) = st.libraries.iter_mut().find(|l| l.id == id) else { continue };
                if lib.mode != LibMode::Sync || !lib.has(from, Role::Peer) {
                    continue;
                }
                if lib.members.iter().any(|m| m.device == store)
                    || lib.members.len() >= MAX_MEMBERSHIP
                    || lib.excluded.iter().any(|d| *d == store)
                {
                    continue;
                }
                if !paired {
                    hints.push(format!(
                        "\"{}\" is backed up on a device you are not paired with ({}...). Pair with it (rustsync invite / join) to use that backup.",
                        lib.name,
                        short(&store)
                    ));
                    continue;
                }
                lib.members.push(Member { device: store, role: Role::Store, accepted: false });
                learned += 1;
                changed = true;
                info!("\"{}\": also using the encrypted backup on {}", lib.name, store_name);
            }
        }
        if changed {
            self.save_state();
        }
        for (who, lib) in announce {
            info!("{} offers you \"{}\". Accept it with: rustsync -c <config> accept", who, lib);
        }
        for h in hints {
            info!("{}", h);
        }
        for (lib, who) in gone {
            info!("{} no longer shares \"{}\" with this machine", who, lib);
        }
        (newly, changed)
    }

    // ------------------------------------------------------------ libraries

    /// A library folder must never contain (or be inside) anything that holds secrets: our
    /// state directory or either key file. Otherwise the private key could be synced to other
    /// devices like any other file. (Folders of encrypted backups kept here are libraries too, so
    /// the overlap check in `prepare_folder` keeps synced folders away from them.)
    fn check_protected(&self, canon: &Path) -> Result<()> {
        let protected: Vec<(PathBuf, &str)> = vec![
            (self.cfg.dir.clone(), "rustsync's own data"),
            (self.cfg.private_key_path(), "this machine's private key"),
            (self.cfg.public_key_path(), "this machine's public key"),
        ];
        for (p, what) in protected {
            let real = std::fs::canonicalize(&p).unwrap_or_else(|_| p.clone());
            for q in [p, real] {
                if q.starts_with(canon) || canon.starts_with(&q) {
                    bail!(
                        "that folder contains {} ({}), which must not be synced; choose a different folder",
                        what,
                        q.display()
                    );
                }
            }
        }
        Ok(())
    }

    /// Check a folder is usable for a library and return its canonical path.
    fn prepare_folder(&self, folder: &str, backup: bool) -> Result<String> {
        let p = Path::new(folder);
        if !p.is_absolute() {
            bail!("the folder must be an absolute path");
        }
        std::fs::create_dir_all(p).with_context(|| format!("cannot create {}", folder))?;
        let canon = std::fs::canonicalize(p)?;
        if backup {
            // Encrypted backups hold nothing readable, so they may live inside our data
            // directory (the default), but never around it or around the key files.
            let own = std::fs::canonicalize(&self.cfg.dir).unwrap_or_else(|_| self.cfg.dir.clone());
            let mut keys = vec![self.cfg.private_key_path(), self.cfg.public_key_path()];
            keys.push(own);
            for q in keys {
                let real = std::fs::canonicalize(&q).unwrap_or_else(|_| q.clone());
                if real.starts_with(&canon) {
                    bail!("that folder contains rustsync's own data or keys ({}); choose a different folder", real.display());
                }
            }
        } else {
            self.check_protected(&canon)?;
        }
        let st = self.state.lock().unwrap();
        for l in &st.libraries {
            let other = std::fs::canonicalize(&l.folder).unwrap_or_else(|_| PathBuf::from(&l.folder));
            if other.starts_with(&canon) || canon.starts_with(&other) {
                bail!("that folder overlaps with the library \"{}\" ({})", l.name, l.folder);
            }
        }
        Ok(canon.to_string_lossy().to_string())
    }

    fn unique_name(&self, wanted: &str) -> String {
        let st = self.state.lock().unwrap();
        let taken = |n: &str| st.libraries.iter().any(|l| l.name == n);
        if !taken(wanted) {
            return wanted.to_string();
        }
        let mut i = 2;
        loop {
            let candidate = format!("{}-{}", wanted, i);
            if !taken(&candidate) {
                return candidate;
            }
            i += 1;
        }
    }

    /// Open the folder, scan it and start watching it.
    pub async fn start_library(self: &Arc<Self>, lib: &Library) -> Result<()> {
        if !Path::new(&lib.folder).is_dir() {
            bail!(
                "the folder {} does not exist; not starting this library (so a missing or unmounted folder is never mistaken for deleted files)",
                lib.folder
            );
        }
        self.check_protected(&std::fs::canonicalize(&lib.folder)?)?;
        let skew = self.cfg.sync_options.max_clock_skew_secs.saturating_mul(1_000_000_000);
        let folder = Arc::new(Folder::open(lib.id.clone(), &lib.folder, &self.cfg.dir, skew)?);
        index::clean_tmp(&folder.root);
        let vault = Arc::new(Vault::new(&net::parse_hex32(&lib.vault, "the library's vault key")?));
        let rt = Arc::new(LibRuntime { folder: folder.clone(), vault, cancel: CancellationToken::new() });

        // Pick up everything that changed while we were not running.
        let f2 = folder.clone();
        let _ = tokio::task::spawn_blocking(move || index::scan(&f2, &[String::new()], false)).await?;

        let (watcher, rx) = start_watcher(&folder)?;
        let every = self.cfg.sync_options.full_scan_interval;
        let deep_every = self.cfg.sync_options.deep_scan_every;
        tokio::spawn(watch_loop(self.clone(), rt.clone(), rx, watcher));
        tokio::spawn(periodic_loop(self.clone(), rt.clone(), every, deep_every));
        self.libs.lock().unwrap().insert(lib.id.clone(), rt);
        info!("library \"{}\" -> {}", lib.name, folder.root.display());
        Ok(())
    }

    /// Open the folder that holds an encrypted backup we keep for other devices.
    pub fn start_backup(&self, lib: &Library, create: bool) -> Result<()> {
        let depot = Depot::open(Path::new(&lib.folder), &lib.id, create, lib.quota)?;
        self.depots.lock().unwrap().insert(lib.id.clone(), Arc::new(depot));
        info!("keeping an encrypted backup ({}) in {}", &lib.id[..8], lib.folder);
        Ok(())
    }

    fn set_usage(&self, lib: &str, dev: &str, used: u64, quota: u64) {
        self.usage.lock().unwrap().insert((lib.to_string(), dev.to_string()), (used, quota));
    }

    /// Change the size limit of an encrypted backup kept on this machine (0 = none).
    pub fn set_quota(&self, lib_id: &str, bytes: u64) -> Result<String> {
        let name = {
            let mut st = self.state.lock().unwrap();
            let Some(lib) = st.libraries.iter_mut().find(|l| l.id == lib_id && l.mode == LibMode::Backup) else {
                bail!("no such encrypted backup here");
            };
            lib.quota = bytes;
            lib.name.clone()
        };
        if let Some(d) = self.depots.lock().unwrap().get(lib_id) {
            d.set_quota(bytes);
        }
        self.save_state();
        Ok(if bytes == 0 {
            format!("{} now has no size limit.", name)
        } else {
            format!("{} is now limited to {}. Data already stored stays; new data is refused beyond the limit.", name, fmt_size(bytes))
        })
    }

    /// Ask the machines that keep an encrypted backup of this library to delete chunks that
    /// nothing refers to any more (old versions of changed or deleted files).
    pub fn prune_library(&self, lib_id: &str) -> Result<String> {
        let stores: Vec<String> = {
            let st = self.state.lock().unwrap();
            let Some(l) = st.libraries.iter().find(|l| l.id == lib_id && l.mode == LibMode::Sync) else {
                bail!("no such library with files on this machine");
            };
            l.devices(Role::Store)
        };
        if stores.is_empty() {
            bail!("that library has no encrypted backup to clean up");
        }
        let sessions = self.sessions.lock().unwrap();
        let mut asked = 0;
        for d in &stores {
            if let Some(s) = sessions.get(d) {
                let _ = s.jobs.send(DepotJob::Prune(lib_id.to_string()));
                asked += 1;
            }
        }
        if asked == 0 {
            bail!("none of the machines keeping a backup of it are online right now");
        }
        Ok("Cleaning up old data in the backup in the background (only data not used for over 7 days is removed). See the usage with: rustsync status".to_string())
    }

    pub async fn create_library(self: &Arc<Self>, name: &str, folder: &str) -> Result<String> {
        let wanted = clean_name(name);
        if self.state.lock().unwrap().libraries.iter().any(|l| l.name == wanted) {
            bail!("you already have a library named \"{}\"", wanted);
        }
        let canon = self.prepare_folder(folder, false)?;
        let lib = Library {
            id: net::hex(&random_bytes(16)),
            name: wanted.clone(),
            folder: canon,
            vault: net::hex(&random_bytes(32)),
            members: Vec::new(),
            mode: LibMode::Sync,
            access: String::new(),
            quota: 0,
            excluded: Vec::new(),
        };
        self.start_library(&lib).await?;
        self.state.lock().unwrap().libraries.push(lib.clone());
        self.save_state();
        Ok(format!("Created library \"{}\" for {}", wanted, lib.folder))
    }

    pub fn remove_library(&self, id: &str) -> Result<String> {
        let (name, backup) = {
            let mut st = self.state.lock().unwrap();
            let Some(pos) = st.libraries.iter().position(|l| l.id == id) else {
                bail!("no such library");
            };
            let l = st.libraries.remove(pos);
            (l.name, l.mode == LibMode::Backup)
        };
        let rt = self.libs.lock().unwrap().remove(id);
        if let Some(rt) = rt {
            rt.cancel.cancel();
        }
        self.depots.lock().unwrap().remove(id);
        let _ = std::fs::remove_file(self.cfg.dir.join(format!("folder-{}.json", id)));
        self.save_state();
        // Other devices notice the library is gone from our list and stop treating us as a member.
        self.notify_libraries(None);
        if backup {
            Ok(format!(
                "Stopped keeping the encrypted backup \"{}\". The stored data was not deleted; delete its folder yourself to free the space.",
                name
            ))
        } else {
            Ok(format!("Removed library \"{}\" from this machine. Its files were not deleted.", name))
        }
    }

    /// Stop sharing a library with a device. The files stay on both machines. To share again,
    /// use `share` (syncing or backup) or, for a backup this machine keeps, `accept`.
    pub fn unshare_library(&self, lib_id: &str, dev_key: &str) -> Result<String> {
        let (lname, dname, backup) = {
            let mut st = self.state.lock().unwrap();
            let dname = st.device_name(dev_key);
            let Some(lib) = st.libraries.iter_mut().find(|l| l.id == lib_id) else {
                bail!("no such library");
            };
            let before = lib.members.len();
            lib.members.retain(|m| m.device != dev_key);
            if lib.members.len() == before {
                bail!("\"{}\" is not shared with {}", lib.name, dname);
            }
            // Keep other devices' hints (about backups) from quietly adding it back.
            if !lib.excluded.iter().any(|d| d == dev_key) {
                lib.excluded.push(dev_key.to_string());
            }
            (lib.name.clone(), dname, lib.mode == LibMode::Backup)
        };
        self.save_state();
        // The other side sees the library missing from our list and drops us too.
        self.notify_libraries(None);
        Ok(if backup {
            format!("{} no longer uses the encrypted backup \"{}\" here. If it asks again you can allow it with: accept", dname, lname)
        } else {
            format!("Stopped sharing \"{}\" with {}. The files stay on both machines; share again any time.", lname, dname)
        })
    }

    /// Re-send our library lists to everyone connected, ask them for theirs, and re-check
    /// the backups. Useful after pairing with a new device.
    pub fn refresh(&self) -> Result<String> {
        self.notify_libraries(None);
        self.request_resync();
        let sessions = self.sessions.lock().unwrap();
        for s in sessions.values() {
            let _ = s.jobs.send(DepotJob::SyncAll);
        }
        Ok(format!("Refreshed with {} connected device(s).", sessions.len()))
    }

    /// Ask every connected device to send its library list again (it may now mention backups
    /// on a device we just paired with).
    fn request_resync(&self) {
        let sessions = self.sessions.lock().unwrap();
        for s in sessions.values() {
            s.push(Msg::Resync);
        }
    }

    pub fn share_library(&self, lib_id: &str, dev_key: &str, backup: bool) -> Result<String> {
        let role = if backup { Role::Store } else { Role::Peer };
        let (lname, dname, confirmed) = {
            let mut st = self.state.lock().unwrap();
            let Some(dev) = st.device(dev_key).cloned() else {
                bail!("no such device");
            };
            let Some(lib) = st.libraries.iter_mut().find(|l| l.id == lib_id) else {
                bail!("no such library");
            };
            if lib.mode == LibMode::Backup {
                bail!("this machine only keeps an encrypted backup of that library, so it can't share it. Share it from a machine that has the files.");
            }
            let existing: Option<(Role, bool)> = lib
                .members
                .iter()
                .find(|m| m.device == dev_key)
                .map(|m| (m.role, m.accepted));
            let mut confirmed = false;
            match existing {
                Some((r, _)) if r != role => bail!(
                    "\"{}\" is already shared with {} ({}); one device can't have both. Use the other library name, or `forget` the device first.",
                    lib.name,
                    dev.name,
                    r.word()
                ),
                Some((_, accepted)) => confirmed = accepted,
                None => lib.members.push(Member { device: dev_key.to_string(), role, accepted: false }),
            }
            lib.excluded.retain(|d| d != dev_key);
            (lib.name.clone(), dev.name, confirmed)
        };
        if confirmed {
            return Ok(format!("\"{}\" is already shared with {}.", lname, dname));
        }
        self.save_state();
        self.notify_libraries(Some(dev_key));
        let online = self.is_connected(dev_key);
        Ok(match (role, online) {
            (Role::Store, true) => format!(
                "Backing up \"{}\" on {}. Only encrypted data is uploaded; {} can't read it.",
                lname, dname, dname
            ),
            (Role::Store, false) => format!(
                "Backing up \"{}\" on {}, which is offline. It is set up when {} connects.",
                lname, dname, dname
            ),
            (Role::Peer, true) => format!(
                "Offered \"{}\" to {}. It starts syncing once {} accepts (rustsync -c <their config> accept).",
                lname, dname, dname
            ),
            (Role::Peer, false) => {
                format!("Offered \"{}\" to {}, which is offline. The offer is delivered when it connects.", lname, dname)
            }
            (Role::Client, _) => String::new(),
        })
    }

    pub async fn accept_offer(self: &Arc<Self>, offer_id: &str, folder: &str, quota: u64) -> Result<String> {
        let offer = {
            let st = self.state.lock().unwrap();
            st.offers.iter().find(|o| o.id == offer_id).cloned()
        };
        let Some(offer) = offer else {
            bail!("no such offer (it may have been withdrawn)");
        };
        let have_backup = self
            .state
            .lock()
            .unwrap()
            .libraries
            .iter()
            .any(|l| l.id == offer.id && l.mode == LibMode::Backup);
        if offer.backup && have_backup {
            // A device we stopped serving asks again: allow it, no new folder needed.
            let dname = {
                let mut st = self.state.lock().unwrap();
                if let Some(l) = st.libraries.iter_mut().find(|l| l.id == offer.id) {
                    l.excluded.retain(|d| *d != offer.from);
                    l.members.retain(|m| m.device != offer.from);
                    l.members.push(Member { device: offer.from.clone(), role: Role::Client, accepted: true });
                }
                st.offers.retain(|o| !(o.id == offer.id && o.from == offer.from));
                st.device_name(&offer.from)
            };
            self.save_state();
            self.notify_libraries(Some(&offer.from));
            return Ok(format!("{} can use the encrypted backup kept here again.", dname));
        }
        if self.state.lock().unwrap().libraries.iter().any(|l| l.id == offer.id) {
            bail!("this machine already has that library");
        }
        let canon = self.prepare_folder(folder, offer.backup)?;

        if offer.backup {
            // Keep an encrypted copy for the sender, in the folder chosen here. Nothing in it
            // can be read on this machine.
            if canon_has_files(&canon) {
                bail!("{} is not empty; choose an empty folder for the encrypted backup", canon);
            }
            let lib = Library {
                id: offer.id.clone(),
                name: offer.name.clone(),
                folder: canon,
                vault: String::new(),
                members: vec![Member { device: offer.from.clone(), role: Role::Client, accepted: true }],
                mode: LibMode::Backup,
                access: offer.access.clone(),
                quota,
                excluded: Vec::new(),
            };
            self.start_backup(&lib, true)?;
            {
                let mut st = self.state.lock().unwrap();
                st.offers.retain(|o| o.id != lib.id);
                st.libraries.push(lib.clone());
            }
            self.save_state();
            self.notify_libraries(Some(&offer.from));
            return Ok(format!(
                "Keeping an encrypted backup for {} in {} ({}). Your paired devices that hold the library key can use it too; you can't read it here.",
                self.state.lock().unwrap().device_name(&offer.from),
                lib.folder,
                if quota == 0 { "no size limit".to_string() } else { format!("limit {}", fmt_size(quota)) }
            ));
        }

        let lib = Library {
            id: offer.id.clone(),
            name: self.unique_name(&offer.name),
            folder: canon,
            vault: offer.vault.clone(),
            members: vec![Member { device: offer.from.clone(), role: Role::Peer, accepted: true }],
            mode: LibMode::Sync,
            access: String::new(),
            quota: 0,
            excluded: Vec::new(),
        };
        self.start_library(&lib).await?;
        {
            let mut st = self.state.lock().unwrap();
            st.offers.retain(|o| o.id != lib.id);
            st.libraries.push(lib.clone());
        }
        self.save_state();
        self.notify_libraries(Some(&offer.from));
        self.push_index(&offer.from, &lib.id);
        Ok(format!("Accepted \"{}\"; syncing into {}", lib.name, lib.folder))
    }

    /// The recovery code of one library, or (no argument) the recovery kit for all libraries
    /// plus every individual code. Anyone holding a code or kit and a copy of the encrypted
    /// backup can read the files, so these belong in a password manager, not in a chat or a log.
    pub fn recovery_codes(&self, which: Option<&str>) -> Result<String> {
        let st = self.state.lock().unwrap();
        let mut out = String::new();
        let mut kit: Vec<(String, String, String)> = Vec::new();
        let mut n = 0;
        for l in st.libraries.iter().filter(|l| l.mode == LibMode::Sync) {
            if let Some(w) = which {
                if l.name != w && !l.id.starts_with(w) {
                    continue;
                }
            }
            n += 1;
            kit.push((l.id.clone(), l.name.clone(), l.vault.clone()));
            out.push_str(&format!("{}  (id {})\n  {}\n", l.name, &l.id[..8], vault::recovery_code(&l.id, &l.vault)));
        }
        if n == 0 {
            match which {
                Some(w) => bail!("no library named \"{}\" with files on this machine", w),
                None => bail!("this machine has no libraries with files to protect"),
            }
        }
        if which.is_some() {
            return Ok(out.trim_end().to_string());
        }
        Ok(format!(
            "Recovery kit (all {} libraries, one line):\n  {}\n\nIndividual codes:\n{}",
            n,
            vault::recovery_kit(&kit),
            out.trim_end()
        ))
    }

    /// Set up one restored library: folder, key and the device whose backup to read from.
    async fn restore_one(self: &Arc<Self>, id: &str, vault_hex: &str, name: &str, folder: &str, device_key: &str) -> Result<Library> {
        let canon = self.prepare_folder(folder, false)?;
        let lib = Library {
            id: id.to_string(),
            name: self.unique_name(&clean_name(name)),
            folder: canon,
            vault: vault_hex.to_string(),
            members: vec![Member { device: device_key.to_string(), role: Role::Store, accepted: false }],
            mode: LibMode::Sync,
            access: String::new(),
            quota: 0,
            excluded: Vec::new(),
        };
        self.start_library(&lib).await?;
        self.state.lock().unwrap().libraries.push(lib.clone());
        self.save_state();
        Ok(lib)
    }

    /// Bring libraries back after losing every machine that had them: rebuild from a recovery
    /// code (one library) or a recovery kit (all of them, one subfolder each) and the encrypted
    /// backup kept on `device_key`.
    pub async fn restore_library(self: &Arc<Self>, code: &str, folder: &str, device_key: &str) -> Result<String> {
        let dname = {
            let st = self.state.lock().unwrap();
            let Some(dev) = st.device(device_key) else { bail!("no such device") };
            dev.name.clone()
        };
        let code = code.trim();
        let mut done: Vec<String> = Vec::new();
        let mut skipped: Vec<String> = Vec::new();
        if code.to_lowercase().starts_with("rsk1-") {
            let items = vault::parse_recovery_kit(code)?;
            if items.is_empty() {
                bail!("the recovery kit holds no libraries");
            }
            let mut used_dirs: HashSet<String> = HashSet::new();
            for (id, name, vault_hex) in items {
                if self.state.lock().unwrap().libraries.iter().any(|l| l.id == id) {
                    skipped.push(name);
                    continue;
                }
                let base = safe_dir_name(&name);
                let mut dir = base.clone();
                let mut i = 2;
                while !used_dirs.insert(dir.clone()) {
                    dir = format!("{}-{}", base, i);
                    i += 1;
                }
                let target = Path::new(folder).join(&dir).to_string_lossy().to_string();
                self.restore_one(&id, &vault_hex, &name, &target, device_key)
                    .await
                    .with_context(|| format!("restoring \"{}\"", name))?;
                done.push(name);
            }
        } else {
            let (id, vault_hex) = vault::parse_recovery_code(code)?;
            if self.state.lock().unwrap().libraries.iter().any(|l| l.id == id) {
                bail!("this machine already has that library");
            }
            let name = Path::new(folder)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("restored-{}", &id[..8]));
            self.restore_one(&id, &vault_hex, &name, folder, device_key).await?;
            done.push(name);
        }
        if done.is_empty() {
            return Ok("Nothing to restore: this machine already has all of those libraries.".to_string());
        }
        // The offer carries the proof of the key; the backup machine lets us in because of it.
        self.notify_libraries(Some(device_key));
        Ok(format!(
            "Restoring {} from the backup on {} into {}.{}{}",
            done.join(", "),
            dname,
            folder,
            if skipped.is_empty() { String::new() } else { format!(" (Skipped, already here: {}.)", skipped.join(", ")) },
            if self.is_connected(device_key) {
                " Files appear as they download (a library this device keeps no backup of stays empty); watch progress with: rustsync status"
            } else {
                " That machine is offline right now; the restore starts when it connects."
            }
        ))
    }

    pub fn decline_offer(&self, offer_id: &str) -> Result<String> {
        let name = {
            let mut st = self.state.lock().unwrap();
            let Some(pos) = st.offers.iter().position(|o| o.id == offer_id) else {
                bail!("no such offer");
            };
            st.offers.remove(pos).name
        };
        self.save_state();
        Ok(format!("Declined \"{}\".", name))
    }

    pub fn forget_device(&self, key: &str) -> Result<String> {
        let name = {
            let mut st = self.state.lock().unwrap();
            let Some(pos) = st.devices.iter().position(|d| d.key == key) else {
                bail!("no such device");
            };
            let dev = st.devices.remove(pos);
            for l in st.libraries.iter_mut() {
                l.members.retain(|m| m.device != key);
            }
            st.offers.retain(|o| o.from != key);
            dev.name
        };
        self.save_state();
        if let Some(s) = self.sessions.lock().unwrap().get(key) {
            s.cancel.cancel();
        }
        Ok(format!("Forgot {}.", name))
    }

    /// Change the address this machine dials to reach a paired device.
    pub fn set_device_address(self: &Arc<Self>, key: &str, addr: &str) -> Result<String> {
        let addr = addr.trim();
        let target = if addr.is_empty() { None } else { Some(parse_host_port(addr, self.cfg.server.port)?) };
        let name = {
            let mut st = self.state.lock().unwrap();
            let Some(dev) = st.devices.iter_mut().find(|d| d.key == key) else {
                bail!("no such device");
            };
            match &target {
                Some((h, p)) => {
                    dev.host = Some(h.clone());
                    dev.port = Some(*p);
                }
                None => {
                    dev.host = None;
                    dev.port = None;
                }
            }
            dev.name.clone()
        };
        self.save_state();
        match target {
            Some((h, p)) => {
                // Starts dialing if nothing was (the device may have only ever called us).
                self.ensure_dialer(key);
                Ok(format!("{} is now reached at {}:{}.", name, h, p))
            }
            None => Ok(format!("{} will have to connect to this machine; it is no longer dialed.", name)),
        }
    }

    pub fn status(&self) -> StatusInfo {
        let online: HashSet<String> = self.sessions.lock().unwrap().keys().cloned().collect();
        let st = self.state.lock().unwrap();
        let devices: Vec<DeviceInfo> = st
            .devices
            .iter()
            .map(|d| DeviceInfo {
                key: d.key.clone(),
                name: d.name.clone(),
                online: online.contains(&d.key),
                addr: match (&d.host, d.port) {
                    (Some(h), Some(p)) => Some(format!("{}:{}", h, p)),
                    _ => None,
                },
            })
            .collect();
        let names = |keys: &Vec<String>| -> Vec<String> { keys.iter().map(|k| st.device_name(k)).collect() };
        let depots = self.depots.lock().unwrap();
        let usage = self.usage.lock().unwrap();
        let libraries: Vec<LibraryInfo> = st
            .libraries
            .iter()
            .map(|l| LibraryInfo {
                id: l.id.clone(),
                name: l.name.clone(),
                folder: l.folder.clone(),
                backup_only: l.mode == LibMode::Backup,
                kept_for: names(&l.devices(Role::Client)),
                syncing_with: names(&l.devices(Role::Peer)),
                backed_up_on: names(&l.devices(Role::Store)),
                used: depots.get(&l.id).map(|d| d.used()).unwrap_or(0),
                quota: depots.get(&l.id).map(|d| d.quota()).unwrap_or(l.quota),
                backups: l
                    .devices(Role::Store)
                    .iter()
                    .map(|d| {
                        let u = usage.get(&(l.id.clone(), d.clone())).copied();
                        BackupUsage {
                            device: st.device_name(d),
                            known: u.is_some(),
                            used: u.map(|x| x.0).unwrap_or(0),
                            quota: u.map(|x| x.1).unwrap_or(0),
                        }
                    })
                    .collect(),
                pending: l
                    .members
                    .iter()
                    .filter(|m| !m.accepted)
                    .map(|m| {
                        let n = st.device_name(&m.device);
                        if m.role == Role::Store {
                            format!("{} (encrypted backup)", n)
                        } else {
                            n
                        }
                    })
                    .collect(),
            })
            .collect();
        let offers: Vec<OfferInfo> = st
            .offers
            .iter()
            .map(|o| OfferInfo {
                id: o.id.clone(),
                name: o.name.clone(),
                from_key: o.from.clone(),
                from_name: st.device_name(&o.from),
                backup: o.backup,
                again: o.backup && st.libraries.iter().any(|l| l.id == o.id),
            })
            .collect();
        StatusInfo {
            name: self.name.clone(),
            key: net::hex(&self.keys.public),
            listen: format!("{}:{}", self.cfg.server.host, self.cfg.server.port),
            backup_dir: self.cfg.backup_dir_path().to_string_lossy().to_string(),
            devices,
            libraries,
            offers,
        }
    }

    // ------------------------------------------------------------ pairing

    fn create_invite(&self, host: &str) -> (String, Vec<u8>, mpsc::UnboundedReceiver<InviteEvent>) {
        let secret = random_bytes(32);
        let (tx, rx) = mpsc::unbounded_channel();
        self.invites.lock().unwrap().push(Invite {
            secret: secret.clone(),
            expires: Instant::now() + Duration::from_secs(INVITE_SECS),
            events: tx,
        });
        let addr = if host.contains(':') {
            host.to_string()
        } else {
            format!("{}:{}", host, self.cfg.server.port)
        };
        let token = format!("{}.{}@{}", net::hex(&self.keys.public), net::hex(&secret), addr);
        (token, secret, rx)
    }

    fn cancel_invite(&self, secret: &[u8]) {
        self.invites.lock().unwrap().retain(|i| i.secret != secret);
    }

    /// Every secret an incoming connection may legitimately use: one per paired device, plus
    /// the currently valid one-time invites.
    fn candidates(&self) -> Vec<(Cand, Vec<u8>)> {
        let mut v: Vec<(Cand, Vec<u8>)> = Vec::new();
        {
            let st = self.state.lock().unwrap();
            for d in &st.devices {
                if let Ok(p) = net::parse_hex32(&d.psk, "a device secret") {
                    v.push((Cand::Device(d.key.clone()), p));
                }
            }
        }
        let mut inv = self.invites.lock().unwrap();
        let now = Instant::now();
        inv.retain(|i| i.expires > now);
        for i in inv.iter() {
            v.push((Cand::Invite(i.secret.clone()), i.secret.clone()));
        }
        v
    }

    /// Use a pairing code shown by `invite` on another machine. Before anything is trusted, the
    /// person at this device has to confirm the verification code through `prompt`.
    pub async fn join(self: &Arc<Self>, token: &str, prompt: mpsc::UnboundedSender<PairPrompt>) -> Result<String> {
        let (key, secret, host, port) = parse_token(token)?;
        if key == self.keys.public {
            bail!("that code belongs to this machine");
        }
        let addr = format!("{}:{}", host, port);
        let mut stream = tokio::time::timeout(Duration::from_secs(10), TcpStream::connect(&addr))
            .await
            .context("timed out connecting")?
            .with_context(|| format!("cannot connect to {}", addr))?;
        stream.set_nodelay(true).ok();
        let (mut transport, remote) =
            tokio::time::timeout(Duration::from_secs(10), net::handshake(&mut stream, &self.keys, true, &secret))
                .await
                .context("handshake timed out")?
                .context("handshake failed: is the other machine still waiting, and is the code complete and unused?")?;
        if remote != key {
            bail!("the machine at {} has a different key than the code says (wrong machine?)", addr);
        }
        let hello = PairHello { name: self.name.clone() };
        net::send_message(&mut stream, &mut transport, &bincode::serialize(&hello)?).await?;
        let bytes = recv_bytes(&mut stream, &mut transport).await?;
        let info: PairInfo = bincode::deserialize(&bytes)?;
        let inviter_name = clean_name(&info.name);

        // Show the same code the other screen shows, and wait for the person to decide.
        let (decide, answer) = oneshot::channel();
        let ask = PairPrompt {
            name: inviter_name.clone(),
            fingerprint: fingerprint(&key),
            code: pairing_code(&secret, &self.keys.public, &key),
            decide,
        };
        if prompt.send(ask).is_err() {
            bail!("nobody is there to confirm the pairing");
        }
        let ok = matches!(
            tokio::time::timeout(Duration::from_secs(APPROVE_SECS), answer).await,
            Ok(Ok(true))
        );
        net::send_message(&mut stream, &mut transport, &bincode::serialize(&PairConfirm { ok })?).await?;
        if !ok {
            bail!("pairing cancelled");
        }
        let bytes = recv_bytes_for(&mut stream, &mut transport, APPROVE_SECS + 30).await?;
        let reply: PairAccept = bincode::deserialize(&bytes)?;
        if !reply.ok {
            bail!("the other device did not approve the pairing");
        }
        if reply.psk.len() != 32 {
            bail!("the other machine sent an invalid reply");
        }
        let name = inviter_name;
        let key_hex = net::hex(&key);
        {
            let mut st = self.state.lock().unwrap();
            st.devices.retain(|d| d.key != key_hex);
            st.devices.push(Device {
                key: key_hex.clone(),
                name: name.clone(),
                psk: net::hex(&reply.psk),
                host: Some(host),
                port: Some(port),
            });
        }
        self.save_state();
        self.ensure_dialer(&key_hex);
        // Devices we already talk to may know of backups on this one: ask them again.
        self.request_resync();
        Ok(name)
    }

    fn ensure_dialer(self: &Arc<Self>, key: &str) {
        let inserted = self.dialers.lock().unwrap().insert(key.to_string());
        if inserted {
            tokio::spawn(dial_loop(self.clone(), key.to_string()));
        }
    }

    // ------------------------------------------------------------ sessions

    fn register(
        &self,
        key: &str,
        tx: mpsc::Sender<Msg>,
        jobs: mpsc::UnboundedSender<DepotJob>,
        cancel: CancellationToken,
    ) -> u64 {
        let id = self.next_session.fetch_add(1, Ordering::Relaxed);
        let mut sessions = self.sessions.lock().unwrap();
        // Newest connection wins: a reconnect replaces a half-dead old one.
        if let Some(old) = sessions.insert(key.to_string(), SessionHandle { id, tx, jobs, cancel }) {
            old.cancel.cancel();
        }
        id
    }

    fn unregister(&self, key: &str, id: u64) {
        let mut sessions = self.sessions.lock().unwrap();
        if sessions.get(key).map(|h| h.id == id).unwrap_or(false) {
            sessions.remove(key);
        }
    }

    // ------------------------------------------------------------ control requests

    pub async fn handle(self: &Arc<Self>, req: Request) -> Result<Vec<Reply>> {
        let out = |s: String| Ok(vec![Reply::Out(s), Reply::Done]);
        match req {
            Request::Status => Ok(vec![Reply::Info(self.status()), Reply::Done]),
            Request::Forget { device } => out(self.forget_device(&device)?),
            Request::SetAddress { device, addr } => out(self.set_device_address(&device, &addr)?),
            Request::Create { name, folder } => out(self.create_library(&name, &folder).await?),
            Request::RemoveLibrary { library } => out(self.remove_library(&library)?),
            Request::Share { library, device, backup } => out(self.share_library(&library, &device, backup)?),
            Request::Accept { offer, folder, quota } => out(self.accept_offer(&offer, &folder, quota).await?),
            Request::SetQuota { library, bytes } => out(self.set_quota(&library, bytes)?),
            Request::Unshare { library, device } => out(self.unshare_library(&library, &device)?),
            Request::Refresh => out(self.refresh()?),
            Request::Prune { library } => out(self.prune_library(&library)?),
            Request::Decline { offer } => out(self.decline_offer(&offer)?),
            Request::Recovery { library } => out(self.recovery_codes(library.as_deref())?),
            Request::Restore { code, folder, device } => out(self.restore_library(&code, &folder, &device).await?),
            Request::Stop => {
                self.shutdown.cancel();
                out("Stopping.".to_string())
            }
            Request::Invite { .. } | Request::Join { .. } | Request::PairDecision { .. } => {
                bail!("internal error: pairing is handled separately")
            }
        }
    }
}

/// `host`, `host:port` or `[v6]:port` -> (host, port); `default_port` when there is none.
fn parse_host_port(input: &str, default_port: u16) -> Result<(String, u16)> {
    let s = input.trim();
    let bad_port = |p: &str| anyhow!("invalid port {:?}", p);
    if let Some(rest) = s.strip_prefix('[') {
        let (host, tail) = rest.split_once(']').ok_or_else(|| anyhow!("missing ] in the address"))?;
        let port = match tail.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| bad_port(p))?,
            None if tail.is_empty() => default_port,
            None => bail!("cannot read the address {:?}", s),
        };
        return Ok((host.to_string(), port));
    }
    let (host, port) = match s.matches(':').count() {
        0 => (s, default_port),
        1 => {
            let (h, p) = s.split_once(':').unwrap();
            (h, p.parse().map_err(|_| bad_port(p))?)
        }
        _ => (s, default_port), // a bare IPv6 address
    };
    if host.is_empty() || host.contains(char::is_whitespace) || host.contains('/') {
        bail!("cannot read the address {:?}; use something like 192.168.1.20 or host:3032", s);
    }
    Ok((host.to_string(), port))
}

/// `KEY.SECRET@HOST:PORT` -> (key, one-time secret, host, port)
fn parse_token(input: &str) -> Result<(Vec<u8>, Vec<u8>, String, u16)> {
    let s = input.trim();
    let s = s.strip_prefix("rustsync://pair/").unwrap_or(s);
    let (creds, addr) = s
        .split_once('@')
        .ok_or_else(|| anyhow!("a pairing code looks like KEY.SECRET@HOST:PORT"))?;
    let (key, secret) = creds
        .split_once('.')
        .ok_or_else(|| anyhow!("a pairing code looks like KEY.SECRET@HOST:PORT"))?;
    let (host, port) = addr
        .rsplit_once(':')
        .ok_or_else(|| anyhow!("the pairing code is missing :PORT after the host"))?;
    if host.is_empty() {
        bail!("the pairing code is missing the host");
    }
    let port: u16 = port.parse().map_err(|_| anyhow!("invalid port {:?} in the pairing code", port))?;
    let key = net::parse_hex32(key, "the key in the pairing code")?;
    let secret = net::parse_hex32(secret, "the secret in the pairing code")?;
    Ok((key, secret, host.to_string(), port))
}

// ---------------------------------------------------------------- daemon startup

pub async fn run_daemon(cfg: Config, keys: Keys) -> Result<()> {
    let sock = cfg.socket_path();
    if UnixStream::connect(&sock).await.is_ok() {
        bail!("rustsync is already running with this config");
    }
    let _ = std::fs::remove_file(&sock); // stale socket from a crash

    let state = State::load(&cfg.state_file())?;
    let node = Arc::new(Node {
        name: cfg.device_name(),
        cfg,
        keys,
        depots: Mutex::new(HashMap::new()),
        usage: Mutex::new(HashMap::new()),
        state: Mutex::new(state),
        libs: Mutex::new(HashMap::new()),
        sessions: Mutex::new(HashMap::new()),
        invites: Mutex::new(Vec::new()),
        dialers: Mutex::new(HashSet::new()),
        handshakes: Arc::new(Semaphore::new(MAX_HANDSHAKES)),
        ip_conns: Mutex::new(HashMap::new()),
        next_session: AtomicU64::new(1),
        shutdown: CancellationToken::new(),
    });

    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("cannot create the control socket {}", sock.display()))?;
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o600));
    }

    let libs: Vec<Library> = node.state.lock().unwrap().libraries.clone();
    for lib in libs {
        let r = match lib.mode {
            LibMode::Sync => node.start_library(&lib).await,
            LibMode::Backup => node.start_backup(&lib, false),
        };
        if let Err(e) = r {
            warn!("library \"{}\": {:#}", lib.name, e);
        }
    }
    let dial: Vec<String> = {
        let st = node.state.lock().unwrap();
        st.devices
            .iter()
            .filter(|d| d.host.is_some() && d.port.is_some())
            .map(|d| d.key.clone())
            .collect()
    };
    for key in dial {
        node.ensure_dialer(&key);
    }

    tokio::spawn(listen(node.clone()));
    tokio::spawn(control_loop(node.clone(), listener));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = node.shutdown.cancelled() => {}
    }
    info!("shutting down");
    node.shutdown.cancel();
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

// ---------------------------------------------------------------- control socket

async fn write_reply(w: &mut tokio::net::unix::OwnedWriteHalf, r: &Reply) -> Result<()> {
    let mut line = serde_json::to_string(r)?;
    line.push('\n');
    w.write_all(line.as_bytes()).await?;
    Ok(())
}

async fn control_loop(node: Arc<Node>, listener: UnixListener) {
    loop {
        tokio::select! {
            _ = node.shutdown.cancelled() => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    let node = node.clone();
                    tokio::spawn(async move {
                        if let Err(e) = serve_control(node, stream).await {
                            debug!("control connection: {:#}", e);
                        }
                    });
                }
                Err(e) => {
                    warn!("control accept failed: {}", e);
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }
}

async fn serve_control(node: Arc<Node>, stream: UnixStream) -> Result<()> {
    let (r, mut w) = stream.into_split();
    let mut reader = BufReader::new(r);
    let mut line = String::new();
    // A request is one short line; do not buffer an endless one.
    const MAX_REQUEST_LINE: u64 = 1024 * 1024;
    if (&mut reader).take(MAX_REQUEST_LINE).read_line(&mut line).await? == 0 {
        return Ok(());
    }
    if !line.ends_with('\n') && line.len() as u64 >= MAX_REQUEST_LINE {
        bail!("request too large");
    }
    let req: Request = serde_json::from_str(line.trim()).context("bad request")?;
    match req {
        Request::Invite { host } => serve_invite(&node, host, reader, w).await,
        Request::Join { token } => serve_join(&node, token, reader, w).await,
        other => {
            let replies = match node.handle(other).await {
                Ok(v) => v,
                Err(e) => vec![Reply::Error(format!("{:#}", e))],
            };
            for r in replies {
                write_reply(&mut w, &r).await?;
            }
            Ok(())
        }
    }
}

/// Reads the answers a person gives to a pairing prompt over the control connection. The
/// channel closes when the connection does (the command was cancelled).
fn read_decisions(mut reader: BufReader<tokio::net::unix::OwnedReadHalf>) -> mpsc::UnboundedReceiver<bool> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match (&mut reader).take(64 * 1024).read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(Request::PairDecision { accept }) = serde_json::from_str::<Request>(line.trim()) {
                        if tx.send(accept).is_err() {
                            break;
                        }
                    }
                }
            }
        }
    });
    rx
}

fn verify_reply(p: &PairPrompt, joining: bool) -> Reply {
    Reply::Verify {
        name: p.name.clone(),
        fingerprint: p.fingerprint.clone(),
        code: p.code.clone(),
        joining,
    }
}

/// `invite`: show a one-time code and keep the command waiting until someone joins with it,
/// then ask for approval of whoever it is.
async fn serve_invite(
    node: &Arc<Node>,
    host: String,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let (token, secret, mut events) = node.create_invite(&host);
    write_reply(&mut w, &Reply::Note("On the other machine run:  rustsync -c <its config> join".to_string())).await?;
    write_reply(
        &mut w,
        &Reply::Note(format!("and paste this code (it works once, for the next {} minutes):", INVITE_SECS / 60)),
    )
    .await?;
    write_reply(&mut w, &Reply::Out(token)).await?;
    write_reply(&mut w, &Reply::Note("Waiting for the other machine... (Ctrl-C to cancel)".to_string())).await?;

    let mut decisions = read_decisions(reader);
    let mut waiting: Option<oneshot::Sender<bool>> = None;
    let deadline = tokio::time::sleep(Duration::from_secs(INVITE_SECS));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            ev = events.recv() => match ev {
                Some(InviteEvent::Verify(p)) => {
                    // Somebody connected with the code: give the person time to compare.
                    deadline.as_mut().reset(tokio::time::Instant::now() + Duration::from_secs(APPROVE_SECS + 30));
                    write_reply(&mut w, &verify_reply(&p, false)).await?;
                    waiting = Some(p.decide);
                }
                Some(InviteEvent::Paired(name)) => {
                    write_reply(&mut w, &Reply::Note(format!("Paired with {}.", name))).await?;
                    write_reply(&mut w, &Reply::Done).await?;
                    break;
                }
                Some(InviteEvent::Failed(msg)) => {
                    write_reply(&mut w, &Reply::Error(msg)).await?;
                    break;
                }
                None => {
                    write_reply(&mut w, &Reply::Error("the invite expired".to_string())).await?;
                    break;
                }
            },
            d = decisions.recv() => match d {
                Some(accept) => {
                    if let Some(tx) = waiting.take() {
                        let _ = tx.send(accept);
                    }
                }
                None => {
                    // The command was cancelled (Ctrl-C): the code is no longer valid.
                    node.cancel_invite(&secret);
                    if let Some(tx) = waiting.take() {
                        let _ = tx.send(false);
                    }
                    break;
                }
            },
            _ = &mut deadline => {
                node.cancel_invite(&secret);
                if let Some(tx) = waiting.take() {
                    let _ = tx.send(false);
                }
                write_reply(&mut w, &Reply::Error("nobody joined in time; run invite again".to_string())).await?;
                break;
            }
        }
    }
    Ok(())
}

/// `join`: contact the inviting machine, show the verification code and wait for the person.
async fn serve_join(
    node: &Arc<Node>,
    token: String,
    reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut w: tokio::net::unix::OwnedWriteHalf,
) -> Result<()> {
    let mut decisions = read_decisions(reader);
    let (ptx, mut prompts) = mpsc::unbounded_channel::<PairPrompt>();
    let n = node.clone();
    let mut task = tokio::spawn(async move { n.join(&token, ptx).await });
    let mut waiting: Option<oneshot::Sender<bool>> = None;
    let mut prompts_open = true;
    loop {
        tokio::select! {
            p = prompts.recv(), if prompts_open => match p {
                Some(p) => {
                    write_reply(&mut w, &verify_reply(&p, true)).await?;
                    waiting = Some(p.decide);
                }
                None => prompts_open = false,
            },
            d = decisions.recv() => match d {
                Some(accept) => {
                    if let Some(tx) = waiting.take() {
                        let _ = tx.send(accept);
                    }
                }
                None => {
                    // The command was cancelled.
                    task.abort();
                    break;
                }
            },
            res = &mut task => {
                let reply = match res {
                    Ok(Ok(name)) => Reply::Out(format!("Paired with {}.", name)),
                    Ok(Err(e)) => Reply::Error(format!("{:#}", e)),
                    Err(_) => Reply::Error("pairing was interrupted".to_string()),
                };
                let ok = matches!(reply, Reply::Out(_));
                write_reply(&mut w, &reply).await?;
                if ok {
                    write_reply(&mut w, &Reply::Done).await?;
                }
                break;
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- local changes

async fn scan_and_broadcast(node: &Arc<Node>, rt: &Arc<LibRuntime>, roots: Vec<String>, deep: bool) {
    let folder = &rt.folder;
    let changed = {
        let _guard = folder.fs_lock.lock().await;
        let f = folder.clone();
        match tokio::task::spawn_blocking(move || index::scan(&f, &roots, deep)).await {
            Ok(c) => c,
            Err(e) => {
                warn!("scan failed: {}", e);
                return;
            }
        }
    };
    if !changed.is_empty() {
        debug!("library {}: {} local change(s)", short(&folder.id), changed.len());
        node.broadcast(&folder.id, &changed, None);
    }
}

fn start_watcher(folder: &Arc<Folder>) -> Result<(RecommendedWatcher, mpsc::Receiver<PathBuf>)> {
    // Bounded: if events pile up faster than we handle them, dropping some is fine because
    // the periodic full scan finds every change anyway.
    let (tx, rx) = mpsc::channel(65_536);
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res {
            if matches!(ev.kind, notify::EventKind::Access(_)) {
                return;
            }
            for p in ev.paths {
                let _ = tx.try_send(p);
            }
        }
    })?;
    w.watch(&folder.root, RecursiveMode::Recursive)?;
    Ok((w, rx))
}

/// Realtime path: collect filesystem events for a short quiet period, then rescan just those paths.
async fn watch_loop(
    node: Arc<Node>,
    rt: Arc<LibRuntime>,
    mut rx: mpsc::Receiver<PathBuf>,
    _watcher: RecommendedWatcher, // kept alive for as long as this task runs
) {
    loop {
        let first = tokio::select! {
            _ = rt.cancel.cancelled() => return,
            p = rx.recv() => match p {
                Some(p) => p,
                None => return,
            },
        };
        let mut paths: HashSet<PathBuf> = HashSet::new();
        paths.insert(first);
        loop {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(p)) => {
                    paths.insert(p);
                }
                _ => break,
            }
        }
        let roots: Vec<String> = if paths.len() > 1000 {
            vec![String::new()]
        } else {
            paths
                .iter()
                .filter_map(|p| p.strip_prefix(&rt.folder.root).ok())
                .map(|r| r.to_string_lossy().replace('\\', "/"))
                .collect()
        };
        if roots.is_empty() {
            continue;
        }
        scan_and_broadcast(&node, &rt, roots, false).await;
    }
}

/// Safety net: full rescan (catches dropped watcher events) and full index re-announcement
/// (catches anything a peer missed). Every Nth scan re-hashes every file.
async fn periodic_loop(node: Arc<Node>, rt: Arc<LibRuntime>, every: u64, deep_every: u64) {
    let mut tick = tokio::time::interval(Duration::from_secs(every.max(1)));
    tick.tick().await;
    let mut n = 0u64;
    loop {
        tokio::select! {
            _ = rt.cancel.cancelled() => return,
            _ = tick.tick() => {}
        }
        n += 1;
        let deep = deep_every > 0 && n % deep_every == 0;
        scan_and_broadcast(&node, &rt, vec![String::new()], deep).await;
        node.broadcast(&rt.folder.id, &rt.folder.snapshot(), None);
    }
}

// ---------------------------------------------------------------- networking

async fn listen(node: Arc<Node>) {
    let addr = format!("{}:{}", node.cfg.server.host, node.cfg.server.port);
    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => l,
        Err(e) => {
            warn!("cannot listen on {}: {}", addr, e);
            return;
        }
    };
    info!("listening on {}", addr);
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                // Limit connections that have not authenticated yet (overall and per address).
                let ip = peer.ip();
                let Ok(permit) = node.handshakes.clone().try_acquire_owned() else {
                    debug!("too many connections; dropping {}", peer);
                    continue;
                };
                {
                    let mut m = node.ip_conns.lock().unwrap();
                    let c = m.entry(ip).or_insert(0);
                    if *c >= MAX_HANDSHAKES_PER_IP {
                        debug!("too many connections from {}; dropping one", ip);
                        continue;
                    }
                    *c += 1;
                }
                let pre = PreAuth { node: node.clone(), ip, _permit: permit };
                let node = node.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_inbound(node, stream, pre).await {
                        debug!("inbound {}: {:#}", peer, e);
                    }
                });
            }
            Err(e) => {
                warn!("accept failed: {}", e);
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

async fn handle_inbound(node: Arc<Node>, mut stream: TcpStream, pre: PreAuth) -> Result<()> {
    stream.set_nodelay(true).ok();
    let cands = node.candidates();
    let psks: Vec<Vec<u8>> = cands.iter().map(|(_, p)| p.clone()).collect();
    let (transport, remote, idx) =
        tokio::time::timeout(Duration::from_secs(10), net::handshake_responder(&mut stream, &node.keys, &psks))
            .await
            .context("handshake timed out")??;
    drop(pre); // authenticated: no longer counts as an anonymous connection
    let remote_hex = net::hex(&remote);
    match &cands[idx].0 {
        Cand::Device(key) => {
            if *key != remote_hex {
                bail!("a device presented another device's secret");
            }
            let dev = node.state.lock().unwrap().device(key).cloned();
            let Some(dev) = dev else { bail!("that device was removed") };
            establish(node, stream, transport, dev).await
        }
        Cand::Invite(secret) => serve_pairing(node, stream, transport, remote_hex, secret.clone()).await,
    }
}

/// The inviting side of pairing: someone proved they hold a valid one-time code. They are not
/// trusted yet: the person at this device has to compare the verification code with the other
/// screen and approve, and so does the person at the other device.
async fn serve_pairing(
    node: Arc<Node>,
    mut stream: TcpStream,
    mut transport: snow::TransportState,
    remote_hex: String,
    secret: Vec<u8>,
) -> Result<()> {
    // One use only.
    let taken: Option<Invite> = {
        let mut inv = node.invites.lock().unwrap();
        let pos = inv.iter().position(|i| i.secret == secret);
        let found = pos.map(|p| inv.remove(p));
        found
    };
    let Some(invite) = taken else { bail!("this invite was already used") };
    let res = pair_inviter(&node, &mut stream, &mut transport, &remote_hex, &secret, &invite.events).await;
    match &res {
        Ok(name) => {
            let _ = invite.events.send(InviteEvent::Paired(name.clone()));
        }
        Err(e) => {
            let _ = invite.events.send(InviteEvent::Failed(format!("{:#}", e)));
        }
    }
    res.map(|_| ())
}

async fn pair_inviter(
    node: &Arc<Node>,
    stream: &mut TcpStream,
    transport: &mut snow::TransportState,
    remote_hex: &str,
    secret: &[u8],
    events: &mpsc::UnboundedSender<InviteEvent>,
) -> Result<String> {
    let remote = net::parse_hex32(remote_hex, "a device key")?;
    let bytes = recv_bytes(stream, transport).await?;
    let hello: PairHello = bincode::deserialize(&bytes)?;
    let name = clean_name(&hello.name);
    net::send_message(stream, transport, &bincode::serialize(&PairInfo { name: node.name.clone() })?).await?;

    // Ask the person at this device, while the other device asks its person.
    let (decide, answer) = oneshot::channel();
    let prompt = PairPrompt {
        name: name.clone(),
        fingerprint: fingerprint(&remote),
        code: pairing_code(secret, &remote, &node.keys.public),
        decide,
    };
    if events.send(InviteEvent::Verify(prompt)).is_err() {
        bail!("the person who asked for this code is gone");
    }
    let (local_ok, remote_ok) = {
        let mut confirm = Box::pin(recv_bytes_for(stream, transport, APPROVE_SECS));
        let mut answer = answer;
        let mut local: Option<bool> = None;
        let mut theirs: Option<bool> = None;
        let waited = tokio::time::timeout(Duration::from_secs(APPROVE_SECS), async {
            while local.is_none() || theirs.is_none() {
                tokio::select! {
                    a = &mut answer, if local.is_none() => {
                        local = Some(a.unwrap_or(false));
                    }
                    c = &mut confirm, if theirs.is_none() => {
                        theirs = Some(match c {
                            Ok(b) => bincode::deserialize::<PairConfirm>(&b).map(|c| c.ok).unwrap_or(false),
                            Err(_) => false,
                        });
                    }
                }
                if local == Some(false) || theirs == Some(false) {
                    break;
                }
            }
        })
        .await;
        if waited.is_err() {
            bail!("pairing timed out: it was not approved on both devices in time");
        }
        (local.unwrap_or(false), theirs.unwrap_or(false))
    };
    if !(local_ok && remote_ok) {
        let no = PairAccept { ok: false, psk: Vec::new(), name: node.name.clone() };
        let _ = net::send_message(stream, transport, &bincode::serialize(&no)?).await;
        if !local_ok {
            bail!("pairing cancelled");
        }
        bail!("the other device did not approve the pairing");
    }

    let psk = random_bytes(32);
    {
        let mut st = node.state.lock().unwrap();
        st.devices.retain(|d| d.key != remote_hex);
        st.devices.push(Device {
            key: remote_hex.to_string(),
            name: name.clone(),
            psk: net::hex(&psk),
            host: None,
            port: None,
        });
    }
    node.save_state();
    let reply = PairAccept { ok: true, psk, name: node.name.clone() };
    net::send_message(stream, transport, &bincode::serialize(&reply)?).await?;
    info!("paired with {} ({})", name, short(remote_hex));
    node.request_resync();
    Ok(name)
}

/// Keeps one outgoing connection to a device alive. It only dials while we are not already
/// connected to that device, and redials after a drop.
async fn dial_loop(node: Arc<Node>, key: String) {
    let mut backoff = 1u64;
    loop {
        if node.shutdown.is_cancelled() {
            break;
        }
        let dev = node.state.lock().unwrap().device(&key).cloned();
        let Some(dev) = dev else { break }; // forgotten
        let (Some(host), Some(port)) = (dev.host.clone(), dev.port) else { break };
        if node.is_connected(&key) {
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        let addr = format!("{}:{}", host, port);
        match TcpStream::connect(&addr).await {
            Ok(stream) => {
                backoff = 1;
                if let Err(e) = run_outbound(node.clone(), stream, dev).await {
                    warn!("device at {}: {:#}", addr, e);
                }
            }
            Err(e) => debug!("cannot reach {}: {}", addr, e),
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(15);
    }
    node.dialers.lock().unwrap().remove(&key);
}

async fn run_outbound(node: Arc<Node>, mut stream: TcpStream, dev: Device) -> Result<()> {
    stream.set_nodelay(true).ok();
    let key = net::parse_hex32(&dev.key, "a device key")?;
    let psk = net::parse_hex32(&dev.psk, "a device secret")?;
    let (transport, remote) =
        tokio::time::timeout(Duration::from_secs(10), net::handshake(&mut stream, &node.keys, true, &psk))
            .await
            .context("handshake timed out")?
            .context("handshake failed (was this machine removed on the other side?)")?;
    if remote != key {
        bail!(
            "wrong key: expected {} but the machine answering has {} (wrong machine or impersonation)",
            dev.key,
            net::hex(&remote)
        );
    }
    establish(node, stream, transport, dev).await
}

async fn recv_bytes(stream: &mut TcpStream, t: &mut snow::TransportState) -> Result<Vec<u8>> {
    recv_bytes_for(stream, t, 15).await
}

/// Like `recv_bytes`, for when the other side may be waiting for a person.
async fn recv_bytes_for(stream: &mut TcpStream, t: &mut snow::TransportState, secs: u64) -> Result<Vec<u8>> {
    let mut asm = Reassembler::default();
    let bytes = tokio::time::timeout(Duration::from_secs(secs), async {
        loop {
            let frame = net::read_frame(stream).await?;
            if let Some(b) = asm.push(t, &frame)? {
                return Ok::<Vec<u8>, anyhow::Error>(b);
            }
        }
    })
    .await
    .context("timed out waiting for the other machine")??;
    Ok(bytes)
}

async fn send(wr: &mut OwnedWriteHalf, t: &mut snow::TransportState, msg: &Msg) -> Result<()> {
    let bytes = bincode::serialize(msg)?;
    tokio::time::timeout(WRITE_TIMEOUT, net::send_message(wr, t, &bytes))
        .await
        .map_err(|_| anyhow!("the other side stopped reading"))?
}

/// Both sides have proven who they are. Exchange roles before anything else is sent, so a
/// mistake can never make a client hand its plain file index to a depot (or the reverse).
async fn establish(
    node: Arc<Node>,
    mut stream: TcpStream,
    mut transport: snow::TransportState,
    dev: Device,
) -> Result<()> {
    let hello = Msg::Hello { name: node.name.clone() };
    net::send_message(&mut stream, &mut transport, &bincode::serialize(&hello)?).await?;
    let first: Msg = bincode::deserialize(&recv_bytes(&mut stream, &mut transport).await?)?;
    if !matches!(first, Msg::Hello { .. }) {
        bail!("unexpected first message");
    }

    let (rd, wr) = stream.into_split();
    let (tx, out_rx) = mpsc::channel::<Msg>(OUT_QUEUE);
    let (jobs_tx, jobs_rx) = mpsc::unbounded_channel::<DepotJob>();
    let cancel = CancellationToken::new();
    let id = node.register(&dev.key, tx.clone(), jobs_tx.clone(), cancel.clone());
    info!("connected to {}", dev.name);

    let result = session_loop(&node, &dev, transport, rd, wr, tx, out_rx, jobs_tx, jobs_rx, cancel.clone()).await;
    node.unregister(&dev.key, id);
    cancel.cancel();
    info!("disconnected from {}", dev.name);
    result
}

#[allow(clippy::too_many_arguments)]
async fn session_loop(
    node: &Arc<Node>,
    dev: &Device,
    mut transport: snow::TransportState,
    rd: OwnedReadHalf,
    mut wr: OwnedWriteHalf,
    tx: mpsc::Sender<Msg>,
    mut out_rx: mpsc::Receiver<Msg>,
    jobs_tx: mpsc::UnboundedSender<DepotJob>,
    jobs_rx: mpsc::UnboundedReceiver<DepotJob>,
    cancel: CancellationToken,
) -> Result<()> {
    // Reader task: raw frames only, so no read is ever cancelled half way through. The queue
    // is small: when we fall behind, we stop reading and TCP slows the sender down.
    let (frame_tx, mut frame_rx) = mpsc::channel::<std::io::Result<Vec<u8>>>(FRAME_QUEUE);
    let reader = tokio::spawn(async move {
        let mut rd = rd;
        loop {
            let f = match tokio::time::timeout(Duration::from_secs(45), net::read_frame(&mut rd)).await {
                Ok(r) => r,
                Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "peer silent for 45s")),
            };
            let stop = f.is_err();
            if frame_tx.send(f).await.is_err() || stop {
                break;
            }
        }
    });

    let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
    let handle = Remote {
        tx: tx.clone(),
        pending: pending.clone(),
        next: Arc::new(AtomicU64::new(1)),
        vault: None,
    };

    // Libraries we sync with this device: its index batches are applied one after another.
    // `queued` counts the bytes waiting; beyond the budget new batches are skipped (they are
    // re-sent at the next full scan), so a flood cannot grow our memory without limit.
    let queued = Arc::new(AtomicUsize::new(0));
    let (apply_tx, mut apply_rx) = mpsc::unbounded_channel::<(String, Vec<Entry>, usize)>();
    {
        let node = node.clone();
        let origin = dev.key.clone();
        let handle = handle.clone();
        let queued = queued.clone();
        tokio::spawn(async move {
            while let Some((lib, entries, cost)) = apply_rx.recv().await {
                if let Some(rt) = node.lib_rt(&lib) {
                    match apply::apply_entries(&rt.folder, &handle, entries).await {
                        Ok(applied) => node.broadcast(&lib, &applied, Some(origin.as_str())),
                        Err(e) => warn!("apply failed: {:#}", e),
                    }
                }
                queued.fetch_sub(cost, Ordering::Relaxed);
            }
        });
    }

    // Libraries this device keeps an encrypted backup of: one worker uploads (encrypted) and
    // pulls (and decrypts). It does nothing when there are none.
    tokio::spawn(depot_worker(node.clone(), handle.clone(), dev.key.clone(), jobs_rx, cancel.clone()));

    // Tell the other side which libraries we share/offer; send our index of the ones we sync.
    let _ = tx.try_send(node.libraries_msg_for(&dev.key));
    for (id, _) in node.libs_with(&dev.key, Role::Peer) {
        node.push_index(&dev.key, &id);
    }
    let _ = jobs_tx.send(DepotJob::SyncAll);

    // At most MAX_SERVE requests from this device are answered at the same time; any more are
    // refused straight away (the asker retries later).
    let serve_slots = Arc::new(Semaphore::new(MAX_SERVE));
    let mut asm = Reassembler::default();
    let mut ping = tokio::time::interval(Duration::from_secs(15));
    let res: Result<()> = loop {
        tokio::select! {
            _ = cancel.cancelled() => { break Ok(()); }
            _ = ping.tick() => {
                if let Err(e) = send(&mut wr, &mut transport, &Msg::Ping).await { break Err(e); }
            }
            Some(m) = out_rx.recv() => {
                if let Err(e) = send(&mut wr, &mut transport, &m).await { break Err(e); }
            }
            f = frame_rx.recv() => {
                let frame = match f {
                    None => break Ok(()),
                    Some(Err(e)) => break Err(anyhow::Error::from(e)),
                    Some(Ok(fr)) => fr,
                };
                let bytes = match asm.push(&mut transport, &frame) {
                    Err(e) => break Err(e),
                    Ok(None) => continue,
                    Ok(Some(b)) => b,
                };
                let msg: Msg = match bincode::deserialize(&bytes) {
                    Ok(m) => m,
                    Err(e) => break Err(anyhow::Error::from(e)),
                };
                match msg {
                    Msg::Ping | Msg::Hello { .. } => {}
                    Msg::Resync => {
                        let _ = tx.try_send(node.libraries_msg_for(&dev.key));
                    }
                    Msg::Chunk { req, data } => {
                        let waiter = pending.lock().unwrap().remove(&req);
                        if let Some(w) = waiter {
                            let _ = w.send(data);
                        }
                    }
                    Msg::Libraries { shared, offers, backups } => {
                        if shared.len() > MAX_MEMBERSHIP || offers.len() > MAX_MEMBERSHIP || backups.len() > MAX_MEMBERSHIP {
                            break Err(anyhow!("oversized library list"));
                        }
                        let (newly, changed) = node.process_libraries(&dev.key, shared, offers, backups);
                        if changed {
                            // Tell everyone connected: besides this device, a backup server we
                            // just learned about needs to hear our offer.
                            node.notify_libraries(None);
                        }
                        for (id, role) in newly {
                            match role {
                                Role::Peer => node.push_index(&dev.key, &id),
                                Role::Store | Role::Client => {
                                    let _ = jobs_tx.send(DepotJob::SyncAll);
                                }
                            }
                        }
                    }
                    // A backup server says it has new data for a library we use it for.
                    Msg::DepotChanged { lib } => {
                        if node.is_store(&lib, &dev.key) {
                            let _ = jobs_tx.send(DepotJob::SyncAll);
                        }
                    }
                    // Plain file traffic is only accepted for libraries this device syncs with us,
                    // never from a device that merely keeps an encrypted backup.
                    Msg::Index { lib, entries } => {
                        if entries.len() > MAX_RECV_ENTRIES || bytes.len() > MAX_INDEX_MSG {
                            break Err(anyhow!("oversized index message"));
                        }
                        if node.is_peer(&lib, &dev.key) {
                            let cost = bytes.len().max(256);
                            if queued.load(Ordering::Relaxed) + cost > MAX_QUEUED_APPLY {
                                warn!("{} is sending faster than we can apply; skipped an update (it is re-sent at the next full scan)", dev.name);
                            } else {
                                queued.fetch_add(cost, Ordering::Relaxed);
                                if apply_tx.send((lib, entries, cost)).is_err() {
                                    queued.fetch_sub(cost, Ordering::Relaxed);
                                }
                            }
                        }
                    }
                    Msg::GetChunk { req, lib, path, index, hash } => {
                        match serve_slots.clone().try_acquire_owned() {
                            Ok(permit) => {
                                let node = node.clone();
                                let tx = tx.clone();
                                let from = dev.key.clone();
                                tokio::spawn(async move {
                                    let _permit = permit;
                                    let data = serve_chunk(&node, &from, &lib, &path, index, &hash).await;
                                    let _ = tx.send(Msg::Chunk { req, data }).await;
                                });
                            }
                            Err(_) => {
                                let _ = tx.try_send(Msg::Chunk { req, data: None });
                            }
                        }
                    }
                    // Encrypted-backup requests: only answered for libraries this device was
                    // allowed to back up here.
                    other => {
                        let info = other.depot_req();
                        if let Some((req, lib)) = info {
                            match serve_slots.clone().try_acquire_owned() {
                                Ok(permit) => {
                                    let depot = node.depot_for(&dev.key, &lib);
                                    let wrote_record =
                                        matches!(&other, Msg::DepotPut { kind, .. } if *kind == KIND_RECORD);
                                    let tx = tx.clone();
                                    let node = node.clone();
                                    let from = dev.key.clone();
                                    tokio::spawn(async move {
                                        let _permit = permit;
                                        let data = match depot {
                                            Some(d) => d.serve(other).await,
                                            None => None,
                                        };
                                        if wrote_record && data.is_some() {
                                            // Other devices that back up here can fetch it right away.
                                            node.notify_depot_changed(&lib, &from);
                                        }
                                        let _ = tx.send(Msg::Chunk { req, data }).await;
                                    });
                                }
                                Err(_) => {
                                    let _ = tx.try_send(Msg::Chunk { req, data: None });
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    reader.abort();
    // Fail any fetches still waiting on this session (apply tasks clean up their temp files).
    pending.lock().unwrap().clear();
    drop(apply_tx);
    res
}

async fn read_local_chunk(f: &Folder, path: &str, index: u32) -> Option<Vec<u8>> {
    if !index::safe_rel(path) {
        return None;
    }
    // Never follow a symlink, and make sure the file really is inside the library folder.
    let std_file = index::open_nofollow(&f.abs(path), &f.root).ok()?;
    let mut file = tokio::fs::File::from_std(std_file);
    file.seek(SeekFrom::Start(index as u64 * CHUNK as u64)).await.ok()?;
    let mut buf = vec![0u8; CHUNK];
    let mut n = 0;
    while n < CHUNK {
        let k = file.read(&mut buf[n..]).await.ok()?;
        if k == 0 {
            break;
        }
        n += k;
    }
    buf.truncate(n);
    Some(buf)
}

async fn serve_chunk(node: &Node, from: &str, lib: &str, path: &str, index: u32, hash: &str) -> Option<Vec<u8>> {
    if !node.is_peer(lib, from) {
        return None;
    }
    let rt = node.lib_rt(lib)?;
    {
        // Only serve files that are in our index.
        let idx = rt.folder.index.lock().unwrap();
        let e = idx.get(path)?;
        if !e.is_live_file() {
            return None;
        }
    }
    let buf = read_local_chunk(&rt.folder, path, index).await?;
    if blake3::hash(&buf).to_hex().as_str() == hash {
        Some(buf)
    } else {
        None
    }
}

// ---------------------------------------------------------------- depot (client side)

/// Talks to a depot: pushes our changes (encrypted) and pulls what other machines left there.
async fn depot_worker(
    node: Arc<Node>,
    remote: Remote,
    depot_key: String,
    mut jobs: mpsc::UnboundedReceiver<DepotJob>,
    cancel: CancellationToken,
) {
    // (library, slot) -> stamp the depot already holds
    let mut known: HashMap<(String, String), u64> = HashMap::new();
    let mut last_prune: HashMap<String, Instant> = HashMap::new();
    let period = Duration::from_secs(node.cfg.sync_options.full_scan_interval.max(1));
    let mut poll = tokio::time::interval_at(tokio::time::Instant::now() + period, period);

    loop {
        let job = tokio::select! {
            _ = cancel.cancelled() => break,
            _ = poll.tick() => DepotJob::SyncAll,
            j = jobs.recv() => match j {
                Some(j) => j,
                None => break,
            },
        };
        match job {
            DepotJob::SyncAll => {
                for (lib_id, rt) in node.libs_with(&depot_key, Role::Store) {
                    if let Err(e) = depot_pull(&node, &remote, &depot_key, &lib_id, &rt, &mut known).await {
                        warn!("depot pull: {:#}", e);
                        continue;
                    }
                    let snapshot = rt.folder.snapshot();
                    if let Err(e) = depot_push(&remote, &lib_id, &rt, snapshot, &mut known).await {
                        warn!("depot push: {:#}", e);
                    }
                    // How full is the backup? If nearly full, clean up (at most hourly).
                    if let Some((used, quota)) = depot_stat(&node, &remote, &depot_key, &lib_id).await {
                        let nearly_full = quota > 0 && (used as u128) * 10 >= (quota as u128) * 9;
                        let due = last_prune.get(&lib_id).map_or(true, |t| t.elapsed() > AUTO_PRUNE_EVERY);
                        if nearly_full && due {
                            last_prune.insert(lib_id.clone(), Instant::now());
                            info!("the backup of \"{}\" is nearly full; removing chunks nothing uses any more", &lib_id[..8]);
                            run_prune(&node, &remote, &depot_key, &lib_id, &rt).await;
                        }
                    }
                }
            }
            DepotJob::Push(lib_id, entries) => {
                if !node.is_store(&lib_id, &depot_key) {
                    continue;
                }
                if let Some(rt) = node.lib_rt(&lib_id) {
                    if let Err(e) = depot_push(&remote, &lib_id, &rt, entries, &mut known).await {
                        warn!("depot push: {:#}", e);
                    }
                    depot_stat(&node, &remote, &depot_key, &lib_id).await;
                }
            }
            DepotJob::Prune(lib_id) => {
                if !node.is_store(&lib_id, &depot_key) {
                    continue;
                }
                if let Some(rt) = node.lib_rt(&lib_id) {
                    last_prune.insert(lib_id.clone(), Instant::now());
                    run_prune(&node, &remote, &depot_key, &lib_id, &rt).await;
                }
            }
        }
    }
}

/// Ask the backup how much it holds and remember the answer for `status`.
async fn depot_stat(node: &Arc<Node>, remote: &Remote, depot_key: &str, lib: &str) -> Option<(u64, u64)> {
    let lib_s = lib.to_string();
    let raw = remote.call(move |req| Msg::DepotStat { req, lib: lib_s }).await.ok()?;
    let (used, quota): (u64, u64) = bincode::deserialize(&raw).ok()?;
    node.set_usage(lib, depot_key, used, quota);
    Some((used, quota))
}

async fn run_prune(node: &Arc<Node>, remote: &Remote, depot_key: &str, lib: &str, rt: &Arc<LibRuntime>) {
    match depot_prune(remote, lib, rt).await {
        Ok((n, bytes)) => info!("backup of \"{}\": removed {} unused chunk(s), freed {}", &lib[..8], n, fmt_size(bytes)),
        Err(e) => warn!("cleaning up the backup failed (nothing was deleted that is in use): {:#}", e),
    }
    depot_stat(node, remote, depot_key, lib).await;
}

/// Delete chunks in the backup that no record refers to. Every record is read and decrypted
/// first; if any cannot be, nothing is deleted. Only chunks older than a week are considered,
/// so chunks uploaded just before their record by another device are never touched.
async fn depot_prune(remote: &Remote, lib: &str, rt: &Arc<LibRuntime>) -> Result<(u64, u64)> {
    let lib_s = lib.to_string();
    let raw = remote.call(move |req| Msg::DepotList { req, lib: lib_s }).await?;
    let slots: Vec<(String, u64)> = bincode::deserialize(&raw)?;
    let mut live: HashSet<String> = HashSet::new();
    for (slot, _) in slots {
        let (lib_s, id) = (lib.to_string(), slot.clone());
        let rec = remote
            .call(move |req| Msg::DepotGet { req, kind: KIND_RECORD, lib: lib_s, id })
            .await?;
        let entry = rt.vault.open_record(&slot, &rec)?;
        if entry.is_live_file() {
            for h in &entry.chunks {
                live.insert(rt.vault.blob_id(h));
            }
        }
    }
    let (mut removed, mut freed) = (0u64, 0u64);
    let mut after = String::new();
    loop {
        let (lib_s, after_s) = (lib.to_string(), after.clone());
        let raw = remote
            .call(move |req| Msg::DepotBlobs { req, lib: lib_s, after: after_s, min_age: PRUNE_MIN_AGE_SECS })
            .await?;
        let page: Vec<(String, u64)> = bincode::deserialize(&raw)?;
        let Some(last) = page.last() else { break };
        after = last.0.clone();
        let dead: Vec<(String, u64)> = page.iter().filter(|(id, _)| !live.contains(id)).cloned().collect();
        for group in dead.chunks(500) {
            let ids: Vec<String> = group.iter().map(|(i, _)| i.clone()).collect();
            let lib_s = lib.to_string();
            let raw = remote.call(move |req| Msg::DepotDelete { req, lib: lib_s, ids }).await?;
            let n: u64 = bincode::deserialize(&raw)?;
            removed += n;
            freed += group.iter().map(|(_, sz)| *sz).sum::<u64>();
        }
    }
    Ok((removed, freed))
}

/// Download records that are new or newer than ours, decrypt them and apply them.
async fn depot_pull(
    node: &Arc<Node>,
    remote: &Remote,
    depot_key: &str,
    lib: &str,
    rt: &Arc<LibRuntime>,
    known: &mut HashMap<(String, String), u64>,
) -> Result<()> {
    let lib_s = lib.to_string();
    let raw = remote.call(move |req| Msg::DepotList { req, lib: lib_s }).await?;
    let list: Vec<(String, u64)> = bincode::deserialize(&raw)?;
    let local: HashMap<String, u64> = rt
        .folder
        .snapshot()
        .iter()
        .map(|e| (rt.vault.slot(&e.path), e.stamp))
        .collect();

    let mut want: Vec<String> = Vec::new();
    for (slot, dstamp) in list {
        known.insert((lib.to_string(), slot.clone()), dstamp);
        if local.get(&slot).map_or(true, |&ls| dstamp > ls) {
            want.push(slot);
        }
    }

    let mut entries: Vec<Entry> = Vec::new();
    for slot in want {
        let (lib_s, id) = (lib.to_string(), slot.clone());
        let raw = match remote
            .call(move |req| Msg::DepotGet { req, kind: KIND_RECORD, lib: lib_s, id })
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!("cannot fetch a record from the depot: {:#}", e);
                continue;
            }
        };
        match rt.vault.open_record(&slot, &raw) {
            Ok(entry) => entries.push(entry),
            Err(e) => warn!("{:#}", e),
        }
    }
    if entries.is_empty() {
        return Ok(());
    }
    debug!("depot: {} new/changed record(s)", entries.len());
    let with_vault = remote.with_vault(rt.vault.clone());
    let applied = apply::apply_entries(&rt.folder, &with_vault, entries).await?;
    node.broadcast(lib, &applied, Some(depot_key));
    Ok(())
}

/// Encrypt and upload entries (and the chunks the depot is missing) that the depot lacks.
async fn depot_push(
    remote: &Remote,
    lib: &str,
    rt: &Arc<LibRuntime>,
    entries: Vec<Entry>,
    known: &mut HashMap<(String, String), u64>,
) -> Result<()> {
    for e in entries {
        let slot = rt.vault.slot(&e.path);
        if known
            .get(&(lib.to_string(), slot.clone()))
            .map_or(false, |&s| s >= e.stamp)
        {
            continue; // the depot already has this version or a newer one
        }

        if e.is_live_file() && !e.chunks.is_empty() {
            let ids: Vec<String> = e.chunks.iter().map(|h| rt.vault.blob_id(h)).collect();
            let (lib_s, ids_c) = (lib.to_string(), ids.clone());
            let raw = remote
                .call(move |req| Msg::DepotHave { req, lib: lib_s, ids: ids_c })
                .await?;
            let missing: HashSet<String> = bincode::deserialize::<Vec<String>>(&raw)?.into_iter().collect();

            let mut complete = true;
            for (i, id) in ids.iter().enumerate() {
                if !missing.contains(id) {
                    continue;
                }
                let plain = match read_local_chunk(&rt.folder, &e.path, i as u32).await {
                    Some(p) if blake3::hash(&p).to_hex().as_str() == e.chunks[i] => p,
                    _ => {
                        // The file changed since it was indexed; the next scan will re-send it.
                        complete = false;
                        break;
                    }
                };
                let sealed = rt.vault.seal_blob(id, &plain);
                let (lib_s, id2) = (lib.to_string(), id.clone());
                remote
                    .call(move |req| Msg::DepotPut { req, kind: KIND_BLOB, lib: lib_s, id: id2, stamp: 0, data: sealed })
                    .await?;
            }
            if !complete {
                debug!("not uploading {}: it changed while uploading", e.path);
                continue;
            }
        }

        let record = rt.vault.seal_record(&slot, &e)?;
        let (lib_s, id, stamp) = (lib.to_string(), slot.clone(), e.stamp);
        remote
            .call(move |req| Msg::DepotPut { req, kind: KIND_RECORD, lib: lib_s, id, stamp, data: record })
            .await?;
        known.insert((lib.to_string(), slot), stamp);
        debug!("uploaded {}", e.path);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A message that merely *claims* to hold an enormous list or string must be refused
    /// quickly: decoding must not try to reserve memory for what the sender says is coming.
    #[test]
    fn huge_declared_lengths_are_refused() {
        // [u32 variant][u64 length of `shared`] ...
        let good = bincode::serialize(&Msg::Libraries { shared: vec![], offers: vec![], backups: vec![] }).unwrap();
        for len in [u64::MAX, 1u64 << 40, 1u64 << 32] {
            let mut bad = good.clone();
            bad[4..12].copy_from_slice(&len.to_le_bytes());
            assert!(bincode::deserialize::<Msg>(&bad).is_err(), "list of {} accepted", len);
        }
        // [u32 variant][u64 req][u8 Some][u64 length of the bytes] ...
        let good = bincode::serialize(&Msg::Chunk { req: 1, data: Some(vec![1, 2, 3]) }).unwrap();
        for len in [u64::MAX, 1u64 << 40] {
            let mut bad = good.clone();
            bad[13..21].copy_from_slice(&len.to_le_bytes());
            assert!(bincode::deserialize::<Msg>(&bad).is_err(), "byte string of {} accepted", len);
        }
        // [u32 variant][u64 length of the name] ...
        let good = bincode::serialize(&Msg::Hello { name: "x".to_string() }).unwrap();
        let mut bad = good.clone();
        bad[4..12].copy_from_slice(&(1u64 << 40).to_le_bytes());
        assert!(bincode::deserialize::<Msg>(&bad).is_err());
        // and the honest versions still decode
        assert!(bincode::deserialize::<Msg>(&good).is_ok());
    }
}
