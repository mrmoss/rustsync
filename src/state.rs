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
    /// When it was last connected (seconds since 1970); 0 = never.
    #[serde(default)]
    pub last_seen: u64,
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
        // An unpredictable name, created exclusively and never through a symlink.
        let tmp = crate::net::sibling(path, &format!(".tmp-{:016x}", rand::random::<u64>()));
        // If this open fails, the name belongs to somebody else: leave it alone and give up.
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&tmp)?;
        // From here the file is ours, so it is ours to clean up.
        let written = serde_json::to_vec_pretty(self)
            .map_err(std::io::Error::other)
            .and_then(|bytes| f.write_all(&bytes));
        drop(f);
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

    pub fn device(&self, key: &str) -> Option<&Device> {
        self.devices.iter().find(|d| d.key == key)
    }

    pub fn device_name(&self, key: &str) -> String {
        self.device(key)
            .map(|d| d.name.clone())
            .unwrap_or_else(|| key.chars().take(8).collect())
    }
}
