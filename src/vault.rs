//! Encryption for data that leaves our machines for a depot.
//!
//! One 32-byte vault key, shared by your own machines and NEVER given to the depot, is split
//! (BLAKE3 key derivation) into independent keys for: chunk encryption, record encryption,
//! hiding file paths, and naming chunks. Everything is symmetric (XChaCha20-Poly1305 and
//! BLAKE3), so it is not materially weakened by quantum computers (symmetric 256-bit keys).
//!
//! The depot sees: how many files exist, when they changed, chunk sizes, and which chunks are
//! identical. It does not see names, paths, contents or modes.

use crate::index::Entry;
use anyhow::{anyhow, bail, Result};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

/// New records start with this, so records written by older versions (which have a different
/// layout) can still be read.
const RECORD_MAGIC: [u8; 4] = [0, b'r', b's', 2];

/// Read a record the way versions before 0.6 wrote it (a fixed-width binary layout: 8-byte
/// lengths, 4-byte enum numbers, little endian). Only used to read old backups; every record
/// is written in the new layout the next time it changes.
fn legacy_entry(b: &[u8]) -> Result<Entry> {
    struct R<'a>(&'a [u8]);
    impl<'a> R<'a> {
        fn take(&mut self, n: usize) -> Result<&'a [u8]> {
            if self.0.len() < n {
                bail!("the record is damaged");
            }
            let (a, rest) = self.0.split_at(n);
            self.0 = rest;
            Ok(a)
        }
        fn u32(&mut self) -> Result<u32> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }
        fn u64(&mut self) -> Result<u64> {
            Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
        }
        fn bool(&mut self) -> Result<bool> {
            match self.take(1)?[0] {
                0 => Ok(false),
                1 => Ok(true),
                _ => bail!("the record is damaged"),
            }
        }
        fn string(&mut self) -> Result<String> {
            let n = self.u64()?;
            if n > self.0.len() as u64 {
                bail!("the record is damaged");
            }
            Ok(String::from_utf8(self.take(n as usize)?.to_vec())?)
        }
    }
    let mut r = R(b);
    let path = r.string()?;
    let kind = match r.u32()? {
        0 => crate::index::Kind::File,
        1 => crate::index::Kind::Dir,
        _ => bail!("the record is damaged"),
    };
    let deleted = r.bool()?;
    let mode = r.u32()?;
    let size = r.u64()?;
    let mtime_ns = r.u64()?;
    let stamp = r.u64()?;
    let hash = r.string()?;
    let n = r.u64()?;
    // each chunk name takes at least 8 bytes
    if n > (r.0.len() / 8) as u64 {
        bail!("the record is damaged");
    }
    let mut chunks = Vec::new();
    for _ in 0..n {
        chunks.push(r.string()?);
    }
    Ok(Entry { path, kind, deleted, mode, size, mtime_ns, stamp, hash, chunks })
}

pub struct Vault {
    blobs: XChaCha20Poly1305,
    records: XChaCha20Poly1305,
    slot_key: [u8; 32],
    blob_id_key: [u8; 32],
}

fn seal(c: &XChaCha20Poly1305, aad: &[u8], plain: &[u8]) -> Vec<u8> {
    let nonce: [u8; 24] = rand::random();
    let ct = c
        .encrypt(XNonce::from_slice(&nonce), Payload { msg: plain, aad })
        .expect("encryption cannot fail");
    let mut out = nonce.to_vec();
    out.extend_from_slice(&ct);
    out
}

fn open(c: &XChaCha20Poly1305, aad: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 24 + 16 {
        bail!("truncated data from the depot");
    }
    let (nonce, ct) = data.split_at(24);
    c.decrypt(XNonce::from_slice(nonce), Payload { msg: ct, aad })
        .map_err(|_| anyhow!("decryption failed: wrong vault key, or the depot's data was tampered with"))
}

/// Proof that a device knows a library's key, without revealing it. A machine keeping an
/// encrypted backup stores this when it first accepts the library; later a paired device
/// (another of your machines, or a replacement after a loss) that presents the same value is
/// let in without further questions.
///
/// The value is made for one backup machine (`server` is its public key): what one backup
/// machine holds is useless at any other, so a backup machine that is lost or hacked cannot use
/// it to get into the other backups of the same library.
pub fn access_token(master: &[u8], server: &str) -> String {
    let mut input = master.to_vec();
    input.extend_from_slice(server.as_bytes());
    crate::net::hex(&blake3::derive_key("rustsync vault v2 backup access for one server", &input))
}

