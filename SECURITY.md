# Security notes

## Trust model

- **Pairing is the trust step.** Only devices you paired (one-time code, then a per-pair secret
  mixed into a Noise XX handshake with X25519 + ChaCha20-Poly1305 + BLAKE2s) can connect.
  Strangers are dropped during the handshake.
- **How pairing is protected.** The pairing code (also shown as a QR code) holds the inviting
  device's public key, a random one-time secret and its address. The joining device only goes on
  if the machine it reaches proves it owns that key, so it cannot be tricked into pairing with an
  impostor. The secret is single-use and expires after 10 minutes. But anyone who *sees* the code
  (a screenshot, a shoulder-surfer, a chat it was pasted into) could use it first, so nothing is
  trusted on the code alone: both devices derive an 8-digit verification code from the secret and
  both public keys, show it with the other device's name and key fingerprint, and **a person on
  each side must confirm they match** before the per-pair secret is handed over or any device is
  stored. An uninvited joiner shows up on the inviting device as a request that has no matching
  code on any screen of yours, and the answer is no. Treat the code like a password until it has
  been used, do not post it anywhere, and answer no whenever you did not just start pairing yourself.
  Limits: the 8 digits are a guard for a person comparing two screens, not a secret, and a person
  who clicks yes without looking defeats it. Pairing messages changed (all your devices need the
  new version to pair again; already paired devices keep working).
- **A device you share a library with, for syncing, can change that library's files on this
  machine.** That is what syncing means. A device that only keeps an *encrypted backup*
  (`share --backup`) has no write access to your files at all: it never receives the key, the
  library name or any plaintext, and plain updates from it are ignored.
- **Who may use an encrypted backup.** The machine that keeps it decides: the first offer has to be
  accepted by hand (`accept`, where you also choose the folder). Afterwards a paired device is let
  in without asking only if it proves it holds the library's key (a value derived from the key with
  BLAKE3; the key itself is never sent). This is how your other machines, and a replacement machine
  after a loss, reach the backup.
- **Membership stays in step.** Each device lists the libraries it holds for the other. A device
  that stops listing one (library removed, or unshared) is dropped as a member on the other side.
  A device you unshared is not added back by other devices' hints about backups, and for a backup
  you keep it must be allowed again by hand. Anyone holding a library's key can still ask for
  access to its backup, so unsharing is housekeeping, not revocation: to lock out a key holder,
  make a new library.
- **Recovery kit and codes** (`rustsync recovery`) contain library ids, names and keys. Anyone with
  a kit or code *and* a copy of the encrypted backup can read the library, so keep them in a
  password manager (a secure note is fine; it is encrypted there). They never appear in logs or the
  status output. The kit holds no device identity or pairing secrets, so losing it to someone does
  not let them impersonate your machines, and restoring needs a fresh pairing.
- Nothing from another device is trusted blindly. Everything below is checked first.

## What is enforced

