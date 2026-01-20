use clap::Parser;
use libp2p::identity;
use std::path::PathBuf;
use anyhow::Result;

use rustsync::keys::{save_keypair, load_keypair, default_rustsync_dir, test_rustsync_dir};

#[derive(Parser)]
#[command(name = "key-gen", about = "Generate rustsync peer keys")]
struct Args {
    #[arg(short = 'O', long = "output", default_value_t = default_rustsync_dir())]
    output: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dir = PathBuf::from(&args.output);
    test_rustsync_dir(&dir)?;

    println!("Generating new Ed25519 keypair...");
    let keypair = identity::Keypair::generate_ed25519();

    let peer_id = save_keypair(&dir, &keypair)?;
    println!("Peer ID: {peer_id}");

    // Sanity check
    let loaded = load_keypair(&dir, &peer_id)?;
    assert_eq!(
        loaded.public().to_peer_id(),
        keypair.public().to_peer_id()
    );

    println!("Keys written to {:?}", dir);
    Ok(())
}
