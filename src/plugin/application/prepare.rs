//! Non-networking initialization for the existing shared inventory PVC.
use super::PreinstalledInventory;
use crate::error::AppError;
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::Path,
};

fn validate_directory(directory: &Path) -> Result<(), AppError> {
    if !directory.is_absolute()
        || directory.parent().is_none()
        || directory
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(AppError::Forbidden);
    }
    let metadata = std::fs::symlink_metadata(directory).map_err(|_| AppError::Internal)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub fn check_inventory_directory(directory: &Path) -> Result<(), AppError> {
    validate_directory(directory)?;
    validate_existing(&directory.join("inventory.json"))
}

pub fn prepare_inventory_directory(directory: &Path) -> Result<(), AppError> {
    validate_directory(directory)?;
    let inventory = directory.join("inventory.json");
    if inventory.try_exists().map_err(|_| AppError::Internal)? {
        return validate_existing(&inventory);
    }
    // Publish only a complete fsynced file; create-new on the final filename
    // alone would leave a permanently empty/truncated file after a crash.
    let temporary = directory.join(format!(".mtc-inventory-init-{}", uuid::Uuid::now_v7()));
    let mut options = std::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o640);
    }
    let result = (|| {
        let mut file = options.open(&temporary).map_err(|_| AppError::Internal)?;
        file.write_all(b"{}").map_err(|_| AppError::Internal)?;
        file.sync_all().map_err(|_| AppError::Internal)?;
        match std::fs::hard_link(&temporary, &inventory) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return Err(AppError::Internal),
        }
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(|_| AppError::Internal)?;
        validate_existing(&inventory)
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

fn validate_existing(path: &Path) -> Result<(), AppError> {
    let metadata = std::fs::symlink_metadata(path).map_err(|_| AppError::Internal)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(AppError::Forbidden);
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|_| AppError::Internal)?
        .take(4 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AppError::Internal)?;
    if bytes.len() > 4 * 1024 * 1024 {
        return Err(AppError::Forbidden);
    }
    let _: BTreeMap<String, PreinstalledInventory> =
        serde_json::from_slice(&bytes).map_err(|_| AppError::Forbidden)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initializes_actual_empty_inventory_schema_and_never_overwrites() {
        let root = tempfile::tempdir().unwrap();
        assert!(check_inventory_directory(root.path()).is_err());
        assert!(!root.path().join("inventory.json").exists());
        prepare_inventory_directory(root.path()).unwrap();
        check_inventory_directory(root.path()).unwrap();
        assert_eq!(
            std::fs::read(root.path().join("inventory.json")).unwrap(),
            b"{}"
        );
        let value =
            serde_json::json!({"retained":{"root":"/var/lib/plugins/retained","grants":{}}})
                .to_string();
        std::fs::write(root.path().join("inventory.json"), &value).unwrap();
        prepare_inventory_directory(root.path()).unwrap();
        assert_eq!(
            std::fs::read_to_string(root.path().join("inventory.json")).unwrap(),
            value
        );
        std::fs::write(root.path().join("inventory.json"), b"").unwrap();
        assert!(prepare_inventory_directory(root.path()).is_err());
        assert!(
            std::fs::read(root.path().join("inventory.json"))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn concurrent_initializers_converge_and_crashed_temporary_file_is_ignored() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".mtc-inventory-init-crashed"), b"").unwrap();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| prepare_inventory_directory(root.path()).unwrap());
            }
        });
        assert_eq!(
            std::fs::read(root.path().join("inventory.json")).unwrap(),
            b"{}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn never_follows_an_existing_inventory_symlink() {
        let root = tempfile::tempdir().unwrap();
        let outside = root.path().join("outside");
        std::fs::write(&outside, b"{}").unwrap();
        std::os::unix::fs::symlink(&outside, root.path().join("inventory.json")).unwrap();
        assert!(prepare_inventory_directory(root.path()).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"{}");
    }
}
