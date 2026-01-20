use anyhow::{Context, Result};
use dirs::home_dir;
use libp2p::identity;
use std::{
    fs,
    path::{Path, PathBuf},
    process,
};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

fn write_key(path: &Path, data: &[u8], mode: u32) -> Result<()> {
    fs::write(path, data)?;
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    Ok(())
}

pub fn save_keypair(dir: &Path, keypair: &identity::Keypair) -> Result<String> {
    fs::create_dir_all(dir)?;

    let peer_id = keypair.public().to_peer_id().to_string();
    let private_path = dir.join(&peer_id).with_extension("private");
    let public_path  = dir.join(&peer_id).with_extension("public");

    let private = keypair
        .to_protobuf_encoding()
        .context("Failed to encode private key")?;
    let public = keypair.public().encode_protobuf();

    write_key(&private_path, &private, 0o600)?;
    write_key(&public_path, &public, 0o644)?;

    Ok(peer_id)
}

pub fn load_keypair(dir: &Path, peer_id: &str) -> Result<identity::Keypair> {
    let private_path = dir.join(peer_id).with_extension("private");

    let private = fs::read(&private_path)
        .with_context(|| format!("Failed to read {:?}", private_path))?;

    let keypair = identity::Keypair::from_protobuf_encoding(&private)
        .context("Invalid private key encoding")?;

    let derived = keypair.public().to_peer_id().to_string();
    if derived != peer_id {
        anyhow::bail!(
            "Peer ID mismatch: expected {}, got {}",
            peer_id,
            derived
        );
    }

    Ok(keypair)
}

pub fn default_rustsync_dir() -> String {
    home_dir()
        .expect("No home directory")
        .join(".rustsync")
        .to_string_lossy()
        .into_owned()
}

pub fn test_rustsync_dir(dir: &PathBuf) -> Result<()> {
    #[cfg(unix)]
    {
        if dir.exists() {
            let perms = fs::metadata(&dir)?.permissions();
            if perms.mode() & 0o077 != 0 {
                eprintln!(
                    "Error: {:?} must not be accessible by group or others",
                    dir
                );
                process::exit(1);
            }
        }
    }
    Ok(())
}
