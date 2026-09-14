//! Draft application integration. No automatic production activation. Inventory
//! roots and grants are provisioned by the host, never by management requests.
use std::{
    collections::{BTreeMap, VecDeque},
    path::PathBuf,
    sync::{Arc, LazyLock},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::{
    PluginRuntime,
    lifecycle::{self, PluginGrant, RevisionReason, RuntimeSnapshot},
};
use crate::{db::Database, error::AppError, provider::ProviderCatalog};

const CACHED_REVISIONS: usize = 2;
const ADMISSION_WAIT: Duration = Duration::from_secs(5);
const COMPILATION_DEADLINE: Duration = Duration::from_secs(35);
// Shared by request pinning and administrative staging across all AppStates.
// An abandoned blocking compilation retains its permit until it really ends.
static COMPILATION_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

/// Trusted deployment input. Each root must be a distinct immutable revision
/// directory containing the complete required plugin set, not a mutable symlink.
#[derive(Clone)]
pub struct PreinstalledInventory {
    pub root: PathBuf,
    pub grants: BTreeMap<String, Vec<PluginGrant>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ApplicationRevision {
    pub revision: i64,
    pub inventory_id: String,
    pub reason: String,
    #[serde(skip)]
    pub(crate) identity_digest: String,
    #[serde(skip)]
    pub(crate) contract_digest: String,
}

/// One indivisible request pin. Neither field is independently published.
pub struct ApplicationPluginSnapshot {
    pub receipt: ApplicationRevision,
    pub runtime: Arc<RuntimeSnapshot>,
    pub providers: ProviderCatalog,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishApplicationPlugin {
    pub inventory_id: String,
    pub expected_revision: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RollbackApplicationPlugin {
    pub target_revision: i64,
    pub expected_revision: i64,
}

pub struct ApplicationPlugins {
    db: Database,
    inventory: BTreeMap<String, PreinstalledInventory>,
    contract_digest: String,
    snapshots: tokio::sync::Mutex<VecDeque<Arc<ApplicationPluginSnapshot>>>,
    #[cfg(test)]
    compilations: std::sync::atomic::AtomicUsize,
}

impl ApplicationPlugins {
    pub fn new(
        db: Database,
        inventory: BTreeMap<String, PreinstalledInventory>,
        baseline: &PluginRuntime,
    ) -> Result<Self, AppError> {
        for (id, entry) in &inventory {
            validate_inventory_id(id)?;
            if !entry.root.is_absolute() {
                return Err(AppError::Forbidden);
            }
        }
        Ok(Self {
            db,
            inventory,
            contract_digest: contract_digest(baseline)?,
            snapshots: tokio::sync::Mutex::new(VecDeque::new()),
            #[cfg(test)]
            compilations: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    async fn load(
        &self,
        id: &str,
        revision: i64,
        reason: &str,
    ) -> Result<ApplicationPluginSnapshot, AppError> {
        validate_inventory_id(id)?;
        let entry = self.inventory.get(id).cloned().ok_or(AppError::Forbidden)?;
        let db = self.db.clone();
        let permit =
            tokio::time::timeout(ADMISSION_WAIT, COMPILATION_PERMITS.clone().acquire_owned())
                .await
                .map_err(|_| AppError::Overloaded)?
                .map_err(|_| AppError::Internal)?;
        #[cfg(test)]
        self.compilations
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        // Build only on cache miss/staging. Neither timeout nor caller cancellation
        // releases capacity while Wasmtime compilation still occupies a thread.
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let metadata =
                std::fs::symlink_metadata(&entry.root).map_err(|_| AppError::Internal)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AppError::Forbidden);
            }
            let root = entry.root.to_str().ok_or(AppError::Forbidden)?;
            let runtime = PluginRuntime::load(Some(root), db).map_err(|_| AppError::Internal)?;
            lifecycle::validate_grants(&runtime, &entry.grants)?;
            Ok::<_, AppError>(runtime)
        });
        let runtime = tokio::time::timeout(COMPILATION_DEADLINE, task)
            .await
            .map_err(|_| AppError::Overloaded)?
            .map_err(|_| AppError::Internal)??;
        let contract_digest = contract_digest(&runtime)?;
        if contract_digest != self.contract_digest {
            return Err(AppError::Forbidden);
        }
        runtime.validate_stored_configurations().await?;
        let identity_digest = super::plugin_configuration_schema_digest(&json!({
            "manifests": runtime.manifests(), "identities": runtime.package_identities()
        }))?;
        let mut providers = ProviderCatalog::builtins();
        providers.extend(runtime.provider_types())?;
        let revision_reason = match reason {
            "initial" => RevisionReason::Initial,
            "reload" => RevisionReason::Reload,
            "rollback" => RevisionReason::Rollback,
            _ => return Err(AppError::Internal),
        };
        Ok(ApplicationPluginSnapshot {
            receipt: ApplicationRevision {
                revision,
                inventory_id: id.to_owned(),
                reason: reason.to_owned(),
                identity_digest,
                contract_digest,
            },
            runtime: lifecycle::snapshot(
                runtime,
                revision.try_into().map_err(|_| AppError::Internal)?,
                revision_reason,
            ),
            providers,
        })
    }

    pub async fn stage(&self, inventory_id: &str) -> Result<(), AppError> {
        let candidate = self.load(inventory_id, 1, "initial").await?;
        self.db
            .stage_application_plugin_candidate(
                inventory_id,
                &candidate.receipt.identity_digest,
                &candidate.receipt.contract_digest,
            )
            .await
    }

    pub async fn publish(
        &self,
        input: PublishApplicationPlugin,
        key: &str,
    ) -> Result<ApplicationRevision, AppError> {
        validate_operation(input.expected_revision, key)?;
        let reason = if input.expected_revision == 0 {
            "initial"
        } else {
            "reload"
        };
        self.stage(&input.inventory_id).await?;
        let hash = super::plugin_configuration_schema_digest(
            &json!({ "action": "publish", "inventory_id": input.inventory_id, "expected_revision": input.expected_revision }),
        )?;
        self.db
            .publish_application_plugin(
                &input.inventory_id,
                input.expected_revision,
                reason,
                key,
                &hash,
            )
            .await
    }

    pub async fn rollback(
        &self,
        input: RollbackApplicationPlugin,
        key: &str,
    ) -> Result<ApplicationRevision, AppError> {
        validate_operation(input.expected_revision, key)?;
        if input.target_revision <= 0 || input.target_revision >= input.expected_revision {
            return Err(AppError::BadRequest(
                "rollback target must be an earlier revision".into(),
            ));
        }
        let target = self
            .db
            .application_plugin_revision(input.target_revision)
            .await?;
        let candidate = self
            .load(&target.inventory_id, target.revision, "rollback")
            .await?;
        validate_receipt(&candidate.receipt, &target)?;
        let hash = super::plugin_configuration_schema_digest(
            &json!({ "action": "rollback", "target_revision": input.target_revision, "expected_revision": input.expected_revision }),
        )?;
        self.db
            .publish_application_plugin(
                &target.inventory_id,
                input.expected_revision,
                "rollback",
                key,
                &hash,
            )
            .await
    }

    pub async fn pin(&self) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
        // The cache never supplies authority. Even a warm hit must read the
        // primary head and validate the exact immutable receipt on this request.
        let head = self.db.application_plugin_head().await?;
        let entry = self
            .inventory
            .get(&head.inventory_id)
            .ok_or(AppError::Forbidden)?;
        let metadata = tokio::fs::symlink_metadata(&entry.root)
            .await
            .map_err(|_| AppError::Internal)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(AppError::Forbidden);
        }
        // Hold the lock through loading/publication: concurrent cold pins join
        // one compilation instead of each producing a new engine and epoch task.
        let mut snapshots = tokio::time::timeout(ADMISSION_WAIT, self.snapshots.lock())
            .await
            .map_err(|_| AppError::Overloaded)?;
        if let Some(snapshot) = snapshots
            .iter()
            .find(|snapshot| snapshot.receipt.revision == head.revision)
        {
            if snapshot.receipt.inventory_id != head.inventory_id
                || snapshot.receipt.reason != head.reason
            {
                return Err(AppError::Forbidden);
            }
            validate_receipt(&snapshot.receipt, &head)?;
            return Ok(snapshot.clone());
        }
        let snapshot = self
            .load(&head.inventory_id, head.revision, &head.reason)
            .await?;
        validate_receipt(&snapshot.receipt, &head)?;
        let snapshot = Arc::new(snapshot);
        if snapshots.len() == CACHED_REVISIONS {
            snapshots.pop_front();
        }
        snapshots.push_back(snapshot.clone());
        Ok(snapshot)
    }
}

fn validate_receipt(
    local: &ApplicationRevision,
    stored: &ApplicationRevision,
) -> Result<(), AppError> {
    if local.identity_digest != stored.identity_digest
        || local.contract_digest != stored.contract_digest
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn contract_digest(runtime: &PluginRuntime) -> Result<String, AppError> {
    let contracts: BTreeMap<_, _> = runtime.manifests().into_iter().map(|manifest| {
        (manifest.id, json!({ "wit_version": manifest.wit_version, "capabilities": manifest.capabilities, "contributions": manifest.contributions }))
    }).collect();
    super::plugin_configuration_schema_digest(&json!(contracts))
}

fn validate_inventory_id(id: &str) -> Result<(), AppError> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(AppError::BadRequest(
            "invalid preinstalled inventory ID".into(),
        ));
    }
    Ok(())
}

fn validate_operation(expected: i64, key: &str) -> Result<(), AppError> {
    if expected < 0
        || expected == i64::MAX
        || key.is_empty()
        || key.len() > 200
        || key.chars().any(char::is_control)
    {
        return Err(AppError::BadRequest(
            "invalid runtime operation metadata".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
