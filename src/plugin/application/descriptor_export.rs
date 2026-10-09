//! Control-owned descriptor generation. Export is neither publication nor reader readiness.
//! All grants and package identities come from the host inventory and its loaded assets.

use std::{
    collections::BTreeMap,
    path::{Component, Path},
    sync::{Arc, LazyLock},
};

use serde::{Deserialize, Serialize};

use super::{
    ADMISSION_WAIT, ApplicationPlugins, ApplicationRevision, COMPILATION_DEADLINE,
    DescriptorInventory, PluginInventoryDescriptor, PreinstalledInventory, contract_digest,
    validate_inventory_id, validate_operation,
};
use crate::{
    error::AppError,
    plugin::{PluginRuntime, lifecycle, plugin_configuration_schema_digest},
    provider::ProviderCatalog,
};

static EXPORT_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportPluginDescriptor {
    pub inventory_id: Option<String>,
    pub expected_revision: i64,
}

#[derive(Serialize)]
pub struct PluginDescriptorExport {
    pub descriptor: PluginInventoryDescriptor,
    pub descriptor_digest: String,
    pub observed_head: Option<ApplicationRevision>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DescriptorCandidateReceipt {
    pub identity_digest: String,
    pub contract_digest: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DescriptorAuthoritySnapshot {
    pub head: Option<ApplicationRevision>,
    pub candidates: BTreeMap<String, DescriptorCandidateReceipt>,
    pub revisions: Vec<ApplicationRevision>,
}

impl ApplicationPlugins {
    pub(crate) async fn export_descriptor(
        &self,
        input: ExportPluginDescriptor,
        destination_root: &Path,
    ) -> Result<PluginDescriptorExport, AppError> {
        validate_operation(input.expected_revision, "descriptor-export")?;
        if let Some(inventory_id) = &input.inventory_id {
            validate_inventory_id(inventory_id)?;
        }
        if !destination_root.is_absolute()
            || destination_root.parent().is_none()
            || destination_root
                .components()
                .any(|component| !matches!(component, Component::RootDir | Component::Normal(_)))
        {
            return Err(AppError::Forbidden);
        }
        let before = self.db.plugin_descriptor_authority_snapshot().await?;
        if before.head.as_ref().map_or(0, |head| head.revision) != input.expected_revision {
            return Err(AppError::Conflict("plugin runtime revision changed".into()));
        }
        if input
            .inventory_id
            .as_ref()
            .is_some_and(|id| !before.candidates.contains_key(id))
        {
            return Err(AppError::NotFound);
        }
        let inventory = self.read_export_inventory().await?;
        if before
            .candidates
            .keys()
            .any(|id| !inventory.contains_key(id))
        {
            return Err(AppError::Forbidden);
        }
        let mut inventories = BTreeMap::new();
        for (inventory_id, source) in &inventory {
            validate_inventory_id(inventory_id)?;
            let runtime = self.load_export_inventory(source).await?;
            let mut providers = ProviderCatalog::builtins();
            providers.extend(runtime.provider_types())?;
            let identity_digest = plugin_configuration_schema_digest(&serde_json::json!({
                "manifests": runtime.manifests(), "identities": runtime.package_identities()
            }))?;
            let contract_digest = contract_digest(&runtime)?;
            if let Some(candidate) = before.candidates.get(inventory_id)
                && (candidate.identity_digest != identity_digest
                    || candidate.contract_digest != contract_digest)
            {
                return Err(AppError::Forbidden);
            }
            inventories.insert(
                inventory_id.clone(),
                DescriptorInventory {
                    inventory: PreinstalledInventory {
                        root: destination_root.join(format!("inventory-{inventory_id}")),
                        grants: source.grants.clone(),
                    },
                    packages: runtime.package_identities(),
                    identity_digest,
                    contract_digest,
                },
            );
        }
        let descriptor = PluginInventoryDescriptor {
            format_version: PluginInventoryDescriptor::CURRENT_FORMAT_VERSION,
            inventories,
        };
        let descriptor_digest = descriptor.digest()?;
        let current_inventory = self.read_export_inventory().await?;
        if serde_json::to_value(&inventory).map_err(|_| AppError::Internal)?
            != serde_json::to_value(&current_inventory).map_err(|_| AppError::Internal)?
            || self.db.plugin_descriptor_authority_snapshot().await? != before
        {
            return Err(AppError::Conflict("plugin export authority changed".into()));
        }
        Ok(PluginDescriptorExport {
            descriptor,
            descriptor_digest,
            observed_head: before.head,
        })
    }

    async fn read_export_inventory(
        &self,
    ) -> Result<BTreeMap<String, PreinstalledInventory>, AppError> {
        use tokio::io::AsyncReadExt;

        let retained = self.inventory.read().await.clone();
        let Some(path) = &self.inventory_file else {
            return Ok(retained);
        };
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| AppError::Internal)?;
        let metadata = file.metadata().await.map_err(|_| AppError::Internal)?;
        let limit = PluginInventoryDescriptor::MAX_PARSE_BYTES;
        if !metadata.is_file() || metadata.len() > limit as u64 {
            return Err(AppError::Forbidden);
        }
        let mut bytes = Vec::new();
        file.take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| AppError::Internal)?;
        if bytes.len() > limit {
            return Err(AppError::Forbidden);
        }
        let incoming: BTreeMap<String, PreinstalledInventory> =
            serde_json::from_slice(&bytes).map_err(|_| AppError::Forbidden)?;
        for (inventory_id, previous) in retained {
            let entry = incoming.get(&inventory_id).ok_or(AppError::Forbidden)?;
            if serde_json::to_value(previous).map_err(|_| AppError::Internal)?
                != serde_json::to_value(entry).map_err(|_| AppError::Internal)?
            {
                return Err(AppError::Forbidden);
            }
        }
        Ok(incoming)
    }

    async fn load_export_inventory(
        &self,
        entry: &PreinstalledInventory,
    ) -> Result<PluginRuntime, AppError> {
        let entry = entry.clone();
        if !entry.root.is_absolute() {
            return Err(AppError::Forbidden);
        }
        let db = self.db.clone();
        let permit = tokio::time::timeout(ADMISSION_WAIT, EXPORT_PERMITS.clone().acquire_owned())
            .await
            .map_err(|_| AppError::Overloaded)?
            .map_err(|_| AppError::Internal)?;
        #[cfg(test)]
        let gate = self.compile_gate.lock().unwrap().take();
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            #[cfg(test)]
            if let Some((entered, release)) = gate {
                let _ = entered.send(());
                release.recv().map_err(|_| AppError::Internal)?;
            }
            let metadata =
                std::fs::symlink_metadata(&entry.root).map_err(|_| AppError::Internal)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AppError::Forbidden);
            }
            let root = entry.root.to_str().ok_or(AppError::Forbidden)?;
            let runtime = PluginRuntime::load(Some(root), db).map_err(|_| AppError::Forbidden)?;
            lifecycle::validate_grants(&runtime, &entry.grants)?;
            Ok::<_, AppError>(runtime)
        });
        tokio::time::timeout(COMPILATION_DEADLINE, task)
            .await
            .map_err(|_| AppError::Overloaded)?
            .map_err(|_| AppError::Internal)?
    }
}

#[cfg(test)]
mod tests;
