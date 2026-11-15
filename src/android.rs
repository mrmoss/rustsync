//! What the Android app calls (through JNI, see `android/`). The app runs the same daemon as the
//! desktop program inside its own process, and talks to it over the same local control socket
//! (JSON lines) as the command line and the desktop window do.

use crate::{activity, config, net, sync};
use jni::objects::{JClass, JString};
use jni::sys::jint;
use jni::EnvUnowned;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static DAEMON: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn state_dir(dir: &Path) -> PathBuf {
    dir.join("state")
}

fn start_daemon(dir: &Path, name: &str, port: u16) -> anyhow::Result<()> {
    let mut guard = DAEMON.lock().unwrap();
    if let Some(h) = guard.as_ref() {
        if !h.is_finished() {
            return Ok(()); // already running
        }
    }
    std::fs::create_dir_all(dir)?;
    // An app from before 0.6 has a YAML config: convert it once.
    let cfg_path = config::resolve(&dir.join("config.toml"))?;
    if !cfg_path.exists() {
        let text = format!(
            "name = {}\nstate_dir = {}\n\n[server]\nport = {}\n\n[sync_options]\nfull_scan_interval = 60\n\n[logging]\nlevel = \"info\"\n",
            toml::Value::String(name.to_string()),
            toml::Value::String(state_dir(dir).to_string_lossy().into_owned()),
            port
        );
        std::fs::write(&cfg_path, text)?;
    }
    let cfg = config::load(&cfg_path)?;
    cfg.ensure_dir()?;
    let keys = net::load_or_create_keys(&cfg.private_key_path(), &cfg.public_key_path())?;
    activity::init(&cfg, tracing::Level::INFO, true);
    let handle = std::thread::Builder::new().name("rustsync".to_string()).spawn(move || {
        match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
            Ok(rt) => {
                if let Err(e) = rt.block_on(sync::run_daemon(cfg, keys)) {
                    tracing::error!("rustsync stopped: {:#}", e);
                }
            }
            Err(e) => tracing::error!("cannot start the runtime: {}", e),
        }
    })?;
    *guard = Some(handle);
    Ok(())
}

/// `Core.start(dataDir, deviceName, port)`: start the daemon (if it is not running). Returns an
/// empty string, or what went wrong.
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_start<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    data_dir: JString<'caller>,
    name: JString<'caller>,
    port: jint,
) -> JString<'caller> {
    let outcome = unowned_env.with_env(|env| -> Result<_, jni::errors::Error> {
        let dir = PathBuf::from(data_dir.to_string());
        let name = name.to_string();
        let port = u16::try_from(port).unwrap_or(3032);
        let msg = match start_daemon(&dir, &name, port) {
            Ok(()) => String::new(),
            Err(e) => format!("{:#}", e),
        };
        JString::from_str(env, msg)
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// `Core.socketPath(dataDir)`: where the daemon's control socket is.
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_socketPath<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    data_dir: JString<'caller>,
) -> JString<'caller> {
    let outcome = unowned_env.with_env(|env| -> Result<_, jni::errors::Error> {
        let dir = PathBuf::from(data_dir.to_string());
        let sock = state_dir(&dir).join("control.sock");
        JString::from_str(env, sock.to_string_lossy())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// `Core.stop(dataDir)`: ask the daemon to stop.
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_stop<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
    data_dir: JString<'caller>,
) {
    let outcome = unowned_env.with_env(|_env| -> Result<_, jni::errors::Error> {
        let dir = PathBuf::from(data_dir.to_string());
        let sock = state_dir(&dir).join("control.sock");
        if let Ok(mut s) = std::os::unix::net::UnixStream::connect(sock) {
            let _ = s.write_all(b"\"Stop\"\n");
        }
        Ok(())
    });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}

/// `Core.version()`
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_version<'caller>(
    mut unowned_env: EnvUnowned<'caller>,
    _class: JClass<'caller>,
) -> JString<'caller> {
    let outcome = unowned_env
        .with_env(|env| -> Result<_, jni::errors::Error> { JString::from_str(env, env!("CARGO_PKG_VERSION")) });
    outcome.resolve::<jni::errors::ThrowRuntimeExAndDefault>()
}
