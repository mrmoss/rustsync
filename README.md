# Rustsync

## Intro

Trying to knock off Syncthing but:

1. In rust because that's what all the cool kids are doing these days apparently
1. Adding support for a "vaults" that only encrypt/relay encrypted blobs (so you can store your files on an untrusted/semi-trusted space, and then pull them down whenever)

Windows portion is untested (and probably will never be tested by me...)

Super vibe coded. I feel disgusting. But it's also my first rust project lol, so whatever.

## Building

    cargo build

## Running

Below will mirror test/input to test/output:

    mkdir -p test/input test/output
    cargo run test/input test/output
