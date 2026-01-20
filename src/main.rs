use clap::Parser;
use notify::{
    event::{DataChange, MetadataKind, ModifyKind, RenameMode},
    EventKind, RecommendedWatcher, RecursiveMode, Watcher,
};
use std::{
    ffi::CString,
    fs,
    io,
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::mpsc::channel,
};
use filetime::FileTime;

fn cross_platform_symlink(path: &Path, sym_path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs as unix_fs;
        unix_fs::symlink(path, sym_path)
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs as windows_fs;
        if path.is_dir() {
            windows_fs::symlink_dir(path, sym_path)
        } else {
            windows_fs::symlink_file(path, sym_path)
        }
    }
}

fn change_root(watch_root: &Path, output_root: &Path, path: &Path) -> Option<PathBuf> {
    path.strip_prefix(watch_root).ok().map(|relative| output_root.join(relative))
}

fn handle_watch_error(error: &notify::Error) {
    eprintln!("Watch error: {:?}", error);
}

fn handle_not_under_watch_error(watch_root: &Path, path: &Path) {
    eprintln!("Path {:?} is not under watch root {:?}", path, watch_root);
}

fn handle_get_metadata_error(path: &Path, error: &io::Error) {
    eprintln!("Failed to get metadata for {:?}: {:?}", path, error);
}

fn handle_create_dir_error(path: &Path, error: &io::Error) {
    eprintln!("Failed to create dir {:?}: {:?}", path, error);
}

fn handle_event_unknown(event: &notify::Event, path: &Path) {
    eprintln!("Unknown[unsupported]: {:?} {:?}", path, event);
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

fn handle_event_create_hardlink(_watch_root: &Path, _output_root: &Path, path: &Path) {
    eprintln!("Created[unsupported][hardlink]: {:?}", path);
}

fn handle_event_delete(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Deleted: {:?}", path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, path),
    };

    let result = if mirrored_path.is_dir() {
        fs::remove_dir_all(&mirrored_path)
    } else {
        fs::remove_file(&mirrored_path)
    };

    if let Err(error) = result {
        eprintln!("Failed to delete {:?}: {}", mirrored_path, error);
    }
}

fn handle_event_rename(watch_root: &Path, output_root: &Path, path: &Path, new_path: &Path) {
    println!("Renamed: {:?} -> {:?}", path, new_path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, path),
    };

    let mirrored_new_path = match change_root(watch_root, output_root, new_path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, new_path),
    };

    if let Err(error) = fs::rename(&mirrored_path, &mirrored_new_path) {
        eprintln!("Failed to rename {:?} -> {:?}: {}", mirrored_path, mirrored_new_path, error);
    }
}

fn handle_event_metadata(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Modify[metadata]: {:?}", path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, path),
    };

    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return eprintln!("Failed to read metadata for {:?}: {}", path, error),
    };

    if let Err(error) = fs::set_permissions(&mirrored_path, metadata.permissions()) {
        eprintln!("Failed to set permissions for {:?}: {}", mirrored_path, error);
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;

        let atime = FileTime::from_unix_time(metadata.atime(), metadata.atime_nsec() as u32);
        let mtime = FileTime::from_unix_time(metadata.mtime(), metadata.mtime_nsec() as u32);

        if let Err(error) = filetime::set_file_times(&mirrored_path, atime, mtime) {
            eprintln!("Failed to set timestamps for {:?}: {}", mirrored_path, error);
        }

        let c_path = match CString::new(mirrored_path.as_os_str().as_bytes()) {
            Ok(path) => path,
            Err(error) => {
                eprintln!("Failed to convert path for chown {:?}: {}", mirrored_path, error);
                return;
            }
        };

        unsafe {
            if libc::chown(c_path.as_ptr(), metadata.uid(), metadata.gid()) != 0 {
                eprintln!("Failed to set owner/group for {:?}", mirrored_path);
            }
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        let atime =
            FileTime::from_seconds_since_1970(metadata.last_access_time() / 10_000_000, 0);
        let mtime =
            FileTime::from_seconds_since_1970(metadata.last_write_time() / 10_000_000, 0);

        if let Err(error) = filetime::set_file_times(&mirrored_path, atime, mtime) {
            eprintln!("Failed to set timestamps for {:?}: {}", mirrored_path, error);
        }
    }
}

