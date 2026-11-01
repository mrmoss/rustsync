//! rustsync: simple end-to-end encrypted file sync.
//!
//! A background daemon (`rustsync`) keeps your libraries in sync; commands such as `invite`,
//! `join`, `create`, `share` and `accept` talk to it. See `rustsync --help`.
//!
//! The program is a library with a thin command-line wrapper (`src/main.rs`), so the same core
//! also runs inside the Android app (`src/android.rs`).

use clap::{Parser, Subcommand};
use std::path::PathBuf;
use tracing::info;

#[derive(Parser)]
#[command(
    version,
    about = "Simple end-to-end encrypted file sync",
    after_help = "Run with no command to start the daemon in the foreground (or `start` for the background).\nUse -c <file> to pick a config file (default: ~/.config/rustsync/config.toml)."
)]
pub struct Args {
    /// Path to the TOML config file
    #[arg(short = 'c', long = "config", global = true)]
    pub config: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Option<Cmd>,

    /// Print this device's public key (creating the key pair if needed) and exit
    #[arg(long)]
    pub show_key: bool,

    /// When the config file does not exist yet: port to listen on
    #[arg(long)]
    pub port: Option<u16>,
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
        /// Do not draw the code as a QR code
        #[arg(long)]
        no_qr: bool,
        /// Your terminal has a light background (the QR code is drawn for dark ones by default)
        #[arg(long)]
        light_terminal: bool,
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
    /// Change the address used to reach a paired device (after its IP address changed)
    SetAddress {
        /// Number or name of the device (listed if omitted)
        device: Option<String>,
        /// New address, `host` or `host:port` (asked if omitted; the current one is the default)
        address: Option<String>,
    },
    /// Change the port this machine listens on (saved in the config file)
    SetPort {
        /// The new port (asked if omitted)
        port: Option<u16>,
    },
    /// Show the activity log (what rustsync did recently), optionally for one device or library
    Log {
        /// Only lines about this device
        #[arg(short, long)]
        device: Option<String>,
        /// Only lines about this library
        #[arg(short, long)]
        library: Option<String>,
        /// How many of the newest lines to show
        #[arg(short = 'n', long, default_value_t = 100)]
        lines: usize,
        /// Print where the log files are, and how big they may get, instead
        #[arg(long)]
        path: bool,
    },
    /// Change how big the logs may get (saved in the config file), e.g. 5MB
    SetLogSize {
        /// The new limit (asked if omitted)
        size: Option<String>,
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

pub mod activity;
pub mod config;
pub mod state;
pub mod net;
pub mod index;
pub mod apply;
pub mod vault;
pub mod depot;
pub mod proto;
pub mod sync;
pub mod cli;
#[cfg(target_os = "android")]
pub mod android;

/// Everything the `rustsync` program does, given its parsed command line.
pub async fn run(args: Args) -> anyhow::Result<()> {
    // Configs from versions before 0.6 were YAML: convert them once.
    let config_path = config::resolve(&args.config.clone().unwrap_or_else(config::default_config_path))?;

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
    activity::init(&cfg, level, true);

    // Printed regardless of log level so it is easy to copy.
    println!("rustsync: {}", cfg.device_name());
    println!("Public key: {}", my_key);
    println!("Pair another machine:  rustsync -c {} invite", config_path.display());
    println!();
    info!("state directory: {}", cfg.dir.display());
    sync::run_daemon(cfg, keys).await
}

