use notify::{RecommendedWatcher, RecursiveMode, Watcher, EventKind, event::{ModifyKind, DataChange, MetadataKind, RenameMode}};
use std::path::{Path, PathBuf};
use std::fs;
use std::io;
use std::sync::mpsc::channel;
use filetime::FileTime;

fn cross_platform_symlink(path: &Path, sym_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs as unix_fs;
        return unix_fs::symlink(&path, &sym_path);
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs as windows_fs;
        return if path.is_dir() {
            windows_fs::symlink_dir(&path, &sym_path);
        } else {
            windows_fs::symlink_file(&path, &sym_path);
        };
    }
}

fn change_root(watch_root: &Path, output_root: &Path, path: &Path) -> Option<PathBuf> {
    let relative = path.strip_prefix(watch_root).ok()?;
    Some(output_root.join(relative))
}

fn handle_watch_error(error: &notify::Error) {
    eprintln!("Watch error: {:?}", error);
}

fn handle_not_under_watch_error(watch_root: &Path, path: &Path) {
    eprintln!("Path {:?} is not under watch root {:?}", path, watch_root);
}

fn handle_get_metadata_error(path: &Path, error: &std::io::Error) {
    eprintln!("Failed to get metadata for {:?}: {:?}", path, error);
}

fn handle_create_dir_error(path: &Path, error: &std::io::Error) {
    eprintln!("Failed to create dir {:?}: {:?}", path, error);
}

fn handle_event_unknown(path: &Path) {
    eprintln!("Unknown[unsupported]: {:?}", path);
}

fn handle_event_other(_watch_root: &Path, _output_root: &Path, path: &Path) {
    eprintln!("Other[unsupported]: {:?}", path);
}

fn handle_event_modify_other(_watch_root: &Path, _output_root: &Path, path: &Path) {
    eprintln!("Modify[unsupported][other]: {:?}", path);
}

fn handle_event_create_other(_watch_root: &Path, _output_root: &Path, path: &Path) {
    eprintln!("Created[unsupported][other]: {:?}", path);
}

fn handle_event_delete(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Deleted: {:?}", path);

    let new_target = match change_root(watch_root, output_root, path) {
        Some(p) => p,
        None => {
            handle_not_under_watch_error(watch_root, path);
            return;
        }
    };

    if let Err(error) = if new_target.is_dir() {
        fs::remove_dir_all(&new_target)
    } else {
        fs::remove_file(&new_target)
    } {
        eprintln!("Failed to delete {:?}: {}", new_target, error);
    }
}

fn handle_event_rename(watch_root: &Path, output_root: &Path, path: &Path, new_path: &Path) {
    println!("Renamed: {:?} -> {:?}", path, new_path);

    let original_target = match change_root(watch_root, output_root, path) {
        Some(p) => p,
        None => {
            handle_not_under_watch_error(watch_root, path);
            return;
        }
    };

    let new_target = match change_root(watch_root, output_root, new_path) {
        Some(p) => p,
        None => {
            handle_not_under_watch_error(watch_root, new_path);
            return;
        }
    };

    if let Err(e) = fs::rename(&original_target, &new_target) {
        eprintln!("Failed to rename {:?} -> {:?}: {}", original_target, new_target, e);
    }
}

fn handle_event_metadata(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Modify[metadata]: {:?}", path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(p) => p,
        None => {
            handle_not_under_watch_error(watch_root, path);
            return;
        }
    };

    let metadata = match fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to read metadata for {:?}: {}", path, e);
            return;
        }
    };

    // Permissions
    if let Err(e) = fs::set_permissions(&mirrored_path, metadata.permissions()) {
        eprintln!("Failed to set permissions for {:?}: {}", mirrored_path, e);
    }

    // Timestamps
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let atime = FileTime::from_unix_time(metadata.atime(), metadata.atime_nsec() as u32);
        let mtime = FileTime::from_unix_time(metadata.mtime(), metadata.mtime_nsec() as u32);

        if let Err(e) = filetime::set_file_times(&mirrored_path, atime, mtime) {
            eprintln!("Failed to set timestamps for {:?}: {}", mirrored_path, e);
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        let atime = FileTime::from_seconds_since_1970(
            metadata.last_access_time() / 10_000_000,
            0,
        );
        let mtime = FileTime::from_seconds_since_1970(
            metadata.last_write_time() / 10_000_000,
            0,
        );

        if let Err(e) = filetime::set_file_times(&mirrored_path, atime, mtime) {
            eprintln!("Failed to set timestamps for {:?}: {}", mirrored_path, e);
        }
    }
    // Owner / Group (Unix only)
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::ffi::OsStrExt;
        use std::ffi::CString;

        let uid = metadata.uid();
        let gid = metadata.gid();

        let c_path = match CString::new(mirrored_path.as_os_str().as_bytes()) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("Failed to convert path for chown {:?}: {}", mirrored_path, e);
                return;
            }
        };

        unsafe {
            if libc::chown(c_path.as_ptr(), uid, gid) != 0 {
                eprintln!("Failed to set owner/group for {:?}", mirrored_path);
            }
        }
    }
}


