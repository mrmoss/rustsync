//! A small desktop front end for rustsync.
//!
//! It does nothing the command line can't do: it talks to the running rustsync service over
//! the same local control socket (JSON lines) that `rustsync status | invite | join | create |
//! share | accept ...` use. Close this window and the service keeps syncing.
//!
//! Usage:  rustsync-gui [-c /path/to/config.toml] [--tray | --no-tray]
//!   --tray     only the tray icon (the menu bar icon on macOS), no window (start this at login;
//!              the icon opens the window)
//!   --no-tray  window only; do not start the tray icon
//! (default config: ~/.config/rustsync/config.toml, like the command line)

// Some helpers (the tray icon, the XDG launcher) are only used on some systems.
#![allow(dead_code)]

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- protocol
// Mirrors `proto.rs` in the rustsync program (unknown fields are ignored, so adding fields
// there does not break this program). Variant and field names must stay the same.

#[derive(Serialize, Debug, Clone)]
enum Request {
    Status,
    Invite { host: String },
    Join { token: String },
    PairDecision { accept: bool },
    SetPort { port: u16 },
    SetName { name: String },
    Rename { device: String, nickname: String },
    Log { device: Option<String>, library: Option<String>, lines: usize },
    SetLogSize { bytes: u64 },
    Forget { device: String },
    SetAddress { device: String, addr: String },
    Create { name: String, folder: String },
    RemoveLibrary { library: String },
    Share { library: String, device: String, backup: bool },
    Accept { offer: String, folder: String, quota: u64 },
    SetQuota { library: String, bytes: u64 },
    Prune { library: String },
    Unshare { library: String, device: String },
    Refresh,
    Decline { offer: String },
    Recovery { library: Option<String> },
    Restore { code: String, folder: String, device: String },
    Stop,
}

#[derive(Deserialize, Debug)]
enum Reply {
    Info(StatusInfo),
    Out(String),
    Note(String),
    /// Pairing: compare `code` with the other device and answer with `PairDecision`.
    Verify { name: String, fingerprint: String, code: String, joining: bool },
    Done,
    Error(String),
}

#[derive(Deserialize, Debug, Clone, Default)]
struct StatusInfo {
    name: String,
    key: String,
    listen: String,
    #[serde(default)]
    devices: Vec<DeviceInfo>,
    #[serde(default)]
    libraries: Vec<LibraryInfo>,
    #[serde(default)]
    offers: Vec<OfferInfo>,
    /// Suggested folder for encrypted backups kept on this machine.
    #[serde(default)]
    backup_dir: String,
    #[serde(default)]
    port: u16,
    /// This machine's IPv4 addresses: (address, interface).
    #[serde(default)]
    addresses: Vec<(String, String)>,
    #[serde(default)]
    dns_names: Vec<String>,
    #[serde(default)]
    log_max: u64,
    #[serde(default)]
    log_file: String,
}

#[derive(Deserialize, Debug, Clone)]
struct DeviceInfo {
    key: String,
    /// What to show: our nickname for it, else the name its owner set.
    name: String,
    /// The name its owner set.
    #[serde(default)]
    announced: String,
    /// Our own nickname for it (empty if none).
    #[serde(default)]
    nickname: String,
    online: bool,
    addr: Option<String>,
    #[serde(default)]
    last_seen: u64,
    /// Bytes per second to and from it right now.
    #[serde(default)]
    up_rate: u64,
    #[serde(default)]
    down_rate: u64,
}

/// How a library is syncing with one device.
#[derive(Deserialize, Debug, Clone)]
struct SyncInfo {
    #[serde(default)]
    key: String,
    device: String,
    online: bool,
    /// Still to be fetched from that device (files, bytes) ...
    down_files: u64,
    down_bytes: u64,
    /// ... and still to be fetched by that device from us, as it tells us.
    up_files: u64,
    up_bytes: u64,
}

impl SyncInfo {
    fn busy(&self) -> bool {
        self.down_files > 0 || self.down_bytes > 0 || self.up_files > 0 || self.up_bytes > 0
    }
}

#[derive(Deserialize, Debug, Clone)]
struct LibraryInfo {
    id: String,
    name: String,
    folder: String,
    /// This machine only keeps an encrypted copy (it can't read the files).
    #[serde(default)]
    backup_only: bool,
    #[serde(default)]
    kept_for: Vec<String>,
    #[serde(default)]
    syncing_with: Vec<String>,
    #[serde(default)]
    sync: Vec<SyncInfo>,
    #[serde(default)]
    backed_up_on: Vec<String>,
    #[serde(default)]
    pending: Vec<String>,
    /// (backup_only) bytes stored here and the size limit (0 = none).
    #[serde(default)]
    used: u64,
    #[serde(default)]
    quota: u64,
    /// Our encrypted backups on other devices, as they last reported.
    #[serde(default)]
    backups: Vec<BackupUsage>,
}

#[derive(Deserialize, Debug, Clone)]
struct BackupUsage {
    device: String,
    #[serde(default)]
    known: bool,
    #[serde(default)]
    used: u64,
    #[serde(default)]
    quota: u64,
}

/// "500G", "1.5 TB" -> bytes (decimal units, same as the command line). Empty/"none" = 0.
fn parse_size(s: &str) -> Result<u64, String> {
    let t = s.trim().to_lowercase().replace(' ', "");
    if t.is_empty() || t == "none" || t == "unlimited" || t == "0" {
        return Ok(0);
    }
    let split = t.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: f64 = num.parse().map_err(|_| "Enter a size like 500G or 2T".to_string())?;
    let mult: f64 = match unit {
        "" | "b" => 1.0,
        "k" | "kb" => 1e3,
        "m" | "mb" => 1e6,
        "g" | "gb" => 1e9,
        "t" | "tb" => 1e12,
        "p" | "pb" => 1e15,
        _ => return Err("Unknown unit: use K, M, G or T".to_string()),
    };
    let bytes = n * mult;
    if !bytes.is_finite() || bytes < 0.0 || bytes > 1e18 {
        return Err("That size is out of range".to_string());
    }
    Ok(bytes as u64)
}

