//! Preserve exact host-approved packages when deriving an additive installation.
use super::*;
use std::path::Path;

static COPY_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

fn exact_grants(entry: &PreinstalledInventory) -> Result<(), AppError> {
    if entry.grants.values().any(|versions| versions.len() != 1) {
        return Err(AppError::Conflict(
            "retained plugins require one exact approved identity".into(),
        ));
    }
    if entry.grants.len() > 16
        || entry
            .grants
            .keys()
            .any(|id| !super::super::safe_plugin_token(id, 64))
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

/// The caller owns the destination attempt and must discard it after any error.
/// Existing children always conflict: no partial or concurrent copy is reused.
pub(super) async fn retain_approved_installations(
    entry: &PreinstalledInventory,
    destination: &Path,
) -> Result<(), AppError> {
    exact_grants(entry)?;
    if entry.grants.is_empty() {
        return Ok(());
    }
    let entry = entry.clone();
    let destination = destination.to_owned();
    let permit = tokio::time::timeout(ADMISSION_WAIT, COPY_PERMITS.clone().acquire_owned())
        .await
        .map_err(|_| AppError::capacity_error("plugin_retained_copy_capacity"))?
        .map_err(|_| AppError::internal("retained_installation_retain_approved_installations_1"))?;
    tokio::time::timeout(
        Duration::from_secs(60),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            copy_retained(&entry, &destination)
        }),
    )
    .await
    .map_err(|_| AppError::capacity_error("plugin_retained_copy_capacity"))?
    .map_err(|_| AppError::internal("retained_installation_retain_approved_installations_2"))?
}