| Area | Protection |
|---|---|
| Paths | Absolute paths, `..`, `.`, empty parts, NUL/control characters, backslashes, over-long names and our temp-file names are rejected (`index::safe_rel`; unit-tested). Local names that cannot be synced are skipped with a warning instead of being sent. |
| Entry contents | Every received entry is validated before use (`index::sanitize_remote`): size must match the chunk count, hashes must be 64 hex characters, deletions/directories must carry no data, files are limited to 16 GiB. |
| Clock tampering | A change time more than `max_clock_skew_secs` (default 24 h) ahead of this machine's clock is ignored, so a wrong or hostile clock cannot make every later edit lose. Stamps use saturating arithmetic (no overflow). |
| Permissions | Modes from other devices are reduced to `rwxr-xr-x` at most: no setuid/setgid/sticky, never group/world writable, and always usable by the owner. |
| Symlinks / races | Files are opened with `O_NOFOLLOW`, and on Linux the open descriptor is checked to really be inside the library folder (`/proc/self/fd`) before reading (scan, serving chunks) or writing (temp files). Symlinked ancestors are refused. |
| Secrets stay out of libraries | A library folder may not contain (or sit inside) the state directory or either key file, or overlap a folder that holds an encrypted backup. Checked when a library is created/accepted *and* every time it starts. A backup folder may not contain the state directory or the key files, and is only reopened if its marker file is still there (an unmounted drive is refused, not treated as empty). |
| Key files | The private key must be a regular file, owned by you, mode 0600 (not a symlink); otherwise rustsync refuses to start. The public key file is always derived from the private key. New keys are created atomically and never overwrite an existing key. |
| Backup size | Each encrypted backup can have a size limit (`quota`) that the machine keeping it enforces on every write, so a paired device cannot fill the drive past it. Unused chunks are only ever deleted by a machine holding the key, only if older than 7 days, and only after every record in the backup was read and decrypted. |
| Memory / CPU | Small bounded queues everywhere a peer controls the input; a byte budget for pending index updates; at most 16 requests per device served at once (extras refused); index messages capped by size and count; library/offer/backup lists capped; a device that stops reading is dropped after 60 s. |
| Temporary files | The files rustsync writes before renaming them into place (in encrypted backups and for the public key) get unpredictable names, are created exclusively (they fail if anything, a symlink included, is already there), are never opened through a symlink, and, for backups, are readable by the owner only. |
| Local control socket | `control.sock` is owner-only (`0600`) in a `0700` directory, and a request is one line of at most 1 MiB. |
| Connection floods | At most 32 unauthenticated connections at once and 4 per IP address, 10 s handshake deadline. |

## Known limits

- A device you sync a library with can still delete or overwrite files **in that library**.
  Only share libraries with machines you trust; use `--backup` for anything that should not
  have write access. (A send-only / receive-only role would be a natural next step.)
- The last step of replacing a file is a `rename`, which cannot be made fully race-free with
  portable APIs. Do not give untrusted local users write access to a synced folder.
- The descriptor check (is the open file really inside the library folder?) works on Linux (`/proc`) and macOS (`F_GETPATH`). On FreeBSD, OpenBSD and NetBSD only `O_NOFOLLOW` and the ancestor checks guard against a symlink being swapped in while a file is replaced, so do not let untrusted local users write to a synced folder there.
- On case-insensitive file systems (the macOS default) a peer's files whose names differ only in case collide: one replaces the other. They stay inside the library, but a sync between such a machine and a case-sensitive one can lose one of the two files.
- An encrypted backup cannot check its own contents (it has no key). If a stored chunk is damaged, or a machine holding the library key uploaded a bad one first, the backup keeps it: restoring that file fails (it is detected, not silently wrong) until the chunk is deleted and uploaded again. There is no automatic repair yet.
- Treat "holds the library key" as "fully trusted for that backup": such a device can already read, delete and overwrite everything in it.
- Size limits are per backup and optional; with none set, a paired device that may back up there can fill the drive. The limit counts the bytes rustsync stores, not other files on the same drive.
- A recovery code is per library and does not change; save it once. Losing every machine *and* every code means the encrypted backup cannot be opened (that is the point).
- Library names are not sent to a machine that only keeps a backup (its offer shows `backup-<id>`); every library shows its id in `status` and the GUI. A restore from a single code takes the name from the folder; a kit carries the names.
- Files larger than 16 GiB are skipped (with a warning).
- Conflict resolution is last-writer-wins on wall-clock time; keep clocks in sync (NTP).
- Dependencies are not audited here: commit `Cargo.lock` and run `cargo audit` regularly.
- **Android.** The app keeps its keys in its private storage (not in the phone's hardware key store), and
  turns off Android's cloud backup for them. Syncing folders outside the app needs "all files access",
  which lets the app (and so anything that can drive it) read and write every file on shared storage:
  only share libraries you mean to. Unless you set your own signing key (see the README), builds are
  signed with a throw-away key and cannot be told apart from a tampered copy by their signature, so
  take the APK from your own repository's builds or releases.
- **QR codes and the camera.** The QR code is the pairing code, so a photo or screenshot of it is as
  sensitive as the text (it works once, and needs a person's approval on both devices, but do not
  post it). The camera reader (window and Android app) only takes a picture while the scan screen is
  open, decodes it locally, and does nothing with the picture afterwards.