fn recovery_check(id: &str, vault: &str) -> String {
    blake3::hash(format!("rustsync recovery v1|{}|{}", id, vault).as_bytes()).to_hex().as_str()[..8].to_string()
}

/// The text you keep somewhere safe (a password manager): it holds the library's id and key,
/// which is everything needed, together with any encrypted backup, to get the library back.
pub fn recovery_code(lib_id: &str, vault_hex: &str) -> String {
    format!("rsr1-{}-{}-{}", lib_id, vault_hex, recovery_check(lib_id, vault_hex))
}

/// -> (library id, vault key hex)
pub fn parse_recovery_code(code: &str) -> Result<(String, String)> {
    let c: String = code.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let parts: Vec<&str> = c.split('-').collect();
    if parts.len() != 4 || parts[0] != "rsr1" {
        bail!("that does not look like a rustsync recovery code (it starts with rsr1-)");
    }
    if !crate::net::valid_hex(parts[1], 32) || !crate::net::valid_hex(parts[2], 64) || !crate::net::valid_hex(parts[3], 8) {
        bail!("the recovery code is incomplete or has a typo");
    }
    if recovery_check(parts[1], parts[2]) != parts[3] {
        bail!("the recovery code has a typo (its check value does not match)");
    }
    Ok((parts[1].to_string(), parts[2].to_string()))
}

const MAX_KIT_BYTES: usize = 1024 * 1024;

#[derive(serde::Serialize, serde::Deserialize)]
struct KitLibrary {
    id: String,
    name: String,
    vault: String,
}

fn kit_check(payload_hex: &str) -> String {
    blake3::hash(format!("rustsync kit v1|{}", payload_hex).as_bytes()).to_hex().as_str()[..8].to_string()
}

/// One line holding every library's id, name and key: everything (together with the encrypted
/// backups) needed to get all libraries back. Meant for a password manager. It does not contain
/// this device's identity: a replacement machine is simply paired again.
pub fn recovery_kit(libs: &[(String, String, String)]) -> String {
    let items: Vec<KitLibrary> = libs
        .iter()
        .map(|(id, name, vault)| KitLibrary { id: id.clone(), name: name.clone(), vault: vault.clone() })
        .collect();
    let json = serde_json::to_vec(&items).unwrap_or_default();
    let payload = crate::net::hex(&json);
    format!("rsk1-{}-{}", payload, kit_check(&payload))
}

/// -> [(library id, name, vault key hex)]
pub fn parse_recovery_kit(kit: &str) -> Result<Vec<(String, String, String)>> {
    let c: String = kit.chars().filter(|c| !c.is_whitespace()).collect::<String>().to_lowercase();
    let parts: Vec<&str> = c.split('-').collect();
    if parts.len() != 3 || parts[0] != "rsk1" {
        bail!("that does not look like a rustsync recovery kit (it starts with rsk1-)");
    }
    let (payload, check) = (parts[1], parts[2]);
    if payload.len() > MAX_KIT_BYTES * 2 || payload.len() % 2 != 0 || !payload.chars().all(|c| c.is_ascii_hexdigit()) {
        bail!("the recovery kit is incomplete or has a typo");
    }
    if kit_check(payload) != check {
        bail!("the recovery kit has a typo (its check value does not match)");
    }
    let mut bytes = Vec::with_capacity(payload.len() / 2);
    for i in (0..payload.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&payload[i..i + 2], 16).map_err(|_| anyhow!("the recovery kit has a typo"))?);
    }
    let items: Vec<KitLibrary> = serde_json::from_slice(&bytes).map_err(|_| anyhow!("the recovery kit is damaged"))?;
    let mut out = Vec::new();
    for it in items.into_iter().take(10_000) {
        if !crate::net::valid_hex(&it.id, 32) || !crate::net::valid_hex(&it.vault, 64) {
            bail!("the recovery kit holds an invalid library");
        }
        out.push((it.id, it.name, it.vault));
    }
    Ok(out)
}

impl Vault {
    pub fn new(master: &[u8]) -> Vault {
        let derive = |ctx: &str| blake3::derive_key(ctx, master);
        Vault {
            blobs: XChaCha20Poly1305::new_from_slice(&derive("rustsync vault v1 chunk encryption"))
                .expect("32-byte key"),
            records: XChaCha20Poly1305::new_from_slice(&derive("rustsync vault v1 record encryption"))
                .expect("32-byte key"),
            slot_key: derive("rustsync vault v1 path hiding"),
            blob_id_key: derive("rustsync vault v1 chunk naming"),
        }
    }

