//! rustsync: simple end-to-end encrypted file sync.
//!
//! A background daemon (`rustsync`) keeps your libraries in sync; commands such as `invite`,
//! `join`, `create`, `share` and `accept` talk to it. See `rustsync --help`.

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

#[derive(Parser)]
#[command(
    version,
    about = "Simple end-to-end encrypted file sync",
    after_help = "Run with no command to start the daemon in the foreground (or `start` for the background).\nUse -c <file> to pick a config file (default: ~/.config/rustsync/config.yaml)."
)]
struct Args {
    /// Path to the YAML config file
    #[arg(short = 'c', long = "config", global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Cmd>,

    /// Print this device's public key (creating the key pair if needed) and exit
    #[arg(long)]
    show_key: bool,

    /// When the config file does not exist yet: port to listen on
    #[arg(long)]
    port: Option<u16>,
}

#[derive(Subcommand)]
pub enum Cmd {
    /// Start the daemon in the background
    Start,
    /// Stop the daemon
    Stop,
    /// Start rustsync by itself whenever you log in (a systemd user service on Linux, a launch
    /// agent on macOS, a login entry on the BSDs). On Linux it offers to also start it at boot
    InstallService {
        /// Do not ask: also start it at boot
        #[arg(short, long)]
        yes: bool,
    },
    /// Stop starting rustsync by itself (also stops it now)
    UninstallService,
    /// Show this device, paired devices, libraries and waiting offers
    Status,
    /// Pair with another machine: shows a one-time code and waits for it to `join`
    Invite {
        /// Address the other machine uses to reach this one (asked if omitted)
        #[arg(long)]
        host: Option<String>,
    },
    /// Pair with a machine that is waiting in `invite`: paste its code
    Join {
        /// The code (asked if omitted; leave it out so it does not end up in your shell history)
        code: Option<String>,
    },
    /// Stop syncing with a paired device
    Forget {
        /// Number or name of the device (listed if omitted)
        device: Option<String>,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Create a library: a named folder to sync
    Create {
        /// Library name (asked if omitted)
        name: Option<String>,
        /// Folder to sync (asked if omitted)
        folder: Option<String>,
    },
    /// Remove a library from this machine (its files are not deleted)
    Remove {
        /// Number or name of the library (listed if omitted)
        library: Option<String>,
        /// Do not ask for confirmation
        #[arg(short, long)]
        yes: bool,
    },
    /// Share a library with a paired device (syncing, or an encrypted backup with --backup).
    /// Machines that sync the library learn about its backups and use them automatically.
    Share {
        /// Number or name of the library (listed if omitted)
        library: Option<String>,
        /// Number or name of the device (listed if omitted)
        device: Option<String>,
        /// The device only keeps an encrypted copy it can't read (e.g. a server), instead of
        /// syncing the files
        #[arg(long)]
        backup: bool,
    },
    /// Stop sharing a library with a device (the files stay where they are). Share it again later
    /// with `share` (or `accept`, for a backup this machine keeps).
    Unshare {
        /// Number or name of the library (listed if omitted)
        library: Option<String>,
        /// Number or name of the device (those it is shared with are listed if omitted)
        device: Option<String>,
    },
    /// Ask connected devices for their library lists again and re-check encrypted backups
    /// (use after pairing with a new device, if a backup was not picked up by itself)
    Refresh,
    /// Accept a library another device shared with you
    Accept {
        /// Number or name of the offer (listed if omitted)
        offer: Option<String>,
        /// Folder to sync it into, or to store an encrypted backup in (asked if omitted)
        folder: Option<String>,
        /// (Encrypted backups) size limit, e.g. 500G or 2T; "none" for no limit (asked if omitted)
        #[arg(long)]
        quota: Option<String>,
    },
    /// Show or change the size limit of an encrypted backup kept on this machine
    Quota {
        /// Number, name or id of the backup (listed if omitted)
        library: Option<String>,
        /// New limit, e.g. 500G, 2T or "none". Omit to just show the usage
        size: Option<String>,
    },
    /// Delete old data in the encrypted backups of a library that nothing uses any more
    /// (run on a machine that has the files; also happens by itself when a backup is nearly full)
    Prune {
        /// Name of the library (listed if omitted)
        library: Option<String>,
    },
    /// Decline a library another device shared with you
    Decline {
        /// Number or name of the offer (listed if omitted)
        offer: Option<String>,
    },
    /// Show the recovery kit (all libraries in one line) and the recovery code of each library.
    /// They unlock your encrypted backups if every machine is lost: save them in a password manager.
    Recovery {
        /// Name of the library (all of them if omitted)
        library: Option<String>,
    },
    /// Get a library back from an encrypted backup, using its recovery code
    Restore {
        /// A recovery code (rsr1-...) for one library, or the recovery kit (rsk1-...) for all of
        /// them, each into its own subfolder (asked if omitted; leave it out so the code does not
        /// end up in your shell history)
        code: Option<String>,
        /// Empty folder to restore into (asked if omitted)
        folder: Option<String>,
        /// Number or name of the paired device that keeps the backup (listed if omitted)
        #[arg(long)]
        from: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config_path = args.config.clone().unwrap_or_else(config::default_config_path);

    if let Some(cmd) = args.command {
        return cli::run(&config_path, cmd).await;
    }

    // No command: run the daemon.
    cli::ensure_config(&config_path, args.port)?;
    let cfg = config::load(&config_path)?;
    cfg.ensure_dir()?;
    let keys = net::load_or_create_keys(&cfg.private_key_path(), &cfg.public_key_path())?;
    let my_key = net::hex(&keys.public);
    if args.show_key {
        println!("{}", my_key);
        return Ok(());
    }

    let level = cfg
        .logging
        .level
        .parse::<tracing::Level>()
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt().with_max_level(level).init();

    // Printed regardless of log level so it is easy to copy.
    println!("rustsync: {}", cfg.device_name());
    println!("Public key: {}", my_key);
    println!("Pair another machine:  rustsync -c {} invite", config_path.display());
    println!();
    info!("state directory: {}", cfg.dir.display());
    sync::run_daemon(cfg, keys).await
}


// ======================================================================
// config
mod config {
    //! The config file. Everything in it is optional: devices and libraries are not configured
    //! here, they are managed with commands and kept in the state directory.

    use anyhow::{anyhow, Context, Result};
    use serde::Deserialize;
    use std::path::{Path, PathBuf};

    #[derive(Debug, Deserialize)]
    pub struct Config {
        /// Name other devices see (defaults to this machine's host name).
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub key_config: KeyConfig,
        #[serde(default)]
        pub server: ServerConfig,
        #[serde(default)]
        pub sync_options: SyncOptions,
        #[serde(default)]
        pub logging: LoggingConfig,
        /// Where keys, paired devices, libraries and indexes live. Default: next to the config file.
        #[serde(default)]
        pub state_dir: Option<String>,
        /// Suggested place for the encrypted backups other devices keep on this machine (the data
        /// can't be read here). You choose the actual folder for each one when you accept it.
        /// Default: `backups` inside the state directory.
        #[serde(default, alias = "depot_dir")]
        pub backup_dir: Option<String>,
        /// Show offers from paired devices to keep an encrypted backup of a library here.
        #[serde(default = "default_true")]
        pub accept_backups: bool,

        // Old config format: still parsed so we can tell the user it is ignored.
        #[serde(default)]
        folders: Option<serde_yaml::Value>,
        #[serde(default)]
        peers: Option<serde_yaml::Value>,

        /// The resolved state directory (filled in by `load`).
        #[serde(skip)]
        pub dir: PathBuf,
    }

    #[derive(Debug, Deserialize, Default)]
    pub struct KeyConfig {
        #[serde(default)]
        pub private_key_path: Option<PathBuf>,
        #[serde(default)]
        pub public_key_path: Option<PathBuf>,
    }

    #[derive(Debug, Deserialize)]
    pub struct ServerConfig {
        #[serde(default = "default_host")]
        pub host: String,
        #[serde(default = "default_port")]
        pub port: u16,
    }

    impl Default for ServerConfig {
        fn default() -> Self {
            ServerConfig { host: default_host(), port: default_port() }
        }
    }

    #[derive(Debug, Deserialize)]
    pub struct SyncOptions {
        /// Seconds between full scans (and full index re-announcements).
        #[serde(default = "default_scan_interval")]
        pub full_scan_interval: u64,
        /// Every Nth full scan re-hashes every file even if size/mtime are unchanged. 0 = never.
        #[serde(default = "default_deep_every")]
        pub deep_scan_every: u64,
        /// How far ahead of this machine's clock another device's change times may be (seconds).
        /// Changes stamped further ahead are ignored, so a device with a wrong clock cannot make
        /// every later edit lose the conflict rule.
        #[serde(default = "default_max_skew")]
        pub max_clock_skew_secs: u64,
    }

    impl Default for SyncOptions {
        fn default() -> Self {
            SyncOptions {
                full_scan_interval: default_scan_interval(),
                deep_scan_every: default_deep_every(),
                max_clock_skew_secs: default_max_skew(),
            }
        }
    }

    #[derive(Debug, Deserialize)]
    pub struct LoggingConfig {
        #[serde(default = "default_level")]
        pub level: String,
    }

    impl Default for LoggingConfig {
        fn default() -> Self {
            LoggingConfig { level: default_level() }
        }
    }

    fn default_true() -> bool {
        true
    }
    fn default_host() -> String {
        "0.0.0.0".to_string()
    }
    fn default_port() -> u16 {
        3032
    }
    fn default_scan_interval() -> u64 {
        30
    }
    fn default_deep_every() -> u64 {
        10
    }
    fn default_max_skew() -> u64 {
        24 * 3600
    }
    fn default_level() -> String {
        "info".to_string()
    }

    pub fn abs_path(p: &Path) -> PathBuf {
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            std::env::current_dir()
                .map(|d| d.join(p))
                .unwrap_or_else(|_| p.to_path_buf())
        }
    }

    /// Used when you run `rustsync` without `-c`: ~/.config/rustsync/config.yaml
    pub fn default_config_path() -> PathBuf {
        match std::env::var("HOME") {
            Ok(h) if !h.is_empty() => Path::new(&h).join(".config").join("rustsync").join("config.yaml"),
            _ => PathBuf::from("rustsync.yaml"),
        }
    }

    pub fn hostname() -> String {
        std::fs::read_to_string("/etc/hostname")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(os_hostname)
            .or_else(|| std::env::var("HOSTNAME").ok())
            .unwrap_or_else(|| "device".to_string())
    }

    /// What the operating system calls this machine (macOS and the BSDs have no /etc/hostname),
    /// without the domain part: "Marks-MacBook.local" -> "Marks-MacBook".
    fn os_hostname() -> Option<String> {
        let mut buf = [0u8; 256];
        // SAFETY: gethostname writes at most buf.len() bytes.
        if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
            return None;
        }
        let n = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let full = String::from_utf8_lossy(&buf[..n]).trim().to_string();
        let first = full.split('.').next().unwrap_or("").to_string();
        if first.is_empty() {
            None
        } else {
            Some(first)
        }
    }

