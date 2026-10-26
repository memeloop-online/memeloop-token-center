//! Windows publication for plugin installs. Directory identity is a file id
//! taken from an open handle, and names are created relative to that handle
//! so a symlink cannot be followed into another directory.

use std::{
    ffi::OsStr,
    io::{self, Read, Write},
    path::{Component, Path},
    thread,
    time::{Duration, Instant},
};

use crate::windows_dir::{self, Directory};

const OWNER_LIMIT: u64 = 8192;

pub(crate) fn report_io(stage: &'static str, error: &io::Error) {
    eprintln!(
        "{}",
        serde_json::json!({"mtc_plugin_install":1,"stage":stage,"category":"storage","errno":error.raw_os_error()})
    );
}

pub(crate) fn directory_fd(path: &Path) -> io::Result<Directory> {
    windows_dir::open_directory(path)
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    windows_dir::open_directory(path)?.sync()
}

pub(crate) fn claim_directory(root: &Path, owner: &[u8]) -> io::Result<Directory> {
    claim_name(root, owner)?;
    let parent_path = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let parent = windows_dir::open_writable_directory(parent_path)?;
    let binding = format!(".mtc-publish-inode-{name}");
    match windows_dir::create_dir(&parent, OsStr::new(name)) {
        Ok(directory) => {
            let identity = file_identity(&directory)?;
            let temporary = format!(".mtc-publish-claim-{}", uuid::Uuid::now_v7());
            write_exclusive(&parent, parent_path, &temporary, identity.as_bytes())?;
            match windows_dir::hard_link(
                &parent,
                OsStr::new(&temporary),
                &parent,
                OsStr::new(&binding),
            ) {
                Ok(()) => {}
                Err(error) => {
                    let _ = windows_dir::remove_file(&parent, OsStr::new(&temporary));
                    return Err(error);
                }
            }
            let _ = windows_dir::remove_file(&parent, OsStr::new(&temporary));
            directory.sync()?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            windows_dir::open_subdir(&parent, OsStr::new(name))?;
        }
        Err(error) => return Err(error),
    }
    let directory = windows_dir::open_writable_directory(root)?;
    let identity = file_identity(&directory)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match windows_dir::read_limited(&parent, OsStr::new(&binding), OWNER_LIMIT) {
            Ok(bytes) if bytes == identity.as_bytes() => break,
            Err(error) if error.kind() == io::ErrorKind::NotFound && Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
            Err(error) => return Err(error),
        }
    }
    parent.sync()?;
    Ok(directory)
}

