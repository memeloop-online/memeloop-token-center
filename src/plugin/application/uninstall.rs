//! Remove one active plugin by deriving an immutable subset of host approvals.
use super::*;

#[cfg(test)]
mod tests;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UninstallApplicationPlugin {
    pub plugin_id: String,
    pub expected_revision: i64,
}

impl ApplicationPlugins {
    pub async fn uninstall(
        &self,
        input: UninstallApplicationPlugin,
        key: &str,
        actor: &str,
    ) -> Result<ApplicationRevision, AppError> {
        validate_operation(input.expected_revision, key)?;
        if input.expected_revision == 0 || !super::super::safe_plugin_token(&input.plugin_id, 64) {
            return Err(AppError::BadRequest(
                "invalid plugin uninstall request".into(),
            ));
        }
        let hash = super::super::plugin_configuration_schema_digest(&json!({
            "action":"uninstall", "plugin_id":input.plugin_id,
            "expected_revision":input.expected_revision, "actor":actor,
        }))?;
        if let Some(replay) = self
            .db
            .replay_application_plugin_operation(key, &hash)
            .await?
        {
            return Ok(replay);
        }
        let head = self
            .db
            .optional_application_plugin_head()
            .await?
            .ok_or_else(|| AppError::Conflict("plugin runtime has no published revision".into()))?;
        if head.revision != input.expected_revision {
            return Err(AppError::Conflict("plugin runtime revision changed".into()));
        }
        self.refresh_inventory().await?;
        let mut entry = self
            .inventory
            .read()
            .await
            .get(&head.inventory_id)
            .cloned()
            .ok_or(AppError::Forbidden)?;
        if entry.grants.remove(&input.plugin_id).is_none() {
            return Err(AppError::NotFound);
        }
        let path = self.inventory_file.as_ref().ok_or(AppError::Forbidden)?;
        // Repeated keys or different administrators derive the same immutable
        // subset, preventing failed/replayed requests from duplicating its bytes.
        let subset_hash = super::super::plugin_configuration_schema_digest(&json!({
            "source_inventory":head.inventory_id, "source_revision":head.revision,
            "removed_plugin":input.plugin_id,
        }))?;
        let id = format!("uninstall-{}", &subset_hash[..54]);
        validate_inventory_id(&id)?;
        // Removing the last plugin needs no package bytes or signing trust.
        if entry.grants.is_empty() {
            self.register_empty(
                RegisterEmptyInventory {
                    inventory_id: id.clone(),
                },
                &format!("uninstall-empty:{hash}"),
                actor,
            )
            .await?;
        } else {
            // A head receipt pins one exact active identity. A host inventory
            // may approve alternatives, but uninstall must never select another
            // approved version while removing an unrelated broken plugin.
            if entry.grants.values().any(|versions| versions.len() != 1) {
                return Err(AppError::Conflict(
                    "retained plugins need one exact approved identity before uninstall".into(),
                ));
            }
            let root = path
                .parent()
                .ok_or(AppError::Forbidden)?
                .join(format!(".mtc-{id}"));
            let source = entry.root.clone();
            let retained = entry.grants.keys().cloned().collect();
            let destination = root.clone();
            // A blocking copy keeps admission until it actually exits, including
            // after an HTTP disconnect. Bound total concurrent disk copies.
            let permit =
                tokio::time::timeout(ADMISSION_WAIT, SUBSET_PERMITS.clone().acquire_owned())
                    .await
                    .map_err(|_| AppError::capacity_error("plugin_subset_capacity"))?
                    .map_err(|_| AppError::internal("uninstall_uninstall_1"))?;
            tokio::time::timeout(
                Duration::from_secs(60),
                tokio::task::spawn_blocking(move || {
                    let _permit = permit;
                    prepare_subset(&source, &destination, &retained)
                }),
            )
            .await
            .map_err(|_| AppError::capacity_error("plugin_subset_capacity"))?
            .map_err(|_| AppError::internal("uninstall_uninstall_2"))??;
            entry.root = root;
            // Rebuild all providers/UI from the retained set and compare exact
            // approved manifests and identities. Never compile the removed one.
            let candidate = self.load_entry(&id, entry.clone(), 1, "initial").await?;
            installation::append_inventory_file(path, &id, &entry).await?;
            self.db
                .stage_application_plugin_candidate(
                    &id,
                    &candidate.receipt.identity_digest,
                    &candidate.receipt.contract_digest,
                )
                .await?;
        }
        // The source revision is never refreshed/rebased after preparation.
        self.db
            .publish_application_plugin_action(
                &id,
                input.expected_revision,
                "reload",
                key,
                &hash,
                actor,
                "uninstall",
                Some(&format!(
                    "uninstall:{}:{}:{}",
                    input.expected_revision,
                    input.plugin_id,
                    input.expected_revision + 1
                )),
            )
            .await
    }
}