    /// `<config path without extension>.state`, e.g. alpha.yaml -> alpha.state
    fn default_state_dir(config_path: &Path) -> PathBuf {
        let stem = abs_path(config_path).with_extension("");
        crate::net::sibling(&stem, ".state")
    }

    fn q(s: &str) -> String {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    }

    /// Write a minimal config file.
    pub fn write_default(path: &Path, port: u16) -> Result<()> {
        let mut y = String::new();
        y.push_str("# rustsync config. Everything here is optional.\n");
        y.push_str("# You do not edit this file to pair machines or share folders; use commands:\n");
        y.push_str(&format!(
            "#   rustsync -c {} status | invite | join | create | share | accept\n\n",
            path.display()
        ));
        y.push_str(&format!("name: {}\n\n", q(&hostname())));
        y.push_str(&format!("server:\n  port: {}\n\n", port));
        y.push_str("# Other devices can keep encrypted backups on this machine. You pick the folder for each\n");
        y.push_str("# one when you accept it (`rustsync accept`); this is just the suggested location:\n");
        y.push_str("# backup_dir: /mnt/bigdrive/rustsync-backups\n");
        y.push_str("# accept_backups: true      # false = never show backup offers\n\n");
        y.push_str("sync_options:\n  full_scan_interval: 30\n\nlogging:\n  level: info\n");
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("cannot create {}", parent.display()))?;
            }
        }
        std::fs::write(path, y).with_context(|| format!("cannot write {}", path.display()))
    }

    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read config {}", path.display()))?;
        let mut cfg: Config =
            serde_yaml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
        if cfg.folders.is_some() || cfg.peers.is_some() {
            eprintln!(
                "note: `folders` and `peers` in {} are no longer used. Pair machines with `invite`/`join` and share folders with `create`/`share`.",
                path.display()
            );
        }
        cfg.dir = match &cfg.state_dir {
            Some(d) => abs_path(Path::new(d)),
            None => default_state_dir(path),
        };
        Ok(cfg)
    }

    impl Config {
        pub fn private_key_path(&self) -> PathBuf {
            match &self.key_config.private_key_path {
                Some(p) => abs_path(p),
                None => self.dir.join("private.key"),
            }
        }
        pub fn public_key_path(&self) -> PathBuf {
            match &self.key_config.public_key_path {
                Some(p) => abs_path(p),
                None => self.dir.join("public.key"),
            }
        }
        pub fn backup_dir_path(&self) -> PathBuf {
            match &self.backup_dir {
                Some(d) => abs_path(Path::new(d)),
                None => self.dir.join("backups"),
            }
        }
        pub fn state_file(&self) -> PathBuf {
            self.dir.join("state.json")
        }
        pub fn socket_path(&self) -> PathBuf {
            self.dir.join("control.sock")
        }
        pub fn log_path(&self) -> PathBuf {
            self.dir.join("daemon.log")
        }
        pub fn device_name(&self) -> String {
            self.name
                .clone()
                .filter(|n| !n.trim().is_empty())
                .unwrap_or_else(hostname)
        }
        /// Make sure the state directory exists and is private.
        pub fn ensure_dir(&self) -> Result<()> {
            use std::os::unix::fs::PermissionsExt;
            std::fs::create_dir_all(&self.dir)
                .map_err(|e| anyhow!("cannot create {}: {}", self.dir.display(), e))?;
            let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
            Ok(())
        }
    }
}

// ======================================================================
// state
mod state {
    //! Everything the daemon remembers between runs: paired devices, libraries and pending offers.
    //! Stored as JSON in the state directory (mode 0600: it holds pairing and vault secrets).

    use anyhow::{Context, Result};
    use serde::{Deserialize, Serialize};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    /// A machine we are paired with.
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Device {
        /// Public key (hex). This is the device's identity.
        pub key: String,
        pub name: String,
        /// Secret shared by exactly this pair of devices (hex). Mixed into the handshake.
        pub psk: String,
        /// Where to reach it. Only the side that joined has this; the other side waits for it to call.
        #[serde(default)]
        pub host: Option<String>,
        #[serde(default)]
        pub port: Option<u16>,
    }

    /// What another device does with one of our libraries. This is decided per library, so one
    /// machine can sync some libraries normally and only keep encrypted backups of others.
    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
    pub enum Role {
        /// Syncs the plain files with us (and has the key).
        Peer,
        /// Keeps an encrypted copy for us. It can never read it and has no key.
        Store,
        /// (On a backup library) a device that keeps its encrypted backup with us.
        Client,
    }