pub(crate) fn link_file_at(
    source: &Path,
    directory: &Directory,
    relative: &Path,
) -> io::Result<()> {
    let source_parent =
        windows_dir::open_directory(source.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let source_name = source.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let target_parent = parent_directory(directory, relative)?;
    let target_name = relative.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let mut source_file = windows_dir::open_file(&source_parent, source_name)?;
    match windows_dir::hard_link(&source_parent, source_name, &target_parent, target_name) {
        Ok(()) => {
            let linked = windows_dir::open_file(&target_parent, target_name)?;
            if windows_dir::file_id_of(&source_file)? != windows_dir::file_id_of(&linked)? {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut target_file = windows_dir::open_file(&target_parent, target_name)?;
            if !same_bytes(&mut source_file, &mut target_file)? {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    target_parent.sync()
}

pub(crate) fn clear_claimed_directory(root: &Path, owner: &[u8]) -> io::Result<bool> {
    let parent_path = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let parent = windows_dir::open_writable_directory(parent_path)?;
    let owner_marker = format!(".mtc-publish-owner-{name}");
    let inode_marker = format!(".mtc-publish-inode-{name}");
    match windows_dir::read_limited(&parent, OsStr::new(&owner_marker), OWNER_LIMIT) {
        Ok(bytes) if bytes == owner => {}
        Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return match windows_dir::open_subdir(&parent, OsStr::new(name)) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
                Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
                Err(error) => Err(error),
            };
        }
        Err(error) => return Err(error),
    }
    let directory = windows_dir::open_writable_directory(root)?;
    let identity = file_identity(&directory)?;
    match windows_dir::read_limited(&parent, OsStr::new(&inode_marker), OWNER_LIMIT) {
        Ok(bytes) if bytes == identity.as_bytes() => {}
        _ => return Err(io::ErrorKind::AlreadyExists.into()),
    }
    windows_dir::remove_tree(&directory)?;
    windows_dir::remove_dir(&parent, OsStr::new(name))?;
    let _ = windows_dir::remove_file(&parent, OsStr::new(&owner_marker));
    let _ = windows_dir::remove_file(&parent, OsStr::new(&inode_marker));
    parent.sync()?;
    Ok(true)
}

fn claim_name(root: &Path, owner: &[u8]) -> io::Result<()> {
    if owner.is_empty() || owner.len() as u64 > OWNER_LIMIT {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let parent_path = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let parent = windows_dir::open_writable_directory(parent_path)?;
    let marker = format!(".mtc-publish-owner-{name}");
    match windows_dir::read_limited(&parent, OsStr::new(&marker), OWNER_LIMIT) {
        Ok(bytes) if bytes == owner => return Ok(()),
        Ok(_) => return Err(io::ErrorKind::AlreadyExists.into()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    match windows_dir::open_subdir(&parent, OsStr::new(name)) {
        Ok(_) => {
            return match windows_dir::read_limited(&parent, OsStr::new(&marker), OWNER_LIMIT) {
                Ok(bytes) if bytes == owner => Ok(()),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    Err(io::ErrorKind::AlreadyExists.into())
                }
                Ok(_) => Err(io::ErrorKind::AlreadyExists.into()),
                Err(error) => Err(error),
            };
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) if windows_dir::is_reparse_refusal(&error) => {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        Err(error) => return Err(error),
    }
    let temporary = format!(".mtc-publish-claim-{}", uuid::Uuid::now_v7());
    write_exclusive(&parent, parent_path, &temporary, owner)?;
    let linked = windows_dir::hard_link(
        &parent,
        OsStr::new(&temporary),
        &parent,
        OsStr::new(&marker),
    );
    if let Err(error) = linked {
        if error.kind() == io::ErrorKind::AlreadyExists {
            let verified =
                windows_dir::read_limited(&parent, OsStr::new(&marker), OWNER_LIMIT)? == owner;
            let _ = windows_dir::remove_file(&parent, OsStr::new(&temporary));
            if !verified {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
        } else {
            let _ = windows_dir::remove_file(&parent, OsStr::new(&temporary));
            return Err(error);
        }
    } else {
        let _ = windows_dir::remove_file(&parent, OsStr::new(&temporary));
    }
    parent.sync()
}

fn write_exclusive(
    parent: &Directory,
    parent_path: &Path,
    name: &str,
    bytes: &[u8],
) -> io::Result<()> {
    windows_dir::ensure_component(OsStr::new(name))?;
    let mut file = windows_dir::create_file(parent, OsStr::new(name))?;
    let path = parent_path.join(name);
    if path.parent() != Some(parent_path) {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    windows_dir::restrict_to_current_user(&path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

fn parent_directory(root: &Directory, relative: &Path) -> io::Result<Directory> {
    let parent = relative.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let mut directory = root.try_clone()?;
    if parent.as_os_str().is_empty() {
        return Ok(directory);
    }
    for component in parent.components() {
        let Component::Normal(name) = component else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        directory = match windows_dir::open_writable_subdir(&directory, name) {
            Ok(existing) => existing,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                windows_dir::create_dir(&directory, name)?
            }
            Err(error) => return Err(error),
        };
    }
    Ok(directory)
}

fn file_identity(directory: &Directory) -> io::Result<String> {
    let id = directory.file_id()?;
    Ok(format!("{}:{}:{}", id.volume, id.index_high, id.index_low))
}

fn same_bytes(left: &mut std::fs::File, right: &mut std::fs::File) -> io::Result<bool> {
    let mut left_buffer = [0u8; 65536];
    let mut right_buffer = [0u8; 65536];
    loop {
        let count = left.read(&mut left_buffer)?;
        if count == 0 {
            return Ok(right.read(&mut right_buffer)? == 0);
        }
        right.read_exact(&mut right_buffer[..count])?;
        if left_buffer[..count] != right_buffer[..count] {
            return Ok(false);
        }
    }
}
