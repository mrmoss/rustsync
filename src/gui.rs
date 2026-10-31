//! A small desktop front end for rustsync.
//!
//! It does nothing the command line can't do: it talks to the running rustsync service over
//! the same local control socket (JSON lines) that `rustsync status | invite | join | create |
//! share | accept ...` use. Close this window and the service keeps syncing.
//!
//! Usage:  rustsync-gui [-c /path/to/config.yaml] [--tray | --no-tray]
//!   --tray     only the tray icon, no window (start this at login; the icon opens the window)
//!   --no-tray  window only; do not start the tray icon
//! (default config: ~/.config/rustsync/config.yaml, like the command line)

use eframe::egui::{self, Align, Color32, CornerRadius, Layout, RichText, Stroke, Vec2};
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
    Forget { device: String },
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
}

#[derive(Deserialize, Debug, Clone)]
struct DeviceInfo {
    key: String,
    name: String,
    online: bool,
    addr: Option<String>,
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
    let stream = UnixStream::connect(sock).map_err(|_| "rustsync is not running".to_string())?;
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
            Err(e) => return Err(format!("unexpected answer from rustsync: {}", e)),
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
    info.ok_or_else(|| "rustsync did not answer".to_string())
}

// ---------------------------------------------------------------- finding things

fn home() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("."))
}

fn default_config() -> PathBuf {
    home().join(".config").join("rustsync").join("config.yaml")
}

/// Name of the systemd user service for this config (the same rule as `rustsync install-service`).
fn unit_name(config: &Path) -> String {
    let abs = absolute(config);
    if abs == default_config() {
        return "rustsync.service".to_string();
    }
    let slug: String = abs
        .with_extension("")
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '-' })
        .collect();
    format!("rustsync-{}.service", slug.trim_matches('-'))
}

/// Is rustsync set to start by itself (a systemd user service)?
fn service_installed(config: &Path) -> bool {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(d) if !d.is_empty() => PathBuf::from(d),
        _ => home().join(".config"),
    };
    base.join("systemd").join("user").join(unit_name(config)).exists()
}

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
        if let Ok(v) = serde_yaml::from_str::<serde_yaml::Value>(&text) {
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
    /// The recovery code of one library (kept out of the status bar on purpose).
    Recovery(Result<String, String>),
}

#[derive(Default, PartialEq, Clone, Copy)]
enum Page {
    #[default]
    Libraries,
    Devices,
    ThisDevice,
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
    Restore { code: String, folder: String, device: usize },
    Invite { host: String, code: Option<String>, note: String, error: Option<String>, running: bool, paired: bool },
    Join { code: String },
    ConfirmForget { key: String, name: String },
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
    Restore { code: String, folder: String, device: usize },
    Join { token: String },
    Forget { key: String },
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