fn handle_event_data(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Data: {:?}", path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(p) => p,
        None => {
            handle_not_under_watch_error(watch_root, path);
            return;
        }
    };

    // Ensure parent directories exist
    if let Some(parent) = mirrored_path.parent() {
        if let Err(error) = std::fs::create_dir_all(parent) {
            eprintln!("Failed to create parent dirs for {:?}: {}", mirrored_path, error);
            return;
        }
    }

    // Copy the entire file (overwrite if exists)
    if let Err(error) = std::fs::copy(path, &mirrored_path) {
        eprintln!("Failed to copy data {:?} -> {:?}: {}", path, mirrored_path, error);
    }
}

fn handle_event_create_symlink(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Created[symlink]: {:?}", path);

    let new_symlink_path = match change_root(watch_root, output_root, path) {
        Some(p) => p,
        None => {
            handle_not_under_watch_error(watch_root, path);
            return;
        }
    };

    let original_target = match fs::read_link(path) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("Failed to read symlink {:?}: {}", path, e);
            return;
        }
    };

    let new_target = change_root(watch_root, output_root, &original_target).unwrap_or(original_target);

    if let Err(e) = cross_platform_symlink(&new_target, &new_symlink_path) {
        eprintln!("Failed to create symlink {:?} -> {:?}: {}",new_symlink_path, new_target, e);
    }
}

fn handle_event_create_hardlink(_watch_root: &Path, _output_root: &Path, path: &Path) {
    eprintln!("Created[unsupported][hardlink]: {:?}", path);
}

fn handle_event_create_regularfile(_watch_root: &Path, _output_root: &Path, path: &Path) {
    println!("Created[file]: {:?}", path);
}

fn handle_event_create_dir(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Created[dir]: {:?}", path);
    if let Some(new_path) = change_root(watch_root, output_root, path) {
        if let Err(error) = fs::create_dir(&new_path) {
            handle_create_dir_error(&new_path, &error);
        }
    } else {
        handle_not_under_watch_error(watch_root, path);
    }
}

fn handle_event_create_file(watch_root: &Path, output_root: &Path, path: &Path) {
    let meta = match fs::metadata(path) {
        Ok(data) => data,
        Err(error) => {
            handle_get_metadata_error(path, &error);
            return;
        }
    };

    let nlink = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            meta.nlink()
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            meta.number_of_links()
        }
    };

    if nlink > 1 {
        handle_event_create_hardlink(watch_root, output_root, path);
    } else {
        handle_event_create_regularfile(watch_root, output_root, path);
    }
}

fn handle_event(watch_root: &Path, output_root: &Path, event_kind: &EventKind, paths: &[PathBuf]) {
    let path = &paths[0];

    match event_kind {
        EventKind::Other => {
            handle_event_other(watch_root, output_root, path);
        }
        EventKind::Remove(_) => {
            handle_event_delete(watch_root, output_root, path);
        }
        EventKind::Modify(mod_kind) => {
            match mod_kind {
                ModifyKind::Other => {
                    handle_event_modify_other(watch_root, output_root, path);
                }
                ModifyKind::Name(rename_mode) => match rename_mode {
                    RenameMode::Both => {
                        let path_new = &paths[1];
                        handle_event_rename(watch_root, output_root, path, path_new);
                    }
                    _ => {}
                }
                ModifyKind::Metadata(metadata_mode) => match metadata_mode {
                    MetadataKind::Any => {
                        handle_event_metadata(watch_root, output_root, path);
                    }
                    _ => {}
                }
                ModifyKind::Data(data_change) => match data_change {
                    DataChange::Any => {
                        handle_event_data(watch_root, output_root, path);
                    }
                    _ => {}
                }
                _ => {}
            }
        }
        EventKind::Create(_) => {
            if path.is_symlink() {
                handle_event_create_symlink(watch_root, output_root, path);
            } else if path.is_file() {
                handle_event_create_file(watch_root, output_root, path);
            } else if path.is_dir() {
                handle_event_create_dir(watch_root, output_root, path);
            } else {
                handle_event_create_other(watch_root, output_root, path);
            }
        }
        EventKind::Access(_) => {
        }
        _ => {
            handle_event_unknown(path);
        }
    }
}

fn main() -> notify::Result<()> {
    let (tx, rx) = channel();
    let mut watcher: RecommendedWatcher = Watcher::new(tx, notify::Config::default())?;
    let watch_root = &fs::canonicalize(Path::new("./test/input"))?;
    let output_root = &fs::canonicalize(Path::new("./test/output"))?;
    match watcher.watch(watch_root, RecursiveMode::Recursive) {
        Ok(_) => {
            println!("Watching {:?} (Ctrl+C to quit)", watch_root);

            for result in rx {
                match result {
                    Ok(event) => {
                        handle_event(watch_root, output_root, &event.kind, &event.paths);
                    }
                    Err(error) => {
                        handle_watch_error(&error);
                    }
                }
            }
        }
        Err(e) => {
            eprintln!("Failed to watch {:?}: {}", watch_root, e);
        }
    }

    Ok(())
}