    impl Role {
        pub fn word(self) -> &'static str {
            match self {
                Role::Peer => "sync",
                Role::Store => "encrypted backup",
                Role::Client => "backs up here",
            }
        }
    }

    /// A device that a library is shared with, and how.
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Member {
        pub device: String,
        pub role: Role,
        /// False until the device confirmed it (accepted the offer / agreed to store it).
        pub accepted: bool,
    }

    /// What a library is on this machine.
    #[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
    pub enum LibMode {
        /// A normal folder of plain files that is kept in sync.
        #[default]
        Sync,
        /// This machine only stores the library encrypted (for other devices); the folder holds
        /// scrambled data this machine cannot read, and there is no vault key here.
        Backup,
    }

    /// A named, synced folder. Shared between devices by id; each device picks its own local folder.
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Library {
        pub id: String,
        pub name: String,
        pub folder: String,
        /// Encrypts what goes to encrypted-backup devices (hex). Shared with your own syncing
        /// devices, never with a device that only keeps a backup.
        pub vault: String,
        #[serde(default)]
        pub members: Vec<Member>,
        #[serde(default)]
        pub mode: LibMode,
        /// (Backup mode) proof that a device knows the library's key: hash derived from it. A paired
        /// device that presents it may use this backup without asking again.
        #[serde(default)]
        pub access: String,
        /// (Backup mode) most bytes the encrypted backup may use; 0 = no limit.
        #[serde(default)]
        pub quota: u64,
        /// Devices the owner explicitly unshared this library from. They are not added back by
        /// other devices' hints, and (backup mode) must be allowed again by hand.
        #[serde(default)]
        pub excluded: Vec<String>,
    }

    impl Library {
        /// Has this device confirmed it has this library in this role?
        pub fn has(&self, dev: &str, role: Role) -> bool {
            self.members.iter().any(|m| m.device == dev && m.role == role && m.accepted)
        }

        /// Confirmed devices in this role.
        pub fn devices(&self, role: Role) -> Vec<String> {
            self.members
                .iter()
                .filter(|m| m.role == role && m.accepted)
                .map(|m| m.device.clone())
                .collect()
        }
    }

    /// A library another device wants to share with us, waiting for `accept`.
    #[derive(Serialize, Deserialize, Clone, Debug)]
    pub struct Offer {
        pub from: String,
        pub id: String,
        pub name: String,
        pub vault: String,
        /// An offer to keep an encrypted backup (no name, no key) rather than to sync.
        #[serde(default)]
        pub backup: bool,
        #[serde(default)]
        pub access: String,
    }

    #[derive(Serialize, Deserialize, Default, Clone, Debug)]
    pub struct State {
        #[serde(default)]
        pub devices: Vec<Device>,
        #[serde(default)]
        pub libraries: Vec<Library>,
        #[serde(default)]
        pub offers: Vec<Offer>,
    }

    impl State {
        pub fn load(path: &Path) -> Result<State> {
            match std::fs::read(path) {
                Ok(bytes) => {
                    serde_json::from_slice(&bytes).with_context(|| format!("cannot read {}", path.display()))
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
                Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
            }
        }

        pub fn save(&self, path: &Path) -> Result<()> {
            let tmp = crate::net::sibling(path, ".tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(self)?)?;
            drop(f);
            std::fs::rename(&tmp, path)?;
            Ok(())
        }

        pub fn device(&self, key: &str) -> Option<&Device> {
            self.devices.iter().find(|d| d.key == key)
        }

        pub fn device_name(&self, key: &str) -> String {
            self.device(key)
                .map(|d| d.name.clone())
                .unwrap_or_else(|| key.chars().take(8).collect())
        }
    }
}

// ======================================================================
// net
mod net {
    //! Transport security.
    //!
    //! Every connection runs the Noise protocol `Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s`
    //! (the same family WireGuard uses): mutual authentication with long-term X25519
    //! keys, ephemeral keys for forward secrecy, ChaCha20-Poly1305 for records, and a
    //! 32-byte pre-shared secret per pair of devices mixed into every key. As long as that secret
    //! stays secret, recorded traffic stays safe even from a future quantum computer (X25519 alone
    //! would not be); the identity keys themselves are ordinary X25519.
    //!
    //! Wire format: a sequence of frames `[u16 big-endian length][ciphertext]`.
    //! Each decrypted record is `[flag][data]` where flag 1 means "more records
    //! follow for this message" and 0 means "last record".

    use anyhow::{anyhow, bail, Result};
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};


    const PARAMS: &str = "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s";
    const PROLOGUE: &[u8] = b"rustsync/3";
    const MAX_RECORD: usize = 65535;
    const PLAIN_CHUNK: usize = 60000;
    const MAX_MESSAGE: usize = 24 * 1024 * 1024;

    pub struct Keys {
        pub private: Vec<u8>,
        pub public: Vec<u8>,
    }

    pub fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{:02x}", x)).collect()
    }

    pub fn unhex(s: &str) -> Result<Vec<u8>> {
        let s = s.trim();
        if !s.is_ascii() || s.len() % 2 != 0 {
            bail!("invalid hex string");
        }
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!(e)))
            .collect()
    }

    /// The public key belonging to a private key (X25519).
    fn public_from_private(private: &[u8]) -> Result<Vec<u8>> {
        let arr: [u8; 32] = private
            .try_into()
            .map_err(|_| anyhow!("the private key file must contain 32 bytes of hex"))?;
        let secret = x25519_dalek::StaticSecret::from(arr);
        Ok(x25519_dalek::PublicKey::from(&secret).as_bytes().to_vec())
    }

    /// Refuse to use a private key that others could have read or replaced.
    fn check_private_file(path: &Path) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::symlink_metadata(path)?;
        if md.file_type().is_symlink() {
            bail!(
                "{} is a symlink; set key_config.private_key_path to the real file instead",
                path.display()
            );
        }
        if !md.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        if md.uid() != unsafe { libc::geteuid() } {
            bail!("{} belongs to another user", path.display());
        }
        if md.mode() & 0o077 != 0 {
            bail!(
                "{} can be read by other users (mode {:o}); fix it with:  chmod 600 {}",
                path.display(),
                md.mode() & 0o777,
                path.display()
            );
        }
        Ok(())
    }

    /// The public key file is only a convenience copy (so you can `cat` it): always derived from
    /// the private key, replaced atomically.
    fn write_public(path: &Path, public: &[u8]) -> Result<()> {
        // Unpredictable name, created exclusively and never through a symlink (like the private key).
        let tmp = sibling(path, &format!(".new-{:016x}", rand::random::<u64>()));
        let written = (|| -> std::io::Result<()> {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            f.write_all(format!("{}\n", hex(public)).as_bytes())
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        if let Err(e) = std::fs::rename(&tmp, path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        Ok(())
    }

    /// Load the key pair, or generate and save a new one if there is no private key yet.
    pub fn load_or_create_keys(private_path: &Path, public_path: &Path) -> Result<Keys> {
        if std::fs::symlink_metadata(private_path).is_err() {
            if public_path.exists() {
                bail!(
                    "{} exists but {} does not; refusing to guess. Delete {} to generate a new pair",
                    public_path.display(),
                    private_path.display(),
                    public_path.display()
                );
            }
            let params: snow::params::NoiseParams = PARAMS.parse()?;
            let kp = snow::Builder::new(params).generate_keypair()?;
            for p in [private_path, public_path] {
                if let Some(parent) = p.parent() {
                    std::fs::create_dir_all(parent)?;
                }
            }
            // Write under a temporary name, then link it into place: the private key either
            // appears complete or not at all, and an existing key is never overwritten.
            let tmp = sibling(private_path, ".new");
            let _ = std::fs::remove_file(&tmp);
            {
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)?;
                writeln!(f, "{}", hex(&kp.private))?;
                f.sync_all()?;
            }
            let placed = match std::fs::hard_link(&tmp, private_path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
                // some file systems have no hard links
                Err(_) => std::fs::rename(&tmp, private_path),
            };
            let _ = std::fs::remove_file(&tmp);
            placed.map_err(|e| anyhow!("cannot create {}: {}", private_path.display(), e))?;
            write_public(public_path, &kp.public)?;
            return Ok(Keys { private: kp.private, public: kp.public });
        }

        check_private_file(private_path)?;
        // Older versions wrote the key without a trailing newline; add it so `cat` is tidy.
        ensure_trailing_newline(private_path)?;
        let private = unhex(&std::fs::read_to_string(private_path)?)?;
        let public = public_from_private(&private)?;
        let stored = std::fs::read_to_string(public_path).ok().and_then(|t| unhex(&t).ok());
        if stored.as_deref() != Some(public.as_slice()) {
            if stored.is_some() {
                eprintln!(
                    "warning: {} did not match the private key; rewriting it",
                    public_path.display()
                );
            }
            write_public(public_path, &public)?;
        }
        Ok(Keys { private, public })
    }

    fn ensure_trailing_newline(path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path)?;
        if !text.is_empty() && !text.ends_with('\n') {
            let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
            f.write_all(b"\n")?;
        }
        Ok(())
    }

    /// Parse a 32-byte value written as 64 hex characters (keys, codes).
    pub fn parse_hex32(s: &str, what: &str) -> Result<Vec<u8>> {
        let k = unhex(s).map_err(|_| anyhow!("{} is not valid hex", what))?;
        if k.len() != 32 {
            bail!("{} must be 64 hex characters, got {}", what, s.trim().len());
        }
        Ok(k)
    }

    /// `<path><suffix>` (appends to the file name instead of replacing the extension).
    pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
        let mut s = path.as_os_str().to_owned();
        s.push(suffix);
        PathBuf::from(s)
    }

    /// True if `s` is exactly `len` lowercase/uppercase hex characters.
    pub fn valid_hex(s: &str, len: usize) -> bool {
        s.len() == len && s.bytes().all(|c| c.is_ascii_hexdigit())
    }

    pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
        let len = r.read_u16().await? as usize;
        let mut buf = vec![0u8; len];
        r.read_exact(&mut buf).await?;
        Ok(buf)
    }

    async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
        w.write_all(&(data.len() as u16).to_be_bytes()).await?;
        w.write_all(data).await
    }

    /// Run the Noise XX handshake. Returns the transport state and the peer's static public key.
    /// `psk` is the 32-byte secret code of the machine being called. It is mixed into every key
    /// from the first message on, so recorded traffic stays safe even if X25519 is broken later,
    /// and a machine without the code cannot even complete the handshake.
    pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        keys: &Keys,
        initiator: bool,
        psk: &[u8],
    ) -> Result<(snow::TransportState, Vec<u8>)> {
        let params: snow::params::NoiseParams = PARAMS.parse()?;
        let builder = snow::Builder::new(params)
            .local_private_key(&keys.private)
            .psk(0, psk)
            .prologue(PROLOGUE);
        let mut hs = if initiator { builder.build_initiator()? } else { builder.build_responder()? };
        let mut buf = vec![0u8; MAX_RECORD];
        let mut my_turn = initiator;
        while !hs.is_handshake_finished() {
            if my_turn {
                let n = hs.write_message(&[], &mut buf)?;
                write_frame(stream, &buf[..n]).await?;
            } else {
                let frame = read_frame(stream).await?;
                hs.read_message(&frame, &mut buf)?;
            }
            my_turn = !my_turn;
        }
        let remote = hs
            .get_remote_static()
            .ok_or_else(|| anyhow!("peer sent no static key"))?
            .to_vec();
        Ok((hs.into_transport_mode()?, remote))
    }

    /// Responder side when we do not yet know who is calling: the first handshake message only
    /// decrypts with the right pre-shared secret, so we try each candidate in turn. Returns the
    /// transport, the caller's public key and the index of the secret that matched.
    pub async fn handshake_responder<S: AsyncRead + AsyncWrite + Unpin>(
        stream: &mut S,
        keys: &Keys,
        psks: &[Vec<u8>],
    ) -> Result<(snow::TransportState, Vec<u8>, usize)> {
        let first = read_frame(stream).await?;
        let mut buf = vec![0u8; MAX_RECORD];
        for (i, psk) in psks.iter().enumerate() {
            let params: snow::params::NoiseParams = PARAMS.parse()?;
            let mut hs = snow::Builder::new(params)
                .local_private_key(&keys.private)
                .psk(0, psk.as_slice())
                .prologue(PROLOGUE)
                .build_responder()?;
            if hs.read_message(&first, &mut buf).is_err() {
                continue;
            }
            let n = hs.write_message(&[], &mut buf)?;
            write_frame(stream, &buf[..n]).await?;
            let frame = read_frame(stream).await?;
            hs.read_message(&frame, &mut buf)?;
            let remote = hs
                .get_remote_static()
                .ok_or_else(|| anyhow!("peer sent no static key"))?
                .to_vec();
            return Ok((hs.into_transport_mode()?, remote, i));
        }
        bail!("unknown device or wrong code")
    }

    /// Encrypt and send one application message (split into records as needed).
    pub async fn send_message<W: AsyncWrite + Unpin>(
        w: &mut W,
        t: &mut snow::TransportState,
        data: &[u8],
    ) -> Result<()> {
        let mut out = vec![0u8; MAX_RECORD];
        let mut plain = Vec::with_capacity(PLAIN_CHUNK + 1);
        let mut pieces = data.chunks(PLAIN_CHUNK).peekable();
        while let Some(piece) = pieces.next() {
            plain.clear();
            plain.push(if pieces.peek().is_some() { 1 } else { 0 });
            plain.extend_from_slice(piece);
            let n = t.write_message(&plain, &mut out)?;
            write_frame(w, &out[..n]).await?;
        }
        Ok(())
    }

    /// Collects decrypted records until a complete message is available.
    #[derive(Default)]
    pub struct Reassembler {
        buf: Vec<u8>,
    }

    impl Reassembler {
        pub fn push(&mut self, t: &mut snow::TransportState, frame: &[u8]) -> Result<Option<Vec<u8>>> {
            let mut plain = vec![0u8; frame.len()];
            let n = t.read_message(frame, &mut plain)?;
            if n == 0 {
                bail!("empty record");
            }
            self.buf.extend_from_slice(&plain[1..n]);
            if self.buf.len() > MAX_MESSAGE {
                bail!("message too large");
            }
            if plain[0] == 0 {
                Ok(Some(std::mem::take(&mut self.buf)))
            } else {
                Ok(None)
            }
        }
    }
}

// ======================================================================
// index
mod index {
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
    /// folder being swapped for a symlink between our checks and the open. Linux and macOS can tell us
    /// where an open descriptor really is; on the BSDs there is no portable way, so we rely on the
    /// other safeguards there (O_NOFOLLOW and the ancestor checks).
    #[cfg(target_os = "linux")]
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

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
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
}

// ======================================================================
// apply
mod apply {
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
            match apply_file(folder, remote, &e).await {
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
                bail!("{} would be written outside the library folder", e.path);
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
                    if blake3::hash(&data).to_hex().as_str() != want {
                        bail!("chunk {} failed verification", i);
                    }
                    Ok(Some(data))
                }
            })
            .buffered(8);
        futures_util::pin_mut!(fetches);

        let mut i = 0usize;
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
            whole.update(&data);
            out.write_all(&data).await?;
            i += 1;
        }
        out.flush().await?;
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
}

// ======================================================================
// vault
mod vault {
    //! Encryption for data that leaves our machines for a depot.
    //!
    //! One 32-byte vault key, shared by your own machines and NEVER given to the depot, is split
    //! (BLAKE3 key derivation) into independent keys for: chunk encryption, record encryption,
    //! hiding file paths, and naming chunks. Everything is symmetric (XChaCha20-Poly1305 and
    //! BLAKE3), so it is not materially weakened by quantum computers (symmetric 256-bit keys).
    //!
    //! The depot sees: how many files exist, when they changed, chunk sizes, and which chunks are
    //! identical. It does not see names, paths, contents or modes.

    use crate::index::Entry;
    use anyhow::{anyhow, bail, Result};
    use chacha20poly1305::aead::{Aead, KeyInit, Payload};
    use chacha20poly1305::{XChaCha20Poly1305, XNonce};

    pub struct Vault {
        blobs: XChaCha20Poly1305,
        records: XChaCha20Poly1305,
        slot_key: [u8; 32],
        blob_id_key: [u8; 32],
    }

    fn seal(c: &XChaCha20Poly1305, aad: &[u8], plain: &[u8]) -> Vec<u8> {
        let nonce: [u8; 24] = rand::random();
        let ct = c
            .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
            .expect("encryption cannot fail");
        let mut out = nonce.to_vec();
        out.extend_from_slice(&ct);
        out
    }

    fn open(c: &XChaCha20Poly1305, aad: &[u8], data: &[u8]) -> Result<Vec<u8>> {
        if data.len() < 24 + 16 {
            bail!("truncated data from the depot");
        }
        let (nonce, ct) = data.split_at(24);
        c.decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
            .map_err(|_| anyhow!("decryption failed: wrong vault key, or the depot's data was tampered with"))
    }

    /// Proof that a device knows a library's key, without revealing it. A machine keeping an
    /// encrypted backup stores this when it first accepts the library; later a paired device
    /// (another of your machines, or a replacement after a loss) that presents the same value is
    /// let in without further questions.
    pub fn access_token(master: &[u8]) -> String {
        crate::net::hex(&blake3::derive_key("rustsync vault v1 backup access", master))
    }

