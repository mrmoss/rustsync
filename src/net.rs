//! Transport security.
//!
//! Every connection runs the Noise protocol `Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s`
//! (the same family WireGuard uses): mutual authentication with long-term X25519
//! keys, ephemeral keys for forward secrecy, ChaCha20-Poly1305 for records, and a
//! 32-byte pre-shared secret per pair of devices mixed into every key. As long as that secret
//! stays secret, recorded traffic stays safe even from a future quantum computer (X25519 alone
//! would not be); the identity keys themselves are ordinary X25519.
//!
//! Wire format: a sequence of frames `[u16 big-endian length][ciphertext]`.
//! Each decrypted record is `[flag][data]` where flag 1 means "more records
//! follow for this message" and 0 means "last record".

use anyhow::{anyhow, bail, Result};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};


const PARAMS: &str = "Noise_XXpsk0_25519_ChaChaPoly_BLAKE2s";
// Part of the key exchange: devices that speak a different message format (this changes with the
// protocol version) cannot complete the handshake, so they refuse each other cleanly.
const PROLOGUE: &[u8] = b"rustsync/5";
const MAX_RECORD: usize = 65535;
const PLAIN_CHUNK: usize = 60000;
const MAX_MESSAGE: usize = 24 * 1024 * 1024;

pub struct Keys {
    pub private: Vec<u8>,
    pub public: Vec<u8>,
}

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{:02x}", x)).collect()
}

pub fn unhex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim();
    if !s.is_ascii() || s.len() % 2 != 0 {
        bail!("invalid hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(|e| anyhow!(e)))
        .collect()
}

/// The public key belonging to a private key (X25519).
fn public_from_private(private: &[u8]) -> Result<Vec<u8>> {
    let arr: [u8; 32] = private
        .try_into()
        .map_err(|_| anyhow!("the private key file must contain 32 bytes of hex"))?;
    let secret = x25519_dalek::StaticSecret::from(arr);
    Ok(x25519_dalek::PublicKey::from(&secret).as_bytes().to_vec())
}

/// Refuse to use a private key that others could have read or replaced.
fn check_private_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let md = std::fs::symlink_metadata(path)?;
    if md.file_type().is_symlink() {
        bail!(
            "{} is a symlink; set key_config.private_key_path to the real file instead",
            path.display()
        );
    }
    if !md.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    if md.uid() != unsafe { libc::geteuid() } {
        bail!("{} belongs to another user", path.display());
    }
    if md.mode() & 0o077 != 0 {
        bail!(
            "{} can be read by other users (mode {:o}); fix it with:  chmod 600 {}",
            path.display(),
            md.mode() & 0o777,
            path.display()
        );
    }
    Ok(())
}

/// The public key file is only a convenience copy (so you can `cat` it): always derived from
/// the private key, replaced atomically.
fn write_public(path: &Path, public: &[u8]) -> Result<()> {
    // Unpredictable name, created exclusively and never through a symlink (like the private key).
    let tmp = sibling(path, &format!(".new-{:016x}", rand::random::<u64>()));
    // If this open fails, the name belongs to somebody else: leave it alone and give up.
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .custom_flags(libc::O_NOFOLLOW)
        .open(&tmp)?;
    // From here the file is ours, so it is ours to clean up.
    let written = f.write_all(format!("{}\n", hex(public)).as_bytes());
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

/// Load the key pair, or generate and save a new one if there is no private key yet.
pub fn load_or_create_keys(private_path: &Path, public_path: &Path) -> Result<Keys> {
    if std::fs::symlink_metadata(private_path).is_err() {
        if public_path.exists() {
            bail!(
                "{} exists but {} does not; refusing to guess. Delete {} to generate a new pair",
                public_path.display(),
                private_path.display(),
                public_path.display()
            );
        }
        let params: snow::params::NoiseParams = PARAMS.parse()?;
        let kp = snow::Builder::new(params).generate_keypair()?;
        for p in [private_path, public_path] {
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        // Write under a temporary name, then link it into place: the private key either
        // appears complete or not at all, and an existing key is never overwritten.
        let tmp = sibling(private_path, ".new");
        let _ = std::fs::remove_file(&tmp);
        {
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&tmp)?;
            writeln!(f, "{}", hex(&kp.private))?;
            f.sync_all()?;
        }
        let placed = match std::fs::hard_link(&tmp, private_path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(e),
            // some file systems have no hard links
            Err(_) => std::fs::rename(&tmp, private_path),
        };
        let _ = std::fs::remove_file(&tmp);
        placed.map_err(|e| anyhow!("cannot create {}: {}", private_path.display(), e))?;
        write_public(public_path, &kp.public)?;
        return Ok(Keys { private: kp.private, public: kp.public });
    }

    check_private_file(private_path)?;
    // Older versions wrote the key without a trailing newline; add it so `cat` is tidy.
    ensure_trailing_newline(private_path)?;
    let private = unhex(&std::fs::read_to_string(private_path)?)?;
    let public = public_from_private(&private)?;
    let stored = std::fs::read_to_string(public_path).ok().and_then(|t| unhex(&t).ok());
    if stored.as_deref() != Some(public.as_slice()) {
        if stored.is_some() {
            eprintln!(
                "warning: {} did not match the private key; rewriting it",
                public_path.display()
            );
        }
        write_public(public_path, &public)?;
    }
    Ok(Keys { private, public })
}

fn ensure_trailing_newline(path: &Path) -> Result<()> {
    let text = std::fs::read_to_string(path)?;
    if !text.is_empty() && !text.ends_with('\n') {
        let mut f = std::fs::OpenOptions::new().append(true).open(path)?;
        f.write_all(b"\n")?;
    }
    Ok(())
}

/// Parse a 32-byte value written as 64 hex characters (keys, codes).
pub fn parse_hex32(s: &str, what: &str) -> Result<Vec<u8>> {
    let k = unhex(s).map_err(|_| anyhow!("{} is not valid hex", what))?;
    if k.len() != 32 {
        bail!("{} must be 64 hex characters, got {}", what, s.trim().len());
    }
    Ok(k)
}

/// `<path><suffix>` (appends to the file name instead of replacing the extension).
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

/// True if `s` is exactly `len` lowercase/uppercase hex characters.
pub fn valid_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|c| c.is_ascii_hexdigit())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> std::io::Result<Vec<u8>> {
    let len = r.read_u16().await? as usize;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> std::io::Result<()> {
    w.write_all(&(data.len() as u16).to_be_bytes()).await?;
    w.write_all(data).await
}

/// The pre-shared secret as the fixed-size array the Noise library wants.
fn psk_array(psk: &[u8]) -> Result<&[u8; 32]> {
    psk.try_into().map_err(|_| anyhow!("a pairing secret must be exactly 32 bytes"))
}

/// Run the Noise XX handshake. Returns the transport state and the peer's static public key.
/// `psk` is the 32-byte secret code of the machine being called. It is mixed into every key
/// from the first message on, so recorded traffic stays safe even if X25519 is broken later,
/// and a machine without the code cannot even complete the handshake.
pub async fn handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    keys: &Keys,
    initiator: bool,
    psk: &[u8],
) -> Result<(snow::TransportState, Vec<u8>)> {
    let params: snow::params::NoiseParams = PARAMS.parse()?;
    let builder = snow::Builder::new(params)
        .local_private_key(&keys.private)?
        .psk(0, psk_array(psk)?)?
        .prologue(PROLOGUE)?;
    let mut hs = if initiator { builder.build_initiator()? } else { builder.build_responder()? };
    let mut buf = vec![0u8; MAX_RECORD];
    let mut my_turn = initiator;
    while !hs.is_handshake_finished() {
        if my_turn {
            let n = hs.write_message(&[], &mut buf)?;
            write_frame(stream, &buf[..n]).await?;
        } else {
            let frame = read_frame(stream).await?;
            hs.read_message(&frame, &mut buf)?;
        }
        my_turn = !my_turn;
    }
    let remote = hs
        .get_remote_static()
        .ok_or_else(|| anyhow!("peer sent no static key"))?
        .to_vec();
    Ok((hs.into_transport_mode()?, remote))
}

