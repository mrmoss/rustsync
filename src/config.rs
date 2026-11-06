//! The config file. Everything in it is optional: devices and libraries are not configured
//! here, they are managed with commands and kept in the state directory.

use anyhow::{anyhow, bail, Context, Result};
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
    /// The desktop window.
    #[serde(default)]
    pub gui: GuiConfig,

    // Old config format: still parsed so we can tell the user it is ignored.
    #[serde(default)]
    folders: Option<toml::Value>,
    #[serde(default)]
    peers: Option<toml::Value>,

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

/// Settings of the desktop window (`rustsync-gui`).
#[derive(Debug, Deserialize)]
pub struct GuiConfig {
    /// On a Wayland desktop, open the window through X11 so the desktop draws its title bar and
    /// buttons (they then look like every other window). Turn off for native Wayland.
    #[serde(default = "default_true")]
    pub x11: bool,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig { x11: true }
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
    /// How much the activity log (and `daemon.log`) may grow, e.g. `2MB`.
    #[serde(default = "default_log_size")]
    pub max_size: String,
}

impl LoggingConfig {
    /// `max_size` in bytes (the default if it cannot be read).
    pub fn max_bytes(&self) -> u64 {
        match crate::proto::parse_size(&self.max_size) {
            Ok(b) if b > 0 => b,
            _ => crate::activity::DEFAULT_MAX,
        }
    }
}

impl Default for LoggingConfig {
    fn default() -> Self {
        LoggingConfig { level: default_level(), max_size: default_log_size() }
    }
}