    /// Opaque, stable name for a path (the depot keeps one record per slot).
    pub fn slot(&self, path: &str) -> String {
        blake3::keyed_hash(&self.slot_key, path.as_bytes()).to_hex().to_string()
    }

    /// Opaque name for a chunk, derived from its plaintext hash.
    pub fn blob_id(&self, plain_hash: &str) -> String {
        blake3::keyed_hash(&self.blob_id_key, plain_hash.as_bytes()).to_hex().to_string()
    }

    pub fn seal_blob(&self, id: &str, chunk: &[u8]) -> Vec<u8> {
        seal(&self.blobs, id.as_bytes(), chunk)
    }

    pub fn open_blob(&self, id: &str, sealed: &[u8]) -> Result<Vec<u8>> {
        open(&self.blobs, id.as_bytes(), sealed)
    }

    pub fn seal_record(&self, slot: &str, e: &Entry) -> Result<Vec<u8>> {
        let mut plain = RECORD_MAGIC.to_vec();
        plain.extend(postcard::to_allocvec(e)?);
        Ok(seal(&self.records, slot.as_bytes(), &plain))
    }

    pub fn open_record(&self, slot: &str, sealed: &[u8]) -> Result<Entry> {
        let plain = open(&self.records, slot.as_bytes(), sealed)?;
        let e: Entry = match plain.strip_prefix(&RECORD_MAGIC[..]) {
            Some(rest) => postcard::from_bytes(rest)?,
            // Written by an older version.
            None => legacy_entry(&plain)?,
        };
        if self.slot(&e.path) != slot {
            bail!("the depot returned a record for the wrong path");
        }
        Ok(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::Kind;

    #[test]
    fn the_access_value_is_made_for_one_backup_machine() {
        let key = [7u8; 32];
        let (a, b) = ("a".repeat(64), "b".repeat(64));
        assert_eq!(access_token(&key, &a), access_token(&key, &a));
        assert_ne!(access_token(&key, &a), access_token(&key, &b));
        assert_ne!(access_token(&key, &a), access_token(&[8u8; 32], &a));
        assert_eq!(access_token(&key, &a).len(), 64);
    }

    fn entry() -> Entry {
        Entry {
            path: "docs/a.txt".to_string(),
            kind: Kind::File,
            deleted: false,
            mode: 0o644,
            size: 5,
            mtime_ns: 7,
            stamp: 9,
            hash: "ab".repeat(32),
            chunks: vec!["cd".repeat(32), "ef".repeat(32)],
        }
    }

    /// A record as versions before 0.6 stored it.
    fn old_layout(e: &Entry) -> Vec<u8> {
        let mut b = Vec::new();
        let s = |b: &mut Vec<u8>, t: &str| {
            b.extend((t.len() as u64).to_le_bytes());
            b.extend(t.as_bytes());
        };
        s(&mut b, &e.path);
        b.extend(0u32.to_le_bytes());
        b.push(e.deleted as u8);
        b.extend(e.mode.to_le_bytes());
        b.extend(e.size.to_le_bytes());
        b.extend(e.mtime_ns.to_le_bytes());
        b.extend(e.stamp.to_le_bytes());
        s(&mut b, &e.hash);
        b.extend((e.chunks.len() as u64).to_le_bytes());
        for c in &e.chunks {
            s(&mut b, c);
        }
        b
    }

    #[test]
    fn records_round_trip_and_old_ones_still_open() {
        let v = Vault::new(&[7u8; 32]);
        let e = entry();
        let slot = v.slot(&e.path);
        let back = v.open_record(&slot, &v.seal_record(&slot, &e).unwrap()).unwrap();
        assert_eq!(back.path, e.path);
        assert_eq!(back.chunks, e.chunks);
        assert_eq!(back.stamp, 9);

        let old = seal(&v.records, slot.as_bytes(), &old_layout(&e));
        let back = v.open_record(&slot, &old).unwrap();
        assert_eq!(back.path, e.path);
        assert_eq!(back.hash, e.hash);
        assert_eq!(back.chunks, e.chunks);
        assert_eq!(back.mode, 0o644);
        assert_eq!(back.mtime_ns, 7);
    }

    #[test]
    fn damaged_old_records_are_errors() {
        let b = old_layout(&entry());
        for cut in 0..b.len() {
            assert!(legacy_entry(&b[..cut]).is_err(), "cut at {} accepted", cut);
        }
        let mut huge = b.clone();
        huge[..8].copy_from_slice(&u64::MAX.to_le_bytes());
        assert!(legacy_entry(&huge).is_err());
    }
}
