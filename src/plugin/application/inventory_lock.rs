//! All mutable inventory writers coordinate on a stable sibling inode, not the
//! inventory inode replaced by atomic publication. The OS owns lock release,
//! including process exit; no expiring lease can admit a still-running writer.
use std::{fs::File, path::Path};

use crate::error::AppError;

pub(super) fn write_lock(inventory: &Path) -> Result<File, AppError> {
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(inventory.with_extension("lock"))
        .map_err(|_| AppError::internal("plugin_inventory_lock_open"))?;
    // Called only from CLI initialization or an owned blocking task. Queue
    // competing controls instead of returning a transient installation 503.
    // The descriptor stays owned by that task until publication + fsync finish,
    // even if its HTTP caller disconnects.
    lock.lock()
        .map_err(|_| AppError::internal("plugin_inventory_lock_acquire"))?;
    Ok(lock)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn concurrent_inventory_appends_preserve_both_immutable_entries() {
        let directory = tempfile::tempdir().unwrap();
        super::super::prepare::prepare_inventory_directory(directory.path()).unwrap();
        let path = directory.path().join("inventory.json");
        let entry = super::super::PreinstalledInventory {
            root: directory.path().join("packages"),
            grants: Default::default(),
        };
        let (first, second) = tokio::join!(
            super::super::installation::append_inventory_file(&path, "first", &entry),
            super::super::installation::append_inventory_file(&path, "second", &entry),
        );
        first.unwrap();
        second.unwrap();
        let inventory: std::collections::BTreeMap<String, super::super::PreinstalledInventory> =
            serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(inventory.len(), 2);
        assert!(inventory.contains_key("first"));
        assert!(inventory.contains_key("second"));
    }

    #[test]
    fn writer_lock_is_visible_to_an_independent_process_and_released_on_drop() {
        let directory = tempfile::tempdir().unwrap();
        let inventory = directory.path().join("inventory.json");
        let guard = write_lock(&inventory).unwrap();
        let run_child = |locked: bool| {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "plugin::application::inventory_lock::tests::child_checks_lock",
                    "--ignored",
                ])
                .env("MTC_TEST_INVENTORY_LOCK", &inventory)
                .env("MTC_TEST_INVENTORY_LOCKED", if locked { "1" } else { "0" })
                .status()
                .unwrap()
        };
        assert!(run_child(true).success());
        drop(guard);
        assert!(run_child(false).success());
    }

    #[test]
    #[ignore = "subprocess fixture"]
    fn child_checks_lock() {
        let path = std::env::var_os("MTC_TEST_INVENTORY_LOCK").unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(Path::new(&path).with_extension("lock"))
            .unwrap();
        let expected_locked = std::env::var("MTC_TEST_INVENTORY_LOCKED").unwrap() == "1";
        assert_eq!(file.try_lock().is_err(), expected_locked);
    }
}