fn fmt_size(b: u64) -> String {
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

#[derive(Deserialize, Debug, Clone)]
struct OfferInfo {
    id: String,
    name: String,
    from_name: String,
    /// An offer to keep an encrypted backup here (rather than to sync).
    #[serde(default)]
    backup: bool,
    /// A device we stopped serving asks to use a backup we already keep (no folder needed).
    #[serde(default)]
    again: bool,
}

/// (name, key) of the paired devices a library is shared with, as `status` lists them.
fn related_devices(l: &LibraryInfo, devices: &[DeviceInfo]) -> Vec<(String, String)> {
    devices
        .iter()
        .filter(|d| {
            l.syncing_with.contains(&d.name)
                || l.backed_up_on.contains(&d.name)
                || l.kept_for.contains(&d.name)
                || l.pending.iter().any(|p| *p == d.name || p.starts_with(&format!("{} (", d.name)))
        })
        .map(|d| (d.name.clone(), d.key.clone()))
        .collect()
}

// ---------------------------------------------------------------- talking to the service

/// Send one request and hand every answer line to `on_reply` until the service says Done.
/// `cancel` (optional) receives a handle that can be shut down to cancel a long request
/// (an `Invite` waiting for the other machine).
fn call(
    sock: &Path,
    req: &Request,
    cancel: Option<&Arc<Mutex<Option<UnixStream>>>>,
    mut on_reply: impl FnMut(Reply),
) -> Result<(), String> {
    let stream = UnixStream::connect(sock).map_err(|_| "RustSync is not running".to_string())?;
    if let Some(slot) = cancel {
        if let Ok(handle) = stream.try_clone() {
            *slot.lock().unwrap() = Some(handle);
        }
    }
    let mut w = stream.try_clone().map_err(|e| e.to_string())?;
    let line = serde_json::to_string(req).map_err(|e| e.to_string())?;
    w.write_all(line.as_bytes())
        .and_then(|_| w.write_all(b"\n"))
        .map_err(|e| e.to_string())?;
    for line in BufReader::new(stream).lines() {
        let line = line.map_err(|e| e.to_string())?;
        match serde_json::from_str::<Reply>(&line) {
            Ok(Reply::Done) => return Ok(()),
            Ok(Reply::Error(e)) => return Err(e),
            Ok(other) => on_reply(other),
            Err(e) => return Err(format!("unexpected answer from RustSync: {}", e)),
        }
    }
    Ok(())
}

fn fetch_status(sock: &Path) -> Result<StatusInfo, String> {
    let mut info = None;
    call(sock, &Request::Status, None, |r| {
        if let Reply::Info(i) = r {
            info = Some(i);
        }
    })?;
    info.ok_or_else(|| "RustSync did not answer".to_string())
}

// ---------------------------------------------------------------- finding things

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

fn default_config() -> PathBuf {
    home().join(".config").join("rustsync").join("config.toml")
}

/// None for the default config, otherwise a short name made from its path (the same rule as
/// `rustsync install-service`, so that each config can have its own service).
fn service_slug(config: &Path) -> Option<String> {
    let abs = absolute(config);
    if abs == default_config() {
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

/// `~/.config` (or $XDG_CONFIG_HOME).
#[cfg(not(target_os = "macos"))]
fn config_home() -> PathBuf {
    match std::env::var("XDG_CONFIG_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".config"),
    }
}

/// Is rustsync set to start by itself? (A systemd user service on Linux, a launch agent on macOS,
/// a login entry on the BSDs.)
#[cfg(target_os = "linux")]
fn service_installed(config: &Path) -> bool {
    let name = match service_slug(config) {
        None => "rustsync.service".to_string(),
        Some(slug) => format!("rustsync-{}.service", slug),
    };
    config_home().join("systemd").join("user").join(name).exists()
}

#[cfg(target_os = "macos")]
fn service_installed(config: &Path) -> bool {
    let label = match service_slug(config) {
        None => "com.rustsync.daemon".to_string(),
        Some(slug) => format!("com.rustsync.daemon.{}", slug),
    };
    home().join("Library").join("LaunchAgents").join(format!("{}.plist", label)).exists()
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn service_installed(config: &Path) -> bool {
    let name = match service_slug(config) {
        None => "rustsync-daemon.desktop".to_string(),
        Some(slug) => format!("rustsync-daemon-{}.desktop", slug),
    };
    config_home().join("autostart").join(name).exists()
}

/// The wording of the "start automatically" switch on this system.
#[cfg(target_os = "linux")]
const AUTOSTART_LABEL: &str = "Start RustSync automatically (when you log in, or at boot)";
#[cfg(not(target_os = "linux"))]
const AUTOSTART_LABEL: &str = "Start RustSync automatically (when you log in)";

/// Turn "start by itself" on or off through the `rustsync` program.
fn set_service_blocking(config: &Path, on: bool) -> Result<(), String> {
    let bin = rustsync_bin();
    let mut cmd = Command::new(&bin);
    cmd.arg("-c").arg(config);
    if on {
        cmd.arg("install-service").arg("--yes");
    } else {
        cmd.arg("uninstall-service");
    }
    let out = cmd
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {} ({})", bin.display(), e))?;
    if out.status.success() {
        Ok(())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("it failed");
        Err(last.trim().to_string())
    }
}

fn absolute(p: &Path) -> PathBuf {
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().map(|d| d.join(p)).unwrap_or_else(|_| p.to_path_buf())
    }
}

/// Where the service keeps its files (and its control socket): the `state_dir` from the config
/// file, otherwise `<config path without extension>.state`, exactly like the service.
fn state_dir(config: &Path) -> PathBuf {
    if let Ok(text) = std::fs::read_to_string(config) {
        if let Ok(v) = text.parse::<toml::Table>() {
            if let Some(d) = v.get("state_dir").and_then(|d| d.as_str()) {
                return absolute(Path::new(d));
            }
        }
    }
    let mut s = absolute(config).with_extension("").into_os_string();
    s.push(".state");
    PathBuf::from(s)
}

/// The rustsync program: $RUSTSYNC_BIN, else next to this program, else on the PATH.
fn rustsync_bin() -> PathBuf {
    if let Ok(b) = std::env::var("RUSTSYNC_BIN") {
        return PathBuf::from(b);
    }
    if let Ok(me) = std::env::current_exe() {
        if let Some(dir) = me.parent() {
            let next_to_me = dir.join("rustsync");
            if next_to_me.exists() {
                return next_to_me;
            }
        }
    }
    PathBuf::from("rustsync")
}

/// `~/x` -> `/home/me/x`
fn expand(path: &str) -> String {
    let p = path.trim();
    if p == "~" {
        return home().to_string_lossy().to_string();
    }
    if let Some(rest) = p.strip_prefix("~/") {
        return home().join(rest).to_string_lossy().to_string();
    }
    p.to_string()
}

/// "5 minutes ago" for a time in seconds since 1970.
fn ago_text(then: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = now.saturating_sub(then);
    let (n, unit) = match s {
        0..=59 => return "just now".to_string(),
        60..=3599 => (s / 60, "minute"),
        3600..=86_399 => (s / 3600, "hour"),
        _ => (s / 86_400, "day"),
    };
    format!("{} {}{} ago", n, unit, if n == 1 { "" } else { "s" })
}

/// Bytes as megabytes for the log size box: "2" or "0.5".
fn fmt_rate(bytes_per_sec: u64) -> String {
    format!("{}/s", fmt_size(bytes_per_sec))
}

fn files_text(n: u64) -> String {
    if n == 1 {
        "1 file".to_string()
    } else {
        format!("{} files", n)
    }
}

/// "receiving 3 files, 120 MB left; sending 1 file, 4 MB left", "up to date" or "offline".
fn sync_summary(s: &SyncInfo) -> String {
    let mut parts = Vec::new();
    if s.down_files > 0 || s.down_bytes > 0 {
        parts.push(format!("receiving {}, {} left", files_text(s.down_files), fmt_size(s.down_bytes)));
    }
    if s.up_files > 0 || s.up_bytes > 0 {
        parts.push(format!("sending {}, {} left", files_text(s.up_files), fmt_size(s.up_bytes)));
    }
    match (parts.is_empty(), s.online) {
        (false, _) => parts.join("; "),
        (true, true) => "up to date".to_string(),
        (true, false) => "offline".to_string(),
    }
}

fn fmt_mb(bytes: u64) -> String {
    let v = bytes as f64 / 1e6;
    if (v - v.round()).abs() < 1e-9 {
        format!("{}", v.round() as u64)
    } else {
        format!("{:.1}", v)
    }
}

fn short(key: &str) -> &str {
    &key[..8.min(key.len())]
}

// ---------------------------------------------------------------- app

enum Ev {
    Status(Result<StatusInfo, String>),
    Op { ok: bool, msg: String },
    InviteCode(String),
    InviteNote(String),
    InviteEnded(Result<(), String>),
    /// Pairing reached the point where the person must compare the code.
    PairVerify(Verify),
    JoinEnded(Result<(), String>),
    /// A frame from the camera while scanning a QR code, and what came of the scan.
    CameraFrame(egui::ColorImage),
    /// What the scanner is doing, shown under the picture.
    ScanDebug(String),
    Scanned(String),
    ScanFailed(String),
    /// The recovery code of one library (kept out of the status bar on purpose).
    Recovery(Result<String, String>),
    /// The newest lines of the activity log.
    Log(Result<String, String>),
}

/// What the person compares while pairing.
#[derive(Clone, Debug)]
struct Verify {
    name: String,
    fingerprint: String,
    code: String,
    /// True on the device that entered the pairing code.
    joining: bool,
}

#[derive(Default, PartialEq, Clone, Copy)]
enum Page {
    #[default]
    Home,
    Libraries,
    Devices,
    Settings,
}

#[derive(Default)]
enum Dialog {
    #[default]
    None,
    NewLibrary { name: String, folder: String },
    Share { lib_id: String, lib_name: String, device: usize, backup: bool },
    Accept { offer_id: String, name: String, folder: String, backup: bool, from: String, quota: String },
    Quota { id: String, name: String, text: String },
    Unshare { lib_id: String, lib_name: String, candidates: Vec<(String, String)>, sel: usize },
    Recovery { name: String, code: Option<String>, error: Option<String> },
    Log { title: String, device: Option<String>, library: Option<String>, text: Option<String>, error: Option<String> },
    Restore { code: String, folder: String, device: usize },
    Invite { host: String, manual: bool, code: Option<String>, note: String, error: Option<String>, running: bool, paired: bool },
    Join { code: String, running: bool, error: Option<String>, scanning: bool },
    ConfirmForget { key: String, name: String },
    Address { key: String, name: String, text: String },
    /// Rename this device (the name other devices see).
    OwnName { text: String },
    /// A nickname for a paired device, used on this machine only.
    Nickname { key: String, announced: String, text: String },
    ConfirmRemove { id: String, name: String },
}

enum Action {
    Create { name: String, folder: String },
    Share { library: String, device: usize, backup: bool },
    Accept { offer: String, folder: String, quota: u64 },
    SetQuota { id: String, bytes: u64 },
    Prune { id: String },
    Unshare { id: String, device: String },
    Refresh,
    ShowRecovery { id: Option<String>, name: String },
    ShowLog { title: String, device: Option<String>, library: Option<String> },
    SetLogSize { bytes: u64 },
    Restore { code: String, folder: String, device: usize },
    StartJoin { token: String },
    StartScan,
    StopScan,
    PairDecide { accept: bool },
    Forget { key: String },
    SetAddress { key: String, addr: String },
    SetOwnName { name: String },
    SetNickname { key: String, nickname: String },
    SetPort { port: u16 },
    Remove { id: String },
    StartInvite { host: String },
    CancelInvite,
}

struct App {
    ctx: egui::Context,
    config: PathBuf,
    sock: PathBuf,
    tx: Sender<Ev>,
    rx: Receiver<Ev>,
    status: Option<StatusInfo>,
    offline_reason: String,
    starting_since: Option<Instant>,
    page: Page,
    dialog: Dialog,
    message: Option<(bool, String)>,
    invite_cancel: Arc<Mutex<Option<UnixStream>>>,
    /// Set while the person has to compare a pairing code.
    verify: Option<Verify>,
    scan_stop: Option<Arc<AtomicBool>>,
    scan_tex: Option<egui::TextureHandle>,
    scan_debug: String,
    /// The folder browser, while one is open over a dialog.
    browse: Option<Browse>,
    logo: egui::TextureHandle,
    /// The port being typed on the Home page (None: show the current one).
    port_edit: Option<String>,
    /// The log size being typed on the Settings page (None: show the current one).
    log_edit: Option<String>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, config: PathBuf) -> App {
        let (tx, rx) = channel();
        let sock = state_dir(&config).join("control.sock");

        // Ask the service for its state every couple of seconds.
        {
            let (tx, sock, ctx) = (tx.clone(), sock.clone(), cc.egui_ctx.clone());
            std::thread::spawn(move || loop {
                if tx.send(Ev::Status(fetch_status(&sock))).is_err() {
                    break; // window closed
                }
                ctx.request_repaint();
                std::thread::sleep(Duration::from_secs(2));
            });
        }

        let logo = cc.egui_ctx.load_texture(
            "logo",
            egui::ColorImage::from_rgba_unmultiplied([128, 128], ICON_128),
            egui::TextureOptions::LINEAR,
        );

        App {
            ctx: cc.egui_ctx.clone(),
            config,
            sock,
            tx,
            rx,
            status: None,
            offline_reason: String::new(),
            starting_since: None,
            page: Page::Home,
            dialog: Dialog::None,
            message: None,
            invite_cancel: Arc::new(Mutex::new(None)),
            verify: None,
            scan_stop: None,
            scan_tex: None,
            scan_debug: String::new(),
            browse: None,
            logo,
            port_edit: None,
            log_edit: None,
        }
    }

    fn say(&mut self, ok: bool, msg: impl Into<String>) {
        self.message = Some((ok, msg.into()));
    }

    /// Run a request in the background and show what the service answers.
    fn run(&self, req: Request) {
        let (tx, sock, ctx) = (self.tx.clone(), self.sock.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let mut out: Vec<String> = Vec::new();
            let res = call(&sock, &req, None, |r| {
                if let Reply::Out(s) | Reply::Note(s) = r {
                    out.push(s);
                }
            });
            let ev = match res {
                Ok(()) => Ev::Op { ok: true, msg: out.join(" ") },
                Err(e) => Ev::Op { ok: false, msg: e },
            };
            let _ = tx.send(ev);
            let _ = tx.send(Ev::Status(fetch_status(&sock)));
            ctx.request_repaint();
        });
    }

    /// Ask the service for one library's recovery code.
    fn fetch_recovery(&self, library: Option<String>) {
        let prefix = if library.is_some() { "rsr1-" } else { "rsk1-" };
        let (tx, sock, ctx) = (self.tx.clone(), self.sock.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let mut out = String::new();
            let res = call(&sock, &Request::Recovery { library }, None, |r| {
                if let Reply::Out(s) = r {
                    out.push_str(&s);
                }
            });
            let ev = match res {
                Ok(()) => match out.lines().map(|l| l.trim()).find(|l| l.starts_with(prefix)) {
                    Some(c) => Ev::Recovery(Ok(c.to_string())),
                    None => Ev::Recovery(Err("no recovery code available".to_string())),
                },
                Err(e) => Ev::Recovery(Err(e)),
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    /// Ask the service for the newest lines of the activity log.
    fn fetch_log(&self, device: Option<String>, library: Option<String>) {
        let (tx, sock, ctx) = (self.tx.clone(), self.sock.clone(), self.ctx.clone());
        std::thread::spawn(move || {
            let mut out = String::new();
            let res = call(&sock, &Request::Log { device, library, lines: 300 }, None, |r| {
                if let Reply::Out(s) = r {
                    out.push_str(&s);
                }
            });
            let _ = tx.send(Ev::Log(res.map(|_| out)));
            ctx.request_repaint();
        });
    }

    /// Turn "start rustsync by itself" on or off (a systemd user service).
    fn set_service(&mut self, on: bool) {
        let (tx, ctx, config) = (self.tx.clone(), self.ctx.clone(), self.config.clone());
        std::thread::spawn(move || {
            let ev = match set_service_blocking(&config, on) {
                Ok(()) => Ev::Op {
                    ok: true,
                    msg: if on {
                        "RustSync will start by itself".to_string()
                    } else {
                        "RustSync no longer starts by itself, and has been stopped".to_string()
                    },
                },
                Err(msg) => Ev::Op { ok: false, msg },
            };
            let _ = tx.send(ev);
            ctx.request_repaint();
        });
    }

    /// Start the background service (the same as `rustsync -c <config> start`).
    fn start_service(&mut self) {
        self.starting_since = Some(Instant::now());
        let (tx, ctx, config) = (self.tx.clone(), self.ctx.clone(), self.config.clone());
        std::thread::spawn(move || {
            let bin = rustsync_bin();
            let res = Command::new(&bin)
                .arg("-c")
                .arg(&config)
                .arg("start")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let ev = match res {
                Ok(s) if s.success() => None,
                Ok(_) => Some(format!(
                    "RustSync did not start; see daemon.log in {}",
                    state_dir(&config).display()
                )),
                Err(e) => Some(format!(
                    "cannot run {} ({}). Put the rustsync program next to this one, or set RUSTSYNC_BIN.",
                    bin.display(),
                    e
                )),
            };
            if let Some(msg) = ev {
                let _ = tx.send(Ev::Op { ok: false, msg });
            }
            ctx.request_repaint();
        });
    }

    /// Stop the service and start it again (the same as `rustsync -c <config> restart`).
    fn restart_service(&mut self) {
        self.starting_since = Some(Instant::now());
        let (tx, ctx, config) = (self.tx.clone(), self.ctx.clone(), self.config.clone());
        std::thread::spawn(move || {
            let bin = rustsync_bin();
            let res = Command::new(&bin)
                .arg("-c")
                .arg(&config)
                .arg("restart")
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
            let msg = match res {
                Ok(s) if s.success() => None,
                Ok(_) => Some(format!(
                    "RustSync did not restart; see daemon.log in {}",
                    state_dir(&config).display()
                )),
                Err(e) => Some(format!(
                    "cannot run {} ({}). Put the rustsync program next to this one, or set RUSTSYNC_BIN.",
                    bin.display(),
                    e
                )),
            };
            if let Some(msg) = msg {
                let _ = tx.send(Ev::Op { ok: false, msg });
            }
            ctx.request_repaint();
        });
    }

    fn start_invite(&mut self, host: String) {
        *self.invite_cancel.lock().unwrap() = None;
        let (tx, sock, ctx, slot) = (self.tx.clone(), self.sock.clone(), self.ctx.clone(), self.invite_cancel.clone());
        std::thread::spawn(move || {
            let res = call(&sock, &Request::Invite { host }, Some(&slot), |r| match r {
                Reply::Out(code) => {
                    let _ = tx.send(Ev::InviteCode(code));
                    ctx.request_repaint();
                }
                Reply::Note(n) => {
                    let _ = tx.send(Ev::InviteNote(n));
                    ctx.request_repaint();
                }
                Reply::Verify { name, fingerprint, code, joining } => {
                    let _ = tx.send(Ev::PairVerify(Verify { name, fingerprint, code, joining }));
                    ctx.request_repaint();
                }
                _ => {}
            });
            let _ = tx.send(Ev::InviteEnded(res));
            let _ = tx.send(Ev::Status(fetch_status(&sock)));
            ctx.request_repaint();
        });
    }

    fn start_join(&mut self, token: String) {
        *self.invite_cancel.lock().unwrap() = None;
        let (tx, sock, ctx, slot) = (self.tx.clone(), self.sock.clone(), self.ctx.clone(), self.invite_cancel.clone());
        std::thread::spawn(move || {
            let mut paired = String::new();
            let res = call(&sock, &Request::Join { token }, Some(&slot), |r| match r {
                Reply::Verify { name, fingerprint, code, joining } => {
                    let _ = tx.send(Ev::PairVerify(Verify { name, fingerprint, code, joining }));
                    ctx.request_repaint();
                }
                Reply::Out(s) => paired = s,
                _ => {}
            });
            if res.is_ok() {
                let _ = tx.send(Ev::Op { ok: true, msg: paired });
            }
            let _ = tx.send(Ev::JoinEnded(res));
            let _ = tx.send(Ev::Status(fetch_status(&sock)));
            ctx.request_repaint();
        });
    }

    /// Tell the service what the person decided about the pairing code.
    fn send_decision(&mut self, accept: bool) {
        self.verify = None;
        if let Some(stream) = self.invite_cancel.lock().unwrap().as_ref() {
            let mut w: &UnixStream = stream;
            if let Ok(line) = serde_json::to_string(&Request::PairDecision { accept }) {
                let _ = w.write_all(line.as_bytes()).and_then(|_| w.write_all(b"\n"));
            }
        }
        if accept {
            self.say(true, "Waiting for the other device to confirm too…".to_string());
        }
    }

    fn start_scan(&mut self) {
        self.stop_scan();
        let stop = Arc::new(AtomicBool::new(false));
        self.scan_stop = Some(stop.clone());
        camera::spawn(self.tx.clone(), self.ctx.clone(), stop);
    }

    fn stop_scan(&mut self) {
        if let Some(stop) = self.scan_stop.take() {
            stop.store(true, Ordering::Relaxed);
        }
        self.scan_tex = None;
        self.scan_debug.clear();
    }

    fn cancel_invite(&mut self) {
        if let Some(stream) = self.invite_cancel.lock().unwrap().take() {
            // Closing the connection tells the service to forget the code.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }

    fn handle(&mut self, ev: Ev) {
        match ev {
            Ev::Status(Ok(s)) => {
                self.status = Some(s);
                self.offline_reason.clear();
                self.starting_since = None;
            }
            Ev::Status(Err(e)) => {
                self.status = None;
                self.offline_reason = e;
            }
            Ev::Op { ok, msg } => {
                if !msg.is_empty() || !ok {
                    self.say(ok, msg);
                }
                if !ok {
                    self.starting_since = None;
                }
            }
            Ev::InviteCode(c) => {
                if let Dialog::Invite { code, .. } = &mut self.dialog {
                    *code = Some(c);
                }
            }
            Ev::InviteNote(n) => {
                let paired = n.starts_with("Paired with");
                if let Dialog::Invite { note, paired: p, .. } = &mut self.dialog {
                    *note = n.clone();
                    if paired {
                        *p = true;
                    }
                }
                if paired {
                    self.say(true, n);
                }
            }
            Ev::PairVerify(v) => self.verify = Some(v),
            Ev::JoinEnded(res) => {
                self.verify = None;
                match res {
                    Ok(()) => {
                        self.stop_scan();
                        self.dialog = Dialog::None;
                    }
                    Err(e) => {
                        if let Dialog::Join { running, error, scanning, .. } = &mut self.dialog {
                            *running = false;
                            *scanning = false;
                            *error = Some(e);
                        } else {
                            self.say(false, e);
                        }
                    }
                }
            }
            Ev::CameraFrame(img) => {
                if self.scan_stop.is_some() {
                    if let Some(t) = self.scan_tex.as_mut() {
                        t.set(img, egui::TextureOptions::LINEAR);
                    } else {
                        let t = self.ctx.load_texture("camera", img, egui::TextureOptions::LINEAR);
                        self.scan_tex = Some(t);
                    }
                }
            }
            Ev::ScanDebug(text) => {
                if self.scan_stop.is_some() {
                    self.scan_debug = text;
                }
            }
            Ev::Scanned(text) => {
                self.stop_scan();
                let is_join = matches!(self.dialog, Dialog::Join { .. });
                if let Dialog::Join { code, scanning, .. } = &mut self.dialog {
                    *code = text.clone();
                    *scanning = false;
                }
                if is_join {
                    self.perform(Action::StartJoin { token: text });
                }
            }
            Ev::ScanFailed(msg) => {
                self.stop_scan();
                if let Dialog::Join { scanning, error, .. } = &mut self.dialog {
                    *scanning = false;
                    *error = Some(msg);
                }
            }
            Ev::Recovery(res) => {
                if let Dialog::Recovery { code, error, .. } = &mut self.dialog {
                    match res {
                        Ok(c) => *code = Some(c),
                        Err(e) => *error = Some(e),
                    }
                }
            }
            Ev::Log(res) => {
                if let Dialog::Log { text, error, .. } = &mut self.dialog {
                    match res {
                        Ok(t) => *text = Some(t),
                        Err(e) => *error = Some(e),
                    }
                }
            }
            Ev::InviteEnded(res) => {
                self.verify = None;
                let was_paired = matches!(&self.dialog, Dialog::Invite { paired: true, .. });
                match res {
                    Err(e) => {
                        if let Dialog::Invite { error, running, code, .. } = &mut self.dialog {
                            *error = Some(e);
                            *running = false;
                            *code = None; // a code that failed or timed out is no use
                        }
                    }
                    Ok(()) => {
                        if was_paired {
                            self.dialog = Dialog::None; // done
                        } else if let Dialog::Invite { running, code, error, .. } = &mut self.dialog {
                            *running = false;
                            *code = None;
                            *error = Some("That code is no longer valid. Create a new one.".to_string());
                        }
                    }
                }
            }
        }
    }

    fn perform(&mut self, action: Action) {
        match action {
            Action::Create { name, folder } => self.run(Request::Create { name, folder }),
            Action::Share { library, device, backup } => {
                let key = self.status.as_ref().and_then(|s| s.devices.get(device)).map(|d| d.key.clone());
                if let Some(device) = key {
                    self.run(Request::Share { library, device, backup });
                }
            }
            Action::Accept { offer, folder, quota } => self.run(Request::Accept { offer, folder, quota }),
            Action::SetQuota { id, bytes } => self.run(Request::SetQuota { library: id, bytes }),
            Action::Prune { id } => self.run(Request::Prune { library: id }),
            Action::Unshare { id, device } => self.run(Request::Unshare { library: id, device }),
            Action::Refresh => self.run(Request::Refresh),
            Action::ShowRecovery { id, name } => {
                self.dialog = Dialog::Recovery { name, code: None, error: None };
                self.fetch_recovery(id);
            }
            Action::ShowLog { title, device, library } => {
                self.dialog = Dialog::Log { title, device: device.clone(), library: library.clone(), text: None, error: None };
                self.fetch_log(device, library);
            }
            Action::SetLogSize { bytes } => {
                self.log_edit = None;
                self.run(Request::SetLogSize { bytes })
            }
            Action::Restore { code, folder, device } => {
                let key = self.status.as_ref().and_then(|s| s.devices.get(device)).map(|d| d.key.clone());
                if let Some(device) = key {
                    self.run(Request::Restore { code, folder, device });
                }
            }
            Action::StartJoin { token } => {
                self.stop_scan();
                if let Dialog::Join { running, error, scanning, .. } = &mut self.dialog {
                    *running = true;
                    *error = None;
                    *scanning = false;
                }
                self.start_join(token);
            }
            Action::StartScan => {
                if let Dialog::Join { error, scanning, .. } = &mut self.dialog {
                    *error = None;
                    *scanning = true;
                }
                self.start_scan();
            }
            Action::StopScan => {
                self.stop_scan();
                if let Dialog::Join { scanning, .. } = &mut self.dialog {
                    *scanning = false;
                }
            }
            Action::PairDecide { accept } => {
                // On "no" the service ends the pairing and reports why.
                self.send_decision(accept);
            }
            Action::Forget { key } => self.run(Request::Forget { device: key }),
            Action::SetPort { port } => {
                self.port_edit = None;
                self.run(Request::SetPort { port })
            }
            Action::SetAddress { key, addr } => self.run(Request::SetAddress { device: key, addr }),
            Action::SetOwnName { name } => self.run(Request::SetName { name }),
            Action::SetNickname { key, nickname } => self.run(Request::Rename { device: key, nickname }),
            Action::Remove { id } => self.run(Request::RemoveLibrary { library: id }),
            Action::StartInvite { host } => {
                if let Dialog::Invite { running, code, error, note, .. } = &mut self.dialog {
                    *running = true;
                    *code = None;
                    *error = None;
                    note.clear();
                }
                self.start_invite(host);
            }
            Action::CancelInvite => self.cancel_invite(),
        }
    }
}

// ---------------------------------------------------------------- look

// One palette for every RustSync app (the Android app uses the same values): a neutral,
// slightly warm graphite with the logo's orange as the only accent.
const ACCENT: Color32 = Color32::from_rgb(255, 138, 76);
const ACCENT_SOFT: Color32 = Color32::from_rgb(52, 34, 26);
const ACCENT_LINE: Color32 = Color32::from_rgb(122, 70, 42);
const GREEN: Color32 = Color32::from_rgb(82, 201, 140);
const GREEN_SOFT: Color32 = Color32::from_rgb(25, 50, 40);
const RED: Color32 = Color32::from_rgb(240, 98, 98);
const RED_SOFT: Color32 = Color32::from_rgb(58, 28, 28);
const AMBER: Color32 = Color32::from_rgb(240, 182, 80);
const MUTED: Color32 = Color32::from_rgb(150, 150, 162);
const OFFLINE: Color32 = Color32::from_rgb(110, 110, 120);
const TEXT: Color32 = Color32::from_rgb(226, 226, 232);
const PANEL: Color32 = Color32::from_rgb(18, 18, 20);
const SIDEBAR: Color32 = Color32::from_rgb(14, 14, 16);
const CARD: Color32 = Color32::from_rgb(27, 27, 31);
const CARD_LINE: Color32 = Color32::from_rgb(44, 44, 50);
const FIELD: Color32 = Color32::from_rgb(11, 11, 13);
const HOVER: Color32 = Color32::from_rgb(38, 38, 44);

fn configure_style(ctx: &egui::Context) {
    // The window is dark whatever the system theme is: change both of egui's styles.
    ctx.all_styles_mut(style_dark);
}

fn style_dark(style: &mut egui::Style) {
    style.visuals = egui::Visuals::dark();
    let v = &mut style.visuals;
    v.panel_fill = PANEL;
    v.window_fill = CARD;
    v.window_stroke = Stroke::new(1.0_f32, CARD_LINE);
    v.window_corner_radius = CornerRadius::same(12);
    v.extreme_bg_color = FIELD;
    v.hyperlink_color = ACCENT;
    v.selection.bg_fill = Color32::from_rgb(140, 78, 46);
    v.selection.stroke = Stroke::new(1.0_f32, ACCENT);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0_f32, TEXT);
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0_f32, CARD_LINE);
    v.widgets.inactive.bg_fill = Color32::from_rgb(40, 40, 46);
    v.widgets.inactive.weak_bg_fill = Color32::from_rgb(40, 40, 46);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(220, 220, 226));
    v.widgets.hovered.bg_fill = Color32::from_rgb(54, 54, 62);
    v.widgets.hovered.weak_bg_fill = Color32::from_rgb(54, 54, 62);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0_f32, Color32::from_rgb(245, 245, 248));
    v.widgets.active.bg_fill = Color32::from_rgb(112, 64, 40);
    v.widgets.active.weak_bg_fill = Color32::from_rgb(112, 64, 40);
    for w in [
        &mut v.widgets.noninteractive,
        &mut v.widgets.inactive,
        &mut v.widgets.hovered,
        &mut v.widgets.active,
        &mut v.widgets.open,
    ] {
        w.corner_radius = CornerRadius::same(8);
    }
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let style = (*ui.style()).clone();
    egui::Frame::group(&style)
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, CARD_LINE))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

/// The main button of a dialog or page.
fn primary(text: &str) -> egui::Button<'static> {
    egui::Button::new(RichText::new(text).strong().color(Color32::from_rgb(28, 16, 8)))
        .fill(ACCENT)
        .corner_radius(CornerRadius::same(8))
}