fn default_log_size() -> String {
    "2MB".to_string()
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

/// Used when you run `rustsync` without `-c`: ~/.config/rustsync/config.toml
pub fn default_config_path() -> PathBuf {
    match std::env::var("HOME") {
        Ok(h) if !h.is_empty() => Path::new(&h).join(".config").join("rustsync").join("config.toml"),
        _ => PathBuf::from("rustsync.toml"),
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

/// `<config path without extension>.state`, e.g. alpha.toml -> alpha.state
fn default_state_dir(config_path: &Path) -> PathBuf {
    let stem = abs_path(config_path).with_extension("");
    crate::net::sibling(&stem, ".state")
}

/// A string as a TOML value, quoted and escaped.
fn q(s: &str) -> String {
    toml::Value::String(s.to_string()).to_string()
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
    y.push_str(&format!("name = {}\n\n", q(&hostname())));
    y.push_str("# Other devices can keep encrypted backups on this machine. You pick the folder for each\n");
    y.push_str("# one when you accept it (`rustsync accept`); this is just the suggested location:\n");
    y.push_str("# backup_dir = \"/mnt/bigdrive/rustsync-backups\"\n");
    y.push_str("# accept_backups = true      # false = never show backup offers\n\n");
    y.push_str(&format!("[server]\nport = {}\n\n", port));
    y.push_str("[sync_options]\nfull_scan_interval = 30\n\n[logging]\nlevel = \"info\"\n");
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
    }
    std::fs::write(path, y).with_context(|| format!("cannot write {}", path.display()))
}

/// Which file to use for a config path. Versions before 0.6 kept the config in YAML; if the path
/// names a `.yaml`/`.yml` file (or the `.toml` next to an old `.yaml` is missing), the old file is
/// converted once to a `.toml` next to it (comments kept) and that is used from then on. The old
/// file is left alone and no longer read.
pub fn resolve(path: &Path) -> Result<PathBuf> {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    let toml_path = if ext == "yaml" || ext == "yml" { path.with_extension("toml") } else { path.to_path_buf() };
    if toml_path.exists() {
        return Ok(toml_path);
    }
    let old = [path.to_path_buf(), toml_path.with_extension("yaml"), toml_path.with_extension("yml")]
        .into_iter()
        .find(|p| p.is_file() && p != &toml_path);
    if let Some(old) = old {
        convert_yaml_file(&old, &toml_path)?;
    }
    Ok(toml_path)
}

fn convert_yaml_file(old: &Path, new: &Path) -> Result<()> {
    let text = std::fs::read_to_string(old).with_context(|| format!("cannot read config {}", old.display()))?;
    let converted = yaml_to_toml(&text).with_context(|| format!("cannot convert {} to TOML", old.display()))?;
    if let Err(e) = toml::from_str::<Config>(&converted) {
        bail!(
            "cannot convert {} to TOML ({}). Write {} by hand: the README shows what it looks like.",
            old.display(),
            e.message(),
            new.display()
        );
    }
    let tmp = new.with_extension("toml.tmp");
    std::fs::write(&tmp, converted)?;
    std::fs::rename(&tmp, new)?;
    eprintln!(
        "note: the config format changed to TOML. {} was converted to {}; the old file is no longer used.",
        old.display(),
        new.display()
    );
    Ok(())
}

/// Convert the (simple) YAML config files rustsync used to read into TOML: `key: value` lines,
/// one level of sections, comments. The old `folders`/`peers` sections are dropped.
fn yaml_to_toml(yaml: &str) -> Result<String> {
    // TOML wants the plain keys first and the [tables] after them; YAML does not.
    let (mut top, mut tables, mut pending) = (String::new(), String::new(), String::new());
    let (mut in_table, mut skipping) = (false, false);
    for (n, raw) in yaml.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            pending.push_str(line);
            pending.push('\n');
            continue;
        }
        if line == "---" || line == "..." {
            continue;
        }
        let indent = raw.len() - raw.trim_start().len();
        if skipping && (indent > 0 || line == "-" || line.starts_with("- ")) {
            continue;
        }
        let colon = line.find(':').ok_or_else(|| anyhow!("line {}: expected `key: value`", n + 1))?;
        let key = line[..colon].trim();
        let rest = strip_yaml_comment(&line[colon + 1..]).trim().to_string();
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            bail!("line {}: cannot convert the key `{}`", n + 1, key);
        }
        if indent == 0 {
            skipping = false;
            in_table = false;
            if rest.is_empty() || rest.starts_with('{') {
                if key == "folders" || key == "peers" {
                    skipping = true;
                    pending.clear();
                    continue;
                }
                in_table = true;
                tables.push_str(&std::mem::take(&mut pending));
                tables.push_str(&format!("[{}]\n", key));
                if let Some(flow) = rest.strip_prefix('{') {
                    let inner = flow.strip_suffix('}').ok_or_else(|| anyhow!("line {}: cannot convert `{}`", n + 1, rest))?;
                    for item in inner.split(',').filter(|i| !i.trim().is_empty()) {
                        let (k, v) = item.split_once(':').ok_or_else(|| anyhow!("line {}: cannot convert `{}`", n + 1, item.trim()))?;
                        if let Some(v) = yaml_scalar(v.trim(), n + 1)? {
                            tables.push_str(&format!("{} = {}\n", k.trim(), v));
                        }
                    }
                    in_table = false;
                }
            } else if key == "folders" || key == "peers" {
                pending.clear();
            } else if let Some(v) = yaml_scalar(&rest, n + 1)? {
                top.push_str(&std::mem::take(&mut pending));
                top.push_str(&format!("{} = {}\n", key, v));
            }
        } else {
            if !in_table {
                bail!("line {}: cannot convert this nesting", n + 1);
            }
            if rest.is_empty() {
                bail!("line {}: cannot convert nested sections", n + 1);
            }
            if let Some(v) = yaml_scalar(&rest, n + 1)? {
                tables.push_str(&std::mem::take(&mut pending));
                tables.push_str(&format!("{} = {}\n", key, v));
            }
        }
    }
    let mut out = top;
    if !out.is_empty() && !tables.is_empty() && !out.ends_with("\n\n") {
        out.push('\n');
    }
    out.push_str(&tables);
    if !pending.trim().is_empty() {
        out.push_str(&pending);
    }
    Ok(out)
}

/// `value  # comment` -> `value  ` (a `#` inside quotes is not a comment).
fn strip_yaml_comment(s: &str) -> &str {
    let (mut single, mut double, mut prev) = (false, false, ' ');
    let mut escaped = false;
    for (i, c) in s.char_indices() {
        if double && escaped {
            escaped = false;
        } else if double && c == '\\' {
            escaped = true;
        } else if c == '"' && !single {
            double = !double;
        } else if c == '\'' && !double {
            single = !single;
        } else if c == '#' && !single && !double && prev.is_whitespace() {
            return &s[..i];
        }
        prev = c;
    }
    s
}

