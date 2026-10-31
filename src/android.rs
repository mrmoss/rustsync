//! What the Android app calls (through JNI, see `android/`). The app runs the same daemon as the
//! desktop program inside its own process, and talks to it over the same local control socket
//! (JSON lines) as the command line and the desktop window do.

use crate::{config, net, sync};
use jni::objects::{JClass, JString};
use jni::sys::{jint, jstring};
use jni::JNIEnv;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

static DAEMON: Mutex<Option<std::thread::JoinHandle<()>>> = Mutex::new(None);

fn read_string(env: &mut JNIEnv, s: &JString) -> String {
    env.get_string(s).map(String::from).unwrap_or_default()
}

fn to_java(env: &JNIEnv, s: &str) -> jstring {
    env.new_string(s).map(|j| j.into_raw()).unwrap_or(std::ptr::null_mut())
}

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
    let cfg_path = dir.join("config.yaml");
    if !cfg_path.exists() {
        let yaml = format!(
            "name: {}\nstate_dir: {}\nserver:\n  port: {}\nsync_options:\n  full_scan_interval: 60\nlogging:\n  level: info\n",
            serde_yaml::to_string(name)?.trim(),
            serde_yaml::to_string(&state_dir(dir).to_string_lossy())?.trim(),
            port
        );
        std::fs::write(&cfg_path, yaml)?;
    }
    let cfg = config::load(&cfg_path)?;
    cfg.ensure_dir()?;
    let keys = net::load_or_create_keys(&cfg.private_key_path(), &cfg.public_key_path())?;
    let _ = tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).try_init();
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
pub extern "system" fn Java_app_rustsync_Core_start(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
    name: JString,
    port: jint,
) -> jstring {
    let dir = PathBuf::from(read_string(&mut env, &data_dir));
    let name = read_string(&mut env, &name);
    let port = u16::try_from(port).unwrap_or(3032);
    let msg = match start_daemon(&dir, &name, port) {
        Ok(()) => String::new(),
        Err(e) => format!("{:#}", e),
    };
    to_java(&env, &msg)
}

/// `Core.socketPath(dataDir)`: where the daemon's control socket is.
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_socketPath(
    mut env: JNIEnv,
    _class: JClass,
    data_dir: JString,
) -> jstring {
    let dir = PathBuf::from(read_string(&mut env, &data_dir));
    let sock = state_dir(&dir).join("control.sock");
    to_java(&env, &sock.to_string_lossy())
}

/// `Core.stop(dataDir)`: ask the daemon to stop.
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_stop(mut env: JNIEnv, _class: JClass, data_dir: JString) {
    let dir = PathBuf::from(read_string(&mut env, &data_dir));
    let sock = state_dir(&dir).join("control.sock");
    if let Ok(mut s) = std::os::unix::net::UnixStream::connect(sock) {
        let _ = s.write_all(b"\"Stop\"\n");
    }
}

/// `Core.version()`
#[no_mangle]
pub extern "system" fn Java_app_rustsync_Core_version(env: JNIEnv, _class: JClass) -> jstring {
    to_java(&env, env!("CARGO_PKG_VERSION"))
}