// ---- icons, drawn with lines so they look the same on every system (fonts differ a lot).

#[derive(Clone, Copy, PartialEq)]
enum Glyph {
    Home,
    Folder,
    Devices,
    Settings,
    Plus,
    Refresh,
}

fn paint_glyph(painter: &egui::Painter, glyph: Glyph, rect: egui::Rect, color: Color32) {
    let size = rect.width().min(rect.height());
    let at = |x: f32, y: f32| egui::pos2(rect.min.x + x * size, rect.min.y + y * size);
    let width = (size / 11.0).max(1.5);
    let stroke = Stroke::new(width, color);
    // A line through the points with round ends and corners.
    let poly = |pts: Vec<egui::Pos2>, closed: bool| {
        for p in &pts {
            painter.circle_filled(*p, width / 2.0, color);
        }
        if closed {
            painter.add(egui::Shape::closed_line(pts, stroke));
        } else {
            painter.add(egui::Shape::line(pts, stroke));
        }
    };
    match glyph {
        Glyph::Home => {
            poly(vec![at(0.10, 0.50), at(0.50, 0.14), at(0.90, 0.50)], false);
            poly(vec![at(0.22, 0.42), at(0.22, 0.86), at(0.78, 0.86), at(0.78, 0.42)], false);
            poly(vec![at(0.42, 0.86), at(0.42, 0.60), at(0.58, 0.60), at(0.58, 0.86)], false);
        }
        Glyph::Folder => {
            poly(
                vec![at(0.10, 0.22), at(0.40, 0.22), at(0.50, 0.34), at(0.90, 0.34), at(0.90, 0.80), at(0.10, 0.80)],
                true,
            );
        }
        Glyph::Devices => {
            poly(vec![at(0.10, 0.20), at(0.90, 0.20), at(0.90, 0.64), at(0.10, 0.64)], true);
            poly(vec![at(0.50, 0.64), at(0.50, 0.82)], false);
            poly(vec![at(0.30, 0.84), at(0.70, 0.84)], false);
        }
        Glyph::Settings => {
            // Three sliders.
            for (y, x) in [(0.24f32, 0.66f32), (0.50, 0.34), (0.76, 0.58)] {
                poly(vec![at(0.12, y), at(0.88, y)], false);
                painter.circle_filled(at(x, y), size * 0.11, color);
            }
        }
        Glyph::Plus => {
            poly(vec![at(0.50, 0.18), at(0.50, 0.82)], false);
            poly(vec![at(0.18, 0.50), at(0.82, 0.50)], false);
        }
        Glyph::Refresh => {
            // An open circle with an arrow head at its end.
            let (cx, cy, r) = (0.5f32, 0.5f32, 0.32f32);
            let (a0, a1) = (50.0f32.to_radians(), 330.0f32.to_radians());
            let steps = 24;
            let pts: Vec<egui::Pos2> = (0..=steps)
                .map(|i| {
                    let a = a0 + (a1 - a0) * i as f32 / steps as f32;
                    at(cx + r * a.cos(), cy + r * a.sin())
                })
                .collect();
            painter.add(egui::Shape::line(pts, stroke));
            let (c, sn) = (a1.cos(), a1.sin());
            let end = (cx + r * c, cy + r * sn);
            let tangent = (-sn, c);
            let tip = at(end.0 + tangent.0 * 0.17, end.1 + tangent.1 * 0.17);
            let left = at(end.0 + c * 0.13, end.1 + sn * 0.13);
            let right = at(end.0 - c * 0.13, end.1 - sn * 0.13);
            painter.add(egui::Shape::convex_polygon(vec![tip, left, right], color, Stroke::NONE));
        }
    }
}

/// A rounded square with an icon, for the head of a card.
fn tile(ui: &mut egui::Ui, glyph: Glyph, color: Color32, fill: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(42.0), egui::Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(11), fill);
    paint_glyph(ui.painter(), glyph, rect.shrink(10.0), color);
}

/// A row with an icon and a label (the side menu, the folder list).
fn icon_row(ui: &mut egui::Ui, glyph: Glyph, label: &str, selected: bool, height: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), egui::Sense::click());
    let hovered = resp.hovered();
    let fill = if selected {
        ACCENT_SOFT
    } else if hovered {
        HOVER
    } else {
        Color32::TRANSPARENT
    };
    let color = if selected { ACCENT } else { TEXT };
    let painter = ui.painter();
    painter.rect_filled(rect, CornerRadius::same(8), fill);
    if selected {
        painter.rect_filled(
            egui::Rect::from_min_size(egui::pos2(rect.min.x, rect.center().y - 10.0), Vec2::new(3.0, 20.0)),
            CornerRadius::same(2),
            ACCENT,
        );
    }
    let icon = egui::Rect::from_center_size(egui::pos2(rect.min.x + 24.0, rect.center().y), Vec2::splat(20.0));
    paint_glyph(painter, glyph, icon, color);
    painter.text(
        egui::pos2(rect.min.x + 44.0, rect.center().y),
        egui::Align2::LEFT_CENTER,
        label,
        egui::FontId::proportional(15.0),
        color,
    );
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A small square icon button; true when clicked.
fn icon_button(ui: &mut egui::Ui, glyph: Glyph, tip: &str) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(34.0), egui::Sense::click());
    let hovered = resp.hovered();
    if hovered {
        ui.painter().rect_filled(rect, CornerRadius::same(9), HOVER);
    }
    let icon = egui::Rect::from_center_size(rect.center(), Vec2::splat(19.0));
    paint_glyph(ui.painter(), glyph, icon, if hovered { TEXT } else { MUTED });
    resp.on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked()
}

fn heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.heading(RichText::new(title).size(28.0).strong());
    ui.label(RichText::new(subtitle).color(MUTED));
    ui.add_space(12.0);
}

fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).strong().color(MUTED));
}

/// A bar showing how much of a size limit is used (just the amount when there is no limit).
fn usage_bar(ui: &mut egui::Ui, used: u64, quota: u64) {
    if quota == 0 {
        ui.label(format!("{} used · no size limit", fmt_size(used)));
        return;
    }
    let frac = (used as f32 / quota as f32).clamp(0.0, 1.0);
    let color = if frac >= 0.95 {
        RED
    } else if frac >= 0.8 {
        AMBER
    } else {
        GREEN
    };
    ui.add(
        egui::ProgressBar::new(frac)
            .fill(color)
            // The bar takes the width it is given literally, so use what is left in the card
            // (INFINITY, which text boxes accept, would make it run off the window).
            .desired_width(ui.available_width())
            .text(format!("{} of {}  ({:.0}%)", fmt_size(used), fmt_size(quota), frac * 100.0)),
    );
    if used >= quota {
        ui.label(RichText::new("Full: new data is being refused. Raise the limit or free space.").color(RED).small());
    }
}

/// A small filled circle (drawn, because the "●" character is a blank box in some fonts).
fn status_dot(ui: &mut egui::Ui, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(12.0), egui::Sense::hover());
    ui.painter().circle_filled(rect.center(), 4.5, color);
}

/// Draw `text` as a QR code (black on white, with the quiet zone scanners need).
fn qr_view(ui: &mut egui::Ui, text: &str, size: f32) {
    use qrcode::{Color, EcLevel, QrCode};
    let Ok(code) = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L) else {
        ui.label(RichText::new("This code is too long for a QR code; copy it instead.").small().weak());
        return;
    };
    let n = code.width();
    let quiet = 4usize;
    let modules = n + 2 * quiet;
    let cell = (size / modules as f32).floor().max(2.0);
    let total = cell * modules as f32;
    let (rect, _) = ui.allocate_exact_size(Vec2::splat(total), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(4), Color32::WHITE);
    let colors = code.to_colors();
    for y in 0..n {
        for x in 0..n {
            if colors[y * n + x] == Color::Dark {
                let min = rect.min + Vec2::new((x + quiet) as f32 * cell, (y + quiet) as f32 * cell);
                painter.rect_filled(egui::Rect::from_min_size(min, Vec2::splat(cell)), CornerRadius::ZERO, Color32::BLACK);
            }
        }
    }
}

/// A read-only path with a button that opens the folder browser.
fn folder_picker(ui: &mut egui::Ui, folder: &str, empty_hint: &str, want: &mut Option<String>) {
    ui.horizontal(|ui| {
        let width = (ui.available_width() - 170.0).max(180.0);
        egui::Frame::new()
            .fill(FIELD)
            .stroke(Stroke::new(1.0_f32, CARD_LINE))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(egui::Margin::symmetric(10, 8))
            .show(ui, |ui| {
                ui.set_width(width);
                let text = if folder.trim().is_empty() {
                    RichText::new(empty_hint).color(MUTED)
                } else {
                    RichText::new(folder).monospace()
                };
                ui.add(egui::Label::new(text).truncate());
            });
        if ui.button("Choose folder…").clicked() {
            *want = Some(folder.to_string());
        }
    });
}

/// The state of the folder browser window.
struct Browse {
    dir: PathBuf,
    hidden: bool,
    new_name: String,
    error: Option<String>,
    entries: Vec<(String, PathBuf)>,
    unreadable: bool,
    listed: Option<(PathBuf, bool)>,
}

impl Browse {
    /// Start in `start` (a path from the dialog), or the nearest folder above it that exists.
    fn new(start: &str) -> Browse {
        let mut dir = PathBuf::from(expand(start));
        if !dir.is_absolute() {
            dir = home();
        }
        while !dir.is_dir() {
            if !dir.pop() {
                dir = home();
                break;
            }
        }
        Browse { dir, hidden: false, new_name: String::new(), error: None, entries: Vec::new(), unreadable: false, listed: None }
    }

    /// Read the folder again, but only when the place or the hidden-files switch changed.
    fn relist(&mut self) {
        let key = (self.dir.clone(), self.hidden);
        if self.listed.as_ref() == Some(&key) {
            return;
        }
        self.entries.clear();
        self.unreadable = false;
        match std::fs::read_dir(&self.dir) {
            Ok(rd) => {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().to_string();
                    if !self.hidden && name.starts_with('.') {
                        continue;
                    }
                    let path = e.path();
                    if path.is_dir() {
                        self.entries.push((name, path));
                    }
                }
                self.entries.sort_by_key(|(n, _)| n.to_lowercase());
            }
            Err(_) => self.unreadable = true,
        }
        self.listed = Some(key);
    }
}

/// Handy starting points: the home folder and wherever drives usually show up.
fn browse_places() -> Vec<(&'static str, PathBuf)> {
    let user = std::env::var("USER").unwrap_or_default();
    let mut places = vec![("Home", home())];
    let more = [
        ("Drives", PathBuf::from("/Volumes")),
        ("Drives", PathBuf::from("/media").join(&user)),
        ("Drives", PathBuf::from("/run/media").join(&user)),
        ("Mounts", PathBuf::from("/mnt")),
        ("Computer", PathBuf::from("/")),
    ];
    for (name, path) in more {
        if path.is_dir() {
            places.push((name, path));
        }
    }
    places
}

/// The folder browser. `Some(Some(path))` when a folder was chosen, `Some(None)` on cancel.
fn browse_window(ctx: &egui::Context, b: &mut Browse) -> Option<Option<PathBuf>> {
    b.relist();
    let mut result: Option<Option<PathBuf>> = None;
    let mut go: Option<PathBuf> = None;
    egui::Window::new("Choose a folder")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .fixed_size([480.0, 470.0])
        .show(ctx, |ui| {
            ui.horizontal(|ui| {
                let has_parent = b.dir.parent().is_some();
                if ui.add_enabled(has_parent, egui::Button::new("Up")).on_hover_text("The folder around this one").clicked() {
                    go = b.dir.parent().map(|p| p.to_path_buf());
                }
                for (name, path) in browse_places() {
                    if ui.button(name).clicked() {
                        go = Some(path);
                    }
                }
            });
            ui.add(egui::Label::new(RichText::new(b.dir.to_string_lossy()).monospace().small().color(MUTED)).truncate());
            egui::ScrollArea::vertical().min_scrolled_height(250.0).max_height(250.0).auto_shrink([false, false]).show(ui, |ui| {
                if b.unreadable {
                    ui.label(RichText::new("This folder can't be opened.").color(RED));
                } else if b.entries.is_empty() {
                    ui.label(RichText::new("No folders in here.").color(MUTED));
                }
                for (name, path) in &b.entries {
                    if icon_row(ui, Glyph::Folder, name, false, 30.0).clicked() {
                        go = Some(path.clone());
                    }
                }
            });
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut b.new_name).hint_text("Name for a new folder").desired_width(210.0));
                let name = b.new_name.trim().to_string();
                let ok = !name.is_empty() && !name.contains('/') && name != "." && name != "..";
                if ui.add_enabled(ok, egui::Button::new("Create folder")).clicked() {
                    let target = b.dir.join(&name);
                    match std::fs::create_dir(&target) {
                        Ok(()) => {
                            b.new_name.clear();
                            b.error = None;
                            go = Some(target);
                        }
                        Err(e) => b.error = Some(format!("Can't create it: {}", e)),
                    }
                }
                ui.checkbox(&mut b.hidden, "Hidden");
            });
            if let Some(e) = &b.error {
                ui.label(RichText::new(e).color(RED).small());
            }
            ui.horizontal(|ui| {
                if ui.add(primary("Use this folder")).clicked() {
                    result = Some(Some(b.dir.clone()));
                }
                if ui.button("Cancel").clicked() {
                    result = Some(None);
                }
            });
        });
    if let Some(p) = go {
        b.dir = p;
        b.error = None;
    }
    result
}

