//! The `rustsync` program: see the library docs in lib.rs.

use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    rustsync::run(rustsync::Args::parse()).await
}