fn handle_event_create_symlink(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Created[symlink]: {:?}", path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, path),
    };

    let original_target = match fs::read_link(path) {
        Ok(target) => target,
        Err(error) => {
            eprintln!("Failed to read symlink {:?}: {}", path, error);
            return;
        }
    };

    let mirrored_target =
        change_root(watch_root, output_root, &original_target).unwrap_or(original_target);

    if let Err(error) = cross_platform_symlink(&mirrored_target, &mirrored_path) {
        eprintln!("Failed to create symlink {:?} -> {:?}: {}", mirrored_path, mirrored_target, error);
    }
}

fn sync_file_to_mirror(watch_root: &Path, output_root: &Path, path: &Path, event_label: &str) {
    println!("{}: {:?}", event_label, path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, path),
    };

    if let Some(parent) = mirrored_path.parent() {
        if let Err(error) = fs::create_dir_all(parent) {
            eprintln!("Failed to create parent dirs for {:?}: {}", mirrored_path, error);
            return;
        }
    }

    if let Err(error) = fs::copy(path, &mirrored_path) {
        eprintln!("Failed to copy file {:?} -> {:?}: {}", path, mirrored_path, error);
    }
}

fn handle_event_create_regularfile(watch_root: &Path, output_root: &Path, path: &Path) {
    sync_file_to_mirror(watch_root, output_root, path, "Created[file]");
}

fn handle_event_data(watch_root: &Path, output_root: &Path, path: &Path) {
    sync_file_to_mirror(watch_root, output_root, path, "Modified[file]");
}

fn handle_event_create_dir(watch_root: &Path, output_root: &Path, path: &Path) {
    println!("Created[dir]: {:?}", path);

    let mirrored_path = match change_root(watch_root, output_root, path) {
        Some(path) => path,
        None => return handle_not_under_watch_error(watch_root, path),
    };

    if let Err(error) = fs::create_dir(&mirrored_path) {
        handle_create_dir_error(&mirrored_path, &error);
    }
}

fn handle_event_create_file(watch_root: &Path, output_root: &Path, path: &Path) {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => return handle_get_metadata_error(path, &error),
    };

    let link_count = {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            metadata.nlink()
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            metadata.number_of_links()
        }
    };

    if link_count > 1 {
        handle_event_create_hardlink(watch_root, output_root, path);
    } else {
        handle_event_create_regularfile(watch_root, output_root, path);
    }
}

fn handle_event(watch_root: &Path, output_root: &Path, event: &notify::Event) {
    let event_kind = &event.kind;
    let paths = &event.paths;
    let path = &paths[0];

    match event_kind {
        EventKind::Other => handle_event_other(watch_root, output_root, path),
        EventKind::Remove(_) => handle_event_delete(watch_root, output_root, path),
        EventKind::Modify(modify_kind) => match modify_kind {
            ModifyKind::Other => handle_event_modify_other(watch_root, output_root, path),
            ModifyKind::Name(RenameMode::Both) => {
                handle_event_rename(watch_root, output_root, path, &paths[1])
            }
            ModifyKind::Metadata(MetadataKind::Any) => {
                handle_event_metadata(watch_root, output_root, path)
            }
            ModifyKind::Data(DataChange::Any) => handle_event_data(watch_root, output_root, path),
            _ => {}
        },
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
        EventKind::Access(_) => {}
        _ => handle_event_unknown(event, path),
    }
}

#[derive(Parser)]
struct Args {
    watch_root: PathBuf,
    output_root: PathBuf,
}

fn main() -> notify::Result<()> {
    let args = Args::parse();
    let watch_root = fs::canonicalize(PathBuf::from(args.watch_root))?;
    let output_root = fs::canonicalize(PathBuf::from(args.output_root))?;

    let (sender, receiver) = channel();
    let mut watcher: RecommendedWatcher = Watcher::new(sender, notify::Config::default())?;

    watcher.watch(&watch_root, RecursiveMode::Recursive)?;

    println!("Watching {:?}", watch_root);
    println!("Outputting to {:?}", output_root);
    println!("(Ctrl+C to quit)");

    for result in receiver {
        match result {
            Ok(event) => handle_event(&watch_root, &output_root, &event),
            Err(error) => handle_watch_error(&error),
        }
    }

    Ok(())
}
