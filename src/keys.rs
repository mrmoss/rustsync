use clap::Parser;
use dirs::home_dir;
use libp2p::identity;
use std::{
    fs,
    path::PathBuf,
    process,
    os::unix::fs::PermissionsExt,
};
use anyhow::{
    Context,
    Result
};

pub struct QuicPeerKeys {
    pub id: String,
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

pub fn force_err() -> Result<QuicPeerKeys> {
    anyhow::bail!("forced failure for test");
}

/*pub fn load_ed25519(path: &str) -> identity::Keypair {
    let key_bytes = fs::read(path).expect("key file");
    identity::Keypair::ed25519_from_bytes(key_bytes).expect("valid ed25519 key")
}*/

pub fn generate_ed25519() -> Result<QuicPeerKeys> {
    let keypair = identity::Keypair::generate_ed25519();

    Ok(QuicPeerKeys {
        id: keypair.public().to_peer_id().to_string(),
        private: keypair.to_protobuf_encoding().context("Private key encode failure")?,
        public: keypair.public().encode_protobuf(),
    })
}

fn default_rustsync_dir() -> String {
    let home = home_dir().expect("Failed to get home directory");
    home.join(".rustsync").to_str().expect("Home path is not valid UTF-8").to_string()
}

#[derive(Parser)]
#[command(name = "rustsync-keygen", about = "Generate rustsync peer keys")]
struct Args {
    #[arg(short = 'O', long = "output", default_value_t = default_rustsync_dir())]
    rustsync_keys_dpath: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dpath = PathBuf::from(&args.rustsync_keys_dpath);

    #[cfg(unix)]
    {
        println!("Verifying permissions on {:?}...", &dpath.display());
        let metadata = fs::metadata(&dpath)?;
        let perms = metadata.permissions();

        if perms.mode() & 0o077 > 0 {
            eprintln!("Error: Invalid permissions on {:?}", &dpath.display());
            process::exit(-1);            
        }
    }

    println!("Generating keys");
    let peer = generate_ed25519()?;
    let private_key_path = &dpath.join(&peer.id).with_extension("private");
    let public_key_path = &dpath.join(&peer.id).with_extension("public");

    println!("Writing private key:\t{:?}", &private_key_path.display());
    fs::write(&private_key_path, &peer.private)?;
    fs::set_permissions(&private_key_path, fs::Permissions::from_mode(0o600))?;

    println!("Writing public key:\t{:?}", &public_key_path.display());
    fs::write(&public_key_path, &peer.public)?;
    fs::set_permissions(&public_key_path, fs::Permissions::from_mode(0o644))?;

    Ok(())
}