/// Approval of a newly installed package cannot approve changes to an old one.
/// Full merged-inventory lifecycle validation remains the caller's responsibility.
pub(super) fn verify_retained_grants(
    entry: &PreinstalledInventory,
    runtime: &PluginRuntime,
) -> Result<(), AppError> {
    exact_grants(entry)?;
    let manifests = runtime.manifests();
    let identities = runtime.package_identities();
    for (id, versions) in &entry.grants {
        let grant = &versions[0];
        let mut matching = manifests.iter().filter(|manifest| &manifest.id == id);
        let manifest = matching.next().ok_or(AppError::Forbidden)?;
        if matching.next().is_some()
            || manifest.version != grant.version
            || lifecycle::manifest_digest(manifest)? != grant.manifest_digest
            || identities.get(id) != Some(&grant.identity)
        {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_retained(entry: &PreinstalledInventory, destination: &Path) -> Result<(), AppError> {
    use rustix::fs::{Dir, Mode, OFlags, openat};
    use std::io::Read;
    let source =
        crate::plugin_publication::directory_fd(&entry.root).map_err(|_| AppError::Forbidden)?;
    let destination =
        crate::plugin_publication::directory_fd(destination).map_err(|_| AppError::Forbidden)?;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let mut packages = Vec::new();
    match openat(&source, "plugin.json", flags, Mode::empty()) {
        Ok(_) => packages.push(std::fs::File::from(
            openat(&source, ".", flags | OFlags::DIRECTORY, Mode::empty())
                .map_err(|_| AppError::Forbidden)?,
        )),
        Err(rustix::io::Errno::NOENT) => {
            let mut count = 0;
            for child in Dir::read_from(&source).map_err(|_| AppError::Forbidden)? {
                let child = child.map_err(|_| AppError::Forbidden)?;
                if child.file_name().to_bytes().starts_with(b".") {
                    continue;
                }
                count += 1;
                if count > 64 {
                    return Err(AppError::Forbidden);
                }
                packages.push(std::fs::File::from(
                    openat(
                        &source,
                        child.file_name(),
                        flags | OFlags::DIRECTORY,
                        Mode::empty(),
                    )
                    .map_err(|_| AppError::Forbidden)?,
                ));
            }
        }
        Err(_) => return Err(AppError::Forbidden),
    }
    let mut found = std::collections::BTreeSet::new();
    let mut total = 0;
    for package in packages {
        let manifest = std::fs::File::from(
            openat(&package, "plugin.json", flags, Mode::empty())
                .map_err(|_| AppError::Forbidden)?,
        );
        if !manifest
            .metadata()
            .map_err(|_| AppError::Forbidden)?
            .is_file()
        {
            return Err(AppError::Forbidden);
        }
        let mut bytes = Vec::new();
        manifest
            .take(1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| AppError::Forbidden)?;
        if bytes.len() > 1024 * 1024 {
            return Err(AppError::Forbidden);
        }
        let value: serde_json::Value =
            serde_json::from_slice(&bytes).map_err(|_| AppError::Forbidden)?;
        let id = value
            .get("id")
            .and_then(|value| value.as_str())
            .ok_or(AppError::Forbidden)?;
        if !entry.grants.contains_key(id) {
            continue;
        }
        if !found.insert(id.to_owned()) {
            return Err(AppError::Forbidden);
        }
        copy_package(package, &destination, id, &mut total)?;
    }
    if found.len() != entry.grants.len() {
        return Err(AppError::Forbidden);
    }
    rustix::fs::fsync(&destination)
        .map_err(|_| AppError::internal("retained_installation_copy_retained_1"))
}

#[cfg(target_os = "linux")]
fn copy_package(
    source: std::fs::File,
    parent: &rustix::fd::OwnedFd,
    name: &str,
    inventory_bytes: &mut u64,
) -> Result<(), AppError> {
    use rustix::fs::{Dir, Mode, OFlags, mkdirat, openat};
    use std::io::Read;
    use std::os::unix::fs::MetadataExt;
    let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    mkdirat(parent, name, Mode::from_raw_mode(0o700)).map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            AppError::Conflict("retained plugin destination already exists".into())
        } else {
            AppError::Forbidden
        }
    })?;
    let target = openat(parent, name, flags | OFlags::DIRECTORY, Mode::empty())
        .map_err(|_| AppError::Forbidden)?;
    let mut pending = vec![(source, target, 0usize)];
    let (mut files, mut entries, mut total) = (0, 0, 0u64);
    while let Some((source, target, depth)) = pending.pop() {
        for child in Dir::read_from(&source).map_err(|_| AppError::Forbidden)? {
            let child = child.map_err(|_| AppError::Forbidden)?;
            let name = child.file_name();
            if matches!(name.to_bytes(), b"." | b"..") {
                continue;
            }
            entries += 1;
            if entries > 64 * 240 || pending.len() >= 128 || depth + name.to_bytes().len() + 1 > 240
            {
                return Err(AppError::Forbidden);
            }
            let input = std::fs::File::from(
                openat(&source, name, flags, Mode::empty()).map_err(|_| AppError::Forbidden)?,
            );
            let before = input.metadata().map_err(|_| AppError::Forbidden)?;
            if before.is_dir() {
                mkdirat(&target, name, Mode::from_raw_mode(0o700))
                    .map_err(|_| AppError::Forbidden)?;
                let output = openat(&target, name, flags | OFlags::DIRECTORY, Mode::empty())
                    .map_err(|_| AppError::Forbidden)?;
                pending.push((input, output, depth + name.to_bytes().len() + 1));
            } else if before.is_file() {
                files += 1;
                if files > 65 || before.len() > 64 * 1024 * 1024 {
                    return Err(AppError::Forbidden);
                }
                let mut output = std::fs::File::from(
                    openat(
                        &target,
                        name,
                        OFlags::WRONLY
                            | OFlags::CREATE
                            | OFlags::EXCL
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC,
                        Mode::from_raw_mode(0o600),
                    )
                    .map_err(|_| AppError::Forbidden)?,
                );
                let copied = std::io::copy(&mut (&input).take(64 * 1024 * 1024 + 1), &mut output)
                    .map_err(|_| AppError::Forbidden)?;
                let after = input.metadata().map_err(|_| AppError::Forbidden)?;
                total += copied;
                *inventory_bytes += copied;
                if copied != before.len()
                    || before.len() != after.len()
                    || before.mtime() != after.mtime()
                    || before.mtime_nsec() != after.mtime_nsec()
                    || before.ctime() != after.ctime()
                    || before.ctime_nsec() != after.ctime_nsec()
                    || copied > 64 * 1024 * 1024
                    || total > 80 * 1024 * 1024 + 16384
                    || *inventory_bytes > 16 * (80 * 1024 * 1024 + 16384)
                {
                    return Err(AppError::Forbidden);
                }
                output
                    .sync_all()
                    .map_err(|_| AppError::internal("retained_installation_copy_package_1"))?;
            } else {
                return Err(AppError::Forbidden);
            }
        }
        rustix::fs::fsync(&target)
            .map_err(|_| AppError::internal("retained_installation_copy_package_2"))?;
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn copy_retained(_: &PreinstalledInventory, _: &Path) -> Result<(), AppError> {
    Err(AppError::Forbidden)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[test]
    fn copy_never_follows_symlinks_or_reuses_existing_destination() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::write(source.join("plugin.json"), b"{}").unwrap();
        let destination = crate::plugin_publication::directory_fd(&target).unwrap();
        let mut bytes = 0;
        copy_package(
            std::fs::File::open(&source).unwrap(),
            &destination,
            "existing",
            &mut bytes,
        )
        .unwrap();
        assert!(matches!(
            copy_package(
                std::fs::File::open(&source).unwrap(),
                &destination,
                "existing",
                &mut bytes
            ),
            Err(AppError::Conflict(_))
        ));
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"private").unwrap();
        symlink(&outside, source.join("escape")).unwrap();
        assert!(
            copy_package(
                std::fs::File::open(&source).unwrap(),
                &destination,
                "linked",
                &mut bytes
            )
            .is_err()
        );
        assert!(!target.join("linked/escape").exists());
        assert_eq!(std::fs::read(outside).unwrap(), b"private");
    }

    #[test]
    fn copy_bounds_sparse_files_before_reading() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let target = root.path().join("target");
        std::fs::create_dir(&source).unwrap();
        std::fs::create_dir(&target).unwrap();
        std::fs::File::create(source.join("huge"))
            .unwrap()
            .set_len(64 * 1024 * 1024 + 1)
            .unwrap();
        let destination = crate::plugin_publication::directory_fd(&target).unwrap();
        assert!(
            copy_package(
                std::fs::File::open(source).unwrap(),
                &destination,
                "bounded",
                &mut 0
            )
            .is_err()
        );
        assert!(!target.join("bounded/huge").exists());
    }
}