        App {
            ctx: cc.egui_ctx.clone(),
            config,
            sock,
            tx,
            rx,
            status: None,
            offline_reason: String::new(),
            starting_since: None,
            page: Page::Libraries,
            dialog: Dialog::None,
            message: None,
            invite_cancel: Arc::new(Mutex::new(None)),
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

    /// Turn "start rustsync by itself" on or off (a systemd user service).
    fn set_service(&mut self, on: bool) {
        let (tx, ctx, config) = (self.tx.clone(), self.ctx.clone(), self.config.clone());
        std::thread::spawn(move || {
            let ev = match set_service_blocking(&config, on) {
                Ok(()) => Ev::Op {
                    ok: true,
                    msg: if on {
                        "rustsync will start by itself".to_string()
                    } else {
                        "rustsync no longer starts by itself, and has been stopped".to_string()
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
                    "rustsync did not start; see daemon.log in {}",
                    state_dir(&config).display()
                )),
                Err(e) => Some(format!(
                    "cannot run {} ({}). Put rustsync next to this program, or set RUSTSYNC_BIN.",
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
                _ => {}
            });
            let _ = tx.send(Ev::InviteEnded(res));
            let _ = tx.send(Ev::Status(fetch_status(&sock)));
            ctx.request_repaint();
        });
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
            Ev::Recovery(res) => {
                if let Dialog::Recovery { code, error, .. } = &mut self.dialog {
                    match res {
                        Ok(c) => *code = Some(c),
                        Err(e) => *error = Some(e),
                    }
                }
            }
            Ev::InviteEnded(res) => {
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
            Action::Restore { code, folder, device } => {
                let key = self.status.as_ref().and_then(|s| s.devices.get(device)).map(|d| d.key.clone());
                if let Some(device) = key {
                    self.run(Request::Restore { code, folder, device });
                }
            }
            Action::Join { token } => self.run(Request::Join { token }),
            Action::Forget { key } => self.run(Request::Forget { device: key }),
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

const BLUE: Color32 = Color32::from_rgb(105, 175, 255);
const GREEN: Color32 = Color32::from_rgb(105, 210, 155);
const RED: Color32 = Color32::from_rgb(235, 110, 110);
const MUTED: Color32 = Color32::from_rgb(145, 165, 192);
const CARD: Color32 = Color32::from_rgb(27, 33, 46);

fn configure_style(ctx: &egui::Context) {
    let mut style = (*ctx.style()).clone();
    style.visuals = egui::Visuals::dark();
    style.visuals.panel_fill = Color32::from_rgb(20, 25, 36);
    style.visuals.window_fill = Color32::from_rgb(27, 33, 46);
    style.visuals.extreme_bg_color = Color32::from_rgb(14, 18, 27);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgb(39, 48, 65);
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(54, 68, 91);
    style.visuals.widgets.active.bg_fill = Color32::from_rgb(55, 112, 190);
    style.visuals.selection.bg_fill = Color32::from_rgb(50, 115, 190);
    style.spacing.item_spacing = Vec2::new(10.0, 10.0);
    style.spacing.button_padding = Vec2::new(14.0, 8.0);
    ctx.set_style(style);
}

fn card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    let style = (*ui.style()).clone();
    egui::Frame::group(&style)
        .fill(CARD)
        .stroke(Stroke::new(1.0, Color32::from_rgb(45, 54, 72)))
        .corner_radius(CornerRadius::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn nav_button(ui: &mut egui::Ui, icon: &str, label: &str, selected: bool) -> bool {
    let fill = if selected { Color32::from_rgb(38, 68, 105) } else { Color32::TRANSPARENT };
    let text_color = if selected { Color32::from_rgb(145, 200, 255) } else { Color32::from_rgb(205, 215, 230) };
    let button = egui::Button::new(RichText::new(format!("{icon}   {label}")).color(text_color))
        .fill(fill)
        .stroke(if selected { Stroke::new(1.0, Color32::from_rgb(60, 120, 190)) } else { Stroke::NONE })
        .corner_radius(CornerRadius::same(7));
    ui.add_sized([ui.available_width(), 38.0], button).clicked()
}

fn heading(ui: &mut egui::Ui, title: &str, subtitle: &str) {
    ui.heading(RichText::new(title).size(28.0).strong());
    ui.label(RichText::new(subtitle).color(MUTED));
    ui.add_space(12.0);
}

fn section(ui: &mut egui::Ui, text: &str) {
    ui.label(RichText::new(text).size(11.0).strong().color(Color32::from_rgb(130, 150, 178)));
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
        Color32::from_rgb(235, 185, 90)
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

fn dot(online: bool) -> RichText {
    RichText::new("●").color(if online { GREEN } else { Color32::from_rgb(110, 120, 138) })
}

// ---------------------------------------------------------------- screens

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
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

        egui::TopBottomPanel::top("top_bar").exact_height(60.0).show(ctx, |ui| {
            ui.horizontal_centered(|ui| {
                ui.add_space(8.0);
                ui.label(RichText::new("◆").size(24.0).color(BLUE));
                ui.vertical(|ui| {
                    ui.label(RichText::new("rustsync").size(19.0).strong());
                    ui.label(RichText::new("Simple, encrypted file sync").size(11.0).weak());
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add_space(8.0);
                    match &self.status {
                        Some(s) => ui.label(RichText::new(format!("●  {} is running", s.name)).color(GREEN)),
                        None => ui.label(RichText::new("●  not running").color(RED)),
                    };
                });
            });
        });

        egui::TopBottomPanel::bottom("status_bar").exact_height(32.0).show(ctx, |ui| {
            ui.horizontal_centered(|ui| match &self.message {
                Some((ok, msg)) => {
                    ui.label(RichText::new("●").color(if *ok { GREEN } else { RED }));
                    ui.label(RichText::new(msg).small());
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.small_button("Dismiss").clicked() {
                            self.message = None;
                        }
                    });
                }
                None => {
                    ui.label(RichText::new("●").color(MUTED));
                    ui.label(RichText::new(format!("Config: {}", self.config.display())).small().weak());
                }
            });
        });

        egui::SidePanel::left("sidebar")
            .resizable(false)
            .exact_width(180.0)
            .frame(egui::Frame::side_top_panel(&ctx.style()).fill(Color32::from_rgb(17, 22, 32)))
            .show(ctx, |ui| {
                ui.add_space(16.0);
                section(ui, "SYNC");
                ui.add_space(6.0);
                if nav_button(ui, "▦", "Libraries", self.page == Page::Libraries) {
                    self.page = Page::Libraries;
                }
                if nav_button(ui, "⌂", "Devices", self.page == Page::Devices) {
                    self.page = Page::Devices;
                }
                if nav_button(ui, "⚙", "This device", self.page == Page::ThisDevice) {
                    self.page = Page::ThisDevice;
                }
                if let Some(s) = &self.status {
                    let waiting = s.offers.len();
                    if waiting > 0 {
                        ui.add_space(16.0);
                        ui.label(RichText::new(format!("{} waiting for you", waiting)).color(BLUE).small());
                    }
                }
            });

        egui::CentralPanel::default()
            .frame(egui::Frame::central_panel(&ctx.style()).inner_margin(egui::Margin::same(24)))
            .show(ctx, |ui| {
                if self.status.is_none() {
                    self.show_offline(ui);
                    return;
                }
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| match self.page {
                    Page::Libraries => self.show_libraries(ui),
                    Page::Devices => self.show_devices(ui),
                    Page::ThisDevice => self.show_this_device(ui),
                });
            });

        self.dialogs(ctx);
    }
}

impl App {
    fn show_offline(&mut self, ui: &mut egui::Ui) {
        heading(ui, "rustsync is not running", "The background service does the syncing; this window only controls it.");
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
            } else if ui.button(RichText::new("Start rustsync").strong()).clicked() {
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
                        ui.label(RichText::new("＋").size(22.0).color(BLUE));
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
                if ui.button("＋  New library").clicked() {
                    self.dialog = Dialog::NewLibrary { name: String::new(), folder: String::new() };
                }
                if ui.add_enabled(!status.devices.is_empty(), egui::Button::new("Restore from backup…")).clicked() {
                    self.dialog = Dialog::Restore { code: String::new(), folder: String::new(), device: 0 };
                }
                if ui
                    .button("Refresh")
                    .on_hover_text("Ask connected devices for their library lists again and look for backups")
                    .clicked()
                {
                    self.perform(Action::Refresh);
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
                    ui.label(RichText::new("▦").size(24.0).color(BLUE));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&l.name).strong().size(16.0));
                        ui.label(RichText::new(&l.folder).small().weak());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("id {}", short(&l.id))).small().weak().monospace());
                            if ui.small_button("Copy full id").clicked() {
                                ui.ctx().copy_text(l.id.clone());
                                self.say(true, "Library id copied");
                            }
                        });
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Remove").clicked() {
                            self.dialog = Dialog::ConfirmRemove { id: l.id.clone(), name: l.name.clone() };
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
                if !l.syncing_with.is_empty() {
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
                    self.dialog = Dialog::Invite {
                        host: String::new(),
                        code: None,
                        note: String::new(),
                        error: None,
                        running: false,
                        paired: false,
                    };
                }
                if ui.button("Enter a pairing code").clicked() {
                    self.dialog = Dialog::Join { code: String::new() };
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
                    ui.label(dot(d.online));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&d.name).strong());
                        let mut line = format!("{}…", short(&d.key));
                        if let Some(a) = &d.addr {
                            line.push_str(&format!("  ·  {}", a));
                        }
                        line.push_str(if d.online { "  ·  online" } else { "  ·  offline" });
                        ui.label(RichText::new(line).small().weak());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if ui.button("Forget").clicked() {
                            self.dialog = Dialog::ConfirmForget { key: d.key.clone(), name: d.name.clone() };
                        }
                    });
                });
            });
        }
    }

    fn show_this_device(&mut self, ui: &mut egui::Ui) {
        let Some(status) = self.status.clone() else { return };
        heading(ui, "This device", "How this machine identifies itself to the others.");

        card(ui, |ui| {
            ui.label(RichText::new("Name").small().weak());
            ui.label(RichText::new(&status.name).strong().size(16.0));
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

        ui.add_space(8.0);
        card(ui, |ui| {
            ui.label(RichText::new("Service").strong());
            ui.label("Syncing continues in the background when you close this window.");
            let mut auto = service_installed(&self.config);
            if ui
                .checkbox(&mut auto, "Start rustsync automatically (when you log in, or at boot)")
                .on_hover_text("A systemd user service. Turning it off also stops rustsync now.")
                .changed()
            {
                self.set_service(auto);
            }
            if ui.button("Stop rustsync").clicked() {
                self.run(Request::Stop);
            }
        });
    }

    // ------------------------------------------------------------ dialogs

    fn dialogs(&mut self, ctx: &egui::Context) {
        let mut dialog = std::mem::take(&mut self.dialog);
        let devices: Vec<DeviceInfo> = self.status.as_ref().map(|s| s.devices.clone()).unwrap_or_default();
        let mut action: Option<Action> = None;
        let mut close = false;

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
                    ui.label("Folder to sync (full path)");
                    ui.add(
                        egui::TextEdit::singleline(folder)
                            .hint_text("e.g. ~/Photos")
                            .desired_width(f32::INFINITY),
                    );
                    let folder_full = expand(folder);
                    let valid_folder = folder_full.starts_with('/');
                    if !folder.trim().is_empty() && !valid_folder {
                        ui.label(RichText::new("Enter a full path, like /home/me/Photos or ~/Photos").color(RED).small());
                    }
                    ui.horizontal(|ui| {
                        let ok = !name.trim().is_empty() && valid_folder;
                        if ui.add_enabled(ok, egui::Button::new(RichText::new("Create").strong())).clicked() {
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
                        ui.label("Store it in this folder (full path):");
                    } else {
                        ui.label(format!("Sync “{}” into this folder (full path):", name));
                    }
                    ui.add(egui::TextEdit::singleline(folder).desired_width(f32::INFINITY));
                    let folder_full = expand(folder);
                    let mut valid = folder_full.starts_with('/');
                    if !valid {
                        ui.label(RichText::new("Enter a full path, like /home/me/Photos or ~/Photos").color(RED).small());
                    }
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
                        if ui.add_enabled(valid, egui::Button::new(RichText::new("Accept").strong())).clicked() {
                            action = Some(Action::Accept { offer: offer_id.clone(), folder: folder_full.clone(), quota: quota_bytes });
                            close = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    });
                });
            }

            Dialog::Invite { host, code, note, error, running, paired } => {
                window("Show a pairing code").show(ctx, |ui| {
                    if !*running && code.is_none() {
                        ui.label("Address the other machine will use to reach this one:");
                        ui.add(
                            egui::TextEdit::singleline(host)
                                .hint_text("e.g. 192.168.1.20 or myserver.example.com")
                                .desired_width(f32::INFINITY),
                        );
                        ui.label(RichText::new("Add :port if it isn't the default (3032).").small().weak());
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
                    ui.label("Restore into this empty folder (full path; a kit makes one subfolder per library):");
                    ui.add(egui::TextEdit::singleline(folder).hint_text("e.g. ~/Restored").desired_width(f32::INFINITY));
                    let folder_full = expand(folder);
                    let valid = folder_full.starts_with('/') && (code.trim().starts_with("rsr1-") || code.trim().starts_with("rsk1-"));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(valid, egui::Button::new(RichText::new("Restore").strong())).clicked() {
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

            Dialog::Join { code } => {
                window("Enter a pairing code").show(ctx, |ui| {
                    ui.label("Paste the code shown on the other machine (“Show a pairing code”):");
                    ui.add(
                        egui::TextEdit::multiline(code)
                            .font(egui::TextStyle::Monospace)
                            .desired_rows(3)
                            .desired_width(f32::INFINITY),
                    );
                    ui.horizontal(|ui| {
                        let ok = !code.trim().is_empty();
                        if ui.add_enabled(ok, egui::Button::new(RichText::new("Pair").strong())).clicked() {
                            action = Some(Action::Join { token: code.trim().to_string() });
                            close = true;
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

        // Keep the dialog open unless it asked to close (an invite keeps its dialog while it waits).
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
#[cfg(all(feature = "tray", target_os = "linux"))]
const ICON_64: &[u8] = include_bytes!("../assets/icon-64.rgba");
#[cfg(all(feature = "tray", target_os = "linux"))]
const ICON_32: &[u8] = include_bytes!("../assets/icon-32.rgba");
const ICON_PNG: &[u8] = include_bytes!("../assets/icon-256.png");

fn window_icon() -> egui::IconData {
    egui::IconData { rgba: ICON_128.to_vec(), width: 128, height: 128 }
}

/// A path as a quoted .desktop Exec argument; None if it holds characters that need escaping
/// rules we do not want to get wrong.
fn desktop_quote(s: &str) -> Option<String> {
    if s.chars().any(|c| matches!(c, '"' | '`' | '$' | '\\' | '%' | '\n')) {
        None
    } else {
        Some(format!("\"{}\"", s))
    }
}

fn write_if_changed(path: &Path, bytes: &[u8]) {
    if std::fs::read(path).ok().as_deref() == Some(bytes) {
        return;
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, bytes);
}

/// Put rustsync (with its icon) in the desktop's application menu, and let the window use that
/// icon on Wayland, which takes it from this launcher entry rather than from the window.
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
        "[Desktop Entry]\nType=Application\nName=rustsync\nComment=Simple encrypted file sync\nExec={} -c {}\nIcon={}\nTerminal=false\nCategories=Network;FileTransfer;\nStartupWMClass=rustsync\n",
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

#[cfg(not(all(feature = "tray", target_os = "linux")))]
fn run_tray(_config: &Path) -> i32 {
    eprintln!("rustsync-gui: this build has no tray icon support (it is Linux-only, feature \"tray\")");
    1
}

#[cfg(not(all(feature = "tray", target_os = "linux")))]
fn ensure_tray_running(_config: &Path) {}

/// Start the tray icon process if it is not running (the window does this when it opens).
#[cfg(all(feature = "tray", target_os = "linux"))]
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

#[cfg(all(feature = "tray", target_os = "linux"))]
fn run_tray(config: &Path) -> i32 {
    tray::run(config)
}

#[cfg(all(feature = "tray", target_os = "linux"))]
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
                        Some([255u8, 45, 156, 255])
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
                "rustsync (not running)".into()
            } else if self.offers > 0 {
                format!("rustsync: {} offer(s) waiting", self.offers)
            } else {
                "rustsync".into()
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
                "rustsync is not running".to_string()
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
                    label: "Open rustsync".into(),
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
                        label: "Stop the rustsync service".into(),
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
                        label: "Start the rustsync service".into(),
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
                    label: "Start rustsync at boot / login".into(),
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

    fn autostart_path() -> PathBuf {
        let base = match std::env::var("XDG_CONFIG_HOME") {
            Ok(d) if !d.is_empty() => PathBuf::from(d),
            _ => home().join(".config"),
        };
        base.join("autostart").join("rustsync-tray.desktop")
    }

    fn toggle_autostart(config: &Path) {
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
            "[Desktop Entry]\nType=Application\nName=rustsync\nComment=rustsync tray icon\nExec={} --tray -c {}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
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

    pub fn run(config: &Path) -> i32 {
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

fn main() -> eframe::Result<()> {
    let config = absolute(&config_from_args());
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

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("rustsync")
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