    fn recovery_check(id: &str, vault: &str) -> String {
        blake3::hash(format!("rustsync recovery v1|{}|{}", id, vault).as_bytes()).to_hex().as_str()[..8].to_string()
    }

    /// The text you keep somewhere safe (a password manager): it holds the library's id and key,
    /// which is everything needed, together with any encrypted backup, to get the library back.
    pub fn recovery_code(lib_id: &str, vault_hex: &str) -> String {
        format!("rsr1-{}-{}-{}", lib_id, vault_hex, recovery_check(lib_id, vault_hex))
    }

    /// -> (library id, vault key hex)
    pub fn parse_recovery_code(code: &str) -> Result<(String, String)> {
        let c: String = code.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
        let parts: Vec<&str> = c.split('-').collect();
        if parts.len() != 4 || parts[0] != "rsr1" {
            bail!("that does not look like a rustsync recovery code (it starts with rsr1-)");
        }
        if !crate::net::valid_hex(parts[1], 32) || !crate::net::valid_hex(parts[2], 64) || !crate::net::valid_hex(parts[3], 8) {
            bail!("the recovery code is incomplete or has a typo");
        }
        if recovery_check(parts[1], parts[2]) != parts[3] {
            bail!("the recovery code has a typo (its check value does not match)");
        }
        Ok((parts[1].to_string(), parts[2].to_string()))
    }

    const MAX_KIT_BYTES: usize = 1024 * 1024;

    #[derive(serde::Serialize, serde::Deserialize)]
    struct KitLibrary {
        id: String,
        name: String,
        vault: String,
    }

    fn kit_check(payload_hex: &str) -> String {
        blake3::hash(format!("rustsync kit v1|{}", payload_hex).as_bytes()).to_hex().as_str()[..8].to_string()
    }

    /// One line holding every library's id, name and key: everything (together with the encrypted
    /// backups) needed to get all libraries back. Meant for a password manager. It does not contain
    /// this device's identity: a replacement machine is simply paired again.
    pub fn recovery_kit(libs: &[(String, String, String)]) -> String {
        let items: Vec<KitLibrary> = libs
            .iter()
            .map(|(id, name, vault)| KitLibrary { id: id.clone(), name: name.clone(), vault: vault.clone() })
            .collect();
        let json = serde_json::to_vec(&items).unwrap_or_default();
        let payload = crate::net::hex(&json);
        format!("rsk1-{}-{}", payload, kit_check(&payload))
    }

    /// -> [(library id, name, vault key hex)]
    pub fn parse_recovery_kit(kit: &str) -> Result<Vec<(String, String, String)>> {
        let c: String = kit.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
        let parts: Vec<&str> = c.split('-').collect();
        if parts.len() != 3 || parts[0] != "rsk1" {
            bail!("that does not look like a rustsync recovery kit (it starts with rsk1-)");
        }
        let (payload, check) = (parts[1], parts[2]);
        if payload.len() > MAX_KIT_BYTES * 2 || payload.len() % 2 != 0 || !payload.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("the recovery kit is incomplete or has a typo");
        }
        if kit_check(payload) != check {
            bail!("the recovery kit has a typo (its check value does not match)");
        }
        let mut bytes = Vec::with_capacity(payload.len() / 2);
        for i in (0..payload.len()).step_by(2) {
            bytes.push(u8::from_str_radix(&payload[i..i + 2], 16).map_err(|_| anyhow!("the recovery kit has a typo"))?);
        }
        let items: Vec<KitLibrary> = serde_json::from_slice(&bytes).map_err(|_| anyhow!("the recovery kit is damaged"))?;
        let mut out = Vec::new();
        for it in items.into_iter().take(10_000) {
            if !crate::net::valid_hex(&it.id, 32) || !crate::net::valid_hex(&it.vault, 64) {
                bail!("the recovery kit holds an invalid library");
            }
            out.push((it.id, it.name, it.vault));
        }
        Ok(out)
    }

    impl Vault {
        pub fn new(master: &[u8]) -> Vault {
            let derive = |ctx: &str| blake3::derive_key(ctx, master);
            Vault {
                blobs: XChaCha20Poly1305::new_from_slice(&derive("rustsync vault v1 chunk encryption"))
                    .expect("32-byte key"),
                records: XChaCha20Poly1305::new_from_slice(&derive("rustsync vault v1 record encryption"))
                    .expect("32-byte key"),
                slot_key: derive("rustsync vault v1 path hiding"),
                blob_id_key: derive("rustsync vault v1 chunk naming"),
            }
        }

        /// Opaque, stable name for a path (the depot keeps one record per slot).
        pub fn slot(&self, path: &str) -> String {
            blake3::keyed_hash(&self.slot_key, path.as_bytes()).to_hex().to_string()
        }

        /// Opaque name for a chunk, derived from its plaintext hash.
        pub fn blob_id(&self, plain_hash: &str) -> String {
            blake3::keyed_hash(&self.blob_id_key, plain_hash.as_bytes()).to_hex().to_string()
        }

        pub fn seal_blob(&self, id: &str, chunk: &[u8]) -> Vec<u8> {
            seal(&self.blobs, id.as_bytes(), chunk)
        }

        pub fn open_blob(&self, id: &str, sealed: &[u8]) -> Result<Vec<u8>> {
            open(&self.blobs, id.as_bytes(), sealed)
        }

        pub fn seal_record(&self, slot: &str, e: &Entry) -> Result<Vec<u8>> {
            Ok(seal(&self.records, slot.as_bytes(), &bincode::serialize(e)?))
        }

        pub fn open_record(&self, slot: &str, sealed: &[u8]) -> Result<Entry> {
            let plain = open(&self.records, slot.as_bytes(), sealed)?;
            let e: Entry = bincode::deserialize(&plain)?;
            if self.slot(&e.path) != slot {
                bail!("the depot returned a record for the wrong path");
            }
            Ok(e)
        }
    }
}

// ======================================================================
// depot
mod depot {
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
}

// ======================================================================
// proto
mod proto {
    //! What the command line says to the running daemon, and what it answers (JSON lines over a
    //! local unix socket in the state directory).

    use serde::{Deserialize, Serialize};

    #[derive(Serialize, Deserialize, Debug)]
    pub enum Request {
        Status,
        /// Create a one-time pairing code and wait until someone joins with it.
        Invite { host: String },
        Join { token: String },
        Forget { device: String },
        Create { name: String, folder: String },
        RemoveLibrary { library: String },
        Share { library: String, device: String, backup: bool },
        Accept {
            offer: String,
            folder: String,
            /// (Backup offers) size limit in bytes; 0 = none.
            #[serde(default)]
            quota: u64,
        },
        Decline { offer: String },
        /// Stop sharing a library with a device (works for syncing, backups, and offers not yet
        /// accepted). The files stay where they are.
        Unshare { library: String, device: String },
        /// Re-send our library lists to connected devices, ask theirs, and look for backups again.
        Refresh,
        /// Change the size limit of an encrypted backup kept here (0 = none).
        SetQuota { library: String, bytes: u64 },
        /// Delete chunks in the encrypted backups of a library that nothing refers to any more.
        Prune { library: String },
        /// Recovery code(s) for one library, or the recovery kit plus all codes.
        Recovery { library: Option<String> },
        /// Bring a library (recovery code) or all libraries (recovery kit) back from an encrypted
        /// backup kept on `device`.
        Restore { code: String, folder: String, device: String },
        Stop,
    }

