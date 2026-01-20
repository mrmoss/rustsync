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
    pub keypair: identity::Keypair,
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

pub fn force_err() -> Result<QuicPeerKeys> {
    anyhow::bail!("forced failure for test");
}

fn default_rustsync_dir() -> String {
    let home = home_dir().expect("Failed to get home directory");
    home.join(".rustsync").to_str().expect("Home path is not valid UTF-8").to_string()
}

pub fn load_ed25519(dpath: &PathBuf, peer_id: &str) -> Result<identity::Keypair> {
    let private = read_key(&dpath.join(&peer_id).with_extension("private")).context("Could not load private key")?;
    let keypair = identity::Keypair::from_protobuf_encoding(&private).context("Invalid private key")?;

    let loaded_peer_id = keypair.public().to_peer_id().to_string();
    if loaded_peer_id != peer_id {
        anyhow::bail!(
            "Peer ID mismatch: expected {}, got {}",
            peer_id,
            loaded_peer_id
        );
    }

    Ok(keypair)
}

pub fn generate_ed25519() -> Result<QuicPeerKeys> {
    let keypair = identity::Keypair::generate_ed25519();

    Ok(QuicPeerKeys {
        id: keypair.public().to_peer_id().to_string(),
        keypair: keypair.clone(),
        private: keypair.to_protobuf_encoding().context("Private key encode failure")?,
        public: keypair.public().encode_protobuf(),
    })
}

#[derive(Parser)]
#[command(name = "rustsync-keygen", about = "Generate rustsync peer keys")]
struct Args {
    #[arg(short = 'O', long = "output", default_value_t = default_rustsync_dir())]
    rustsync_keys_dpath: String,
}

fn write_key(fpath: &PathBuf, data: &Vec<u8>, permissions: u32) -> Result<()> {
    println!("Writing key:\t{:?}", &fpath.display());
    fs::write(&fpath, &data)?;
    fs::set_permissions(&fpath, fs::Permissions::from_mode(permissions))?;
    Ok(())
}

fn read_key(fpath: &PathBuf) -> Result<Vec<u8>> {
    println!("Reading key:\t{:?}", &fpath.display());
    Ok(fs::read(&fpath)?)
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

    write_key(&dpath.join(&peer.id).with_extension("private"), &peer.private, 0o600)?;
    write_key(&dpath.join(&peer.id).with_extension("public"), &peer.public, 0o644)?;

    println!("Reading keys");
    let testpair = load_ed25519(&dpath, &peer.id);
    println!("{:?}", peer.keypair);
    println!("{:?}", testpair);

    Ok(())
}