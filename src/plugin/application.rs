//! Application integration. Host-configured production activation. Inventory
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
const PIN_DEADLINE: Duration = Duration::from_secs(45);
// Shared by request pinning and administrative staging across all AppStates.
// An abandoned blocking compilation retains its permit until it really ends.
static COMPILATION_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

/// Trusted deployment input. Each root must be a distinct immutable revision
/// directory containing the complete required plugin set, not a mutable symlink.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
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

#[derive(Serialize)]
pub struct ApplicationPluginStatus {
    pub current: Option<ApplicationRevision>,
    pub candidates: Vec<ApplicationPluginCandidate>,
}

#[derive(Serialize)]
pub struct ApplicationPluginCandidate {
    pub inventory_id: String,
    pub staged: bool,
    /// Host-approved versions only; filesystem roots and provenance stay private.
    pub plugins: BTreeMap<String, Vec<String>>,
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
    snapshots: tokio::sync::Mutex<RevisionCache>,
    #[cfg(test)]
    compilations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    compile_gate: std::sync::Mutex<Option<CompileGate>>,
}

#[cfg(test)]
type CompileGate = (
    tokio::sync::oneshot::Sender<()>,
    std::sync::mpsc::Receiver<()>,
);

#[derive(Default)]
struct RevisionCache {
    snapshots: VecDeque<Arc<ApplicationPluginSnapshot>>,
    in_flight: Option<Arc<RevisionLoad>>,
}

struct RevisionLoad {
    revision: i64,
    result: tokio::sync::watch::Receiver<Option<LoadResult>>,
}

type LoadResult = Result<Arc<ApplicationPluginSnapshot>, LoadFailure>;

#[derive(Clone, Copy)]
enum LoadFailure {
    Forbidden,
    Overloaded,
    Internal,
}

impl LoadFailure {
    fn from_error(error: AppError) -> Self {
        match error {
            AppError::Forbidden => Self::Forbidden,
            AppError::Overloaded => Self::Overloaded,
            _ => Self::Internal,
        }
    }

    fn into_error(self) -> AppError {
        match self {
            Self::Forbidden => AppError::Forbidden,
            Self::Overloaded => AppError::Overloaded,
            Self::Internal => AppError::Internal,
        }
    }
}

impl ApplicationPlugins {
    pub async fn status(&self) -> Result<ApplicationPluginStatus, AppError> {
        let current = self.db.optional_application_plugin_head().await?;
        let staged = self.db.staged_application_plugin_ids().await?;
        Ok(ApplicationPluginStatus {
            current,
            candidates: self
                .inventory
                .iter()
                .map(|(id, entry)| ApplicationPluginCandidate {
                    inventory_id: id.clone(),
                    staged: staged.contains(id),
                    plugins: entry
                        .grants
                        .iter()
                        .map(|(plugin, grants)| {
                            (
                                plugin.clone(),
                                grants.iter().map(|grant| grant.version.clone()).collect(),
                            )
                        })
                        .collect(),
                })
                .collect(),
        })
    }

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
            snapshots: tokio::sync::Mutex::new(RevisionCache::default()),
            #[cfg(test)]
            compilations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            compile_gate: std::sync::Mutex::new(None),
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
        #[cfg(test)]
        let gate = self.compile_gate.lock().unwrap().take();
        // Build only on cache miss/staging. Neither timeout nor caller cancellation
        // releases capacity while Wasmtime compilation still occupies a thread.
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

    pub async fn pin(self: &Arc<Self>) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
        // The cache never supplies authority. Even a warm hit must read the
        // primary head and validate the exact immutable receipt on this request.
        let head = self.db.application_plugin_head().await?;
        self.pin_revision(head).await
    }

    pub async fn pin_if_published(
        self: &Arc<Self>,
    ) -> Result<Option<Arc<ApplicationPluginSnapshot>>, AppError> {
        match self.db.optional_application_plugin_head().await? {
            Some(head) => self.pin_revision(head).await.map(Some),
            None => Ok(None),
        }
    }

    async fn pin_revision(
        self: &Arc<Self>,
        head: ApplicationRevision,
    ) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
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
        tokio::time::timeout(PIN_DEADLINE, self.pin_head(head))
            .await
            .map_err(|_| AppError::Overloaded)?
    }

    async fn pin_head(
        self: &Arc<Self>,
        head: ApplicationRevision,
    ) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
        loop {
            let flight = {
                let mut cache = self.snapshots.lock().await;
                if let Some(snapshot) = cache
                    .snapshots
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
                if let Some(flight) = &cache.in_flight {
                    flight.clone()
                } else {
                    let (result, receive) = tokio::sync::watch::channel(None);
                    let flight = Arc::new(RevisionLoad {
                        revision: head.revision,
                        result: receive,
                    });
                    cache.in_flight = Some(flight.clone());
                    let authority = self.clone();
                    let receipt = head.clone();
                    // The manager owns the operation and cache publication, not
                    // the first request. Dropping any/all waiters loses no result.
                    tokio::spawn(async move {
                        let loaded = tokio::time::timeout(PIN_DEADLINE, async {
                            let snapshot = authority
                                .load(&receipt.inventory_id, receipt.revision, &receipt.reason)
                                .await?;
                            validate_receipt(&snapshot.receipt, &receipt)?;
                            Ok::<_, AppError>(Arc::new(snapshot))
                        })
                        .await
                        .unwrap_or(Err(AppError::Overloaded))
                        .map_err(LoadFailure::from_error);
                        let mut cache = authority.snapshots.lock().await;
                        if let Ok(snapshot) = &loaded {
                            if cache.snapshots.len() == CACHED_REVISIONS {
                                cache.snapshots.pop_front();
                            }
                            cache.snapshots.push_back(snapshot.clone());
                        }
                        cache.in_flight = None;
                        result.send_replace(Some(loaded));
                    });
                    flight
                }
            };
            let mut receive = flight.result.clone();
            let result = loop {
                if let Some(result) = receive.borrow().clone() {
                    break result;
                }
                if receive.changed().await.is_err() {
                    let mut cache = self.snapshots.lock().await;
                    if cache
                        .in_flight
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &flight))
                    {
                        cache.in_flight = None;
                    }
                    return Err(AppError::Internal);
                }
            };
            if flight.revision == head.revision {
                let snapshot = result.map_err(LoadFailure::into_error)?;
                if snapshot.receipt.inventory_id != head.inventory_id
                    || snapshot.receipt.reason != head.reason
                {
                    return Err(AppError::Forbidden);
                }
                validate_receipt(&snapshot.receipt, &head)?;
                return Ok(snapshot);
            }
        }
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