/// Responder side when we do not yet know who is calling: the first handshake message only
/// decrypts with the right pre-shared secret, so we try each candidate in turn. Returns the
/// transport, the caller's public key and the index of the secret that matched.
pub async fn handshake_responder<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    keys: &Keys,
    psks: &[Vec<u8>],
) -> Result<(snow::TransportState, Vec<u8>, usize)> {
    let first = read_frame(stream).await?;
    let mut buf = vec![0u8; MAX_RECORD];
    for (i, psk) in psks.iter().enumerate() {
        let params: snow::params::NoiseParams = PARAMS.parse()?;
        let mut hs = snow::Builder::new(params)
            .local_private_key(&keys.private)?
            .psk(0, psk_array(psk)?)?
            .prologue(PROLOGUE)?
            .build_responder()?;
        if hs.read_message(&first, &mut buf).is_err() {
            continue;
        }
        let n = hs.write_message(&[], &mut buf)?;
        write_frame(stream, &buf[..n]).await?;
        let frame = read_frame(stream).await?;
        hs.read_message(&frame, &mut buf)?;
        let remote = hs
            .get_remote_static()
            .ok_or_else(|| anyhow!("peer sent no static key"))?
            .to_vec();
        return Ok((hs.into_transport_mode()?, remote, i));
    }
    bail!("unknown device or wrong code")
}

/// Encrypt and send one application message (split into records as needed).
pub async fn send_message<W: AsyncWrite + Unpin>(
    w: &mut W,
    t: &mut snow::TransportState,
    data: &[u8],
) -> Result<()> {
    let mut out = vec![0u8; MAX_RECORD];
    let mut plain = Vec::with_capacity(PLAIN_CHUNK + 1);
    let mut pieces = data.chunks(PLAIN_CHUNK).peekable();
    while let Some(piece) = pieces.next() {
        plain.clear();
        plain.push(if pieces.peek().is_some() { 1 } else { 0 });
        plain.extend_from_slice(piece);
        let n = t.write_message(&plain, &mut out)?;
        write_frame(w, &out[..n]).await?;
    }
    Ok(())
}

/// Collects decrypted records until a complete message is available.
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
}

impl Reassembler {
    pub fn push(&mut self, t: &mut snow::TransportState, frame: &[u8]) -> Result<Option<Vec<u8>>> {
        let mut plain = vec![0u8; frame.len()];
        let n = t.read_message(frame, &mut plain)?;
        if n == 0 {
            bail!("empty record");
        }
        self.buf.extend_from_slice(&plain[1..n]);
        if self.buf.len() > MAX_MESSAGE {
            bail!("message too large");
        }
        if plain[0] == 0 {
            Ok(Some(std::mem::take(&mut self.buf)))
        } else {
            Ok(None)
        }
    }
}
