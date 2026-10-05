use super::*;

#[cfg(test)]
mod tests;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisterEmptyInventory {
    pub inventory_id: String,
}

impl ApplicationPlugins {
    pub async fn register_empty(
        &self,
        input: RegisterEmptyInventory,
        key: &str,
        actor: &str,
    ) -> Result<(), AppError> {
        validate_inventory_id(&input.inventory_id)?;
        validate_operation(0, key)?;
        let path = self.inventory_file.as_ref().ok_or(AppError::Forbidden)?;
        let parent = path.parent().ok_or(AppError::Forbidden)?;
        let digest = super::super::plugin_configuration_schema_digest(&json!(input.inventory_id))?;
        let root = parent.join(format!(".mtc-empty-{digest}"));
        let event_key = format!(
            "empty-register:{}",
            super::super::plugin_configuration_schema_digest(&json!(key))?
        );
        let directory = root.clone();
        tokio::task::spawn_blocking(move || {
            match std::fs::create_dir(&directory) {
                Ok(()) => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(
                            &directory,
                            std::fs::Permissions::from_mode(0o550),
                        )
                        .map_err(|_| AppError::Internal)?;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(AppError::Internal),
            }
            let metadata = std::fs::symlink_metadata(&directory).map_err(|_| AppError::Internal)?;
            if !metadata.is_dir()
                || metadata.file_type().is_symlink()
                || std::fs::read_dir(&directory)
                    .map_err(|_| AppError::Internal)?
                    .next()
                    .is_some()
            {
                return Err(AppError::Forbidden);
            }
            std::fs::File::open(&directory)
                .and_then(|file| file.sync_all())
                .map_err(|_| AppError::Internal)?;
            std::fs::File::open(directory.parent().ok_or(AppError::Forbidden)?)
                .and_then(|file| file.sync_all())
                .map_err(|_| AppError::Internal)
        })
        .await
        .map_err(|_| AppError::Internal)??;
        let entry = PreinstalledInventory {
            root,
            grants: BTreeMap::new(),
        };
        let candidate = self
            .load_entry(&input.inventory_id, entry.clone(), 1, "initial")
            .await?;
        let transaction = self
            .db
            .begin_empty_plugin_registration(&input.inventory_id, &event_key, actor)
            .await?;
        installation::append_inventory_file(path, &input.inventory_id, &entry).await?;
        self.db
            .finish_empty_plugin_registration(
                transaction,
                &input.inventory_id,
                &event_key,
                &candidate.receipt.identity_digest,
                &candidate.receipt.contract_digest,
            )
            .await
    }
}
