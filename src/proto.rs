//! What the command line says to the running daemon, and what it answers (JSON lines over a
//! local unix socket in the state directory).

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
pub enum Request {
    Status,
    /// Create a one-time pairing code and wait until someone joins with it.
    Invite { host: String },
    Join { token: String },
    /// The answer to a `Verify` reply during `Invite` or `Join`: do the codes match, and do you
    /// want to pair with this device?
    PairDecision { accept: bool },
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
    /// Pairing: compare `code` with the other device's screen and answer with `PairDecision`.
    /// `joining` is true on the device that entered the code.
    Verify { name: String, fingerprint: String, code: String, joining: bool },
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