/// One YAML value as TOML text (None for null).
fn yaml_scalar(v: &str, line: usize) -> Result<Option<String>> {
    if v == "null" || v == "~" || v.is_empty() {
        return Ok(None);
    }
    if v.starts_with('"') {
        if v.len() < 2 || !v.ends_with('"') {
            bail!("line {}: cannot convert `{}`", line, v);
        }
        return Ok(Some(v.to_string()));
    }
    if let Some(inner) = v.strip_prefix('\'') {
        let inner = inner.strip_suffix('\'').ok_or_else(|| anyhow!("line {}: cannot convert `{}`", line, v))?;
        return Ok(Some(q(&inner.replace("''", "'"))));
    }
    if v.starts_with('[') || v.starts_with('{') || v.starts_with('&') || v.starts_with('*') || v.starts_with('|') || v.starts_with('>') {
        bail!("line {}: cannot convert `{}`", line, v);
    }
    if v == "true" || v == "false" {
        return Ok(Some(v.to_string()));
    }
    if let Ok(i) = v.parse::<i64>() {
        return Ok(Some(i.to_string()));
    }
    if v.chars().all(|c| c.is_ascii_digit() || matches!(c, '.' | '-' | 'e' | 'E' | '+')) {
        if let Ok(f) = v.parse::<f64>() {
            return Ok(Some(f.to_string()));
        }
    }
    Ok(Some(q(v)))
}

pub fn load(path: &Path) -> Result<Config> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read config {}", path.display()))?;
    let mut cfg: Config =
        toml::from_str(&text).with_context(|| format!("invalid config {}", path.display()))?;
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
    set_in_section(path, "server", "port", toml_edit::value(port as i64))
}

