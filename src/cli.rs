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
    converse(ctl, req, None).await
}

/// Draw `text` as a QR code in the terminal (on stderr, so stdout stays clean). Terminals are
/// usually dark, where the modules must be drawn "inverted" for a phone to read them.
fn print_qr(text: &str, light_terminal: bool) {
    use qrcode::render::unicode::Dense1x2;
    use qrcode::{EcLevel, QrCode};
    let Ok(code) = QrCode::with_error_correction_level(text.as_bytes(), EcLevel::L) else {
        eprintln!("(this code is too long for a QR code; paste it instead)");
        return;
    };
    let (dark, light) = if light_terminal {
        (Dense1x2::Dark, Dense1x2::Light)
    } else {
        (Dense1x2::Light, Dense1x2::Dark)
    };
    let img = code.render::<Dense1x2>().dark_color(dark).light_color(light).quiet_zone(true).build();
    eprintln!();
    eprintln!("Or scan this with the rustsync app on the other device:");
    eprintln!("{}", img);
    eprintln!();
}

/// Show the verification code of a pairing attempt and ask whether to go on. No answer (for
/// example no terminal) counts as no.
fn verify_prompt(name: &str, fingerprint: &str, code: &str, joining: bool) -> bool {
    eprintln!();
    eprintln!("  Verification code:  {}", code);
    eprintln!("  Device:             {}", name);
    eprintln!("  Its fingerprint:    {}", fingerprint);
    eprintln!();
    if joining {
        eprintln!("The machine that showed you the pairing code must now show exactly this verification code.");
    } else {
        eprintln!("The machine you are pairing with must show exactly this verification code.");
        eprintln!("If you did not just start pairing on a machine of yours, or the codes differ, answer no.");
    }
    confirm("Do the codes match, and do you want to pair with this device?", false).unwrap_or(false)
}