    #[derive(Serialize, Deserialize, Debug)]
    pub enum Reply {
        Info(StatusInfo),
        /// Printed on stdout (things you may want to copy or pipe).
        Out(String),
        /// Printed on stderr (progress and hints).
        Note(String),
        Done,
        Error(String),
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct StatusInfo {
        pub name: String,
        pub key: String,
        pub listen: String,
        /// Suggested place for encrypted backups other devices keep here.
        pub backup_dir: String,
        pub devices: Vec<DeviceInfo>,
        /// Libraries with a folder on this machine (including encrypted backups kept for others).
        pub libraries: Vec<LibraryInfo>,
        pub offers: Vec<OfferInfo>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct DeviceInfo {
        pub key: String,
        pub name: String,
        pub online: bool,
        pub addr: Option<String>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct LibraryInfo {
        pub id: String,
        pub name: String,
        pub folder: String,
        /// True if this machine only keeps an encrypted copy (it cannot read the files).
        pub backup_only: bool,
        /// (backup_only) names of the devices whose backup of it we keep.
        pub kept_for: Vec<String>,
        /// Names of the devices that sync it with us.
        pub syncing_with: Vec<String>,
        /// Names of the devices that keep an encrypted backup of it.
        pub backed_up_on: Vec<String>,
        /// Offers not confirmed yet, e.g. "beta" or "charlie (encrypted backup)".
        pub pending: Vec<String>,
        /// (backup_only) bytes stored here, and the size limit (0 = none).
        pub used: u64,
        pub quota: u64,
        /// Space used by our encrypted backups on other devices, as last reported by them.
        pub backups: Vec<BackupUsage>,
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct BackupUsage {
        pub device: String,
        /// False until the device has reported its usage.
        pub known: bool,
        pub used: u64,
        pub quota: u64,
    }

    /// "500G", "1.5 TB", "200 MB", "1048576" -> bytes. Units are decimal (1 GB = 1,000,000,000).
    /// "none", "0" and an empty string mean no limit (0).
    pub fn parse_size(s: &str) -> Result<u64, String> {
        let t = s.trim().to_lowercase().replace(' ', "");
        if t.is_empty() || t == "none" || t == "unlimited" || t == "0" {
            return Ok(0);
        }
        let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
        let (num, unit) = t.split_at(split);
        let n: f64 = num.parse().map_err(|_| format!("cannot read \"{}\" as a size (try 500G or 2T)", s.trim()))?;
        let mult: f64 = match unit {
            "" | "b" => 1.0,
            "k" | "kb" => 1e3,
            "m" | "mb" => 1e6,
            "g" | "gb" => 1e9,
            "t" | "tb" => 1e12,
            "p" | "pb" => 1e15,
            _ => return Err(format!("unknown unit \"{}\" (use K, M, G or T)", unit)),
        };
        let bytes = n * mult;
        if !bytes.is_finite() || bytes < 0.0 || bytes > 1e18 {
            return Err("that size is out of range".to_string());
        }
        Ok(bytes as u64)
    }

    /// 3_200_000_000 -> "3.2 GB"
    pub fn fmt_size(b: u64) -> String {
        let units = ["B", "KB", "MB", "GB", "TB", "PB"];
        let mut v = b as f64;
        let mut i = 0;
        while v >= 1000.0 && i < units.len() - 1 {
            v /= 1000.0;
            i += 1;
        }
        if i == 0 {
            format!("{} B", b)
        } else {
            format!("{:.1} {}", v, units[i])
        }
    }

    #[derive(Serialize, Deserialize, Debug, Clone)]
    pub struct OfferInfo {
        pub id: String,
        pub name: String,
        pub from_key: String,
        pub from_name: String,
        /// An offer to keep an encrypted backup (needs a folder, shows no name).
        pub backup: bool,
        /// (backup) a device we stopped serving asks to use a backup we already keep: accepting
        /// needs no folder.
        pub again: bool,
    }
}

// ======================================================================
// sync
mod sync {
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

    /// Sent by a joining device over the one-time-code connection.
    #[derive(Serialize, Deserialize)]
    struct PairHello {
        name: String,
    }

    /// The inviting device's answer: the secret this pair of devices will use from now on.
    #[derive(Serialize, Deserialize)]
    struct PairAccept {
        psk: Vec<u8>,
        name: String,
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
        done: Option<oneshot::Sender<String>>,
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

        fn create_invite(&self, host: &str) -> (String, Vec<u8>, oneshot::Receiver<String>) {
            let secret = random_bytes(32);
            let (tx, rx) = oneshot::channel();
            self.invites.lock().unwrap().push(Invite {
                secret: secret.clone(),
                expires: Instant::now() + Duration::from_secs(INVITE_SECS),
                done: Some(tx),
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

        /// Use a pairing code shown by `invite` on another machine.
        pub async fn join(self: &Arc<Self>, token: &str) -> Result<String> {
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
            let reply: PairAccept = bincode::deserialize(&bytes)?;
            if reply.psk.len() != 32 {
                bail!("the other machine sent an invalid reply");
            }
            let name = clean_name(&reply.name);
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
                Request::Join { token } => {
                    let name = self.join(&token).await?;
                    out(format!("Paired with {}.", name))
                }
                Request::Forget { device } => out(self.forget_device(&device)?),
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
                Request::Invite { .. } => bail!("internal error: invites are handled separately"),
            }
        }
    }

    /// `KEY.SECRET@HOST:PORT` -> (key, one-time secret, host, port)
    fn parse_token(input: &str) -> Result<(Vec<u8>, Vec<u8>, String, u16)> {
        let s = input.trim();
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

    /// `invite`: show a one-time code and keep the command waiting until someone joins with it.
    async fn serve_invite(
        node: &Arc<Node>,
        host: String,
        mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
        mut w: tokio::net::unix::OwnedWriteHalf,
    ) -> Result<()> {
        let (token, secret, rx) = node.create_invite(&host);
        write_reply(&mut w, &Reply::Note("On the other machine run:  rustsync -c <its config> join".to_string())).await?;
        write_reply(
            &mut w,
            &Reply::Note(format!("and paste this code (it works once, for the next {} minutes):", INVITE_SECS / 60)),
        )
        .await?;
        write_reply(&mut w, &Reply::Out(token)).await?;
        write_reply(&mut w, &Reply::Note("Waiting for the other machine... (Ctrl-C to cancel)".to_string())).await?;

        let mut eof = String::new();
        tokio::select! {
            res = tokio::time::timeout(Duration::from_secs(INVITE_SECS), rx) => {
                match res {
                    Ok(Ok(name)) => {
                        write_reply(&mut w, &Reply::Note(format!("Paired with {}.", name))).await?;
                        write_reply(&mut w, &Reply::Done).await?;
                    }
                    Ok(Err(_)) => {
                        write_reply(&mut w, &Reply::Error("the invite expired".to_string())).await?;
                    }
                    Err(_) => {
                        node.cancel_invite(&secret);
                        write_reply(&mut w, &Reply::Error("nobody joined in time; run invite again".to_string())).await?;
                    }
                }
            }
            _ = reader.read_line(&mut eof) => {
                // The command was cancelled (Ctrl-C): the code is no longer valid.
                node.cancel_invite(&secret);
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

    /// The inviting side of pairing: someone proved they hold a valid one-time code.
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

        let bytes = recv_bytes(&mut stream, &mut transport).await?;
        let hello: PairHello = bincode::deserialize(&bytes)?;
        let psk = random_bytes(32);
        let name = clean_name(&hello.name);
        {
            let mut st = node.state.lock().unwrap();
            st.devices.retain(|d| d.key != remote_hex);
            st.devices.push(Device {
                key: remote_hex.clone(),
                name: name.clone(),
                psk: net::hex(&psk),
                host: None,
                port: None,
            });
        }
        node.save_state();
        let reply = PairAccept { psk, name: node.name.clone() };
        net::send_message(&mut stream, &mut transport, &bincode::serialize(&reply)?).await?;
        info!("paired with {} ({})", name, short(&remote_hex));
        node.request_resync();
        if let Some(tx) = invite.done {
            let _ = tx.send(name);
        }
        Ok(())
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
        let mut asm = Reassembler::default();
        let bytes = tokio::time::timeout(Duration::from_secs(15), async {
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
}

// ======================================================================
// cli
mod cli {
    //! The command line. Every command except `start` talks to the running daemon over its local
    //! socket, and asks you for anything you left out.

    use crate::config::{self, Config};
    use crate::proto::{self, Reply, Request, StatusInfo};
    use crate::Cmd;
    use anyhow::{anyhow, bail, Context, Result};
    use std::io::{IsTerminal, Write};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::UnixStream;

    struct Ctl {
        sock: PathBuf,
        config: PathBuf,
    }

    // ---------------------------------------------------------------- prompting

    /// Prompts go to stderr so stdout stays clean for output you may want to copy or pipe.
    fn ask(question: &str, default: &str) -> Result<String> {
        if default.is_empty() {
            eprint!("{}: ", question);
        } else {
            eprint!("{} [{}]: ", question, default);
        }
        std::io::stderr().flush()?;
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line)? == 0 {
            bail!("no input available; pass the value on the command line instead");
        }
        let t = line.trim();
        Ok(if t.is_empty() { default.to_string() } else { t.to_string() })
    }

    fn confirm(question: &str, default_yes: bool) -> Result<bool> {
        let a = ask(&format!("{} ({})", question, if default_yes { "Y/n" } else { "y/N" }), "")?;
        Ok(match a.to_lowercase().as_str() {
            "" => default_yes,
            "y" | "yes" => true,
            _ => false,
        })
    }

    /// Choose one of `labels`: by command-line argument (number, name, id prefix), by asking, or
    /// automatically when there is only one.
    fn pick(
        what: &str,
        labels: &[String],
        arg: Option<String>,
        matches: impl Fn(usize, &str) -> bool,
    ) -> Result<usize> {
        if labels.is_empty() {
            bail!("there is no {} to choose from", what);
        }
        let t = match arg {
            Some(t) => t,
            None => {
                if labels.len() == 1 {
                    eprintln!("Using the only {}: {}", what, labels[0]);
                    return Ok(0);
                }
                for (i, l) in labels.iter().enumerate() {
                    eprintln!("  {}) {}", i + 1, l);
                }
                ask(&format!("Which {} (number or name)", what), "")?
            }
        };
        let t = t.trim().to_string();
        if t.is_empty() {
            bail!("nothing selected");
        }
        if let Ok(n) = t.parse::<usize>() {
            if n >= 1 && n <= labels.len() {
                return Ok(n - 1);
            }
        }
        let hits: Vec<usize> = (0..labels.len()).filter(|&i| matches(i, &t)).collect();
        match hits.len() {
            1 => Ok(hits[0]),
            0 => bail!("no {} matches {:?}", what, t),
            _ => bail!("{:?} matches more than one {}; be more specific", t, what),
        }
    }

    /// "[#####---------------] 3.2 GB of 10.0 GB (32%)", or just the amount when there is no limit.
    fn usage_line(used: u64, quota: u64, with_bar: bool) -> String {
        if quota == 0 {
            return format!("{} used, no size limit", proto::fmt_size(used));
        }
        let frac = (used as f64 / quota as f64).clamp(0.0, 1.0);
        let filled = (frac * 20.0).round() as usize;
        let bar = if with_bar { format!("[{}{}] ", "#".repeat(filled), "-".repeat(20 - filled)) } else { String::new() };
        format!(
            "{}{} of {} ({:.0}%){}",
            bar,
            proto::fmt_size(used),
            proto::fmt_size(quota),
            frac * 100.0,
            if used >= quota { "  FULL: new data is refused" } else { "" }
        )
    }

    fn offer_label(o: &proto::OfferInfo) -> String {
        if o.again {
            format!("allow {} to use the encrypted backup {} again", o.from_name, o.name)
        } else if o.backup {
            format!("keep an encrypted backup ({}) for {}", o.name, o.from_name)
        } else {
            format!("\"{}\" from {}", o.name, o.from_name)
        }
    }

    fn name_or_prefix(name: &str, id: &str, t: &str) -> bool {
        let lower = t.to_lowercase();
        name.to_lowercase() == lower || (lower.len() >= 4 && id.starts_with(&lower))
    }

    // ---------------------------------------------------------------- config creation

    /// First run: if the config file does not exist, create it (asking a couple of questions when
    /// run in a terminal).
    pub fn ensure_config(path: &Path, port_flag: Option<u16>) -> Result<()> {
        if path.exists() {
            return Ok(());
        }
        let mut port = port_flag.unwrap_or(3032);
        if std::io::stdin().is_terminal() {
            eprintln!("{} does not exist yet; creating it.", path.display());
            if port_flag.is_none() {
                port = ask("Port to listen on", "3032")?
                    .parse()
                    .context("that is not a valid port number")?;
            }
        }
        config::write_default(path, port)?;
        eprintln!("Created {}", path.display());
        Ok(())
    }

    // ---------------------------------------------------------------- talking to the daemon

    async fn call(ctl: &Ctl, req: &Request) -> Result<Option<StatusInfo>> {
        let stream = UnixStream::connect(&ctl.sock).await.map_err(|_| {
            anyhow!(
                "rustsync is not running for this config.\nStart it in the background with:  rustsync -c {} start\n(or in a terminal with:  rustsync -c {})",
                ctl.config.display(),
                ctl.config.display()
            )
        })?;
        let (r, mut w) = stream.into_split();
        w.write_all(serde_json::to_string(req)?.as_bytes()).await?;
        w.write_all(b"\n").await?;
        // `w` stays open until we return: closing it tells the daemon we cancelled (Ctrl-C).
        let mut lines = BufReader::new(r).lines();
        let mut info = None;
        while let Some(line) = lines.next_line().await? {
            match serde_json::from_str::<Reply>(&line)? {
                Reply::Info(i) => info = Some(i),
                Reply::Out(s) => println!("{}", s),
                Reply::Note(s) => eprintln!("{}", s),
                Reply::Done => return Ok(info),
                Reply::Error(e) => bail!("{}", e),
            }
        }
        Ok(info)
    }

    async fn status(ctl: &Ctl) -> Result<StatusInfo> {
        call(ctl, &Request::Status)
            .await?
            .ok_or_else(|| anyhow!("rustsync did not answer"))
    }

    fn print_status(config: &Path, s: &StatusInfo) {
        let c = config.display();
        println!("This device:   {}", s.name);
        println!("Public key:    {}", s.key);
        println!("Listening on:  {}", s.listen);
        println!();
        println!("Paired devices:");
        if s.devices.is_empty() {
            println!("  (none yet)  pair one with:  rustsync -c {} invite", c);
        }
        for d in &s.devices {
            println!(
                "  {:<16} {}  {}{}",
                d.name,
                if d.online { "online " } else { "offline" },
                &d.key[..8.min(d.key.len())],
                d.addr.as_ref().map(|a| format!("  {}", a)).unwrap_or_default()
            );
        }
        println!();
        println!("Libraries:");
        if s.libraries.is_empty() {
            println!("  (none yet)  create one with:  rustsync -c {} create", c);
        }
        for l in &s.libraries {
            let mut line = if l.backup_only {
                format!("  {:<16} {}  (encrypted backup, unreadable here)", l.name, l.folder)
            } else {
                format!("  {:<16} {}", l.name, l.folder)
            };
            line.push_str(&format!("\n  {:<16} id {}", "", &l.id[..8.min(l.id.len())]));
            if l.backup_only {
                if !l.kept_for.is_empty() {
                    line.push_str(&format!("\n  {:<16} kept for: {}", "", l.kept_for.join(", ")));
                }
                line.push_str(&format!("\n  {:<16} {}", "", usage_line(l.used, l.quota, true)));
                println!("{}", line);
                continue;
            }
            for b in &l.backups {
                let u = if b.known { usage_line(b.used, b.quota, true) } else { "usage not reported yet".to_string() };
                line.push_str(&format!("\n  {:<16} backup on {}: {}", "", b.device, u));
            }
            if !l.syncing_with.is_empty() {
                line.push_str(&format!("\n  {:<16} syncing with: {}", "", l.syncing_with.join(", ")));
            }
            if !l.pending.is_empty() {
                line.push_str(&format!("\n  {:<16} offered, not confirmed yet: {}", "", l.pending.join(", ")));
            }
            println!("{}", line);
        }
        if !s.offers.is_empty() {
            println!();
            println!("Waiting for you to accept:");
            for o in &s.offers {
                let id8 = &o.id[..8.min(o.id.len())];
                if o.again {
                    println!("  {} asks to use the encrypted backup {} again  ->  rustsync -c {} accept", o.from_name, o.name, c);
                } else if o.backup {
                    println!("  keep an encrypted backup ({}) for {}  ->  rustsync -c {} accept", o.name, o.from_name, c);
                } else {
                    println!("  \"{}\" (id {}) from {}  ->  rustsync -c {} accept", o.name, id8, o.from_name, c);
                }
            }
        }
        if s.libraries.iter().any(|l| !l.backup_only) {
            println!();
            println!("If every machine were lost, the encrypted backups can only be opened with a recovery code:");
            println!("  rustsync -c {} recovery     (save the codes in a password manager)", c);
        }
    }

    // ---------------------------------------------------------------- start (background)

    // ---------------------------------------------------------------- start by itself
    //
    // Linux uses a systemd user service, macOS a launchd agent, and the BSDs (which have no per-user
    // service manager) an entry that starts rustsync when you log in to the desktop. Each lives in its
    // own `svc` module below; only one is compiled.

    async fn wait_socket(cfg: &Config, up: bool, tenths: u32) -> bool {
        for _ in 0..tenths {
            if UnixStream::connect(cfg.socket_path()).await.is_ok() == up {
                return true;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        false
    }

    /// `~/.config` (or $XDG_CONFIG_HOME).
    #[cfg(not(target_os = "macos"))]
    fn config_home() -> PathBuf {
        match std::env::var("XDG_CONFIG_HOME") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => match std::env::var("HOME") {
                Ok(h) if !h.is_empty() => PathBuf::from(h).join(".config"),
                _ => PathBuf::from(".config"),
            },
        }
    }

    /// None for the default config, otherwise a short name made from its path (so that several
    /// configs can each have their own service). The GUI computes the same thing.
    fn service_slug(config_path: &Path) -> Option<String> {
        let abs = std::path::absolute(config_path).unwrap_or_else(|_| config_path.to_path_buf());
        if abs == config::default_config_path() {
            return None;
        }
        let slug: String = abs
            .with_extension("")
            .to_string_lossy()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' })
            .collect();
        Some(slug.trim_matches('-').to_string())
    }

    /// A path as a quoted .desktop `Exec` argument; None if it holds characters that need escaping
    /// rules we do not want to get wrong.
    #[cfg(not(target_os = "macos"))]
    fn desktop_quote(s: &str) -> Option<String> {
        if s.chars().any(|c| matches!(c, '"' | '`' | '$' | '\\' | '%' | '\n')) {
            None
        } else {
            Some(format!("\"{}\"", s))
        }
    }

    #[cfg(not(target_os = "macos"))]
    fn tray_autostart_path() -> PathBuf {
        config_home().join("autostart").join("rustsync-tray.desktop")
    }

    /// Make the tray icon (`rustsync-gui --tray`) start when you log in to the desktop. Does
    /// nothing if the GUI program is not installed next to this one. Returns true if it is set up.
    #[cfg(not(target_os = "macos"))]
    fn install_tray_autostart(config_path: &Path) -> bool {
        let Ok(exe) = std::env::current_exe() else { return false };
        let gui = exe.with_file_name("rustsync-gui");
        if !gui.exists() {
            return false;
        }
        let Ok(abs) = std::path::absolute(config_path) else { return false };
        let (Some(g), Some(c)) = (desktop_quote(&gui.to_string_lossy()), desktop_quote(&abs.to_string_lossy())) else {
            return false;
        };
        let path = tray_autostart_path();
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=rustsync\nComment=rustsync tray icon\nExec={} --tray -c {}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            g, c
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, text).is_ok()
    }

    /// Start the daemon in the background (a copy of this program, detached) and wait for it.
    async fn spawn_daemon(config_path: &Path, cfg: &Config) -> Result<()> {
        let log_path = cfg.log_path();
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
        let log2 = log.try_clone()?;
        let exe = std::env::current_exe()?;
        let mut cmd = std::process::Command::new(exe);
        cmd.arg("-c")
            .arg(config_path)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::from(log))
            .stderr(std::process::Stdio::from(log2))
            .process_group(0);
        cmd.spawn().context("cannot start rustsync")?;
        if wait_socket(cfg, true, 50).await {
            println!("rustsync started in the background. Log: {}", log_path.display());
            return Ok(());
        }
        bail!("rustsync did not start; see {}", log_path.display())
    }

    /// Stop a daemon that was started by hand, so a service can take over its socket.
    async fn stop_running(config_path: &Path, cfg: &Config) {
        if UnixStream::connect(cfg.socket_path()).await.is_ok() {
            let ctl = Ctl { sock: cfg.socket_path(), config: config_path.to_path_buf() };
            let _ = call(&ctl, &Request::Stop).await;
            wait_socket(cfg, false, 50).await;
        }
    }

    // ---- Linux: a systemd user service

    #[cfg(target_os = "linux")]
    mod svc {
        use super::*;

        /// The question `start` asks, once, at a terminal.
        pub const OFFER: &str = "Start rustsync automatically when you log in (and optionally at boot)?";

        fn unit_name(config_path: &Path) -> String {
            match service_slug(config_path) {
                None => "rustsync.service".to_string(),
                Some(slug) => format!("rustsync-{}.service", slug),
            }
        }

        fn unit_path(name: &str) -> PathBuf {
            config_home().join("systemd").join("user").join(name)
        }

        /// Run `systemctl --user ...`; Ok(true) if it succeeded.
        fn systemctl(args: &[&str]) -> Result<bool> {
            let status = std::process::Command::new("systemctl")
                .arg("--user")
                .args(args)
                .stdin(std::process::Stdio::null())
                .status()
                .context("cannot run systemctl")?;
            Ok(status.success())
        }

        /// Is there a systemd user manager to talk to?
        pub fn supported() -> bool {
            std::process::Command::new("systemctl")
                .args(["--user", "show-environment"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        pub fn installed(config_path: &Path) -> bool {
            unit_path(&unit_name(config_path)).exists()
        }

        /// A word in a systemd unit's command line.
        fn unit_quote(s: &str) -> Result<String> {
            if s.contains('\n') {
                bail!("a path contains a line break; cannot put it in a service file");
            }
            let mut out = String::from("\"");
            for c in s.chars() {
                match c {
                    '\\' => out.push_str("\\\\"),
                    '"' => out.push_str("\\\""),
                    '%' => out.push_str("%%"),
                    '$' => out.push_str("$$"),
                    c => out.push(c),
                }
            }
            out.push('"');
            Ok(out)
        }

        fn linger_enabled() -> bool {
            // SAFETY: getuid cannot fail.
            let uid = unsafe { libc::getuid() }.to_string();
            std::process::Command::new("loginctl")
                .args(["show-user", &uid, "-p", "Linger", "--value"])
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).trim() == "yes")
                .unwrap_or(false)
        }

        /// With the service installed, start it through systemd. Ok(false): not applicable.
        pub async fn start(config_path: &Path, cfg: &Config) -> Result<bool> {
            if !installed(config_path) || !supported() {
                return Ok(false);
            }
            let name = unit_name(config_path);
            if systemctl(&["start", &name])? && wait_socket(cfg, true, 100).await {
                println!("rustsync started (systemd service {}).", name);
                return Ok(true);
            }
            bail!("the service {} did not start; see systemctl --user status {}", name, name)
        }

        pub async fn install(config_path: &Path, cfg: &Config, yes: bool) -> Result<()> {
            if !supported() {
                bail!("this needs systemd (a Linux desktop or server with a systemd user session)");
            }
            cfg.ensure_dir()?;
            let abs = std::path::absolute(config_path)?;
            let exe = std::env::current_exe()?;
            let name = unit_name(config_path);
            let path = unit_path(&name);
            let log = cfg.log_path();
            let log_esc = log.to_string_lossy().replace('%', "%%");
            let unit = format!(
                "[Unit]\nDescription=rustsync file sync ({})\n\n[Service]\nExecStart={} -c {}\nRestart=on-failure\nRestartSec=5\nStandardOutput=append:{}\nStandardError=append:{}\n\n[Install]\nWantedBy=default.target\n",
                abs.display().to_string().replace(['\n', '%'], " "),
                unit_quote(&exe.to_string_lossy())?,
                unit_quote(&abs.to_string_lossy())?,
                log_esc,
                log_esc,
            );
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, unit).with_context(|| format!("cannot write {}", path.display()))?;
            println!("Wrote {}", path.display());

            // A copy started with `start` would fight the service for the socket: stop it first.
            stop_running(config_path, cfg).await;
            if !systemctl(&["daemon-reload"])? || !systemctl(&["enable", "--now", &name])? {
                bail!("systemctl could not enable {}; see the messages above", name);
            }
            if wait_socket(cfg, true, 100).await {
                println!("rustsync is running and will start by itself when you log in.");
            } else {
                println!(
                    "The service is enabled but rustsync has not answered yet; check:  systemctl --user status {}",
                    name
                );
            }

            if install_tray_autostart(config_path) {
                println!(
                    "The tray icon will appear when you log in to the desktop ({}).",
                    tray_autostart_path().display()
                );
            }

            if !linger_enabled() {
                let want = yes
                    || (std::io::stdin().is_terminal()
                        && confirm("Also start it when the machine boots, before you log in?", true)?);
                if want {
                    let ok = std::process::Command::new("loginctl")
                        .arg("enable-linger")
                        .stdin(std::process::Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if ok {
                        println!("rustsync will also start at boot.");
                    } else {
                        println!("Could not enable that. Ask an administrator to run:  loginctl enable-linger $USER");
                    }
                } else {
                    println!("It starts when you log in. For starting at boot instead:  loginctl enable-linger $USER");
                }
            }
            Ok(())
        }

        pub async fn uninstall(config_path: &Path, cfg: &Config) -> Result<()> {
            let name = unit_name(config_path);
            let path = unit_path(&name);
            if !path.exists() {
                println!("rustsync is not set to start by itself.");
                return Ok(());
            }
            if supported() {
                let _ = systemctl(&["disable", "--now", &name]);
            }
            std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            if supported() {
                let _ = systemctl(&["daemon-reload"]);
            }
            wait_socket(cfg, false, 50).await;
            println!(
                "rustsync no longer starts by itself, and has been stopped. Start it with:  rustsync -c {} start",
                config_path.display()
            );
            Ok(())
        }
    }

    // ---- macOS: a launchd agent (runs when you log in)

    #[cfg(target_os = "macos")]
    mod svc {
        use super::*;

        pub const OFFER: &str = "Start rustsync automatically when you log in?";

        fn label(config_path: &Path) -> String {
            match service_slug(config_path) {
                None => "com.rustsync.daemon".to_string(),
                Some(slug) => format!("com.rustsync.daemon.{}", slug),
            }
        }

        fn plist_path(label: &str) -> PathBuf {
            let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
            home.join("Library").join("LaunchAgents").join(format!("{}.plist", label))
        }

        fn domain() -> String {
            // SAFETY: getuid cannot fail.
            format!("gui/{}", unsafe { libc::getuid() })
        }

        fn launchctl(args: &[&str]) -> bool {
            std::process::Command::new("launchctl")
                .args(args)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        pub fn supported() -> bool {
            std::process::Command::new("launchctl")
                .arg("version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        }

        pub fn installed(config_path: &Path) -> bool {
            plist_path(&label(config_path)).exists()
        }

        fn xml(s: &str) -> String {
            s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
        }

        pub async fn start(config_path: &Path, cfg: &Config) -> Result<bool> {
            if !installed(config_path) {
                return Ok(false);
            }
            let label = label(config_path);
            let target = format!("{}/{}", domain(), label);
            // Loaded already? Kick it. Otherwise load it first.
            if !launchctl(&["kickstart", &target]) {
                launchctl(&["bootstrap", &domain(), &plist_path(&label).to_string_lossy()]);
                launchctl(&["kickstart", &target]);
            }
            if wait_socket(cfg, true, 100).await {
                println!("rustsync started (launch agent {}).", label);
                return Ok(true);
            }
            bail!("the launch agent {} did not start; see {}", label, cfg.log_path().display())
        }

        pub async fn install(config_path: &Path, cfg: &Config, _yes: bool) -> Result<()> {
            if !supported() {
                bail!("launchctl is not available");
            }
            cfg.ensure_dir()?;
            let abs = std::path::absolute(config_path)?;
            let exe = std::env::current_exe()?;
            let label = label(config_path);
            let path = plist_path(&label);
            let log = xml(&cfg.log_path().to_string_lossy());
            let plist = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n  <key>Label</key><string>{}</string>\n  <key>ProgramArguments</key>\n  <array><string>{}</string><string>-c</string><string>{}</string></array>\n  <key>RunAtLoad</key><true/>\n  <key>KeepAlive</key><dict><key>SuccessfulExit</key><false/></dict>\n  <key>ProcessType</key><string>Background</string>\n  <key>StandardOutPath</key><string>{}</string>\n  <key>StandardErrorPath</key><string>{}</string>\n</dict>\n</plist>\n",
                xml(&label),
                xml(&exe.to_string_lossy()),
                xml(&abs.to_string_lossy()),
                log,
                log,
            );
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, plist).with_context(|| format!("cannot write {}", path.display()))?;
            println!("Wrote {}", path.display());

            stop_running(config_path, cfg).await;
            let target = format!("{}/{}", domain(), label);
            launchctl(&["bootout", &target]); // forget an older copy, if any (fails when none)
            if !launchctl(&["bootstrap", &domain(), &path.to_string_lossy()]) {
                bail!("launchctl could not load {}; try:  launchctl bootstrap {} {}", label, domain(), path.display());
            }
            if wait_socket(cfg, true, 100).await {
                println!("rustsync is running and will start by itself when you log in.");
            } else {
                println!("The launch agent is loaded but rustsync has not answered yet; see {}", cfg.log_path().display());
            }
            Ok(())
        }

        pub async fn uninstall(config_path: &Path, cfg: &Config) -> Result<()> {
            let label = label(config_path);
            let path = plist_path(&label);
            if !path.exists() {
                println!("rustsync is not set to start by itself.");
                return Ok(());
            }
            launchctl(&["bootout", &format!("{}/{}", domain(), label)]);
            std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            wait_socket(cfg, false, 50).await;
            println!(
                "rustsync no longer starts by itself, and has been stopped. Start it with:  rustsync -c {} start",
                config_path.display()
            );
            Ok(())
        }
    }

    // ---- FreeBSD, OpenBSD, NetBSD, ...: start at login to the desktop (an XDG autostart entry)

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    mod svc {
        use super::*;

        pub const OFFER: &str = "Start rustsync automatically when you log in to the desktop?";

        fn entry_path(config_path: &Path) -> PathBuf {
            let name = match service_slug(config_path) {
                None => "rustsync-daemon.desktop".to_string(),
                Some(slug) => format!("rustsync-daemon-{}.desktop", slug),
            };
            config_home().join("autostart").join(name)
        }

        pub fn supported() -> bool {
            std::env::var("HOME").map(|h| !h.is_empty()).unwrap_or(false)
        }

        pub fn installed(config_path: &Path) -> bool {
            entry_path(config_path).exists()
        }

        /// There is no service manager to ask: `start` just starts it (Ok(false)).
        pub async fn start(_config_path: &Path, _cfg: &Config) -> Result<bool> {
            Ok(false)
        }

        pub async fn install(config_path: &Path, cfg: &Config, _yes: bool) -> Result<()> {
            cfg.ensure_dir()?;
            let abs = std::path::absolute(config_path)?;
            let exe = std::env::current_exe()?;
            let (Some(e), Some(c)) = (desktop_quote(&exe.to_string_lossy()), desktop_quote(&abs.to_string_lossy())) else {
                bail!("the program or config path has characters that cannot go in a login entry");
            };
            let path = entry_path(config_path);
            let text = format!(
                "[Desktop Entry]\nType=Application\nName=rustsync service\nComment=Starts the rustsync background service\nExec={} -c {} start\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
                e, c
            );
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, text).with_context(|| format!("cannot write {}", path.display()))?;
            println!("Wrote {}", path.display());
            println!("rustsync will start when you log in to the desktop. (On a server with no desktop, start it from an rc script instead; see the README.)");
            if install_tray_autostart(config_path) {
                println!("The tray icon will appear then too ({}).", tray_autostart_path().display());
            }
            if UnixStream::connect(cfg.socket_path()).await.is_err() {
                spawn_daemon(config_path, cfg).await?;
            }
            Ok(())
        }

        pub async fn uninstall(config_path: &Path, _cfg: &Config) -> Result<()> {
            let path = entry_path(config_path);
            if !path.exists() {
                println!("rustsync is not set to start by itself.");
                return Ok(());
            }
            std::fs::remove_file(&path).with_context(|| format!("cannot remove {}", path.display()))?;
            println!(
                "rustsync no longer starts at login. It is still running now; stop it with:  rustsync -c {} stop",
                config_path.display()
            );
            Ok(())
        }
    }

    async fn install_service(config_path: &Path, cfg: &Config, yes: bool) -> Result<()> {
        svc::install(config_path, cfg, yes).await
    }

    async fn uninstall_service(config_path: &Path, cfg: &Config) -> Result<()> {
        svc::uninstall(config_path, cfg).await
    }

    async fn start(config_path: &Path, cfg: &Config) -> Result<()> {
        if UnixStream::connect(cfg.socket_path()).await.is_ok() {
            println!("rustsync is already running.");
            return Ok(());
        }
        cfg.ensure_dir()?;

        // Set up to start by itself: use that rather than a second copy.
        if svc::start(config_path, cfg).await? {
            return Ok(());
        }
        // First time at a terminal: offer to start by itself in the future.
        let marker = cfg.dir.join("service-offered");
        if std::io::stdin().is_terminal() && !marker.exists() && !svc::installed(config_path) && svc::supported() {
            let _ = std::fs::write(&marker, "asked once; rustsync install-service does it later\n");
            if confirm(svc::OFFER, true)? {
                return install_service(config_path, cfg, false).await;
            }
        }
        spawn_daemon(config_path, cfg).await
    }

    // ---------------------------------------------------------------- commands

    pub async fn run(config_path: &Path, cmd: Cmd) -> Result<()> {
        // `start` also works on a brand-new machine (and from the GUI): it creates the config.
        if matches!(cmd, Cmd::Start | Cmd::InstallService { .. }) {
            ensure_config(config_path, None)?;
        }
        if !config_path.exists() {
            bail!(
                "{} does not exist yet. Run rustsync once to create it:  rustsync -c {}",
                config_path.display(),
                config_path.display()
            );
        }
        let cfg = config::load(config_path)?;
        let ctl = Ctl { sock: cfg.socket_path(), config: config_path.to_path_buf() };

        match cmd {
            Cmd::Start => start(config_path, &cfg).await,
            Cmd::InstallService { yes } => install_service(config_path, &cfg, yes).await,
            Cmd::UninstallService => uninstall_service(config_path, &cfg).await,
            Cmd::Stop => {
                call(&ctl, &Request::Stop).await?;
                Ok(())
            }
            Cmd::Status => {
                let info = status(&ctl).await?;
                print_status(config_path, &info);
                Ok(())
            }
            Cmd::Invite { host } => {
                let host = match host {
                    Some(h) => h,
                    None => ask(
                        "Address the other machine will use to reach this one (host or host:port)",
                        &config::hostname(),
                    )?,
                };
                call(&ctl, &Request::Invite { host }).await?;
                Ok(())
            }
            Cmd::Join { code } => {
                let token = match code {
                    Some(c) => c,
                    None => ask("Paste the code shown by `invite` on the other machine", "")?,
                };
                if token.trim().is_empty() {
                    bail!("no code given");
                }
                call(&ctl, &Request::Join { token: token.trim().to_string() }).await?;
                eprintln!("Done. Next, share a library:  rustsync -c {} share   (or accept one the other machine shares)", config_path.display());
                Ok(())
            }
            Cmd::Forget { device, yes } => {
                let info = status(&ctl).await?;
                let labels: Vec<String> = info
                    .devices
                    .iter()
                    .map(|d| format!("{} ({}…)", d.name, &d.key[..8.min(d.key.len())]))
                    .collect();
                let devices = &info.devices;
                let i = pick("device", &labels, device, |i, t| name_or_prefix(&devices[i].name, &devices[i].key, t))?;
                if !yes && !confirm(&format!("Stop syncing with {}?", labels[i]), false)? {
                    println!("Cancelled.");
                    return Ok(());
                }
                call(&ctl, &Request::Forget { device: info.devices[i].key.clone() }).await?;
                Ok(())
            }
            Cmd::Create { name, folder } => {
                let name = match name {
                    Some(n) => n,
                    None => ask("Library name", "")?,
                };
                if name.trim().is_empty() {
                    bail!("a library needs a name");
                }
                let folder = match folder {
                    Some(f) => f,
                    None => ask(&format!("Folder to sync for \"{}\"", name.trim()), &format!("./{}", name.trim()))?,
                };
                let folder = config::abs_path(Path::new(&folder)).to_string_lossy().to_string();
                call(&ctl, &Request::Create { name, folder }).await?;
                eprintln!("Share it with another machine:  rustsync -c {} share", config_path.display());
                Ok(())
            }
            Cmd::Remove { library, yes } => {
                let info = status(&ctl).await?;
                let labels: Vec<String> = info.libraries.iter().map(|l| format!("{}  ({})", l.name, l.folder)).collect();
                let libs = &info.libraries;
                let i = pick("library", &labels, library, |i, t| name_or_prefix(&libs[i].name, &libs[i].id, t))?;
                if !yes && !confirm(&format!("Remove {} from this machine (its files are NOT deleted)?", labels[i]), false)? {
                    println!("Cancelled.");
                    return Ok(());
                }
                call(&ctl, &Request::RemoveLibrary { library: info.libraries[i].id.clone() }).await?;
                Ok(())
            }
            Cmd::Share { library, device, backup } => {
                let info = status(&ctl).await?;
                let interactive = device.is_none() && std::io::stdin().is_terminal();
                let libs: Vec<proto::LibraryInfo> = info.libraries.iter().filter(|l| !l.backup_only).cloned().collect();
                let lib_labels: Vec<String> = libs.iter().map(|l| format!("{}  ({})", l.name, l.folder)).collect();
                let li = pick("library", &lib_labels, library, |i, t| name_or_prefix(&libs[i].name, &libs[i].id, t))?;
                let dev_labels: Vec<String> = info.devices.iter().map(|d| d.name.clone()).collect();
                let devs = &info.devices;
                let di = pick("device", &dev_labels, device, |i, t| name_or_prefix(&devs[i].name, &devs[i].key, t))?;
                let mut backup = backup;
                if interactive && !backup {
                    backup = !confirm(
                        &format!(
                            "Sync the files with {} as normal? (No = it only keeps an encrypted backup it can't read)",
                            info.devices[di].name
                        ),
                        true,
                    )?;
                }
                call(
                    &ctl,
                    &Request::Share {
                        library: libs[li].id.clone(),
                        device: info.devices[di].key.clone(),
                        backup,
                    },
                )
                .await?;
                Ok(())
            }
            Cmd::Accept { offer, folder, quota } => {
                let mut info = status(&ctl).await?;
                if info.offers.is_empty() && offer.is_some() {
                    // The offer may still be on its way.
                    for _ in 0..20 {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        info = status(&ctl).await?;
                        if !info.offers.is_empty() {
                            break;
                        }
                    }
                }
                if info.offers.is_empty() {
                    println!("No libraries are waiting for you.");
                    println!("Ask the other machine to run:  rustsync -c <its config> share");
                    return Ok(());
                }
                let labels: Vec<String> = info.offers.iter().map(offer_label).collect();
                let offers = &info.offers;
                let i = pick("offer", &labels, offer, |i, t| {
                    name_or_prefix(&offers[i].name, &offers[i].id, t) || (offers[i].backup && t.eq_ignore_ascii_case("backup"))
                })?;
                let o = info.offers[i].clone();
                if o.again {
                    // A device this machine stopped serving asks to use a backup it already keeps.
                    call(&ctl, &Request::Accept { offer: o.id, folder: String::new(), quota: 0 }).await?;
                    return Ok(());
                }
                let folder = match folder {
                    Some(f) => f,
                    None if o.backup => {
                        eprintln!("{} wants to keep an encrypted backup here. You choose where on this machine it is stored", o.from_name);
                        eprintln!("(for example on a bigger drive); nothing in it can be read here.");
                        let default = Path::new(&info.backup_dir).join(&o.name).to_string_lossy().to_string();
                        ask("Folder for the encrypted backup", &default)?
                    }
                    None => ask(&format!("Folder to sync \"{}\" into", o.name), &format!("./{}", o.name))?,
                };
                let folder = config::abs_path(Path::new(&folder)).to_string_lossy().to_string();
                let quota = if o.backup {
                    let text = match quota {
                        Some(q) => q,
                        None => ask("Size limit for this backup (e.g. 500G, 2T; empty = no limit)", "")?,
                    };
                    proto::parse_size(&text).map_err(|e| anyhow!(e))?
                } else {
                    0
                };
                call(&ctl, &Request::Accept { offer: o.id, folder, quota }).await?;
                Ok(())
            }
            Cmd::Decline { offer } => {
                let info = status(&ctl).await?;
                if info.offers.is_empty() {
                    println!("No libraries are waiting for you.");
                    return Ok(());
                }
                let labels: Vec<String> = info.offers.iter().map(offer_label).collect();
                let offers = &info.offers;
                let i = pick("offer", &labels, offer, |i, t| name_or_prefix(&offers[i].name, &offers[i].id, t))?;
                call(&ctl, &Request::Decline { offer: info.offers[i].id.clone() }).await?;
                Ok(())
            }
            Cmd::Refresh => {
                call(&ctl, &Request::Refresh).await?;
                Ok(())
            }
            Cmd::Unshare { library, device } => {
                let info = status(&ctl).await?;
                if info.libraries.is_empty() {
                    println!("No libraries.");
                    return Ok(());
                }
                let labels: Vec<String> = info
                    .libraries
                    .iter()
                    .map(|l| format!("{}  (id {})", l.name, &l.id[..8.min(l.id.len())]))
                    .collect();
                let libs = &info.libraries;
                let li = pick("library", &labels, library, |i, t| name_or_prefix(&libs[i].name, &libs[i].id, t))?;
                let l = &info.libraries[li];
                // Devices this library is shared with (by name, as `status` shows them).
                let related: Vec<&proto::DeviceInfo> = info
                    .devices
                    .iter()
                    .filter(|d| {
                        l.syncing_with.contains(&d.name)
                            || l.backed_up_on.contains(&d.name)
                            || l.kept_for.contains(&d.name)
                            || l.pending.iter().any(|p| *p == d.name || p.starts_with(&format!("{} (", d.name)))
                    })
                    .collect();
                if related.is_empty() {
                    println!("\"{}\" is not shared with any device.", l.name);
                    return Ok(());
                }
                let dev_labels: Vec<String> = related.iter().map(|d| d.name.clone()).collect();
                let di = pick("device", &dev_labels, device, |i, t| name_or_prefix(&related[i].name, &related[i].key, t))?;
                call(&ctl, &Request::Unshare { library: l.id.clone(), device: related[di].key.clone() }).await?;
                Ok(())
            }
            Cmd::Quota { library, size } => {
                let info = status(&ctl).await?;
                let backups: Vec<proto::LibraryInfo> = info.libraries.iter().filter(|l| l.backup_only).cloned().collect();
                if backups.is_empty() {
                    println!("No encrypted backups are kept on this machine.");
                    return Ok(());
                }
                let labels: Vec<String> = backups
                    .iter()
                    .map(|l| format!("{} (id {}): {}", l.name, &l.id[..8.min(l.id.len())], usage_line(l.used, l.quota, false)))
                    .collect();
                let i = pick("backup", &labels, library, |i, t| {
                    name_or_prefix(&backups[i].name, &backups[i].id, t) || t.eq_ignore_ascii_case("backup")
                })?;
                match size {
                    None => {
                        println!("{}", usage_line(backups[i].used, backups[i].quota, true));
                        Ok(())
                    }
                    Some(text) => {
                        let bytes = proto::parse_size(&text).map_err(|e| anyhow!(e))?;
                        call(&ctl, &Request::SetQuota { library: backups[i].id.clone(), bytes }).await?;
                        Ok(())
                    }
                }
            }
            Cmd::Prune { library } => {
                let info = status(&ctl).await?;
                let libs: Vec<proto::LibraryInfo> =
                    info.libraries.iter().filter(|l| !l.backup_only && !l.backups.is_empty()).cloned().collect();
                if libs.is_empty() {
                    println!("None of your libraries has an encrypted backup to clean up (run this on a machine that has the files).");
                    return Ok(());
                }
                let labels: Vec<String> = libs.iter().map(|l| format!("{}  ({})", l.name, l.folder)).collect();
                let i = pick("library", &labels, library, |i, t| name_or_prefix(&libs[i].name, &libs[i].id, t))?;
                call(&ctl, &Request::Prune { library: libs[i].id.clone() }).await?;
                Ok(())
            }
            Cmd::Recovery { library } => {
                eprintln!("These codes (with a copy of the encrypted backup) are enough to read your files.");
                eprintln!("Store them offline, e.g. in a password manager. Do not paste them into chats or logs.\n");
                call(&ctl, &Request::Recovery { library }).await?;
                eprintln!("\nTo use the kit or a code after losing every machine: pair the new machine with the one");
                eprintln!("that keeps the backup (the device itself is not part of the kit), then:");
                eprintln!("  rustsync -c {} restore", config_path.display());
                Ok(())
            }
            Cmd::Restore { code, folder, from } => {
                let info = status(&ctl).await?;
                if info.devices.is_empty() {
                    bail!("pair this machine with the one that keeps the backup first:  rustsync -c {} join", config_path.display());
                }
                let code = match code {
                    Some(c) => c,
                    None => ask("Recovery kit or code (starts with rsk1- or rsr1-)", "")?,
                };
                if code.trim().is_empty() {
                    bail!("no recovery code given");
                }
                let folder = match folder {
                    Some(f) => f,
                    None => ask("Empty folder to restore into (a kit makes one subfolder per library)", "./restored")?,
                };
                let folder = config::abs_path(Path::new(&folder)).to_string_lossy().to_string();
                let labels: Vec<String> = info.devices.iter().map(|d| d.name.clone()).collect();
                let devs = &info.devices;
                let di = pick("device that keeps the backup", &labels, from, |i, t| name_or_prefix(&devs[i].name, &devs[i].key, t))?;
                call(&ctl, &Request::Restore { code: code.trim().to_string(), folder, device: info.devices[di].key.clone() }).await?;
                Ok(())
            }
        }
    }
}
