use clap::Parser;
use std::path::PathBuf;
use anyhow::Result;

use rustsync::keys::{load_keypair, default_rustsync_dir, test_rustsync_dir};

#[derive(Parser)]
#[command(name = "p2ptest", about = "Tests p2p functionality")]
struct Args {
    #[arg(short = 'I', long = "input", default_value_t = default_rustsync_dir())]
    input: String,

    peer_id: String,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let dir = PathBuf::from(&args.input);
    test_rustsync_dir(&dir)?;

    let loaded = load_keypair(&dir, &args.peer_id)?;
    assert_eq!(
        loaded.public().to_peer_id().to_string(),
        args.peer_id
    );

    println!("Keypair loaded successfully for peer: {}", args.peer_id);

    Ok(())
}
