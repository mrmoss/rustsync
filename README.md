# rustsync

!!!VIBECODED CLANKER CODE!!!

Simple, end-to-end encrypted file sync. Two programs are built from this one package:

| Program | Source | What it is |
|---|---|---|
| `rustsync` | `src/main.rs` | the sync service and command line (`status`, `invite`, `join`, `create`, `share`, `accept`, ...) |
| `rustsync-gui` | `src/gui.rs` | an optional desktop window (egui) that drives the same service |

    cargo build --release                          # builds both -> target/release/
    cargo build --release --no-default-features    # sync program only (servers: no GUI libraries)

The GUI talks to the running service over its local control socket, so everything it does the
command line can do too, and closing the window does not stop syncing.

    ./target/release/rustsync-gui                  # uses ~/.config/rustsync/config.yaml
    ./target/release/rustsync-gui -c alpha_config.yaml

"Start rustsync" in the GUI runs `rustsync -c <config> start`; it looks for `rustsync` next to
the GUI binary first (where `cargo build` puts it), then on your `PATH` (or set `RUSTSYNC_BIN`).
Linux and macOS (the service uses a Unix socket). See `SECURITY.md` for the threat model.

## Starting at boot, and the tray icon (Linux)

    rustsync install-service        # start rustsync by itself (a systemd *user* service)
    rustsync uninstall-service      # stop doing that (also stops rustsync now)

`install-service` writes `~/.config/systemd/user/rustsync.service` (other configs get their own
name), enables it and starts it. It restarts rustsync if it crashes, and logs to the usual
`daemon.log`. It then offers `loginctl enable-linger`, which is what makes it start at **boot**
instead of when you log in (`--yes` accepts). The first time you run `rustsync start` in a
terminal it offers this too (once). With the service installed, `start` and `stop` simply work
with it. The GUI's *This device* page and the tray menu have the same switch.

The service itself needs no desktop. The tray icon does, so it starts when you **log in to the
desktop** (not at bare boot): `install-service` also adds `~/.config/autostart/rustsync-tray.desktop`
(if `rustsync-gui` sits next to `rustsync`). Without that file, only the background service starts.

`rustsync-gui` also puts an icon in the system tray (Linux, StatusNotifierItem over D-Bus). It
is green when the service is running, amber when libraries are being offered to you, grey when
the service is not running. Click it (or *Open rustsync*) to get the window; closing the window
leaves the icon, and syncing, running. The menu also starts/stops the service and has
*Show this icon at login*.

    rustsync-gui --tray      # only the icon, no window (what the login entry runs)
    rustsync-gui --no-tray   # only the window

The icon is its own small process, so it behaves the same on X11 and Wayland. It needs a desktop
that shows tray icons: KDE, Cinnamon, LXQt, Ubuntu's GNOME and sway/waybar do; plain GNOME needs
the "AppIndicator" extension; i3 needs `snixembed`. Without one, the icon exits quietly and the
window works as before. At login the icon waits a few minutes for the panel to be ready;
what it did is written to `tray.log` in the state directory. Build without it with `--no-default-features --features gui`.

## Encrypted backups (for a server, or if everything is lost)

A library can be kept in two ways on another machine. With `share` the other machine syncs the
files like any device. With `share --backup` it only stores an **encrypted** copy it can't read.

    # on the laptop that has the files
    rustsync share files myserver --backup

    # on the server: you pick where the encrypted data lives (e.g. another drive)
    rustsync accept            # asks for a folder; nothing in it can be read on the server

* **Pairing later is fine.** If a machine is paired with the server *after* it got the library,
  it asks the machines it is connected to for their lists again and joins the backup by itself.
  `rustsync refresh` (the *Refresh* button) does the same by hand, e.g. if the other machine was
  offline at the time.
* **Other machines use it automatically.** Machines that sync `files` with the laptop learn about
  the backup and join it (if they are paired with the server). So when the laptop writes to the
  server and then goes offline, a machine that was off can start up, talk only to the server, and
  catch up. Changes reach online machines right away; others catch up when they connect.
* **Where it is stored** is chosen per backup when you `accept` (a suggested default is
  `backup_dir:` in the config, or `<state dir>/backups`). It is just a folder; put it on any drive.
* **Unsharing and re-sharing.** `rustsync unshare` (*Unshare…* in the GUI) stops sharing a library
  with a device; the files stay on both sides. Removing a library on one machine also tells the
  others, so the owner can simply `share` it again and you `accept` it again. For a backup you
  stopped serving, the device's next request shows up as an offer on the server (*Allow* /
  `accept`) and needs no new folder.
* **Size limit.** `accept` asks how much space the backup may use (e.g. `500G`, `2T`, empty = no
  limit). Change it any time with `rustsync quota` (or *Set limit…* in the GUI). `rustsync status`
  and the GUI show a usage bar on the server and on the machines that use it. At the limit new
  data is refused (what is stored stays); the bar turns red and says so.
* **Freeing space.** Old versions of changed or deleted files leave unused chunks in the backup.
  `rustsync prune` (*Free space in backups* in the GUI), run on a machine that has the files,
  deletes chunks nothing refers to any more, but only ones unused for over 7 days. It also runs by
  itself (at most hourly) when a backup is over 90 % full. If anything can't be read, nothing is
  deleted.
* **Library ids.** The server never learns library names; it calls a backup `backup-<id>`. Every
  library shows its short `id` in `status` and in the GUI (with *Copy full id*), so you can match
  them up.
* **If every machine is lost:**

      rustsync recovery               # run on a machine that has the files

  prints a **recovery kit**: one line (`rsk1-…`) with every library's id, name and key, plus an
  `rsr1-…` code per library. Save the kit as a secure note in your password manager (Bitwarden
  etc.). It does not contain this machine's identity: a replacement is simply paired again.

  On a new machine: pair it with the server (`invite` on the server, `join` here), then

      rustsync restore                # asks for the kit (or one code), an empty folder, the server

  Every library in the kit comes back into its own subfolder, with its name. The server lets the
  new machine in because the kit proves it holds the keys. (A library the server keeps no backup
  of stays empty.) The GUI has the same: *Recovery kit…* and *Restore from backup…* on the
  Libraries page, *Recovery code…* on a single library.