/// Write this device's name into the config file (`None` removes the line, so the host name is
/// used again). The rest of the file stays as it is.
pub fn set_name(path: &Path, name: Option<&str>) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read config {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("invalid config {}", path.display()))?;
    match name {
        Some(n) => {
            // Replace the value in place so the comments around the line stay.
            let table = doc.as_table_mut();
            match table.get_mut("name") {
                Some(item) => {
                    let mut v = toml_edit::Value::from(n);
                    if let Some(old) = item.as_value() {
                        *v.decor_mut() = old.decor().clone();
                    }
                    *item = toml_edit::Item::Value(v);
                }
                None => {
                    table.insert("name", toml_edit::value(n));
                }
            }
        }
        None => {
            doc.as_table_mut().remove("name");
        }
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Write the window's X11 setting into the config file (the config file must already exist).
pub fn set_gui_x11(path: &Path, on: bool) -> Result<()> {
    set_in_section(path, "gui", "x11", toml_edit::value(on))
}

/// Write a new size limit for the logs (e.g. `5MB`) into the config file.
pub fn set_log_size(path: &Path, bytes: u64) -> Result<()> {
    let text = if bytes % 1_000_000 == 0 { format!("{}MB", bytes / 1_000_000) } else { bytes.to_string() };
    set_in_section(path, "logging", "max_size", toml_edit::value(text))
}

/// Set `[section] key = value` in the config file, keeping the rest of the file (comments and
/// all) as it is.
fn set_in_section(path: &Path, section: &str, key: &str, value: toml_edit::Item) -> Result<()> {
    let text = std::fs::read_to_string(path).with_context(|| format!("cannot read config {}", path.display()))?;
    let mut doc: toml_edit::DocumentMut = text.parse().with_context(|| format!("invalid config {}", path.display()))?;
    if !doc.contains_key(section) {
        doc[section] = toml_edit::table();
    }
    let table = doc[section]
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("`{}` in the config file is not a table", section))?;
    table.insert(key, value);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())?;
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
        // Try to make it private, then check what we really got: the keys live here. If the
        // permissions cannot be enforced (some network and FAT-style file systems ignore
        // them), stop instead of carrying on with secrets other users could read.
        let chmod = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
        use std::os::unix::fs::MetadataExt;
        let md = std::fs::metadata(&self.dir)
            .map_err(|e| anyhow!("cannot inspect {}: {}", self.dir.display(), e))?;
        // SAFETY: geteuid has no preconditions.
        let me = unsafe { libc::geteuid() };
        if md.mode() & 0o077 != 0 {
            bail!(
                "{} is not private (mode {:o}{}); it holds this device's keys. Fix its permissions (chmod 700) or choose another state_dir",
                self.dir.display(),
                md.mode() & 0o7777,
                match chmod {
                    Err(e) => format!(", and changing it failed: {}", e),
                    Ok(()) => String::new(),
                }
            );
        }
        // The owner of a directory controls what is in it whatever its mode says (and root, in
        // particular, must never write into a directory somebody else owns).
        if md.uid() != me {
            bail!(
                "{} belongs to another user (uid {}), who could change what is inside; give it to the user running rustsync (chown) or choose another state_dir",
                self.dir.display(),
                md.uid()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_uses_x11_unless_told_otherwise() {
        let cfg: Config = toml::from_str("").unwrap();
        assert!(cfg.gui.x11);
        let cfg: Config = toml::from_str("[gui]\nx11 = false\n").unwrap();
        assert!(!cfg.gui.x11);
    }

    #[test]
    fn old_yaml_configs_convert() {
        let yaml = "# my config\nname: \"my box\"   # shown to others\nserver: {port: 3040, host: 127.0.0.1}\nlogging:\n  level: debug\n  max_size: 5MB # logs\nstate_dir: /var/lib/rs\nsync_options:\n  full_scan_interval: 60\nfolders:\n  - path: /x\n    name: y\npeers:\n- a\n";
        let toml = yaml_to_toml(yaml).unwrap();
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("my box"));
        assert_eq!(cfg.server.port, 3040);
        assert_eq!(cfg.server.host, "127.0.0.1");
        assert_eq!(cfg.logging.level, "debug");
        assert_eq!(cfg.logging.max_bytes(), 5_000_000);
        assert_eq!(cfg.state_dir.as_deref(), Some("/var/lib/rs"));
        assert_eq!(cfg.sync_options.full_scan_interval, 60);
        assert!(cfg.folders.is_none() && cfg.peers.is_none());
        assert!(toml.contains("# my config"));
    }

    #[test]
    fn yaml_that_cannot_be_converted_is_an_error() {
        assert!(yaml_to_toml("server:\n  nested:\n    deeper: 1\n").is_err());
        assert!(yaml_to_toml("name: [a, b]\n").is_err());
        assert!(yaml_to_toml("just words\n").is_err());
    }

    #[test]
    fn quoting_survives() {
        let toml = yaml_to_toml("name: it's 'fine'\nstate_dir: 'C:\\x'\n").unwrap();
        let cfg: Config = toml::from_str(&toml).unwrap();
        assert_eq!(cfg.name.as_deref(), Some("it's 'fine'"));
        assert_eq!(cfg.state_dir.as_deref(), Some("C:\\x"));
    }

    #[test]
    fn edits_keep_comments_and_add_missing_sections() {
        let dir = std::env::temp_dir().join(format!("rustsync-cfgtest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("c.toml");
        std::fs::write(&path, "# keep me\nname = \"a\"\n\n[server]\nport = 1 # old\n").unwrap();
        set_port(&path, 4000).unwrap();
        set_log_size(&path, 5_000_000).unwrap();
        set_name(&path, Some("laptop \"main\"")).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep me"));
        assert_eq!(load(&path).unwrap().name.as_deref(), Some("laptop \"main\""));
        let cfg = load(&path).unwrap();
        assert_eq!(cfg.server.port, 4000);
        assert_eq!(cfg.logging.max_size, "5MB");
        // removing the name goes back to the host name; a name can be added again
        set_name(&path, None).unwrap();
        assert_eq!(load(&path).unwrap().name, None);
        assert_eq!(load(&path).unwrap().server.port, 4000);
        set_name(&path, Some("a")).unwrap();
        assert_eq!(load(&path).unwrap().name.as_deref(), Some("a"));
        // a name with a trailing comment keeps it
        std::fs::write(&path, "name = \"x\" # shown to others\n").unwrap();
        set_name(&path, Some("y")).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("# shown to others"));
        std::fs::write(&path, "[server]\nport = 1\n").unwrap();
        set_name(&path, Some("z")).unwrap();
        assert_eq!(load(&path).unwrap().name.as_deref(), Some("z"));
        std::fs::write(&path, "[server]\nport = 1\n").unwrap();
        // inline tables work too
        std::fs::write(&path, "server = { port = 1 }\n").unwrap();
        set_port(&path, 5000).unwrap();
        assert_eq!(load(&path).unwrap().server.port, 5000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_converts_once() {
        let dir = std::env::temp_dir().join(format!("rustsync-restest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.yaml"), "name: a\nserver:\n  port: 3033\n").unwrap();
        let got = resolve(&dir.join("a.yaml")).unwrap();
        assert_eq!(got, dir.join("a.toml"));
        assert_eq!(load(&got).unwrap().server.port, 3033);
        // a newer .toml wins over the old file
        std::fs::write(dir.join("a.toml"), "name = \"b\"\n").unwrap();
        assert_eq!(resolve(&dir.join("a.yaml")).unwrap(), dir.join("a.toml"));
        assert_eq!(load(&dir.join("a.toml")).unwrap().name.as_deref(), Some("b"));
        // nothing there: the .toml path to create
        assert_eq!(resolve(&dir.join("new.yaml")).unwrap(), dir.join("new.toml"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
