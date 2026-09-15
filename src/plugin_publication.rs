//! Portable no-replace publication primitives for NFS-backed inventories.
//! An atomic hard-link claim reserves a name before any directory is created.
//! Files are immutable links of fully written, verified same-filesystem staging.
use rustix::fs::{Mode, OFlags, open};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::{
    fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
};

const OWNER_LIMIT: u64 = 8192;

pub(crate) fn unsupported_rename(error: rustix::io::Errno) -> bool {
    matches!(
        error,
        rustix::io::Errno::INVAL | rustix::io::Errno::NOSYS | rustix::io::Errno::OPNOTSUPP
    )
}

struct TemporaryClaim(PathBuf);
impl Drop for TemporaryClaim {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn regular_file(path: &Path) -> io::Result<fs::File> {
    let file = fs::File::from(open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    Ok(file)
}

pub(crate) fn sync_directory(path: &Path) -> io::Result<()> {
    let file = fs::File::from(open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    file.sync_all()
}

pub(crate) fn verify_owner(path: &Path, owner: &[u8]) -> io::Result<()> {
    let mut bytes = Vec::new();
    regular_file(path)?
        .take(OWNER_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes != owner {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    Ok(())
}

fn verify_owner_at(directory: &rustix::fd::OwnedFd, name: &str, owner: &[u8]) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags, openat};
    let file = fs::File::from(openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    )?);
    if !file.metadata()?.is_file() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let mut bytes = Vec::new();
    file.take(OWNER_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes != owner {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    Ok(())
}

fn publish_owner_at(directory: &rustix::fd::OwnedFd, name: &str, owner: &[u8]) -> io::Result<()> {
    use rustix::fs::{Mode, OFlags, openat};
    let mut file = fs::File::from(openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )?);
    file.write_all(owner)?;
    file.sync_all()?;
    rustix::fs::fsync(directory)?;
    Ok(())
}

fn claim_name(root: &Path, owner: &[u8]) -> io::Result<()> {
    if owner.is_empty() || owner.len() as u64 > OWNER_LIMIT {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let parent = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let marker = parent.join(format!(".mtc-publish-owner-{name}"));
    match fs::symlink_metadata(&marker) {
        Ok(_) => return verify_owner(&marker, owner),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    // Do not adopt an existing unowned directory, even an empty one.
    match fs::symlink_metadata(root) {
        // A concurrent owner may have published its claim and mkdir since our
        // first lookup. It is safe only if that exact owner is now provable.
        Ok(_) => {
            return verify_owner(&marker, owner).map_err(|error| {
                if error.kind() == io::ErrorKind::NotFound {
                    io::ErrorKind::AlreadyExists.into()
                } else {
                    error
                }
            });
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let temporary = parent.join(format!(".mtc-publish-claim-{}", uuid::Uuid::now_v7()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)?;
    let _cleanup = TemporaryClaim(temporary.clone());
    file.write_all(owner)?;
    file.sync_all()?;
    match fs::hard_link(&temporary, &marker) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => verify_owner(&marker, owner)?,
        Err(error) => return Err(error),
    }
    sync_directory(parent)
}

pub(crate) fn reserve_directory_name(root: &Path, owner: &[u8]) -> io::Result<()> {
    claim_name(root, owner)
}

pub(crate) fn bind_existing_directory(
    root: &Path,
    owner: &[u8],
) -> io::Result<rustix::fd::OwnedFd> {
    claim_name(root, owner)?;
    let parent = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let metadata = fs::symlink_metadata(root)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let directory = directory_fd(root)?;
    let stat = rustix::fs::fstat(&directory)?;
    let identity = format!("{}:{}", stat.st_dev, stat.st_ino);
    let binding = parent.join(format!(".mtc-publish-inode-{name}"));
    match fs::symlink_metadata(&binding) {
        Ok(_) => verify_owner(&binding, identity.as_bytes())?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let temporary = parent.join(format!(".mtc-publish-claim-{}", uuid::Uuid::now_v7()));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            let _cleanup = TemporaryClaim(temporary.clone());
            file.write_all(identity.as_bytes())?;
            file.sync_all()?;
            match fs::hard_link(&temporary, &binding) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    verify_owner(&binding, identity.as_bytes())?
                }
                Err(error) => return Err(error),
            }
        }
        Err(error) => return Err(error),
    }
    sync_directory(parent)?;
    Ok(directory)
}

pub(crate) fn open_claimed_directory(root: &Path, owner: &[u8]) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{Mode, OFlags, ResolveFlags, openat2};
    let parent = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let name_text = name.to_str().ok_or(io::ErrorKind::InvalidInput)?;
    let parent = directory_fd(parent)?;
    let owner_marker = format!(".mtc-publish-owner-{name_text}");
    verify_owner_at(&parent, &owner_marker, owner)?;
    let directory = openat2(
        &parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
    )?;
    let stat = rustix::fs::fstat(&directory)?;
    let identity = format!("{}:{}", stat.st_dev, stat.st_ino);
    let inode_marker = format!(".mtc-publish-inode-{name_text}");
    verify_owner_at(&parent, &inode_marker, identity.as_bytes())?;
    Ok(directory)
}

pub(crate) fn clear_claimed_directory(root: &Path, owner: &[u8]) -> io::Result<bool> {
    let directory = open_claimed_directory(root, owner)?;
    let stat = rustix::fs::fstat(&directory)?;
    let identity = format!("{}:{}", stat.st_dev, stat.st_ino);
    match verify_owner_at(&directory, ".mtc-install-reclaimed", identity.as_bytes()) {
        Ok(()) => return Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    clear_directory_contents(&directory)?;
    publish_owner_at(&directory, ".mtc-install-reclaimed", identity.as_bytes())?;
    Ok(true)
}

fn clear_directory_contents(directory: &rustix::fd::OwnedFd) -> io::Result<()> {
    use rustix::fs::{AtFlags, Dir, Mode, OFlags, ResolveFlags, openat2, unlinkat};
    let mut entries = Dir::read_from(directory)?;
    while let Some(entry) = entries.read() {
        let entry = entry?;
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        match openat2(
            directory,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_XDEV,
        ) {
            Ok(child) => {
                clear_directory_contents(&child)?;
                rustix::fs::fsync(&child)?;
                // Keep directory inodes: unlinking a pathname cannot be made
                // conditional on the inode that was opened and verified.
            }
            Err(rustix::io::Errno::NOTDIR | rustix::io::Errno::LOOP) => {
                match unlinkat(directory, name, AtFlags::empty()) {
                    Ok(()) => {}
                    Err(error) if error == rustix::io::Errno::NOENT => {}
                    Err(error) => return Err(error.into()),
                }
            }
            Err(error) if error == rustix::io::Errno::NOENT => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(rustix::fs::fsync(directory)?)
}

pub(crate) fn claim_directory(root: &Path, owner: &[u8]) -> io::Result<rustix::fd::OwnedFd> {
    claim_directory_inner(root, owner, || {}, || {})
}

fn claim_directory_inner(
    root: &Path,
    owner: &[u8],
    after_mkdir: impl FnOnce(),
    mut on_wait: impl FnMut(),
) -> io::Result<rustix::fd::OwnedFd> {
    claim_name(root, owner)?;
    let parent = root.parent().ok_or(io::ErrorKind::InvalidInput)?;
    let name = root
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(io::ErrorKind::InvalidInput)?;
    let binding = parent.join(format!(".mtc-publish-inode-{name}"));
    match fs::create_dir(root) {
        Ok(()) => {
            after_mkdir();
            let directory = fs::File::from(directory_fd(root)?);
            let metadata = directory.metadata()?;
            let identity = format!("{}:{}", metadata.dev(), metadata.ino());
            let temporary = parent.join(format!(".mtc-publish-claim-{}", uuid::Uuid::now_v7()));
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&temporary)?;
            let _cleanup = TemporaryClaim(temporary.clone());
            file.write_all(identity.as_bytes())?;
            file.sync_all()?;
            // A crash between mkdir and this link intentionally fails closed:
            // without an inode binding we cannot prove that a directory is ours.
            fs::hard_link(&temporary, &binding)?;
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let metadata = fs::symlink_metadata(root)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(error);
            }
        }
        Err(error) => return Err(error),
    }
    let directory = directory_fd(root)?;
    let stat = rustix::fs::fstat(&directory)?;
    let identity = format!("{}:{}", stat.st_dev, stat.st_ino);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match verify_owner(&binding, identity.as_bytes()) {
            Ok(()) => break,
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    && std::time::Instant::now() < deadline =>
            {
                on_wait();
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
    sync_directory(parent)?;
    Ok(directory)
}

pub(crate) fn directory_fd(path: &Path) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::openat;
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = open(
        if path.is_absolute() { "/" } else { "." },
        flags,
        Mode::empty(),
    )?;
    for component in path.components() {
        match component {
            std::path::Component::Normal(name) => {
                directory = openat(&directory, name, flags, Mode::empty())?
            }
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            _ => return Err(io::ErrorKind::InvalidInput.into()),
        }
    }
    Ok(directory)
}

/// Never overwrites, including on same-owner resume. A link error such as EXDEV
/// remains an error; callers place their verified staging under the target root.
#[cfg(test)]
fn link_file(source: &Path, target: &Path) -> io::Result<()> {
    let directory = directory_fd(target.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    link_file_at(
        source,
        &directory,
        Path::new(target.file_name().ok_or(io::ErrorKind::InvalidInput)?),
    )
}

pub(crate) fn link_file_at(
    source: &Path,
    directory: &rustix::fd::OwnedFd,
    relative: &Path,
) -> io::Result<()> {
    use rustix::fs::{AtFlags, linkat, openat};
    let source_parent = directory_fd(source.parent().ok_or(io::ErrorKind::InvalidInput)?)?;
    let target_parent = parent_at(directory, relative)?;
    let source_name = source.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let target_name = relative.file_name().ok_or(io::ErrorKind::InvalidInput)?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut source_file =
        fs::File::from(openat(&source_parent, source_name, flags, Mode::empty())?);
    if !source_file.metadata()?.is_file() {
        return Err(io::ErrorKind::AlreadyExists.into());
    }
    let result = match linkat(
        &source_parent,
        source_name,
        &target_parent,
        target_name,
        AtFlags::empty(),
    )
    .map_err(io::Error::from)
    {
        Ok(()) => {
            let target_file =
                fs::File::from(openat(&target_parent, target_name, flags, Mode::empty())?);
            let original = source_file.metadata()?;
            let linked = target_file.metadata()?;
            if original.dev() != linked.dev() || original.ino() != linked.ino() {
                return Err(io::ErrorKind::AlreadyExists.into());
            }
            Ok(())
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            let mut target_file =
                fs::File::from(openat(&target_parent, target_name, flags, Mode::empty())?);
            if !target_file.metadata()?.is_file() {
                return Err(error);
            }
            if source_file.metadata()?.len() != target_file.metadata()?.len() {
                return Err(error);
            }
            let mut left = [0u8; 65536];
            let mut right = [0u8; 65536];
            loop {
                let count = source_file.read(&mut left)?;
                if count == 0 {
                    break;
                }
                target_file.read_exact(&mut right[..count])?;
                if left[..count] != right[..count] {
                    return Err(error);
                }
            }
            Ok(())
        }
        Err(error) => Err(error),
    };
    result?;
    rustix::fs::fsync(&target_parent)?;
    Ok(())
}

fn parent_at(root: &rustix::fd::OwnedFd, relative: &Path) -> io::Result<rustix::fd::OwnedFd> {
    use rustix::fs::{mkdirat, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut directory = openat(root, ".", flags, Mode::empty())?;
    for component in relative
        .parent()
        .ok_or(io::ErrorKind::InvalidInput)?
        .components()
    {
        let std::path::Component::Normal(name) = component else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        match mkdirat(&directory, name, Mode::from_bits_truncate(0o755)) {
            Ok(()) => rustix::fs::fsync(&directory)?,
            Err(error) if error == rustix::io::Errno::EXIST => {}
            Err(error) => return Err(error.into()),
        }
        directory = openat(&directory, name, flags, Mode::empty())?;
    }
    Ok(directory)
}

pub(crate) fn report_io(stage: &'static str, error: &io::Error) {
    // Only fixed stages and OS error numbers cross the installer boundary.
    // Never serialize io::Error's message, which can contain host paths.
    eprintln!(
        "{}",
        serde_json::json!({"mtc_plugin_install":1,"stage":stage,"category":"storage","errno":error.raw_os_error()})
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_owner_first_claim_waits_for_inflight_inode_binding() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("inventory");
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let root = &root;
            let first = scope.spawn(move || {
                claim_directory_inner(
                    root,
                    b"owner",
                    || {
                        entered_tx.send(()).unwrap();
                        release_rx.recv().unwrap();
                    },
                    || {},
                )
            });
            entered_rx.recv().unwrap();
            let second = scope.spawn(move || {
                let mut notified = false;
                claim_directory_inner(
                    root,
                    b"owner",
                    || {},
                    || {
                        if !notified {
                            waiting_tx.send(()).unwrap();
                            notified = true;
                        }
                    },
                )
            });
            waiting_rx.recv().unwrap();
            release_tx.send(()).unwrap();
            assert!(first.join().unwrap().is_ok());
            assert!(second.join().unwrap().is_ok());
        });
    }
    #[test]
    fn atomic_claim_is_concurrent_owner_fenced_and_resumes_before_mkdir() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("inventory");
        claim_name(&root, b"first").unwrap(); // Interrupted after durable ownership, before mkdir.
        assert!(!root.exists());
        claim_directory(&root, b"first").unwrap();
        let barrier = std::sync::Barrier::new(2);
        std::thread::scope(|scope| {
            let first = scope.spawn(|| {
                barrier.wait();
                claim_directory(&root, b"first")
            });
            let other = scope.spawn(|| {
                barrier.wait();
                claim_directory(&root, b"other")
            });
            assert!(first.join().unwrap().is_ok());
            assert!(other.join().unwrap().is_err());
        });
        let unrelated = directory.path().join("unrelated");
        fs::create_dir(&unrelated).unwrap();
        assert!(claim_directory(&unrelated, b"first").is_err());
        assert_eq!(fs::read_dir(&unrelated).unwrap().count(), 0);
        let replaced = directory.path().join("replaced");
        fs::rename(&root, &replaced).unwrap();
        fs::create_dir(&root).unwrap();
        assert!(claim_directory(&root, b"first").is_err());
        assert_eq!(fs::read_dir(&root).unwrap().count(), 0);
    }

    #[test]
    fn immutable_links_resume_equal_bytes_and_reject_different_bytes_or_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let other = directory.path().join("other");
        let target = directory.path().join("target");
        fs::write(&source, b"verified").unwrap();
        fs::write(&other, b"different").unwrap();
        link_file(&source, &target).unwrap();
        link_file(&source, &target).unwrap();
        assert!(link_file(&other, &target).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        let symlink = directory.path().join("symlink");
        std::os::unix::fs::symlink(&target, &symlink).unwrap();
        assert!(link_file(&source, &symlink).is_err());
        assert_eq!(fs::read(&target).unwrap(), b"verified");
        let root = directory.path().join("owned");
        let owned = claim_directory(&root, b"owner").unwrap();
        let detached = directory.path().join("detached");
        fs::rename(&root, &detached).unwrap();
        fs::create_dir(&root).unwrap();
        link_file_at(&source, &owned, Path::new("payload")).unwrap();
        assert!(!root.join("payload").exists());
        assert_eq!(fs::read(detached.join("payload")).unwrap(), b"verified");
        assert!(claim_directory(&root, b"owner").is_err());
        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, detached.join("assets")).unwrap();
        assert!(link_file_at(&source, &owned, Path::new("assets/payload")).is_err());
        assert!(!outside.join("payload").exists());
    }
}