static SUBSET_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

#[cfg(target_os = "linux")]
fn prepare_subset(
    source: &std::path::Path,
    destination: &std::path::Path,
    retained: &std::collections::BTreeSet<String>,
) -> Result<(), AppError> {
    use rustix::fs::{Dir, Mode, OFlags, RenameFlags, openat, renameat_with};
    use std::io::Read;
    let source =
        crate::plugin_publication::directory_fd(source).map_err(|_| AppError::Forbidden)?;
    let parent = destination.parent().ok_or(AppError::Forbidden)?;
    let parent_fd =
        crate::plugin_publication::directory_fd(parent).map_err(|_| AppError::Forbidden)?;
    let name = destination.file_name().ok_or(AppError::Forbidden)?;
    // Published roots are immutable. A crash or concurrent retry may already
    // have completed this copy; load_entry still verifies every retained grant.
    if openat(
        &parent_fd,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .is_ok()
    {
        return Ok(());
    }
    let temporary = parent.join(format!(".mtc-subset-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&temporary).map_err(|_| AppError::internal("uninstall_prepare_subset_1"))?;
    let result = (|| {
        let mut packages = Vec::new();
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        if openat(&source, "plugin.json", flags, Mode::empty()).is_ok() {
            packages.push((
                "package".into(),
                std::fs::File::from(
                    openat(&source, ".", flags | OFlags::DIRECTORY, Mode::empty())
                        .map_err(|_| AppError::Forbidden)?,
                ),
            ));
        } else {
            let mut count = 0;
            for entry in Dir::read_from(&source).map_err(|_| AppError::internal("uninstall_prepare_subset_2"))? {
                let entry = entry.map_err(|_| AppError::internal("uninstall_prepare_subset_3"))?;
                if entry.file_name().to_bytes().starts_with(b".") {
                    continue;
                }
                count += 1;
                if count > 64 {
                    return Err(AppError::Forbidden);
                }
                let package = openat(
                    &source,
                    entry.file_name(),
                    flags | OFlags::DIRECTORY,
                    Mode::empty(),
                );
                // An unusable removed package must not prevent removing it.
                if let Ok(package) = package {
                    packages.push((
                        entry.file_name().to_string_lossy().into_owned(),
                        std::fs::File::from(package),
                    ));
                }
            }
        }
        let mut found = std::collections::BTreeSet::new();
        let mut inventory_bytes = 0;
        for (name, package) in packages {
            let Ok(manifest) = openat(&package, "plugin.json", flags, Mode::empty()) else {
                continue;
            };
            let manifest = std::fs::File::from(manifest);
            if !manifest
                .metadata()
                .map_err(|_| AppError::internal("uninstall_prepare_subset_4"))?
                .is_file()
            {
                continue;
            }
            let mut bytes = Vec::new();
            manifest
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| AppError::internal("uninstall_prepare_subset_5"))?;
            if bytes.len() > 1024 * 1024 {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(|id| id.as_str()) else {
                continue;
            };
            if !retained.contains(id) {
                continue;
            }
            if !found.insert(id.to_owned()) {
                return Err(AppError::Forbidden);
            }
            copy_package(package, &temporary.join(name), &mut inventory_bytes)?;
        }
        if &found != retained {
            return Err(AppError::Forbidden);
        }
        std::fs::File::open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(|_| AppError::internal("uninstall_prepare_subset_6"))?;
        match renameat_with(
            &parent_fd,
            temporary.file_name().unwrap(),
            &parent_fd,
            name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(error) if crate::plugin_publication::unsupported_rename(error) => {
                // NFS may lack renameat2. Serialize every fallback publisher
                // before the destination check and atomic ordinary rename.
                let lock = std::fs::File::from(
                    openat(
                        &parent_fd,
                        ".mtc-subset-publish.lock",
                        OFlags::RDWR
                            | OFlags::CREATE
                            | OFlags::NOFOLLOW
                            | OFlags::CLOEXEC
                            | OFlags::NONBLOCK,
                        Mode::from_raw_mode(0o600),
                    )
                    .map_err(|_| AppError::internal("uninstall_site_1"))?,
                );
                if !lock.metadata().map_err(|_| AppError::internal("uninstall_site_2"))?.is_file() {
                    return Err(AppError::Forbidden);
                }
                lock.try_lock()
                    .map_err(|_| AppError::capacity_error("plugin_publish_lock"))?;
                match openat(
                    &parent_fd,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW,
                    Mode::empty(),
                ) {
                    Ok(_) => {}
                    Err(rustix::io::Errno::NOENT) => {
                        rustix::fs::renameat(
                            &parent_fd,
                            temporary.file_name().unwrap(),
                            &parent_fd,
                            name,
                        )
                        .map_err(|_| AppError::internal("uninstall_site_3"))?;
                    }
                    Err(_) => return Err(AppError::Forbidden),
                }
            }
            Err(_) => return Err(AppError::internal("uninstall_site_1")),
        }
        rustix::fs::fsync(&parent_fd).map_err(|_| AppError::internal("uninstall_site_4"))?;
        Ok(())
    })();
    // Only this invocation's private staging directory can be reclaimed.
    let _ = std::fs::remove_dir_all(&temporary);
    result
}

#[cfg(target_os = "linux")]
fn copy_package(
    source: std::fs::File,
    destination: &std::path::Path,
    inventory_bytes: &mut u64,
) -> Result<(), AppError> {
    use rustix::fs::{Dir, Mode, OFlags, openat};
    use std::io::Read;
    let mut pending = vec![(PathBuf::new(), source)];
    let mut directories = Vec::new();
    let (mut files, mut entries, mut total) = (0, 0, 0u64);
    while let Some((relative, source)) = pending.pop() {
        if relative.as_os_str().len() > 240 {
            return Err(AppError::Forbidden);
        }
        let metadata = source.metadata().map_err(|_| AppError::internal("uninstall_copy_package_1"))?;
        let target = destination.join(&relative);
        if metadata.is_dir() {
            std::fs::create_dir(&target).map_err(|_| AppError::internal("uninstall_copy_package_2"))?;
            directories.push(target);
            for entry in Dir::read_from(&source).map_err(|_| AppError::internal("uninstall_copy_package_3"))? {
                let entry = entry.map_err(|_| AppError::internal("uninstall_copy_package_4"))?;
                let name = entry.file_name();
                if name.to_bytes() == b"." || name.to_bytes() == b".." {
                    continue;
                }
                entries += 1;
                if entries > 64 * 240 || pending.len() >= 128 {
                    return Err(AppError::Forbidden);
                }
                let child = openat(
                    &source,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                )
                .map_err(|_| AppError::Forbidden)?;
                use std::os::unix::ffi::OsStrExt;
                pending.push((
                    relative.join(std::ffi::OsStr::from_bytes(name.to_bytes())),
                    std::fs::File::from(child),
                ));
            }
        } else if metadata.is_file() {
            files += 1;
            if files > 65 || metadata.len() > 64 * 1024 * 1024 {
                return Err(AppError::Forbidden);
            }
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(target)
                .map_err(|_| AppError::internal("uninstall_copy_package_5"))?;
            let copied = std::io::copy(&mut source.take(64 * 1024 * 1024 + 1), &mut target)
                .map_err(|_| AppError::internal("uninstall_copy_package_6"))?;
            total += copied;
            *inventory_bytes += copied;
            // Match installation's sixteen packages at its established per-
            // package bound, while bounding the entire derived copy as well.
            if *inventory_bytes > 16 * (80 * 1024 * 1024 + 16384) {
                return Err(AppError::Forbidden);
            }
            if copied > 64 * 1024 * 1024 || total > 80 * 1024 * 1024 + 16384 {
                return Err(AppError::Forbidden);
            }
            target.sync_all().map_err(|_| AppError::internal("uninstall_copy_package_7"))?;
        } else {
            return Err(AppError::Forbidden);
        }
    }
    for directory in directories.into_iter().rev() {
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|_| AppError::internal("uninstall_copy_package_8"))?;
    }
    Ok(())
}

#[cfg(windows)]
fn prepare_subset(
    source: &std::path::Path,
    destination: &std::path::Path,
    retained: &std::collections::BTreeSet<String>,
) -> Result<(), AppError> {
    use std::ffi::OsStr;
    let source_dir =
        crate::windows_dir::open_directory(source).map_err(|_| AppError::Forbidden)?;
    let parent_path = destination.parent().ok_or(AppError::Forbidden)?;
    let parent = crate::windows_dir::open_writable_directory(parent_path)
        .map_err(|_| AppError::Forbidden)?;
    let name = destination.file_name().ok_or(AppError::Forbidden)?;
    if retained_directory_ready(&parent, name)? {
        return Ok(());
    }
    let temporary_name = format!(".mtc-subset-{}", uuid::Uuid::now_v7());
    let temporary = crate::windows_dir::create_dir(&parent, OsStr::new(&temporary_name))
        .map_err(|_| AppError::internal("uninstall_prepare_subset_7"))?;
    let result = (|| {
        let mut packages = Vec::new();
        if crate::windows_dir::open_file(&source_dir, OsStr::new("plugin.json")).is_ok()
            || crate::windows_dir::open_subdir(&source_dir, OsStr::new("plugin.json")).is_ok()
        {
            packages.push(("package".to_owned(), source_dir.try_clone().map_err(|_| AppError::Forbidden)?));
        } else {
            let mut count = 0;
            for entry in crate::windows_dir::read_names(&source_dir).map_err(|_| AppError::internal("uninstall_prepare_subset_8"))?
            {
                if entry.to_string_lossy().starts_with('.') {
                    continue;
                }
                count += 1;
                if count > 64 {
                    return Err(AppError::Forbidden);
                }
                if let Ok(package) = crate::windows_dir::open_subdir(&source_dir, entry.as_os_str())
                {
                    packages.push((entry.to_string_lossy().into_owned(), package));
                }
            }
        }
        let mut found = std::collections::BTreeSet::new();
        let mut inventory_bytes = 0u64;
        for (package_name, package) in packages {
            let Ok(bytes) = crate::windows_dir::read_limited(
                &package,
                OsStr::new("plugin.json"),
                1024 * 1024,
            ) else {
                continue;
            };
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
                continue;
            };
            let Some(id) = value.get("id").and_then(|id| id.as_str()) else {
                continue;
            };
            if !retained.contains(id) {
                continue;
            }
            if !found.insert(id.to_owned()) {
                return Err(AppError::Forbidden);
            }
            copy_retained_tree(&package, &temporary, &package_name, &mut inventory_bytes)?;
        }
        if &found != retained {
            return Err(AppError::Forbidden);
        }
        temporary.sync().map_err(|_| AppError::internal("uninstall_prepare_subset_9"))?;
        match crate::windows_dir::rename_noreplace(
            &parent,
            OsStr::new(&temporary_name),
            &parent,
            name,
        ) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if !retained_directory_ready(&parent, name)? {
                    return Err(AppError::Forbidden);
                }
            }
            Err(_) => return Err(AppError::internal("uninstall_prepare_subset_1")),
        }
        parent.sync().map_err(|_| AppError::internal("uninstall_prepare_subset_10"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = crate::windows_dir::remove_tree(&temporary);
        let _ = crate::windows_dir::remove_dir(&parent, OsStr::new(&temporary_name));
    }
    result
}

#[cfg(windows)]
fn retained_directory_ready(
    parent: &crate::windows_dir::Directory,
    name: &std::ffi::OsStr,
) -> Result<bool, AppError> {
    match crate::windows_dir::open_subdir(parent, name) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err(AppError::Forbidden),
    }
}

#[cfg(windows)]
fn copy_retained_tree(
    source: &crate::windows_dir::Directory,
    destination_parent: &crate::windows_dir::Directory,
    name: &str,
    inventory_bytes: &mut u64,
) -> Result<(), AppError> {
    use std::ffi::OsStr;
    use std::io::{Read, Write};
    crate::windows_dir::ensure_component(OsStr::new(name)).map_err(|_| AppError::Forbidden)?;
    let target = crate::windows_dir::create_dir(destination_parent, OsStr::new(name))
        .map_err(|_| AppError::internal("uninstall_copy_retained_tree_1"))?;
    let mut pending = vec![(
        source.try_clone().map_err(|_| AppError::Forbidden)?,
        target,
        0usize,
    )];
    let (mut files, mut entries, mut total) = (0usize, 0usize, 0u64);
    while let Some((directory, destination, path_len)) = pending.pop() {
        if path_len > 240 {
            return Err(AppError::Forbidden);
        }
        for entry in crate::windows_dir::read_names(&directory).map_err(|_| AppError::internal("uninstall_copy_retained_tree_2"))? {
            let child_name = entry.as_os_str();
            let child_len = child_name.as_encoded_bytes().len();
            entries += 1;
            if entries > 64 * 240 || pending.len() >= 128 || path_len + child_len + 1 > 240 {
                return Err(AppError::Forbidden);
            }
            if let Ok(child) = crate::windows_dir::open_subdir(&directory, child_name) {
                let created = crate::windows_dir::create_dir(&destination, child_name)
                    .map_err(|_| AppError::internal("uninstall_copy_retained_tree_3"))?;
                pending.push((child, created, path_len + child_len + 1));
                continue;
            }
            let mut input = match crate::windows_dir::open_file(&directory, child_name) {
                Ok(file) => file,
                Err(error) if crate::windows_dir::is_reparse_refusal(&error) => {
                    return Err(AppError::Forbidden);
                }
                Err(_) => return Err(AppError::Forbidden),
            };
            let length = input.metadata().map_err(|_| AppError::internal("uninstall_copy_retained_tree_4"))?.len();
            files += 1;
            if files > 65 || length > 64 * 1024 * 1024 {
                return Err(AppError::Forbidden);
            }
            let mut output = crate::windows_dir::create_file(&destination, child_name)
                .map_err(|_| AppError::internal("uninstall_copy_retained_tree_5"))?;
            let copied = std::io::copy(&mut input.take(64 * 1024 * 1024 + 1), &mut output)
                .map_err(|_| AppError::internal("uninstall_copy_retained_tree_6"))?;
            total += copied;
            *inventory_bytes += copied;
            if *inventory_bytes > 16 * (80 * 1024 * 1024 + 16384)
                || copied > 64 * 1024 * 1024
                || total > 80 * 1024 * 1024 + 16384
            {
                return Err(AppError::Forbidden);
            }
            output.sync_all().map_err(|_| AppError::internal("uninstall_copy_retained_tree_7"))?;
        }
        destination.sync().map_err(|_| AppError::internal("uninstall_copy_retained_tree_8"))?;
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", windows)))]
fn prepare_subset(
    _: &std::path::Path,
    _: &std::path::Path,
    _: &std::collections::BTreeSet<String>,
) -> Result<(), AppError> {
    Err(AppError::Forbidden)
}
