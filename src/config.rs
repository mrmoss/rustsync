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
    /// Where this config was read from (filled in by `load`).
    #[serde(skip)]
    pub path: PathBuf,
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
    cfg.path = path.to_path_buf();
    cfg.dir = match &cfg.state_dir {
        Some(d) => abs_path(Path::new(d)),
        None => default_state_dir(path),
    };
    Ok(cfg)
}

/// Write a new listening port into the config file, keeping the rest of the file (comments and
/// all) as it is.
pub fn set_port(path: &Path, port: u16) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read config {}", path.display()))?;
    // `server: {port: 1}` and the like: rewrite the whole file (comments are lost).
    if text.lines().any(|l| {
        l.starts_with("server:") && !l["server:".len()..].trim_start().is_empty() && !l["server:".len()..].trim_start().starts_with('#')
    }) {
        let mut doc: serde_yaml::Value = serde_yaml::from_str(&text)?;
        let map = doc.as_mapping_mut().ok_or_else(|| anyhow!("the config file is not a mapping"))?;
        let key = serde_yaml::Value::from("server");
        let mut server = map.get(&key).cloned().unwrap_or(serde_yaml::Value::Mapping(Default::default()));
        if let Some(m) = server.as_mapping_mut() {
            m.insert(serde_yaml::Value::from("port"), serde_yaml::Value::from(port as u64));
        }
        map.insert(key, server);
        std::fs::write(path, serde_yaml::to_string(&doc)?)?;
        return Ok(());
    }
    let mut out: Vec<String> = Vec::new();
    let (mut in_server, mut done) = (false, false);
    for line in text.lines() {
        let indented = line.starts_with(' ') || line.starts_with('\t');
        if !indented && !line.trim().is_empty() && !line.trim_start().starts_with('#') {
            if in_server && !done {
                out.push(format!("  port: {}", port));
                done = true;
            }
            in_server = line.trim_end() == "server:";
        } else if in_server && !done && line.trim_start().starts_with("port:") {
            out.push(format!("  port: {}", port));
            done = true;
            continue;
        }
        out.push(line.to_string());
    }
    if in_server && !done {
        out.push(format!("  port: {}", port));
        done = true;
    }
    if !done {
        out.push("server:".to_string());
        out.push(format!("  port: {}", port));
    }
    let tmp = path.with_extension("yaml.tmp");
    std::fs::write(&tmp, out.join("\n") + "\n")?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The IPv4 addresses of this machine's network interfaces (not loopback, not link-local):
/// (address, interface name).
pub fn local_addresses() -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
    // SAFETY: getifaddrs fills a list that is freed with freeifaddrs below; every pointer in it
    // is only read while the list is alive.
    unsafe {
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            cur = ifa.ifa_next;
            if ifa.ifa_addr.is_null() {
                continue;
            }
            let flags = ifa.ifa_flags as u32;
            if flags & (libc::IFF_UP as u32) == 0 || flags & (libc::IFF_LOOPBACK as u32) != 0 {
                continue;
            }
            if (*ifa.ifa_addr).sa_family as i32 != libc::AF_INET {
                continue;
            }
            let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
            let ip = std::net::Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr));
            if ip.is_link_local() || ip.is_unspecified() || ip.is_loopback() {
                continue;
            }
            let name = std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().to_string();
            let entry = (ip.to_string(), name);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
        libc::freeifaddrs(list);
    }
    // Addresses on a home or office network first.
    out.sort_by_key(|(a, _)| !a.parse::<std::net::Ipv4Addr>().map(|i| i.is_private()).unwrap_or(false));
    out
}

/// Names other machines might be able to use to find this one: the machine's own host name, and
/// `name.local` (found through Bonjour/mDNS on most home networks).
pub fn dns_names() -> Vec<String> {
    let mut buf = [0u8; 256];
    // SAFETY: gethostname writes at most buf.len() bytes.
    if unsafe { libc::gethostname(buf.as_mut_ptr() as *mut libc::c_char, buf.len()) } != 0 {
        return Vec::new();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    let name = String::from_utf8_lossy(&buf[..end]).trim().to_string();
    if name.is_empty() || name == "localhost" {
        return Vec::new();
    }
    let mut names = vec![name.clone()];
    if !name.contains('.') {
        names.push(format!("{}.local", name));
    }
    names
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