/// The pairing check: both people compare this code with the other screen. Returns the answer
/// once it is given.
fn verify_window(ctx: &egui::Context, v: &Verify) -> Option<bool> {
    let mut answer = None;
    egui::Window::new("Check the pairing code")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .min_width(420.0)
        .show(ctx, |ui| {
            ui.label(if v.joining {
                "The other device must show exactly this code:"
            } else {
                "The device that is pairing must show exactly this code:"
            });
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(&v.code).monospace().size(38.0).strong().color(ACCENT));
            });
            ui.add_space(6.0);
            egui::Grid::new("verify_grid").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                ui.label(RichText::new("Device").weak());
                ui.label(RichText::new(&v.name).strong());
                ui.end_row();
                ui.label(RichText::new("Key fingerprint").weak());
                ui.label(RichText::new(&v.fingerprint).monospace());
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.label(
                RichText::new(if v.joining {
                    "If the codes differ, something is wrong (a wrong or intercepted code): choose “Not the same”."
                } else {
                    "If you did not just start pairing on a device of yours, or the codes differ, choose “Not the same”: someone else may have used your code."
                })
                .small()
                .weak(),
            );
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button(RichText::new("The codes match: pair").strong()).clicked() {
                    answer = Some(true);
                }
                if ui.button(RichText::new("Not the same").color(RED)).clicked() {
                    answer = Some(false);
                }
            });
        });
    answer
}

// ---------------------------------------------------------------- scanning a QR code with the camera

#[cfg(all(feature = "camera", any(target_os = "linux", target_os = "macos")))]
mod camera {
    use super::Ev;
    use eframe::egui;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::mpsc::Sender;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    pub const AVAILABLE: bool = true;

    /// Open the first camera in its own thread, show its frames, and report the first QR code
    /// that looks like a pairing code.
    pub fn spawn(tx: Sender<Ev>, ctx: egui::Context, stop: Arc<AtomicBool>) {
        std::thread::spawn(move || {
            if let Err(e) = scan(&tx, &ctx, &stop) {
                if !stop.load(Ordering::Relaxed) {
                    let _ = tx.send(Ev::ScanFailed(e));
                    ctx.request_repaint();
                }
            }
        });
    }

    #[cfg(target_os = "macos")]
    fn ask_permission() -> Result<(), String> {
        nokhwa::nokhwa_initialize(|_granted| {});
        for _ in 0..300 {
            if nokhwa::nokhwa_check() {
                return Ok(());
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        Err("camera access was not allowed (System Settings > Privacy & Security > Camera)".to_string())
    }

    /// Picture formats that need no JPEG decoder. Many webcams (on Linux nearly all) send JPEG
    /// video by default, which this build cannot decode; they nearly always offer a raw format too.
    #[cfg(target_os = "linux")]
    static RAW: [nokhwa::utils::FrameFormat; 4] = [
        nokhwa::utils::FrameFormat::YUYV,
        nokhwa::utils::FrameFormat::NV12,
        nokhwa::utils::FrameFormat::GRAY,
        nokhwa::utils::FrameFormat::RAWRGB,
    ];

    fn open_camera() -> Result<nokhwa::Camera, String> {
        use nokhwa::pixel_format::RgbFormat;
        use nokhwa::utils::{CameraIndex, RequestedFormat, RequestedFormatType};
        use nokhwa::Camera;

        // A pairing code is a dense QR code, so ask for the biggest picture the camera has (the
        // fastest format is often a small one, too coarse to read it).
        let biggest = || RequestedFormat::new::<RgbFormat>(RequestedFormatType::AbsoluteHighestResolution);
        #[cfg(target_os = "linux")]
        {
            let want = RequestedFormat::with_formats(RequestedFormatType::None, &RAW);
            if let Ok(mut cam) = Camera::new(CameraIndex::Index(0), want) {
                // Of the raw formats, the biggest picture that still moves at a few frames a second.
                if let Ok(list) = cam.compatible_camera_formats() {
                    let best = list.into_iter().filter(|f| RAW.contains(&f.format())).max_by_key(|f| {
                        (f.frame_rate() >= 5, u64::from(f.width()) * u64::from(f.height()), f.frame_rate())
                    });
                    if let Some(best) = best {
                        let _ = cam.set_camera_requset(RequestedFormat::with_formats(RequestedFormatType::Exact(best), &RAW));
                    }
                }
                return Ok(cam);
            }
        }
        Camera::new(CameraIndex::Index(0), biggest()).map_err(|e| format!("cannot open the camera: {}", e))
    }

    /// Draw a line into a small RGB picture (used to outline a QR code that was found).
    fn draw_line(img: &mut [u8], w: usize, h: usize, a: (i32, i32), b: (i32, i32)) {
        let steps = (b.0 - a.0).abs().max((b.1 - a.1).abs()).max(1);
        for i in 0..=steps {
            let x = a.0 + (b.0 - a.0) * i / steps;
            let y = a.1 + (b.1 - a.1) * i / steps;
            for dy in -1..=1 {
                for dx in -1..=1 {
                    let (px, py) = (x + dx, y + dy);
                    if px >= 0 && py >= 0 && (px as usize) < w && (py as usize) < h {
                        let o = (py as usize * w + px as usize) * 3;
                        img[o] = 40;
                        img[o + 1] = 230;
                        img[o + 2] = 60;
                    }
                }
            }
        }
    }

    fn scan(tx: &Sender<Ev>, ctx: &egui::Context, stop: &AtomicBool) -> Result<(), String> {
        use nokhwa::pixel_format::RgbFormat;
        use nokhwa::utils::FrameFormat;

        #[cfg(target_os = "macos")]
        ask_permission()?;
        let mut cam = open_camera()?;
        cam.open_stream().map_err(|e| format!("cannot start the camera: {}", e))?;
        let cam_name = cam.info().human_name();
        let (mut last_try, mut invert) = (Instant::now(), false);
        // What the window shows under the picture, to see what the scanner is doing.
        let (mut frames, mut frames_at, mut fps, mut last_debug) = (0u32, Instant::now(), 0.0f32, Instant::now());
        let (mut tries, mut seen_at) = (0u32, None::<Instant>);
        let mut detect = String::from("looking…");
        let mut picture = String::new();
        let mut outline: Option<([(i32, i32); 4], Instant)> = None;
        let mut format_note = String::new();
        while !stop.load(Ordering::Relaxed) {
            let frame = cam.frame().map_err(|e| format!("camera error: {}", e))?;
            let source = frame.source_frame_format();
            let img = frame.decode_image::<RgbFormat>().map_err(|e| {
                if source == FrameFormat::MJPEG {
                    "this camera only sends JPEG video, which this version of RustSync cannot decode; paste the code instead".to_string()
                } else {
                    format!("camera error: {}", e)
                }
            })?;
            let (w, h) = (img.width() as usize, img.height() as usize);
            let rgb = img.as_raw();
            if w == 0 || h == 0 || rgb.len() < w * h * 3 {
                continue;
            }
            frames += 1;
            if frames_at.elapsed() >= Duration::from_secs(1) {
                fps = frames as f32 / frames_at.elapsed().as_secs_f32();
                (frames, frames_at) = (0, Instant::now());
            }
            if format_note.is_empty() {
                format_note = format!("{} {}x{}", source, w, h);
            }
            // What is shown in the window: small enough to send to the screen quickly.
            let pstep = (w / 960).max(1);
            let (pw, ph) = (w / pstep, h / pstep);
            let mut small = Vec::with_capacity(pw * ph * 3);
            for y in 0..ph {
                for x in 0..pw {
                    let i = ((y * pstep) * w + x * pstep) * 3;
                    small.extend_from_slice(&rgb[i..i + 3]);
                }
            }
            // A green box around a QR code that was seen (for a moment after).
            if let Some((pts, when)) = &outline {
                if when.elapsed() < Duration::from_millis(1500) {
                    let sc = |p: (i32, i32)| (p.0 / pstep as i32, p.1 / pstep as i32);
                    for k in 0..4 {
                        draw_line(&mut small, pw, ph, sc(pts[k]), sc(pts[(k + 1) % 4]));
                    }
                }
            }
            let _ = tx.send(Ev::CameraFrame(egui::ColorImage::from_rgb([pw, ph], &small)));
            if last_debug.elapsed() >= Duration::from_millis(400) {
                last_debug = Instant::now();
                let _ = tx.send(Ev::ScanDebug(format!(
                    "camera: {}\npicture: {} at {:.0} frames/s\n{}\nscanner: {} tries, {}",
                    cam_name, format_note, fps, picture, tries, detect
                )));
            }
            ctx.request_repaint();
            // Looking for a code takes longer than showing the picture: a few times a second is
            // enough, and every other try is on the inverted picture (light squares on dark).
            if last_try.elapsed() < Duration::from_millis(120) {
                continue;
            }
            last_try = Instant::now();
            invert = !invert;
            tries += 1;
            let dstep = (w / 1600).max(1);
            let (dw, dh) = (w / dstep, h / dstep);
            let grey = |x: usize, y: usize| -> u8 {
                let i = ((y * dstep) * w + x * dstep) * 3;
                ((rgb[i] as u32 * 299 + rgb[i + 1] as u32 * 587 + rgb[i + 2] as u32 * 114) / 1000) as u8
            };
            // How bright and contrasty the picture is (a code needs both).
            if tries % 4 == 1 {
                let mut hist = [0u32; 256];
                let mut n = 0u32;
                for y in (0..dh).step_by(7) {
                    for x in (0..dw).step_by(7) {
                        hist[grey(x, y) as usize] += 1;
                        n += 1;
                    }
                }
                let at = |q: u32| -> usize {
                    let (mut acc, target) = (0u32, n * q / 100);
                    for (v, c) in hist.iter().enumerate() {
                        acc += c;
                        if acc >= target {
                            return v;
                        }
                    }
                    255
                };
                let (lo, hi) = (at(2), at(98));
                let note = if hi.saturating_sub(lo) < 60 {
                    " (too flat: more light or a brighter screen)"
                } else if lo > 200 {
                    " (washed out: glare or too bright)"
                } else if hi < 60 {
                    " (too dark)"
                } else {
                    ""
                };
                picture = format!("light: dark {} to bright {} of 255{}", lo, hi, note);
            }
            let mut prepared = rqrr::PreparedImage::prepare_from_greyscale(dw, dh, |x, y| {
                let g = grey(x, y);
                if invert {
                    255 - g
                } else {
                    g
                }
            });
            let grids = prepared.detect_grids();
            if grids.is_empty() {
                // Keep the last "seen it" message for a moment so it can be read.
                if seen_at.map_or(true, |t| t.elapsed() > Duration::from_secs(3)) {
                    detect = format!("no QR code seen ({} view)", if invert { "inverted" } else { "normal" });
                }
                continue;
            }
            seen_at = Some(Instant::now());
            for grid in grids {
                let b = grid.bounds;
                let pts = [
                    (b[0].x * dstep as i32, b[0].y * dstep as i32),
                    (b[1].x * dstep as i32, b[1].y * dstep as i32),
                    (b[2].x * dstep as i32, b[2].y * dstep as i32),
                    (b[3].x * dstep as i32, b[3].y * dstep as i32),
                ];
                outline = Some((pts, Instant::now()));
                match grid.decode() {
                    Ok((_meta, text)) => {
                        let text = text.trim().to_string();
                        if text.contains('@') && text.contains('.') {
                            let _ = cam.stop_stream();
                            let _ = tx.send(Ev::Scanned(text));
                            ctx.request_repaint();
                            return Ok(());
                        }
                        let start: String = text.chars().take(24).collect();
                        detect = format!(
                            "read a QR code ({} characters) that is not a RustSync pairing code; it starts with \"{}\"",
                            text.chars().count(),
                            start
                        );
                    }
                    Err(e) => {
                        detect = format!("QR code seen but cannot be read yet ({}): hold it still, closer, less glare", e);
                    }
                }
            }
        }
        let _ = cam.stop_stream();
        Ok(())
    }
}

#[cfg(not(all(feature = "camera", any(target_os = "linux", target_os = "macos"))))]
mod camera {
    use super::Ev;
    use eframe::egui;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc::Sender;
    use std::sync::Arc;

    pub const AVAILABLE: bool = false;

    pub fn spawn(_tx: Sender<Ev>, _ctx: egui::Context, _stop: Arc<AtomicBool>) {}
}

// ---------------------------------------------------------------- screens

impl eframe::App for App {
    /// Work that is not drawing: take in what the background threads found.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        while let Ok(ev) = self.rx.try_recv() {
            self.handle(ev);
        }
        // Keep the "starting..." spinner from hanging around if nothing happens.
        if let Some(t) = self.starting_since {
            if t.elapsed() > Duration::from_secs(15) {
                self.starting_since = None;
            }
            ctx.request_repaint_after(Duration::from_millis(300));
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let style = ui.style().clone();

        let (online, status_text) = match &self.status {
            Some(st) => (true, format!("{} is running", st.name)),
            None => (false, "Not running".to_string()),
        };
        let mut refresh = false;
        egui::Panel::top("top_bar").exact_size(60.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(10.0);
                ui.add(egui::Image::new(&self.logo).fit_to_exact_size(Vec2::splat(36.0)));
                ui.vertical(|ui| {
                    ui.label(RichText::new("RustSync").size(19.0).strong());
                    ui.label(RichText::new("Simple, encrypted file sync").size(11.0).color(MUTED));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(10.0);
                    egui::Frame::new()
                        .fill(if online { GREEN_SOFT } else { RED_SOFT })
                        .corner_radius(CornerRadius::same(14))
                        .inner_margin(egui::Margin::symmetric(12, 5))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = 6.0;
                                let color = if online { GREEN } else { RED };
                                status_dot(ui, color);
                                ui.label(RichText::new(&status_text).small().color(color));
                            });
                        });
                    if online && icon_button(ui, Glyph::Refresh, "Ask connected devices for their library lists again and look for backups") {
                        refresh = true;
                    }
                });
            });
        });
        if refresh {
            self.perform(Action::Refresh);
        }

        egui::Panel::bottom("status_bar").exact_size(32.0).show(ui, |ui| {
            ui.horizontal_centered(|ui| match &self.message {
                Some((ok, msg)) => {
                    status_dot(ui, if *ok { GREEN } else { RED });
                    ui.label(RichText::new(msg).small());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Dismiss").clicked() {
                            self.message = None;
                        }
                    });
                }
                None => {
                    status_dot(ui, MUTED);
                    ui.label(RichText::new(format!("Config: {}", self.config.display())).small().weak());
                }
            });
        });

        egui::Panel::left("sidebar")
            .resizable(false)
            .exact_size(180.0)
            .frame(egui::Frame::side_top_panel(&style).fill(SIDEBAR))
            .show(ui, |ui| {
                ui.add_space(16.0);
                section(ui, "SYNC");
                ui.add_space(6.0);
                if icon_row(ui, Glyph::Home, "Home", self.page == Page::Home, 38.0).clicked() {
                    self.page = Page::Home;
                }
                if icon_row(ui, Glyph::Folder, "Libraries", self.page == Page::Libraries, 38.0).clicked() {
                    self.page = Page::Libraries;
                }
                if icon_row(ui, Glyph::Devices, "Devices", self.page == Page::Devices, 38.0).clicked() {
                    self.page = Page::Devices;
                }
                if icon_row(ui, Glyph::Settings, "Settings", self.page == Page::Settings, 38.0).clicked() {
                    self.page = Page::Settings;
                }
                if let Some(s) = &self.status {
                    let waiting = s.offers.len();
                    if waiting > 0 {
                        ui.add_space(16.0);
                        ui.label(RichText::new(format!("{} waiting for you", waiting)).color(ACCENT).small());
                    }
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&style).inner_margin(egui::Margin::same(24)))
            .show(ui, |ui| {
                if self.status.is_none() {
                    self.show_offline(ui);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.page {
                    Page::Libraries => self.show_libraries(ui),
                    Page::Devices => self.show_devices(ui),
                    Page::Home => self.show_home(ui),
                    Page::Settings => self.show_settings(ui),
                });
            });

        self.dialogs(&ctx);
    }
}