/// Send a request and print what comes back. `qr` (Some(light terminal)) draws the `Out` lines
/// as QR codes: used by `invite`, whose only `Out` line is the pairing code.
async fn converse(ctl: &Ctl, req: &Request, qr: Option<bool>) -> Result<Option<StatusInfo>> {
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
            Reply::Out(s) => {
                println!("{}", s);
                if let Some(light) = qr {
                    print_qr(&s, light);
                }
            }
            Reply::Note(s) => eprintln!("{}", s),
            Reply::Verify { name, fingerprint, code, joining } => {
                let accept =
                    tokio::task::spawn_blocking(move || verify_prompt(&name, &fingerprint, &code, joining))
                        .await
                        .unwrap_or(false);
                w.write_all(serde_json::to_string(&Request::PairDecision { accept })?.as_bytes()).await?;
                w.write_all(b"\n").await?;
            }
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
            "  {:<16} {}  {}{}{}{}",
            if d.nickname.is_empty() { d.name.clone() } else { format!("{} (\"{}\")", d.nickname, d.announced) },
            if d.online { "online " } else { "offline" },
            &d.key[..8.min(d.key.len())],
            d.addr.as_ref().map(|a| format!("  {}", a)).unwrap_or_default(),
            if d.online || d.last_seen == 0 {
                String::new()
            } else {
                format!("  (last seen {})", crate::activity::ago(crate::activity::unix_now(), d.last_seen))
            },
            if d.online && (d.up_rate > 0 || d.down_rate > 0) {
                format!(
                    "  down {}  up {}",
                    crate::proto::fmt_rate(d.down_rate),
                    crate::proto::fmt_rate(d.up_rate)
                )
            } else {
                String::new()
            }
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
        if !l.sync.is_empty() {
            for si in &l.sync {
                line.push_str(&format!("\n  {:<16} syncing with {}: {}", "", si.device, si.summary()));
            }
        } else if !l.syncing_with.is_empty() {
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
async fn stop_running(config_path: &Path, cfg: &Config) {
    if UnixStream::connect(cfg.socket_path()).await.is_ok() {
        let ctl = Ctl { sock: cfg.socket_path(), config: config_path.to_path_buf() };
        let _ = call(&ctl, &Request::Stop).await;
        wait_socket(cfg, false, 50).await;
    }
}

/// The program a service entry will run, as written in the entry itself: the first quoted or
/// `<string>` word after the marker.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn entry_program(text: &str, after: &str, open: &str, close: &str) -> Option<String> {
    let rest = &text[text.find(after)? + after.len()..];
    let rest = &rest[rest.find(open)? + open.len()..];
    Some(rest[..rest.find(close)?].to_string())
}

/// A downloaded Mac app that has not been moved is run by macOS from a temporary copy that
/// disappears at the next restart: a start-at-login entry pointing there can never work.
#[cfg(target_os = "macos")]
fn unstable_exe(exe: &Path) -> Option<String> {
    if exe.to_string_lossy().contains("/AppTranslocation/") {
        Some(
            "RustSync is running from a temporary location (macOS does that to a downloaded app that has not been \
             moved), which is gone after a restart. Move RustSync.app into your Applications folder in Finder, \
             open it from there, and turn this on again."
                .to_string(),
        )
    } else {
        None
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
        if let Ok(unit) = std::fs::read_to_string(unit_path(&name)) {
            if let Some(prog) = entry_program(&unit, "ExecStart=", "\"", "\"") {
                if !Path::new(&prog).exists() {
                    // rustsync was moved or reinstalled somewhere else: point the service at this copy.
                    println!("The service {} runs {} which is gone; updating it to use this copy.", name, prog);
                    install(config_path, cfg, true).await?;
                    return Ok(true);
                }
            }
        }
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
        if let Ok(plist) = std::fs::read_to_string(plist_path(&label)) {
            if let Some(prog) = entry_program(&plist, "<key>ProgramArguments</key>", "<string>", "</string>") {
                if !Path::new(&prog).exists() {
                    // The app was moved: point the launch agent at the copy that is running now.
                    if let Some(problem) = unstable_exe(&std::env::current_exe()?) {
                        bail!("the launch agent {} runs {} which is gone. {}", label, prog, problem);
                    }
                    println!("The launch agent {} runs {} which is gone; updating it to use this copy.", label, prog);
                    install(config_path, cfg, true).await?;
                    return Ok(true);
                }
            }
        }
        // Loaded already? Kick it. Otherwise load it first (and make sure it is not switched off).
        if !launchctl(&["kickstart", &target]) {
            launchctl(&["enable", &target]);
            launchctl(&["bootstrap", &domain(), &plist_path(&label).to_string_lossy()]);
            launchctl(&["kickstart", &target]);
        }
        if wait_socket(cfg, true, 100).await {
            println!("rustsync started (launch agent {}).", label);
            return Ok(true);
        }
        bail!(
            "the launch agent {} did not start (check System Settings, General, Login Items & Extensions, and {})",
            label,
            cfg.log_path().display()
        )
    }

    pub async fn install(config_path: &Path, cfg: &Config, _yes: bool) -> Result<()> {
        if !supported() {
            bail!("launchctl is not available");
        }
        cfg.ensure_dir()?;
        let abs = std::path::absolute(config_path)?;
        let exe = std::env::current_exe()?;
        if let Some(problem) = unstable_exe(&exe) {
            bail!("{}", problem);
        }
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
        launchctl(&["enable", &target]); // in case it was switched off before
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
    match svc::start(config_path, cfg).await {
        Ok(true) => return Ok(()),
        Ok(false) => {}
        Err(e) => {
            // A broken start-at-login entry must not stop you from starting by hand.
            eprintln!("Warning: {:#}", e);
            eprintln!("Starting rustsync directly instead. To repair the entry:  rustsync -c {} install-service", config_path.display());
            return spawn_daemon(config_path, cfg).await;
        }
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

/// Stop the daemon (if it is running), wait for it to let go of its socket, and start it again:
/// through the service when it is set up to start by itself, otherwise as a background copy.
async fn restart(config_path: &Path, cfg: &Config) -> Result<()> {
    if UnixStream::connect(cfg.socket_path()).await.is_ok() {
        let ctl = Ctl { sock: cfg.socket_path(), config: config_path.to_path_buf() };
        let _ = call(&ctl, &Request::Stop).await;
        if !wait_socket(cfg, false, 100).await {
            bail!("rustsync did not stop; see {}", cfg.log_path().display());
        }
    }
    start(config_path, cfg).await
}

// ---------------------------------------------------------------- commands

pub async fn run(config_path: &Path, cmd: Cmd) -> Result<()> {
    // `start` also works on a brand-new machine (and from the GUI): it creates the config.
    if matches!(cmd, Cmd::Start | Cmd::Restart | Cmd::InstallService { .. }) {
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
        Cmd::Restart => restart(config_path, &cfg).await,
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
        Cmd::Invite { host, no_qr, light_terminal } => {
            let host = match host {
                Some(h) => h,
                None => {
                    let info = status(&ctl).await?;
                    let mut choices: Vec<String> = info.addresses.iter().map(|(a, _)| a.clone()).collect();
                    choices.extend(info.dns_names.iter().cloned());
                    if !choices.is_empty() {
                        eprintln!("This machine can be reached at: {}", choices.join(", "));
                    }
                    let default = choices.first().cloned().unwrap_or_else(config::hostname);
                    ask("Address the other machine will use to reach this one (host or host:port)", &default)?
                }
            };
            let qr = if no_qr { None } else { Some(light_terminal) };
            converse(&ctl, &Request::Invite { host }, qr).await?;
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
        Cmd::SetPort { port } => {
            let info = status(&ctl).await?;
            let port = match port {
                Some(p) => p,
                None => ask("New port to listen on", &info.port.to_string())?
                    .trim()
                    .parse::<u16>()
                    .map_err(|_| anyhow!("that is not a port number (1 to 65535)"))?,
            };
            call(&ctl, &Request::SetPort { port }).await?;
            Ok(())
        }
        Cmd::Name { name } => {
            let info = status(&ctl).await?;
            let name = match name {
                Some(n) => n,
                None => ask("Name of this device", &info.name)?,
            };
            call(&ctl, &Request::SetName { name }).await?;
            Ok(())
        }
        Cmd::Rename { device, nickname } => {
            let info = status(&ctl).await?;
            if info.devices.is_empty() {
                bail!("no paired devices yet");
            }
            let labels: Vec<String> = info
                .devices
                .iter()
                .map(|d| {
                    if d.nickname.is_empty() {
                        d.name.clone()
                    } else {
                        format!("{} (calls itself \"{}\")", d.nickname, d.announced)
                    }
                })
                .collect();
            let devices = &info.devices;
            let i = pick("device", &labels, device, |i, t| {
                name_or_prefix(&devices[i].name, &devices[i].key, t) || t.eq_ignore_ascii_case(&devices[i].announced)
            })?;
            let nick = match nickname {
                Some(n) => n,
                None => ask(
                    &format!("Nickname for \"{}\" on this machine (- removes it)", devices[i].announced),
                    &devices[i].nickname,
                )?,
            };
            let nick = if nick.trim() == "-" { String::new() } else { nick };
            call(&ctl, &Request::Rename { device: info.devices[i].key.clone(), nickname: nick }).await?;
            Ok(())
        }
        Cmd::SetAddress { device, address } => {
            let info = status(&ctl).await?;
            if info.devices.is_empty() {
                bail!("no paired devices yet");
            }
            let labels: Vec<String> = info
                .devices
                .iter()
                .map(|d| format!("{} ({})", d.name, d.addr.as_deref().unwrap_or("no address: it calls us")))
                .collect();
            let devices = &info.devices;
            let i = pick("device", &labels, device, |i, t| name_or_prefix(&devices[i].name, &devices[i].key, t))?;
            let addr = match address {
                Some(a) => a,
                None => ask(
                    "New address (host or host:port)",
                    devices[i].addr.as_deref().unwrap_or(""),
                )?,
            };
            call(&ctl, &Request::SetAddress { device: info.devices[i].key.clone(), addr }).await?;
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
        Cmd::Log { device, library, lines, path } => {
            if path {
                println!("{}", cfg.dir.join(crate::activity::FILE).display());
                println!("{}.1", cfg.dir.join(crate::activity::FILE).display());
                println!("{}  (what the program prints)", cfg.log_path().display());
                println!("Limit: {} (change it with:  rustsync -c {} set-log-size)", proto::fmt_size(cfg.logging.max_bytes()), config_path.display());
                return Ok(());
            }
            let text = crate::activity::read(&cfg.dir, device.as_deref(), library.as_deref(), lines);
            if text.is_empty() {
                eprintln!("(nothing logged yet)");
            } else {
                println!("{}", text);
            }
            Ok(())
        }
        Cmd::SetLogSize { size } => {
            let text = match size {
                Some(t) => t,
                None => ask("Largest size the logs may grow to (e.g. 2MB, 10MB)", &proto::fmt_size(cfg.logging.max_bytes()).replace(' ', ""))?,
            };
            let bytes = proto::parse_size(&text).map_err(|e| anyhow!(e))?;
            if bytes < crate::activity::MIN_MAX || bytes > crate::activity::MAX_MAX {
                bail!(
                    "choose between {} and {}",
                    proto::fmt_size(crate::activity::MIN_MAX),
                    proto::fmt_size(crate::activity::MAX_MAX)
                );
            }
            if call(&ctl, &Request::SetLogSize { bytes }).await.is_err() {
                // not running: just save it
                config::set_log_size(config_path, bytes)?;
                println!("Saved. It takes effect the next time rustsync starts.");
            }
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
