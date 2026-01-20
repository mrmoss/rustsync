# Rustsync

## Intro

Trying to knock off Syncthing but:

1. In rust because that's what all the cool kids are doing these days apparently
1. Adding support for a "vaults" that only encrypt/relay encrypted blobs (so you can store your files on an untrusted/semi-trusted space, and then pull them down whenever)

Windows portion is untested (and probably will never be tested by me...)

Super vibe coded. I feel disgusting. But it's also my first rust project lol, so whatever.

## Current status

Right now this is simply mirroring directories on the same machine.

Next step is doing it across the network in plaintext bidirectionally:

- Large file transfers will be tricky.
- I imagine Syncthing is doing block based syncing and not just entire files.
- This is probably the hardest part of the entire project. :(

After that we'll move on to encryption:

- This will be super basic.
- Passworded private+public key pair for authorization+authentication.
- Might be a little tricky when it comes to multiple clients all syncing from a single server (since they all can't have the same key).
- I don't know what's available on rust, but hopefully something like ed25519 can be used.
- Will be slightly tricky, but probably not too bad.

## Building

    cargo build

## Configuration

Do this on both the client and server:

    ssh-keygen -t ed25519 -m PEM -f rustsync.key -N ""


## Running

Below will mirror test/input to test/output:

    mkdir -p test/input test/output
    cargo run test/input test/output