impl App {
    fn show_offline(&mut self, ui: &mut egui::Ui) {
        heading(ui, "RustSync is not running", "The background service does the syncing; this window only controls it.");
        card(ui, |ui| {
            ui.label(format!("Config: {}", self.config.display()));
            if !self.offline_reason.is_empty() {
                ui.label(RichText::new(&self.offline_reason).small().weak());
            }
            ui.add_space(6.0);
            if self.starting_since.is_some() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Starting…");
                });
            } else if ui.add(primary("Start RustSync")).clicked() {
                self.start_service();
            }
            ui.label(
                RichText::new("The first start creates the config file and your device's key pair.")
                    .small()
                    .weak(),
            );
        });
    }

    fn show_libraries(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else { return };
        heading(ui, "Libraries", "A library is a named folder that is kept in sync.");

        if !status.offers.is_empty() {
            section(ui, "WAITING FOR YOU");
            for o in &status.offers {
                card(ui, |ui| {
                    ui.horizontal(|ui| {
                        tile(ui, Glyph::Plus, ACCENT, ACCENT_SOFT);
                        ui.vertical(|ui| {
                            if o.again {
                                ui.label(RichText::new("Use the encrypted backup again").strong());
                                ui.label(
                                    RichText::new(format!("{} asks to use backup {} that you stopped serving", o.from_name, o.name))
                                        .small()
                                        .weak(),
                                );
                            } else if o.backup {
                                ui.label(RichText::new("Keep an encrypted backup").strong());
                                ui.label(
                                    RichText::new(format!("for {} — you choose where it is stored; it can't be read here", o.from_name))
                                        .small()
                                        .weak(),
                                );
                            } else {
                                ui.label(RichText::new(&o.name).strong());
                                ui.label(
                                    RichText::new(format!("shared by {}  ·  id {}", o.from_name, short(&o.id)))
                                        .small()
                                        .weak(),
                                );
                            }
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if ui.button("Decline").clicked() {
                                self.run(Request::Decline { offer: o.id.clone() });
                            }
                            if o.again {
                                if ui.button(RichText::new("Allow").strong()).clicked() {
                                    self.run(Request::Accept { offer: o.id.clone(), folder: String::new(), quota: 0 });
                                }
                            } else if ui.button(RichText::new("Accept…").strong()).clicked() {
                                let base = if o.backup && !status.backup_dir.is_empty() {
                                    PathBuf::from(&status.backup_dir)
                                } else {
                                    home()
                                };
                                let suggestion = base.join(&o.name).to_string_lossy().to_string();
                                self.dialog = Dialog::Accept {
                                    offer_id: o.id.clone(),
                                    name: o.name.clone(),
                                    folder: suggestion,
                                    backup: o.backup,
                                    from: o.from_name.clone(),
                                    quota: String::new(),
                                };
                            }
                        });
                    });
                });
            }
            ui.add_space(8.0);
        }

        ui.horizontal(|ui| {
            section(ui, "YOUR LIBRARIES");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.add(primary("New library")).clicked() {
                    self.dialog = Dialog::NewLibrary { name: String::new(), folder: String::new() };
                }
                if ui.add_enabled(!status.devices.is_empty(), egui::Button::new("Restore from backup…")).clicked() {
                    self.dialog = Dialog::Restore { code: String::new(), folder: String::new(), device: 0 };
                }
                if status.libraries.iter().any(|l| !l.backup_only) && ui.button("Recovery kit…").clicked() {
                    self.perform(Action::ShowRecovery { id: None, name: "all libraries".to_string() });
                }
            });
        });

        if status.libraries.is_empty() {
            card(ui, |ui| {
                ui.label("No libraries yet. Create one from a folder you want to sync, or accept one another device shares with you.");
            });
        }
        for l in &status.libraries {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    tile(ui, Glyph::Folder, ACCENT, ACCENT_SOFT);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&l.name).strong().size(16.0));
                        ui.add(egui::Label::new(RichText::new(&l.folder).small().weak()).wrap());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(format!("id {}", short(&l.id))).small().weak().monospace());
                            if ui.small_button("Copy full id").clicked() {
                                ui.ctx().copy_text(l.id.clone());
                                self.say(true, "Library id copied");
                            }
                        });
                        // The buttons sit under the text, and wrap onto more lines when the window is narrow.
                        ui.horizontal_wrapped(|ui| {
                            if l.backup_only {
                                if ui.button("Set limit…").clicked() {
                                    let text = if l.quota == 0 { String::new() } else { fmt_size(l.quota).replace(' ', "") };
                                    self.dialog = Dialog::Quota { id: l.id.clone(), name: l.name.clone(), text };
                                }
                            } else {
                                let can_share = !status.devices.is_empty();
                                if ui.add_enabled(can_share, egui::Button::new("Share…")).clicked() {
                                    self.dialog = Dialog::Share {
                                        lib_id: l.id.clone(),
                                        lib_name: l.name.clone(),
                                        device: 0,
                                        backup: false,
                                    };
                                }
                                if ui.button("Recovery code…").clicked() {
                                    self.perform(Action::ShowRecovery { id: Some(l.id.clone()), name: l.name.clone() });
                                }
                            }
                            let related = related_devices(l, &status.devices);
                            if !related.is_empty() && ui.button("Unshare…").clicked() {
                                self.dialog = Dialog::Unshare {
                                    lib_id: l.id.clone(),
                                    lib_name: l.name.clone(),
                                    candidates: related,
                                    sel: 0,
                                };
                            }
                            if ui.button("Log…").clicked() {
                                self.perform(Action::ShowLog {
                                    title: format!("Log: {}", l.name),
                                    device: None,
                                    library: Some(l.name.clone()),
                                });
                            }
                            if ui.button("Remove").clicked() {
                                self.dialog = Dialog::ConfirmRemove { id: l.id.clone(), name: l.name.clone() };
                            }
                        });
                    });
                });
                if l.backup_only {
                    ui.label(RichText::new("Encrypted backup — unreadable on this machine").color(MUTED));
                    if !l.kept_for.is_empty() {
                        ui.label(format!("Kept for: {}", l.kept_for.join(", ")));
                    }
                    usage_bar(ui, l.used, l.quota);
                    return;
                }
                for b in &l.backups {
                    ui.add_space(2.0);
                    ui.label(RichText::new(format!("Encrypted backup on {}", b.device)).small());
                    if b.known {
                        usage_bar(ui, b.used, b.quota);
                    } else {
                        ui.label(RichText::new("usage not reported yet").small().weak());
                    }
                }
                if !l.backups.is_empty() {
                    if ui
                        .small_button("Free space in backups")
                        .on_hover_text("Deletes old data in the backups that nothing uses any more (only data unused for over 7 days)")
                        .clicked()
                    {
                        self.perform(Action::Prune { id: l.id.clone() });
                    }
                }
                if !l.sync.is_empty() {
                    let busy = l.sync.iter().any(|s| s.busy());
                    let title = if busy {
                        "Syncing now".to_string()
                    } else if l.sync.iter().all(|s| !s.online) {
                        format!("Syncing with {} (offline)", l.syncing_with.join(", "))
                    } else {
                        format!("Up to date with {}", l.syncing_with.join(", "))
                    };
                    egui::CollapsingHeader::new(RichText::new(title).color(if busy { ACCENT } else { TEXT }))
                        .id_salt(format!("sync-{}", l.id))
                        .show(ui, |ui| {
                            for s in &l.sync {
                                ui.horizontal_wrapped(|ui| {
                                    status_dot(ui, if s.online { GREEN } else { MUTED });
                                    ui.label(RichText::new(&s.device).strong());
                                    ui.label(sync_summary(s));
                                });
                                if let Some(d) = status.devices.iter().find(|d| d.key == s.key) {
                                    if s.online {
                                        ui.label(
                                            RichText::new(format!(
                                                "Speed with this device (all libraries): receiving {}, sending {}",
                                                fmt_rate(d.down_rate),
                                                fmt_rate(d.up_rate)
                                            ))
                                            .small()
                                            .weak(),
                                        );
                                    } else if d.last_seen > 0 {
                                        ui.label(RichText::new(format!("Last seen {}", ago_text(d.last_seen))).small().weak());
                                    }
                                }
                            }
                            ui.label(
                                RichText::new("“Left” is the most that is still to be transferred; parts a device already has are not sent again.")
                                    .small()
                                    .weak(),
                            );
                        });
                } else if !l.syncing_with.is_empty() {
                    ui.label(format!("Syncing with: {}", l.syncing_with.join(", ")));
                }
                if !l.pending.is_empty() {
                    ui.label(
                        RichText::new(format!("Offered, not confirmed yet: {}", l.pending.join(", ")))
                            .color(MUTED),
                    );
                }
                if l.syncing_with.is_empty() && l.backed_up_on.is_empty() && l.pending.is_empty() {
                    let hint = if status.devices.is_empty() {
                        "Not shared yet. Pair a device first (Devices)."
                    } else {
                        "Not shared with anyone yet."
                    };
                    ui.label(RichText::new(hint).small().weak());
                }
            });
        }
    }

    fn show_devices(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else { return };
        heading(ui, "Devices", "Pair your machines once; then share libraries between them.");

        card(ui, |ui| {
            ui.label(RichText::new("Add a device").strong());
            ui.label("On one machine choose “Show a pairing code”; on the other, “Enter a pairing code”.");
            ui.horizontal(|ui| {
                if ui.button("Show a pairing code").clicked() {
                    let first = status
                        .addresses
                        .first()
                        .map(|a| a.0.clone())
                        .or_else(|| status.dns_names.first().cloned())
                        .unwrap_or_default();
                    self.dialog = Dialog::Invite {
                        manual: first.is_empty(),
                        host: first,
                        code: None,
                        note: String::new(),
                        error: None,
                        running: false,
                        paired: false,
                    };
                }
                if ui.button("Enter a pairing code").clicked() {
                    self.dialog = Dialog::Join { code: String::new(), running: false, error: None, scanning: false };
                }
            });
        });
        ui.add_space(8.0);

        section(ui, "PAIRED DEVICES");
        if status.devices.is_empty() {
            card(ui, |ui| {
                ui.label("No devices yet.");
            });
        }
        for d in &status.devices {
            card(ui, |ui| {
                ui.horizontal(|ui| {
                    if d.online {
                        tile(ui, Glyph::Devices, GREEN, GREEN_SOFT);
                    } else {
                        tile(ui, Glyph::Devices, MUTED, HOVER);
                    }
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&d.name).strong());
                        if !d.nickname.is_empty() && !d.announced.is_empty() {
                            ui.label(RichText::new(format!("calls itself “{}”", d.announced)).small().weak());
                        }
                        let mut line = format!("{}…", short(&d.key));
                        if let Some(a) = &d.addr {
                            line.push_str(&format!("  ·  {}", a));
                        }
                        line.push_str(if d.online { "  ·  online" } else { "  ·  offline" });
                        if !d.online && d.last_seen > 0 {
                            line.push_str(&format!("  ·  last seen {}", ago_text(d.last_seen)));
                        }
                        ui.label(RichText::new(line).small().weak());
                        if d.online && (d.down_rate > 0 || d.up_rate > 0) {
                            ui.label(
                                RichText::new(format!("Receiving {}  ·  sending {}", fmt_rate(d.down_rate), fmt_rate(d.up_rate)))
                                    .small()
                                    .color(ACCENT),
                            );
                        }
                        ui.horizontal_wrapped(|ui| {
                            if ui
                                .button("Change address…")
                                .on_hover_text("Use this when the other machine's IP address or host name changed")
                                .clicked()
                            {
                                self.dialog = Dialog::Address {
                                    key: d.key.clone(),
                                    name: d.name.clone(),
                                    text: d.addr.clone().unwrap_or_default(),
                                };
                            }
                            if ui
                                .button("Rename…")
                                .on_hover_text("Give it a name that only this machine uses. The name its owner chose stays as it is.")
                                .clicked()
                            {
                                self.dialog = Dialog::Nickname {
                                    key: d.key.clone(),
                                    announced: if d.announced.is_empty() { d.name.clone() } else { d.announced.clone() },
                                    text: d.nickname.clone(),
                                };
                            }
                            if ui.button("Log…").clicked() {
                                self.perform(Action::ShowLog {
                                    title: format!("Log: {}", d.name),
                                    device: Some(d.name.clone()),
                                    library: None,
                                });
                            }
                            if ui.button("Forget").clicked() {
                                self.dialog = Dialog::ConfirmForget { key: d.key.clone(), name: d.name.clone() };
                            }
                        });
                    });
                });
            });
        }
    }

    fn show_home(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else { return };
        heading(ui, "Home", "This device: how it identifies itself to the others.");
        if let Some(problem) = app_location_problem() {
            card(ui, |ui| {
                ui.label(RichText::new("Move this app first").strong().color(AMBER));
                ui.add(egui::Label::new(problem).wrap());
            });
        }

        card(ui, |ui| {
            ui.label(RichText::new("Name").small().weak());
            ui.horizontal(|ui| {
                ui.label(RichText::new(&status.name).strong().size(16.0));
                if ui
                    .small_button("Rename…")
                    .on_hover_text("Other devices show this name, unless they gave this device a nickname of their own")
                    .clicked()
                {
                    self.dialog = Dialog::OwnName { text: status.name.clone() };
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Public key").small().weak());
            ui.horizontal(|ui| {
                ui.label(RichText::new(&status.key).monospace());
                if ui.small_button("Copy").clicked() {
                    ui.ctx().copy_text(status.key.clone());
                    self.say(true, "Public key copied");
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Listening on").small().weak());
            ui.label(&status.listen);
        });
    }

    fn show_settings(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else { return };
        heading(ui, "Settings", "How this device listens and starts, and about RustSync.");
        if let Some(problem) = app_location_problem() {
            card(ui, |ui| {
                ui.label(RichText::new("Move this app first").strong().color(AMBER));
                ui.add(egui::Label::new(problem).wrap());
            });
            ui.add_space(8.0);
        }
        card(ui, |ui| {
            ui.label(RichText::new("Service").strong());
            ui.label("Syncing continues in the background when you close this window.");
            let mut auto = service_installed(&self.config);
            if ui
                .checkbox(&mut auto, AUTOSTART_LABEL)
                .on_hover_text("A systemd user service. Turning it off also stops RustSync now.")
                .changed()
            {
                self.set_service(auto);
            }
            if tray_supported() {
                let mut on = tray_login_on(&self.config);
                let label = if cfg!(target_os = "macos") {
                    "Show the RustSync icon in the menu bar when I log in"
                } else {
                    "Show the RustSync tray icon when I log in"
                };
                if ui
                    .checkbox(&mut on, label)
                    .on_hover_text("Opening this window also shows the icon. Quit it from its own menu.")
                    .changed()
                {
                    match set_tray_login(&self.config, on) {
                        Ok(()) => {
                            if on {
                                ensure_tray_running(&self.config);
                            }
                        }
                        Err(e) => self.say(false, &e),
                    }
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            if on_wayland() {
                let mut x11 = gui_x11(&self.config);
                if ui
                    .checkbox(&mut x11, "Use the desktop's own window title bar and buttons (X11 mode)")
                    .on_hover_text("On a Wayland desktop, this makes the close, minimise and maximise buttons look like those of other windows. Turn it off to use native Wayland (RustSync then draws its own buttons, and the picture can be sharper on a scaled display).")
                    .changed()
                {
                    match rustsync::config::set_gui_x11(&self.config, x11) {
                        Ok(()) => self.say(true, "Saved. Close this window and open it again to apply."),
                        Err(e) => self.say(false, format!("cannot save that: {:#}", e)),
                    }
                }
            }
            ui.horizontal(|ui| {
                if ui
                    .button("Restart RustSync")
                    .on_hover_text("Stops the background service and starts it again. Syncing resumes in a moment.")
                    .clicked()
                {
                    self.restart_service();
                }
                if ui.button("Stop RustSync").clicked() {
                    self.run(Request::Stop);
                }
            });
        });

        ui.add_space(8.0);
        card(ui, |ui| {
            ui.label(RichText::new("Network").strong());
            ui.label("Port this machine listens on for the other devices:");
            let current = status.port.to_string();
            let mut text = self.port_edit.clone().unwrap_or_else(|| current.clone());
            let mut save = false;
            ui.horizontal(|ui| {
                let resp = ui.add(egui::TextEdit::singleline(&mut text).desired_width(90.0));
                let parsed = text.trim().parse::<u16>().ok().filter(|p| *p > 0);
                if resp.changed() {
                    self.port_edit = Some(text.clone());
                }
                let changed = text.trim() != current;
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(changed && parsed.is_some(), primary("Save")).clicked() || (enter && changed && parsed.is_some()) {
                    save = true;
                }
                if changed && ui.button("Reset").clicked() {
                    self.port_edit = None;
                }
            });
            if !text.trim().is_empty() && text.trim().parse::<u16>().ok().filter(|p| *p > 0).is_none() {
                ui.label(RichText::new("A port is a number from 1 to 65535.").color(RED).small());
            }
            ui.label(
                RichText::new("The default is 3032. Devices that connect to this machine need to use the new port too; use “Change address…” on their side.")
                    .small()
                    .weak(),
            );
            if save {
                if let Ok(port) = text.trim().parse::<u16>() {
                    self.perform(Action::SetPort { port });
                }
            }
        });

        ui.add_space(8.0);
        card(ui, |ui| {
            ui.label(RichText::new("Logs").strong());
            ui.label("A short history of what RustSync did: connections, pairing, files received, errors.");
            if ui.button("Show the log").clicked() {
                self.perform(Action::ShowLog { title: "Activity log".to_string(), device: None, library: None });
            }
            ui.add_space(4.0);
            ui.label("Keep at most this much (the log never grows past it):");
            let current = fmt_mb(status.log_max);
            let mut text = self.log_edit.clone().unwrap_or_else(|| current.clone());
            let parsed = text.trim().parse::<f64>().ok().filter(|v| *v >= 0.1 && *v <= 500.0);
            let mut save = false;
            ui.horizontal(|ui| {
                let resp = ui.add(egui::TextEdit::singleline(&mut text).desired_width(70.0));
                if resp.changed() {
                    self.log_edit = Some(text.clone());
                }
                ui.label("MB");
                let changed = text.trim() != current;
                let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if ui.add_enabled(changed && parsed.is_some(), primary("Save")).clicked() || (enter && changed && parsed.is_some()) {
                    save = true;
                }
                if changed && ui.button("Reset").clicked() {
                    self.log_edit = None;
                }
            });
            if !text.trim().is_empty() && parsed.is_none() {
                ui.label(RichText::new("A size from 0.1 to 500 (megabytes).").color(RED).small());
            }
            if !status.log_file.is_empty() {
                ui.label(
                    RichText::new(format!(
                        "Plain text in {}. On the command line: rustsync log (add --device NAME or --library NAME).",
                        status.log_file
                    ))
                    .small()
                    .weak(),
                );
            }
            if save {
                if let Some(v) = parsed {
                    self.perform(Action::SetLogSize { bytes: (v * 1e6).round() as u64 });
                }
            }
        });

        ui.add_space(8.0);
        card(ui, |ui| {
            ui.label(RichText::new("About").strong());
            ui.label(format!("RustSync {}", env!("CARGO_PKG_VERSION")));
            ui.label("Made by Michael Moss with the aid of clankers.");
            ui.label(
                RichText::new(
                    "Free software, released under the GNU General Public License, version 2 (GPL-2.0-only). \
                     It comes with no warranty.",
                )
                .small()
                .weak(),
            );
            ui.hyperlink_to("github.com/mrmoss/rustsync", "https://github.com/mrmoss/rustsync");
        });
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        // Comparing the pairing code comes before everything else.
        if let Some(v) = self.verify.clone() {
            if let Some(accept) = verify_window(ctx, &v) {
                self.perform(Action::PairDecide { accept });
            }
            return;
        }
        let mut dialog = std::mem::take(&mut self.dialog);
        let devices: Vec<DeviceInfo> = self.status.as_ref().map(|s| s.devices.clone()).unwrap_or_default();
        let (addresses, dns_names, listen_port) = self
            .status
            .as_ref()
            .map(|s| (s.addresses.clone(), s.dns_names.clone(), s.port))
            .unwrap_or_default();
        let mut action: Option<Action> = None;
        let mut close = false;
        let mut want_browse: Option<String> = None;

        let window = |title: &str| {
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
                .min_width(420.0)
        };

        match &mut dialog {
            Dialog::None => {}

            Dialog::NewLibrary { name, folder } => {
                window("New library").show(ctx, |ui| {
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(name).hint_text("e.g. photos").desired_width(f32::INFINITY));
                    ui.label("Folder to sync");
                    folder_picker(ui, folder, "No folder chosen yet", &mut want_browse);
                    let folder_full = expand(folder);
                    let valid_folder = folder_full.starts_with('/');
                    ui.horizontal(|ui| {
                        let ok = !name.trim().is_empty() && valid_folder;
                        if ui.add_enabled(ok, primary("Create")).clicked() {
                            action = Some(Action::Create { name: name.trim().to_string(), folder: folder_full.clone() });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Share { lib_id, lib_name, device, backup } => {
                window("Share a library").show(ctx, |ui| {
                    ui.label(format!("Share “{}” with:", lib_name));
                    let selected = devices.get(*device).map(|d| d.name.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("share_device")
                        .selected_text(selected)
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (i, d) in devices.iter().enumerate() {
                                ui.selectable_value(device, i, &d.name);
                            }
                        });
                    ui.add_space(4.0);
                    ui.radio_value(backup, false, "Sync the files (the other device gets a normal copy)");
                    ui.radio_value(backup, true, "Encrypted backup only (the other device can't read it)");
                    ui.label(
                        RichText::new(if *backup {
                            "Good for a server: it stores scrambled data, so your other devices can still catch up when this one is off."
                        } else {
                            "The other device will be asked to accept it and choose a folder."
                        })
                        .small()
                        .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Share").strong()).clicked() {
                            action = Some(Action::Share { library: lib_id.clone(), device: *device, backup: *backup });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Accept { offer_id, name, folder, backup, from, quota } => {
                window(if *backup { "Keep an encrypted backup" } else { "Accept a library" }).show(ctx, |ui| {
                    if *backup {
                        ui.label(format!("{} wants to keep an encrypted backup on this machine.", from));
                        ui.label(
                            RichText::new("Pick the drive and folder where it should live (use an empty folder). Nothing stored there can be read on this machine.")
                                .small()
                                .weak(),
                        );
                        ui.label("Store it in this folder:");
                    } else {
                        ui.label(format!("Sync “{}” into this folder:", name));
                    }
                    folder_picker(ui, folder, "No folder chosen yet", &mut want_browse);
                    let folder_full = expand(folder);
                    let mut valid = folder_full.starts_with('/');
                    let mut quota_bytes = 0u64;
                    if *backup {
                        ui.label("Size limit (optional), e.g. 500G or 2T:");
                        ui.add(egui::TextEdit::singleline(quota).hint_text("no limit").desired_width(160.0));
                        match parse_size(quota) {
                            Ok(b) => quota_bytes = b,
                            Err(e) => {
                                valid = false;
                                ui.label(RichText::new(e).color(RED).small());
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.add_enabled(valid, primary("Accept")).clicked() {
                            action = Some(Action::Accept { offer: offer_id.clone(), folder: folder_full.clone(), quota: quota_bytes });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Invite { host, manual, code, note, error, running, paired } => {
                window("Show a pairing code").show(ctx, |ui| {
                    if !*running && code.is_none() {
                        ui.label("Address the other machine will use to reach this one:");
                        let mut opts: Vec<(String, String)> =
                            addresses.iter().map(|(a, i)| (format!("{}  ({})", a, i), a.clone())).collect();
                        opts.extend(dns_names.iter().map(|n| (format!("{}  (host name)", n), n.clone())));
                        let shown = if *manual {
                            "Set manually…".to_string()
                        } else {
                            opts.iter().find(|o| o.1 == *host).map(|o| o.0.clone()).unwrap_or_else(|| "Choose an address".to_string())
                        };
                        egui::ComboBox::from_id_salt("invite_host").selected_text(shown).width(330.0).show_ui(ui, |ui| {
                            for (label, value) in &opts {
                                if ui.selectable_label(!*manual && *host == *value, label).clicked() {
                                    *host = value.clone();
                                    *manual = false;
                                }
                            }
                            if ui.selectable_label(*manual, "Set manually…").clicked() {
                                *manual = true;
                            }
                        });
                        if *manual {
                            ui.add(
                                egui::TextEdit::singleline(host)
                                    .hint_text("e.g. 192.168.1.20 or myserver.example.com")
                                    .desired_width(f32::INFINITY),
                            );
                        }
                        ui.label(
                            RichText::new(format!(
                                "Port {} is added for you; type :port after the address to use another. A host name works if the other machine can look it up.",
                                if listen_port == 0 { 3032 } else { listen_port }
                            ))
                            .small()
                            .weak(),
                        );
                        if let Some(e) = error {
                            ui.label(RichText::new(e.as_str()).color(RED));
                        }
                        ui.horizontal(|ui| {
                            let ok = !host.trim().is_empty();
                            if ui.add_enabled(ok, egui::Button::new(RichText::new("Create code").strong())).clicked() {
                                action = Some(Action::StartInvite { host: host.trim().to_string() });
                            }
                            if ui.button("Cancel").clicked() {
                                close = true;
                            }
                        });
                    } else {
                        match code.clone() {
                            Some(c) => {
                                ui.label("On the other machine choose “Enter a pairing code” and paste this:");
                                ui.add(
                                    egui::TextEdit::multiline(&mut c.as_str())
                                        .font(egui::TextStyle::Monospace)
                                        .desired_rows(3)
                                        .desired_width(f32::INFINITY),
                                );
                                ui.add_space(6.0);
                                ui.vertical_centered(|ui| {
                                    qr_view(ui, &c, 230.0);
                                    ui.label(
                                        RichText::new("or scan this with the RustSync app on the other device")
                                            .small()
                                            .weak(),
                                    );
                                });
                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    if ui.button("Copy code").clicked() {
                                        ui.ctx().copy_text(c.clone());
                                    }
                                    if *paired {
                                        ui.label(RichText::new(note.as_str()).color(GREEN));
                                    } else if *running {
                                        ui.spinner();
                                        ui.label("Waiting for the other machine… (works once, for 10 minutes)");
                                    }
                                });
                            }
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Creating code…");
                                });
                            }
                        }
                        if let Some(e) = error {
                            ui.label(RichText::new(e.as_str()).color(RED));
                        }
                        let label = if *paired { "Close" } else { "Cancel" };
                        if ui.button(label).clicked() {
                            if *running {
                                action = Some(Action::CancelInvite);
                            }
                            close = true;
                        }
                    }
                });
            }

            Dialog::Quota { id, name, text } => {
                window("Size limit").show(ctx, |ui| {
                    ui.label(format!("Largest size the encrypted backup “{}” may use on this machine:", name));
                    ui.add(egui::TextEdit::singleline(text).hint_text("e.g. 500G, 2T, or empty for no limit").desired_width(f32::INFINITY));
                    let parsed = parse_size(text);
                    if let Err(e) = &parsed {
                        ui.label(RichText::new(e.as_str()).color(RED).small());
                    }
                    ui.label(RichText::new("Data already stored is kept; once the limit is reached, new data is refused.").small().weak());
                    ui.horizontal(|ui| {
                        if ui.add_enabled(parsed.is_ok(), egui::Button::new(RichText::new("Save").strong())).clicked() {
                            action = Some(Action::SetQuota { id: id.clone(), bytes: parsed.clone().unwrap_or(0) });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Recovery { name, code, error } => {
                window("Recovery code").show(ctx, |ui| {
                    let all = name.as_str() == "all libraries";
                    ui.label(format!(
                        "Recovery {} for {}",
                        if all { "kit" } else { "code" },
                        if all { "all libraries".to_string() } else { format!("“{}”", name) }
                    ));
                    ui.label(
                        RichText::new("Together with an encrypted backup, this code is enough to get all the files back if every machine is lost — and enough for anyone who has it to read them. Save it in a password manager; don't share it.")
                            .small()
                            .weak(),
                    );
                    match code.clone() {
                        Some(c) => {
                            ui.add(
                                egui::TextEdit::multiline(&mut c.as_str())
                                    .font(egui::TextStyle::Monospace)
                                    .desired_rows(3)
                                    .desired_width(f32::INFINITY),
                            );
                            if ui.button("Copy code").clicked() {
                                ui.ctx().copy_text(c.clone());
                            }
                        }
                        None => {
                            if let Some(e) = error {
                                ui.label(RichText::new(e.as_str()).color(RED));
                            } else {
                                ui.spinner();
                            }
                        }
                    }
                    if ui.button("Close").clicked() {
                        close = true;
                    }
                });
            }

            Dialog::Log { title, device, library, text, error } => {
                window(title.as_str()).show(ctx, |ui| {
                    match text.as_ref() {
                        Some(t) => {
                            egui::ScrollArea::vertical().max_height(380.0).stick_to_bottom(true).show(ui, |ui| {
                                ui.add(
                                    egui::TextEdit::multiline(&mut t.as_str())
                                        .font(egui::TextStyle::Monospace)
                                        .desired_rows(16)
                                        .desired_width(f32::INFINITY),
                                );
                            });
                            ui.label(RichText::new("Times are UTC. Newest at the bottom.").small().weak());
                        }
                        None => {
                            if let Some(e) = error.as_ref() {
                                ui.label(RichText::new(e.as_str()).color(RED));
                            } else {
                                ui.spinner();
                            }
                        }
                    }
                    ui.horizontal(|ui| {
                        if ui.button("Refresh").clicked() {
                            action = Some(Action::ShowLog {
                                title: title.clone(),
                                device: device.clone(),
                                library: library.clone(),
                            });
                        }
                        if let Some(t) = text.as_ref() {
                            if ui.button("Copy").clicked() {
                                ui.ctx().copy_text(t.clone());
                            }
                        }
                        if ui.button("Close").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Restore { code, folder, device } => {
                window("Restore from an encrypted backup").show(ctx, |ui| {
                    ui.label("Paste the recovery code (rsr1-…) for one library, or the recovery kit (rsk1-…) for all of them:");
                    ui.add(
                        egui::TextEdit::multiline(code)
                            .font(egui::TextStyle::Monospace)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.label("The paired device that keeps the backup:");
                    let selected = devices.get(*device).map(|d| d.name.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("restore_device")
                        .selected_text(selected)
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (i, d) in devices.iter().enumerate() {
                                ui.selectable_value(device, i, &d.name);
                            }
                        });
                    ui.label("Restore into this empty folder (a kit makes one subfolder per library):");
                    folder_picker(ui, folder, "No folder chosen yet", &mut want_browse);
                    let folder_full = expand(folder);
                    let valid = folder_full.starts_with('/') && (code.trim().starts_with("rsr1-") || code.trim().starts_with("rsk1-"));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(valid, primary("Restore")).clicked() {
                            action = Some(Action::Restore {
                                code: code.trim().to_string(),
                                folder: folder_full.clone(),
                                device: *device,
                            });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Join { code, running, error, scanning } => {
                window("Enter a pairing code").show(ctx, |ui| {
                    if *running {
                        ui.horizontal(|ui| {
                            ui.spinner();
                            ui.label("Contacting the other device…");
                        });
                        if ui.button("Cancel").clicked() {
                            action = Some(Action::CancelInvite);
                            close = true;
                        }
                        return;
                    }
                    if *scanning {
                        ui.label("Point the camera at the QR code on the other device.");
                        ui.label(RichText::new("Hold it still, as big in the picture as you can, with good light on it.").small().weak());
                        match &self.scan_tex {
                            Some(tex) => {
                                let w = ui.available_width().min(380.0);
                                let ratio = tex.size()[1] as f32 / tex.size()[0].max(1) as f32;
                                ui.add(egui::Image::new(tex).fit_to_exact_size(Vec2::new(w, w * ratio)));
                                if !self.scan_debug.is_empty() {
                                    ui.label(RichText::new(self.scan_debug.as_str()).monospace().small().weak());
                                }
                            }
                            None => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.label("Starting the camera…");
                                });
                            }
                        }
                        if ui.button("Stop scanning").clicked() {
                            action = Some(Action::StopScan);
                        }
                        return;
                    }
                    ui.label("Paste the code shown on the other device (“Show a pairing code”):");
                    ui.add(
                        egui::TextEdit::multiline(code)
                            .font(egui::TextStyle::Monospace)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    if camera::AVAILABLE && ui.button("Scan its QR code with the camera").clicked() {
                        action = Some(Action::StartScan);
                    }
                    if let Some(e) = error {
                        ui.label(RichText::new(e.as_str()).color(RED));
                    }
                    ui.horizontal(|ui| {
                        let ok = !code.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new(RichText::new("Pair").strong())).clicked() {
                            action = Some(Action::StartJoin { token: code.trim().to_string() });
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::ConfirmForget { key, name } => {
                window("Forget device").show(ctx, |ui| {
                    ui.label(format!("Stop syncing with {}? Libraries shared with it stay on both machines.", name));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Forget").color(RED)).clicked() {
                            action = Some(Action::Forget { key: key.clone() });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Address { key, name, text } => {
                window("Change a device's address").show(ctx, |ui| {
                    ui.label(format!("Where this machine reaches {}:", name));
                    ui.add(
                        egui::TextEdit::singleline(text)
                            .hint_text("e.g. 192.168.1.20 or myserver.local:3032")
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(
                        RichText::new("Just the address, and :port if it isn't 3032. Leave it empty if that machine always calls this one.")
                            .small()
                            .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.add(primary("Save")).clicked() {
                            action = Some(Action::SetAddress { key: key.clone(), addr: text.trim().to_string() });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::OwnName { text } => {
                window("Rename this device").show(ctx, |ui| {
                    ui.label("The name other devices see for this one:");
                    ui.add(egui::TextEdit::singleline(text).desired_width(f32::INFINITY));
                    ui.label(
                        RichText::new("Paired devices pick it up right away (connections restart for a moment). Each of them can still show its own nickname for this device. Empty: use the host name.")
                            .small()
                            .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.add(primary("Save")).clicked() {
                            action = Some(Action::SetOwnName { name: text.trim().to_string() });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Nickname { key, announced, text } => {
                window("Rename a device").show(ctx, |ui| {
                    ui.label(format!("Its owner calls it “{}”. What should this machine call it?", announced));
                    ui.add(
                        egui::TextEdit::singleline(text)
                            .hint_text(announced.as_str())
                            .desired_width(f32::INFINITY),
                    );
                    ui.label(
                        RichText::new("Only this machine uses the nickname. Leave it empty to show the name its owner chose, including future changes to it.")
                            .small()
                            .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.add(primary("Save")).clicked() {
                            action = Some(Action::SetNickname { key: key.clone(), nickname: text.trim().to_string() });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Unshare { lib_id, lib_name, candidates, sel } => {
                window("Unshare a library").show(ctx, |ui| {
                    ui.label(format!("Stop sharing “{}” with:", lib_name));
                    let selected = candidates.get(*sel).map(|c| c.0.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("unshare_device")
                        .selected_text(selected)
                        .width(240.0)
                        .show_ui(ui, |ui| {
                            for (i, c) in candidates.iter().enumerate() {
                                ui.selectable_value(sel, i, &c.0);
                            }
                        });
                    ui.label(
                        RichText::new("The files stay on both machines. You can share it again later.")
                            .small()
                            .weak(),
                    );
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Unshare").color(RED)).clicked() {
                            if let Some(c) = candidates.get(*sel) {
                                action = Some(Action::Unshare { id: lib_id.clone(), device: c.1.clone() });
                            }
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::ConfirmRemove { id, name } => {
                window("Remove library").show(ctx, |ui| {
                    ui.label(format!("Remove “{}” from this machine? Its files are not deleted.", name));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new("Remove").color(RED)).clicked() {
                            action = Some(Action::Remove { id: id.clone() });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }
        }

        if close {
            self.browse = None;
        } else if let Some(start) = want_browse {
            self.browse = Some(Browse::new(&start));
        }
        if let Some(b) = self.browse.as_mut() {
            if let Some(choice) = browse_window(ctx, b) {
                if let Some(path) = choice {
                    if let Dialog::NewLibrary { folder, .. } | Dialog::Accept { folder, .. } | Dialog::Restore { folder, .. } =
                        &mut dialog
                    {
                        *folder = path.to_string_lossy().to_string();
                    }
                }
                self.browse = None;
            }
        }

        // Keep the dialog open unless it asked to close (an invite keeps its dialog while it waits).
        if close {
            self.stop_scan();
        }
        if !close {
            self.dialog = dialog;
        }
        if let Some(a) = action {
            self.perform(a);
        }
    }
}

// ---------------------------------------------------------------- main

// ---------------------------------------------------------------- tray icon and one window
//
// The tray icon is a small separate process (`rustsync-gui --tray`, no window at all), so it
// works the same on X11 and Wayland (which cannot hide a window) and costs almost nothing.
// "Open" starts the window as its own process; closing the window leaves the icon running.
// Start the icon hidden at login with `rustsync-gui --tray` (the icon's menu can set that up).

// The logo (see assets/): raw RGBA for the window and the tray, the PNG for the launcher entry.
const ICON_128: &[u8] = include_bytes!("../assets/icon-128.rgba");
#[cfg(all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android"))))]
const ICON_64: &[u8] = include_bytes!("../assets/icon-64.rgba");
#[cfg(all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android"))))]
const ICON_32: &[u8] = include_bytes!("../assets/icon-32.rgba");
#[cfg(not(target_os = "macos"))]
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

fn window_icon() -> egui::IconData {
    egui::IconData { rgba: ICON_128.to_vec(), width: 128, height: 128 }
}

/// A path as a quoted .desktop Exec argument; None if it holds characters that need escaping
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
fn write_if_changed(path: &Path, bytes: &[u8]) {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, bytes);
}

/// macOS has its own app bundles (see the README), so there is nothing to write there.
#[cfg(target_os = "macos")]
fn ensure_launcher(_config: &Path) {}

/// Put rustsync (with its icon) in the desktop's application menu, and let the window use that
/// icon on Wayland, which takes it from this launcher entry rather than from the window.
#[cfg(not(target_os = "macos"))]
fn ensure_launcher(config: &Path) {
    let data = match std::env::var("XDG_DATA_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".local").join("share"),
    };
    let icon = data.join("icons").join("hicolor").join("256x256").join("apps").join("rustsync.png");
    let desktop = data.join("applications").join("rustsync.desktop");
    let Ok(exe) = std::env::current_exe() else { return };
    let (Some(exe_q), Some(cfg_q)) =
        (desktop_quote(&exe.to_string_lossy()), desktop_quote(&config.to_string_lossy()))
    else {
        return;
    };
    let icon_s = icon.to_string_lossy().to_string();
    if icon_s.contains('\n') {
        return;
    }
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=RustSync\nComment=Simple encrypted file sync\nExec={} -c {}\nIcon={}\nTerminal=false\nCategories=Network;FileTransfer;\nStartupWMClass=rustsync\n",
        exe_q, cfg_q, icon_s
    );
    write_if_changed(&icon, ICON_PNG);
    write_if_changed(&desktop, text.as_bytes());
}

fn tray_lock_path(config: &Path) -> PathBuf {
    state_dir(config).join("tray.lock")
}

fn gui_sock_path(config: &Path) -> PathBuf {
    state_dir(config).join("gui.sock")
}

/// Take an exclusive lock on `path`; None if another process holds it. The lock lasts as long
/// as the returned file is kept open.
fn lock_file(path: &Path) -> Option<std::fs::File> {
    use std::os::unix::fs::DirBuilderExt;
    use std::os::unix::io::AsRawFd;
    if let Some(parent) = path.parent() {
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(parent);
    }
    let f = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(path).ok()?;
    // SAFETY: plain flock(2) on a descriptor we own.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        Some(f)
    } else {
        None
    }
}

/// If a window is already open, ask it to come forward. True if there was one.
fn raise_existing_window(config: &Path) -> bool {
    match UnixStream::connect(gui_sock_path(config)) {
        Ok(mut s) => {
            let _ = s.write_all(b"raise\n");
            true
        }
        Err(_) => false,
    }
}

/// Listen (in the window process) for "come forward" requests from the tray or a second launch.
fn start_raise_listener(config: &Path, slot: Arc<Mutex<Option<egui::Context>>>) {
    let path = gui_sock_path(config);
    let _ = std::fs::remove_file(&path); // stale socket from a crash
    let Ok(listener) = UnixListener::bind(&path) else { return };
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stream.is_err() {
                continue;
            }
            let ctx = slot.lock().unwrap().clone();
            if let Some(ctx) = ctx {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                ctx.request_repaint();
            }
        }
    });
}

/// Show the window: bring the open one forward, or start one.
fn open_window(config: &Path) {
    if raise_existing_window(config) {
        return;
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .arg("-c")
            .arg(config)
            .arg("--no-tray")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

#[cfg(not(all(feature = "tray", unix, not(any(target_os = "ios", target_os = "android")))))]
fn run_tray(_config: &Path) -> i32 {
    eprintln!("rustsync-gui: this build has no tray icon support (build with the \"tray\" feature)");
    1
}

#[cfg(not(all(feature = "tray", unix, not(any(target_os = "ios", target_os = "android")))))]
fn ensure_tray_running(_config: &Path) {}

/// Start the tray icon (menu bar icon on macOS) process if it is not running (the window does
/// this when it opens).
#[cfg(all(feature = "tray", unix, not(any(target_os = "ios", target_os = "android"))))]
fn ensure_tray_running(config: &Path) {
    use std::os::unix::process::CommandExt;
    match lock_file(&tray_lock_path(config)) {
        Some(f) => drop(f), // nobody holds it: not running (release it for the new process)
        None => return,
    }
    if let Ok(exe) = std::env::current_exe() {
        let _ = Command::new(exe)
            .arg("--tray")
            .arg("-c")
            .arg(config)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn();
    }
}

#[cfg(all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android"))))]
fn run_tray(config: &Path) -> i32 {
    tray::run(config)
}

#[cfg(all(feature = "tray", target_os = "macos"))]
fn run_tray(config: &Path) -> i32 {
    menubar::run(config)
}

/// On a Mac: a downloaded app that has not been moved runs from a temporary copy that is gone after
/// a restart, so nothing that starts it (or its helper) at login can work until it is moved.
#[cfg(target_os = "macos")]
fn app_location_problem() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        Some(
            "RustSync is running from a temporary location (macOS does that to a downloaded app that has not been moved). \
             Move RustSync.app into your Applications folder in Finder and open it from there; until then it cannot start itself at login."
                .to_string(),
        )
    } else {
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn app_location_problem() -> Option<String> {
    None
}

/// Is the icon set to start when the person logs in?
#[cfg(all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android"))))]
fn tray_login_on(_config: &Path) -> bool {
    tray::autostart_path().exists()
}

#[cfg(all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android"))))]
fn set_tray_login(config: &Path, on: bool) -> Result<(), String> {
    if tray::autostart_path().exists() != on {
        tray::toggle_autostart(config);
    }
    Ok(())
}

#[cfg(all(feature = "tray", target_os = "macos"))]
fn tray_login_on(config: &Path) -> bool {
    menubar::login_path(config).exists()
}

#[cfg(all(feature = "tray", target_os = "macos"))]
fn set_tray_login(config: &Path, on: bool) -> Result<(), String> {
    menubar::set_login(config, on)
}

#[cfg(not(any(
    all(feature = "tray", target_os = "macos"),
    all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android")))
)))]
fn tray_login_on(_config: &Path) -> bool {
    false
}

#[cfg(not(any(
    all(feature = "tray", target_os = "macos"),
    all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android")))
)))]
fn set_tray_login(_config: &Path, _on: bool) -> Result<(), String> {
    Ok(())
}

/// Does this build have a tray / menu bar icon at all?
fn tray_supported() -> bool {
    cfg!(any(
        all(feature = "tray", target_os = "macos"),
        all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android")))
    ))
}

#[cfg(all(feature = "tray", unix, not(any(target_os = "macos", target_os = "ios", target_os = "android"))))]
mod tray {
    use super::*;
    use ksni::blocking::TrayMethods;
    use ksni::menu::{CheckmarkItem, StandardItem};
    use ksni::MenuItem;
    use std::sync::mpsc::{self, RecvTimeoutError};

    enum Cmd {
        Open,
        Start,
        Stop,
        Restart,
        ToggleAutostart,
        ToggleService,
        Quit,
    }

    struct StatusTray {
        tx: mpsc::Sender<Cmd>,
        online: bool,
        devices: usize,
        devices_online: usize,
        offers: usize,
        autostart: bool,
        service: bool,
    }

    /// The logo, as the tray protocol wants it (ARGB). Grey and faded when the service is not
    /// running; a blue badge when libraries are being offered to you.
    fn tray_icon(rgba: &[u8], size: i32, online: bool, attention: bool) -> ksni::Icon {
        let n = size as usize;
        let mut data = Vec::with_capacity(rgba.len());
        for px in rgba.chunks_exact(4) {
            let (mut r, mut g, mut b, mut a) = (px[0], px[1], px[2], px[3]);
            if !online {
                let l = ((r as u32 * 30 + g as u32 * 59 + b as u32 * 11) / 100) as u8;
                (r, g, b, a) = (l, l, l, (a as u32 * 55 / 100) as u8);
            }
            data.extend_from_slice(&[a, r, g, b]);
        }
        if attention {
            let (cx, cy) = (n as f32 * 0.76, n as f32 * 0.24);
            let (ring, dot) = (n as f32 * 0.21, n as f32 * 0.15);
            for y in 0..n {
                for x in 0..n {
                    let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt();
                    let px = [255u8, 255, 255, 255];
                    let color = if d <= dot {
                        Some([255u8, 82, 201, 140])
                    } else if d <= ring {
                        Some(px)
                    } else {
                        None
                    };
                    if let Some(c) = color {
                        let i = (y * n + x) * 4;
                        data[i..i + 4].copy_from_slice(&c);
                    }
                }
            }
        }
        ksni::Icon { width: size, height: size, data }
    }

    impl ksni::Tray for StatusTray {
        fn id(&self) -> String {
            "rustsync".into()
        }

        fn title(&self) -> String {
            if !self.online {
                "RustSync (not running)".into()
            } else if self.offers > 0 {
                format!("RustSync: {} offer(s) waiting", self.offers)
            } else {
                "RustSync".into()
            }
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            let attention = self.offers > 0;
            vec![
                tray_icon(ICON_64, 64, self.online, attention),
                tray_icon(ICON_32, 32, self.online, attention),
            ]
        }

        // Left click.
        fn activate(&mut self, _x: i32, _y: i32) {
            let _ = self.tx.send(Cmd::Open);
        }

        fn menu(&self) -> Vec<MenuItem<Self>> {
            let mut m: Vec<MenuItem<Self>> = Vec::new();
            let summary = if self.online {
                format!("{} of {} devices online", self.devices_online, self.devices)
            } else {
                "RustSync is not running".to_string()
            };
            m.push(StandardItem { label: summary, enabled: false, ..Default::default() }.into());
            if self.offers > 0 {
                m.push(
                    StandardItem {
                        label: format!("{} offer(s) waiting…", self.offers),
                        activate: Box::new(|t: &mut Self| {
                            let _ = t.tx.send(Cmd::Open);
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            m.push(
                StandardItem {
                    label: "Open RustSync".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Cmd::Open);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            m.push(MenuItem::Separator);
            if self.online {
                m.push(
                    StandardItem {
                        label: "Restart the RustSync service".into(),
                        activate: Box::new(|t: &mut Self| {
                            let _ = t.tx.send(Cmd::Restart);
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
                m.push(
                    StandardItem {
                        label: "Stop the RustSync service".into(),
                        activate: Box::new(|t: &mut Self| {
                            let _ = t.tx.send(Cmd::Stop);
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            } else {
                m.push(
                    StandardItem {
                        label: "Start the RustSync service".into(),
                        activate: Box::new(|t: &mut Self| {
                            let _ = t.tx.send(Cmd::Start);
                        }),
                        ..Default::default()
                    }
                    .into(),
                );
            }
            m.push(
                CheckmarkItem {
                    label: "Start RustSync at boot / login".into(),
                    checked: self.service,
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Cmd::ToggleService);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            m.push(
                CheckmarkItem {
                    label: "Show this icon at login".into(),
                    checked: self.autostart,
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Cmd::ToggleAutostart);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            m.push(MenuItem::Separator);
            m.push(
                StandardItem {
                    label: "Quit the icon (syncing continues)".into(),
                    activate: Box::new(|t: &mut Self| {
                        let _ = t.tx.send(Cmd::Quit);
                    }),
                    ..Default::default()
                }
                .into(),
            );
            m
        }
    }

    // -------- start the icon at login (an XDG autostart entry)

    pub fn autostart_path() -> PathBuf {
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => home().join(".config"),
        };
        base.join("autostart").join("rustsync-tray.desktop")
    }

    pub fn toggle_autostart(config: &Path) {
        let path = autostart_path();
        if path.exists() {
            let _ = std::fs::remove_file(&path);
            return;
        }
        let Ok(exe) = std::env::current_exe() else { return };
        let (Some(exe_q), Some(cfg_q)) =
            (desktop_quote(&exe.to_string_lossy()), desktop_quote(&config.to_string_lossy()))
        else {
            eprintln!("rustsync-gui: the program or config path has characters that cannot go in an autostart entry");
            return;
        };
        let text = format!(
            "[Desktop Entry]\nType=Application\nName=RustSync\nComment=RustSync tray icon\nExec={} --tray -c {}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
            exe_q, cfg_q
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Err(e) = std::fs::write(&path, text) {
            eprintln!("rustsync-gui: cannot write {}: {}", path.display(), e);
        }
    }

    // -------- the tray process

    /// If the app was moved after the icon was set to start at login, the entry points at a
    /// program that is gone: point it at this copy.
    fn heal_autostart(config: &Path) {
        let path = autostart_path();
        let Ok(text) = std::fs::read_to_string(&path) else { return };
        let Some(start) = text.find("Exec=\"") else { return };
        let rest = &text[start + 6..];
        let Some(end) = rest.find('"') else { return };
        if !Path::new(&rest[..end]).exists() {
            let _ = std::fs::remove_file(&path);
            toggle_autostart(config);
        }
    }

    pub fn run(config: &Path) -> i32 {
        heal_autostart(config);
        // One icon per configuration.
        let Some(_lock) = lock_file(&tray_lock_path(config)) else { return 0 };
        let (tx, rx) = mpsc::channel::<Cmd>();
        // At login this often runs before the panel has registered its tray host, so a failure is
        // retried for a few minutes. If the desktop never gets one, give up (nothing is left
        // running invisibly). What happens is written to tray.log in the state directory.
        let log = state_dir(config).join("tray.log");
        let note = |msg: &str| {
            use std::io::Write as _;
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&log) {
                let _ = writeln!(f, "{}", msg);
            }
        };
        let _ = std::fs::remove_file(&log);
        note(&format!(
            "tray starting (DBUS_SESSION_BUS_ADDRESS={}, XDG_CURRENT_DESKTOP={})",
            std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|_| "(unset)".into()),
            std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "(unset)".into()),
        ));
        let mut handle = None;
        for attempt in 0..90 {
            // `spawn` consumes the tray, so build a fresh one for each try (same channel).
            let tray = StatusTray {
                tx: tx.clone(),
                online: false,
                devices: 0,
                devices_online: 0,
                offers: 0,
                autostart: autostart_path().exists(),
                service: service_installed(config),
            };
            match tray.spawn() {
                Ok(h) => {
                    note(&format!("tray icon shown (attempt {})", attempt + 1));
                    handle = Some(h);
                    break;
                }
                Err(e) => {
                    if attempt == 0 {
                        note(&format!("no tray host yet: {} (retrying for a few minutes)", e));
                    }
                    std::thread::sleep(Duration::from_secs(2));
                }
            }
        }
        let Some(handle) = handle else {
            note("gave up: this desktop shows no tray icons (or its tray host never started)");
            eprintln!("rustsync-gui: this desktop has no system tray to show the icon in; not starting it");
            return 1;
        };
        let sock = state_dir(config).join("control.sock");
        loop {
            match rx.recv_timeout(Duration::from_secs(2)) {
                Ok(Cmd::Open) => open_window(config),
                Ok(Cmd::Start) => {
                    let _ = Command::new(rustsync_bin())
                        .arg("-c")
                        .arg(config)
                        .arg("start")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
                Ok(Cmd::Stop) => {
                    let _ = call(&sock, &Request::Stop, None, |_| {});
                }
                Ok(Cmd::Restart) => {
                    let _ = Command::new(rustsync_bin())
                        .arg("-c")
                        .arg(config)
                        .arg("restart")
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .spawn();
                }
                Ok(Cmd::ToggleAutostart) => toggle_autostart(config),
                Ok(Cmd::ToggleService) => {
                    let _ = set_service_blocking(config, !service_installed(config));
                }
                Ok(Cmd::Quit) => break,
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            let status = fetch_status(&sock).ok();
            let autostart = autostart_path().exists();
            let service = service_installed(config);
            let alive = handle.update(|t| {
                match &status {
                    Some(s) => {
                        t.online = true;
                        t.devices = s.devices.len();
                        t.devices_online = s.devices.iter().filter(|d| d.online).count();
                        t.offers = s.offers.len();
                    }
                    None => {
                        t.online = false;
                        t.devices = 0;
                        t.devices_online = 0;
                        t.offers = 0;
                    }
                }
                t.autostart = autostart;
                t.service = service;
            });
            if alive.is_none() {
                break; // the tray service ended
            }
        }
        handle.shutdown().wait();
        0
    }
}

// ---------------------------------------------------------------- macOS menu bar icon
//
// The same idea as the tray icon on Linux: a small separate process (`rustsync-gui --tray`) that
// shows an icon at the top right of the screen, with no window and no Dock icon. Its menu opens
// the window. It is started when the window opens, and (if you ask for it on the Home page) at
// login through a launch agent.

#[cfg(all(feature = "tray", target_os = "macos"))]
mod menubar {
    use super::*;
    use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
    use winit::application::ApplicationHandler;
    use winit::event::{StartCause, WindowEvent};
    use winit::event_loop::{ActiveEventLoop, EventLoop, EventLoopProxy};
    use winit::platform::macos::{ActivationPolicy, EventLoopBuilderExtMacOS};
    use winit::window::WindowId;

    /// The icon in black on transparent (a "template": macOS colours it for light and dark bars).
    const BASE: &[u8] = include_bytes!("../assets/menubar-44.rgba");
    const SIZE: u32 = 44;

    fn login_label(config: &Path) -> String {
        match service_slug(config) {
            None => "com.rustsync.tray".to_string(),
            Some(slug) => format!("com.rustsync.tray.{}", slug),
        }
    }

    /// The launch agent that shows the icon at login.
    pub fn login_path(config: &Path) -> PathBuf {
        home().join("Library").join("LaunchAgents").join(format!("{}.plist", login_label(config)))
    }

    pub fn set_login(config: &Path, on: bool) -> Result<(), String> {
        let path = login_path(config);
        if !on {
            let _ = std::fs::remove_file(&path);
            return Ok(());
        }
        let exe = std::env::current_exe().map_err(|e| e.to_string())?;
        if let Some(problem) = app_location_problem() {
            return Err(problem);
        }
        let xml = |s: &str| s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
        let label = xml(&login_label(config));
        let exe = xml(&exe.to_string_lossy());
        let cfg = xml(&config.to_string_lossy());
        let text = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{label}</string>
  <key>ProgramArguments</key>
  <array><string>{exe}</string><string>--tray</string><string>-c</string><string>{cfg}</string></array>
  <key>RunAtLoad</key><true/>
  <key>LimitLoadToSessionType</key><string>Aqua</string>
</dict>
</plist>
"#
        );
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        std::fs::write(&path, text).map_err(|e| format!("cannot write {}: {}", path.display(), e))
    }

    enum Msg {
        Status(Option<StatusInfo>),
        Menu(MenuEvent),
    }

    /// What the icon and its menu show (they are only redrawn when this changes).
    #[derive(Clone, PartialEq)]
    struct Snap {
        online: bool,
        devices: usize,
        devices_online: usize,
        offers: usize,
        service: bool,
        login: bool,
    }

    struct Bar {
        config: PathBuf,
        sock: PathBuf,
        tray: Option<TrayIcon>,
        shown: Option<Snap>,
    }

    impl Bar {
        fn snap(&self, st: &Option<StatusInfo>) -> Snap {
            Snap {
                online: st.is_some(),
                devices: st.as_ref().map_or(0, |s| s.devices.len()),
                devices_online: st.as_ref().map_or(0, |s| s.devices.iter().filter(|d| d.online).count()),
                offers: st.as_ref().map_or(0, |s| s.offers.len()),
                service: service_installed(&self.config),
                login: login_path(&self.config).exists(),
            }
        }

        fn icon(snap: &Snap) -> Icon {
            let mut px = BASE.to_vec();
            if !snap.online {
                // Fainter while the service is not running.
                for p in px.chunks_exact_mut(4) {
                    p[3] = (p[3] as f32 * 0.4) as u8;
                }
            }
            if snap.offers > 0 {
                // A dot in the top right corner while something waits for an answer.
                let (cx, cy) = (36.0f32, 8.0f32);
                for y in 0..SIZE {
                    for x in 0..SIZE {
                        let d = ((x as f32 - cx).powi(2) + (y as f32 - cy).powi(2)).sqrt();
                        let i = ((y * SIZE + x) * 4) as usize;
                        if d <= 6.0 {
                            px[i + 3] = 255;
                        } else if d <= 9.0 {
                            px[i + 3] = 0;
                        }
                    }
                }
            }
            Icon::from_rgba(px, SIZE, SIZE).expect("the menu bar icon is 44x44 RGBA")
        }

        fn menu(snap: &Snap) -> Menu {
            let menu = Menu::new();
            let summary = if snap.online {
                format!("{} of {} devices online", snap.devices_online, snap.devices)
            } else {
                "RustSync is not running".to_string()
            };
            let _ = menu.append(&MenuItem::new(summary, false, None));
            if snap.offers > 0 {
                let _ = menu.append(&MenuItem::with_id("offers", format!("{} offer(s) waiting…", snap.offers), true, None));
            }
            let _ = menu.append(&MenuItem::with_id("open", "Open RustSync", true, None));
            let _ = menu.append(&PredefinedMenuItem::separator());
            if snap.online {
                let _ = menu.append(&MenuItem::with_id("restart", "Restart the RustSync service", true, None));
                let _ = menu.append(&MenuItem::with_id("stop", "Stop the RustSync service", true, None));
            } else {
                let _ = menu.append(&MenuItem::with_id("start", "Start the RustSync service", true, None));
            }
            let _ = menu.append(&CheckMenuItem::with_id("service", "Start RustSync at login", true, snap.service, None));
            let _ = menu.append(&CheckMenuItem::with_id("login", "Show this icon at login", true, snap.login, None));
            let _ = menu.append(&PredefinedMenuItem::separator());
            let _ = menu.append(&MenuItem::with_id("quit", "Quit the icon (syncing continues)", true, None));
            menu
        }

        fn apply(&mut self, snap: Snap) {
            if let Some(tray) = &self.tray {
                let _ = tray.set_icon_with_as_template(Some(Self::icon(&snap)), true);
                tray.set_menu(Some(Box::new(Self::menu(&snap))));
                let tip = if !snap.online {
                    "RustSync (not running)".to_string()
                } else if snap.offers > 0 {
                    format!("RustSync: {} offer(s) waiting", snap.offers)
                } else {
                    "RustSync".to_string()
                };
                let _ = tray.set_tooltip(Some(tip));
            }
            self.shown = Some(snap);
        }
    }

    impl ApplicationHandler<Msg> for Bar {
        fn resumed(&mut self, _event_loop: &ActiveEventLoop) {}

        fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, _event: WindowEvent) {}

        fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
            // The icon can only be made once the event loop runs.
            if matches!(cause, StartCause::Init) && self.tray.is_none() {
                let snap = self.snap(&None);
                let built = TrayIconBuilder::new()
                    .with_icon(Self::icon(&snap))
                    .with_icon_as_template(true)
                    .with_menu(Box::new(Self::menu(&snap)))
                    .with_tooltip("RustSync")
                    .build();
                match built {
                    Ok(tray) => {
                        self.tray = Some(tray);
                        self.shown = Some(snap);
                    }
                    Err(e) => {
                        eprintln!("rustsync-gui: cannot show the menu bar icon: {}", e);
                        event_loop.exit();
                    }
                }
            }
        }

        fn user_event(&mut self, event_loop: &ActiveEventLoop, msg: Msg) {
            match msg {
                Msg::Status(st) => {
                    let snap = self.snap(&st);
                    if self.shown.as_ref() != Some(&snap) {
                        self.apply(snap);
                    }
                }
                Msg::Menu(ev) => match ev.id.0.as_str() {
                    "open" | "offers" => open_window(&self.config),
                    "start" => {
                        let _ = Command::new(rustsync_bin())
                            .arg("-c")
                            .arg(&self.config)
                            .arg("start")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                    }
                    "restart" => {
                        let _ = Command::new(rustsync_bin())
                            .arg("-c")
                            .arg(&self.config)
                            .arg("restart")
                            .stdin(Stdio::null())
                            .stdout(Stdio::null())
                            .stderr(Stdio::null())
                            .spawn();
                    }
                    "stop" => {
                        let sock = self.sock.clone();
                        std::thread::spawn(move || {
                            let _ = call(&sock, &Request::Stop, None, |_| {});
                        });
                    }
                    "service" => {
                        let config = self.config.clone();
                        std::thread::spawn(move || {
                            let _ = set_service_blocking(&config, !service_installed(&config));
                        });
                    }
                    "login" => {
                        let _ = set_login(&self.config, !login_path(&self.config).exists());
                    }
                    "quit" => event_loop.exit(),
                    _ => {}
                },
            }
        }
    }

    /// If the app was moved after the icon was set to start at login, the launch agent points at a
    /// program that is gone: point it at this copy (it is read at the next login).
    fn heal_login(config: &Path) {
        let Ok(text) = std::fs::read_to_string(login_path(config)) else { return };
        let Some(start) = text.find("<key>ProgramArguments</key>") else { return };
        let rest = &text[start..];
        let Some(a) = rest.find("<string>") else { return };
        let rest = &rest[a + 8..];
        let Some(b) = rest.find("</string>") else { return };
        let old = rest[..b].replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&");
        if !Path::new(&old).exists() && app_location_problem().is_none() {
            let _ = set_login(config, true);
        }
    }

    pub fn run(config: &Path) -> i32 {
        heal_login(config);
        // One icon per configuration.
        let Some(_lock) = lock_file(&tray_lock_path(config)) else { return 0 };
        let mut builder = EventLoop::<Msg>::with_user_event();
        // No Dock icon and no application menu: this is only the icon at the top right.
        builder.with_activation_policy(ActivationPolicy::Accessory);
        builder.with_default_menu(false);
        let event_loop = match builder.build() {
            Ok(l) => l,
            Err(e) => {
                eprintln!("rustsync-gui: cannot start the menu bar icon: {}", e);
                return 1;
            }
        };
        let proxy: EventLoopProxy<Msg> = event_loop.create_proxy();
        let menu_proxy = proxy.clone();
        MenuEvent::set_event_handler(Some(move |ev| {
            let _ = menu_proxy.send_event(Msg::Menu(ev));
        }));
        let sock = state_dir(config).join("control.sock");
        {
            let sock = sock.clone();
            std::thread::spawn(move || loop {
                let st = fetch_status(&sock).ok();
                if proxy.send_event(Msg::Status(st)).is_err() {
                    break; // the icon was quit
                }
                std::thread::sleep(Duration::from_secs(2));
            });
        }
        let mut bar = Bar { config: config.to_path_buf(), sock, tray: None, shown: None };
        if let Err(e) = event_loop.run_app(&mut bar) {
            eprintln!("rustsync-gui: the menu bar icon stopped: {}", e);
            return 1;
        }
        0
    }
}

fn config_from_args() -> PathBuf {
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "-c" | "--config" if i + 1 < args.len() => return PathBuf::from(&args[i + 1]),
            a if a.starts_with("--config=") => return PathBuf::from(&a["--config=".len()..]),
            _ => {}
        }
        i += 1;
    }
    default_config()
}

/// Should the window open through X11 on a Wayland desktop? (Yes, unless the config says no.)
#[cfg(all(unix, not(target_os = "macos")))]
fn gui_x11(config: &Path) -> bool {
    std::fs::read_to_string(config)
        .ok()
        .and_then(|t| t.parse::<toml::Table>().ok())
        .and_then(|t| t.get("gui")?.get("x11")?.as_bool())
        .unwrap_or(true)
}

/// Is this a Wayland desktop session (so the choice above matters)?
#[cfg(all(unix, not(target_os = "macos")))]
fn on_wayland() -> bool {
    std::env::var("XDG_SESSION_TYPE").map_or(false, |v| v.eq_ignore_ascii_case("wayland"))
}

fn main() -> eframe::Result<()> {
    // A config from before 0.6 is YAML: convert it once (the same as the program does).
    let wanted = absolute(&config_from_args());
    let config = rustsync::config::resolve(&wanted).unwrap_or(wanted);
    let args: Vec<String> = std::env::args().collect();
    let flag = |f: &str| args.iter().any(|a| a == f);

    // `--tray`: only the tray icon, no window (what you start at login).
    if flag("--tray") {
        std::process::exit(run_tray(&config));
    }
    // One window at a time: bring the open one forward instead of opening another.
    if raise_existing_window(&config) {
        return Ok(());
    }
    // Opening the window also makes sure the tray icon is there (it stays after the window closes).
    if !flag("--no-tray") {
        ensure_tray_running(&config);
    }
    let ctx_slot: Arc<Mutex<Option<egui::Context>>> = Arc::new(Mutex::new(None));
    start_raise_listener(&config, ctx_slot.clone());
    ensure_launcher(&config);

    // On Wayland, a window that talks Wayland itself has to draw its own title bar and buttons
    // (GNOME does not draw them for it), which then differ from every other window. Going through
    // X11 (XWayland) lets the desktop draw them. Setting in Settings; on unless turned off.
    #[cfg(all(unix, not(target_os = "macos")))]
    if gui_x11(&config) && std::env::var_os("DISPLAY").is_some() {
        std::env::remove_var("WAYLAND_DISPLAY");
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("RustSync")
            .with_app_id("rustsync")
            .with_icon(window_icon())
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([700.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "rustsync",
        options,
        Box::new(move |cc| {
            configure_style(&cc.egui_ctx);
            *ctx_slot.lock().unwrap() = Some(cc.egui_ctx.clone());
            Ok(Box::new(App::new(cc, config)))
        }),
    )
}
