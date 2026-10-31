//! Application integration. Host-configured production activation. Inventory
//! roots are host-controlled; grants are preconfigured or explicitly approved
//! by a global administrator after signed installation and exact manifest review.
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
pub mod descriptor;
pub use descriptor::{DescriptorInventory, PluginInventoryDescriptor};
pub mod descriptor_export;
pub use descriptor_export::{ExportPluginDescriptor, PluginDescriptorExport};
#[cfg(feature = "plugin-distribution")]
pub mod descriptor_import;
mod empty;
pub mod uninstall;
pub use empty::RegisterEmptyInventory;
pub mod installation;
pub mod prepare;
mod retained_installation;
const ADMISSION_WAIT: Duration = Duration::from_secs(5);
const COMPILATION_DEADLINE: Duration = Duration::from_secs(35);
const PIN_DEADLINE: Duration = Duration::from_secs(45);
// Shared by lifecycle loading and administrative staging across all AppStates.
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
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
    current: std::sync::RwLock<CurrentSnapshot>,
    refresh: tokio::sync::Mutex<()>,
    inventory: tokio::sync::RwLock<BTreeMap<String, PreinstalledInventory>>,
    inventory_file: Option<PathBuf>,
    installation_policy_file: Option<PathBuf>,
    installer_network_key: Option<Vec<u8>>,
    inventory_stamp: tokio::sync::Mutex<Option<InventoryStamp>>,
    #[cfg(test)]
    inventory_reads: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    inventory_refreshes: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    pin_root_inspections: std::sync::atomic::AtomicUsize,
    snapshots: tokio::sync::Mutex<RevisionCache>,
    #[cfg(test)]
    compilations: std::sync::atomic::AtomicUsize,
    #[cfg(test)]
    compile_gate: std::sync::Mutex<Option<CompileGate>>,
}

#[derive(Default)]
struct CurrentSnapshot {
    snapshot: Option<Arc<ApplicationPluginSnapshot>>,
    initialized: bool,
    error: Option<&'static str>,
}

struct RefreshAttempt<'a> {
    current: &'a std::sync::RwLock<CurrentSnapshot>,
    completed: bool,
}

impl Drop for RefreshAttempt<'_> {
    fn drop(&mut self) {
        if !self.completed
            && let Ok(mut current) = self.current.write()
        {
            current.error = Some("snapshot_refresh_cancelled");
            tracing::error!("application plugin snapshot refresh cancelled");
        }
    }
}

#[derive(PartialEq, Eq)]
struct InventoryStamp {
    length: u64,
    modified: std::time::SystemTime,
    #[cfg(unix)]
    inode: (u64, u64, i64, i64),
}

impl InventoryStamp {
    fn from_metadata(metadata: &std::fs::Metadata) -> Result<Self, AppError> {
        if !metadata.is_file() {
            return Err(AppError::Forbidden);
        }
        Ok(Self {
            length: metadata.len(),
            modified: metadata
                .modified()
                .map_err(|_| AppError::internal("application_from_metadata_1"))?,
            #[cfg(unix)]
            inode: {
                use std::os::unix::fs::MetadataExt;
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.ctime(),
                    metadata.ctime_nsec(),
                )
            },
        })
    }
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
    Capacity {
        code: &'static str,
        facts: crate::error::ErrorFacts,
    },
    Upstream {
        code: &'static str,
        facts: crate::error::ErrorFacts,
    },
    Internal {
        code: &'static str,
        facts: crate::error::ErrorFacts,
    },
}

impl LoadFailure {
    fn from_error(error: AppError) -> Self {
        match error {
            AppError::Forbidden => Self::Forbidden,
            AppError::Capacity { code, facts } => Self::Capacity { code, facts },
            AppError::Overloaded => Self::Capacity {
                code: "service_overloaded",
                facts: crate::error::ErrorFacts::new().stage("plugin_snapshot"),
            },
            AppError::Internal { code, facts } => Self::Internal { code, facts },
            AppError::Upstream { code, facts } => Self::Upstream { code, facts },
            other => Self::Internal {
                code: "plugin_snapshot_load_failed",
                facts: crate::error::ErrorFacts::new().stage(other.diagnostic_category()),
            },
        }
    }

    fn into_error(self) -> AppError {
        match self {
            Self::Forbidden => AppError::Forbidden,
            Self::Capacity { code, facts } => AppError::capacity(code, facts),
            Self::Upstream { code, facts } => AppError::upstream_with(code, facts),
            Self::Internal { code, facts } => AppError::Internal { code, facts },
        }
    }
}

impl ApplicationPlugins {
    pub async fn status(&self) -> Result<ApplicationPluginStatus, AppError> {
        self.refresh_inventory().await?;
        let current = self.db.optional_application_plugin_head().await?;
        let staged = self.db.staged_application_plugin_ids().await?;
        Ok(ApplicationPluginStatus {
            current,
            candidates: self
                .inventory
                .read()
                .await
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
        _baseline: &PluginRuntime,
    ) -> Result<Self, AppError> {
        for (id, entry) in &inventory {
            validate_inventory_id(id)?;
            if !entry.root.is_absolute() {
                return Err(AppError::Forbidden);
            }
        }
        Ok(Self {
            db,
            current: std::sync::RwLock::new(CurrentSnapshot::default()),
            refresh: tokio::sync::Mutex::new(()),
            inventory: tokio::sync::RwLock::new(inventory),
            inventory_file: None,
            installation_policy_file: None,
            installer_network_key: None,
            inventory_stamp: tokio::sync::Mutex::new(None),
            #[cfg(test)]
            inventory_reads: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            inventory_refreshes: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            pin_root_inspections: std::sync::atomic::AtomicUsize::new(0),
            snapshots: tokio::sync::Mutex::new(RevisionCache::default()),
            #[cfg(test)]
            compilations: std::sync::atomic::AtomicUsize::new(0),
            #[cfg(test)]
            compile_gate: std::sync::Mutex::new(None),
        })
    }

    /// The installer publishes a complete, host-owned inventory file by atomic
    /// rename. Requests may select IDs, but never supply paths or grants.
    pub async fn from_inventory_file(
        db: Database,
        path: PathBuf,
        baseline: &PluginRuntime,
    ) -> Result<Self, AppError> {
        if !path.is_absolute() {
            return Err(AppError::Forbidden);
        }
        let mut authority = Self::new(db, BTreeMap::new(), baseline)?;
        authority.inventory_file = Some(path);
        authority.refresh_inventory().await?;
        Ok(authority)
    }

    async fn refresh_inventory(&self) -> Result<(), AppError> {
        #[cfg(test)]
        self.inventory_refreshes
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let Some(path) = &self.inventory_file else {
            return Ok(());
        };
        use tokio::io::AsyncReadExt;
        const MAX_INVENTORY_BYTES: u64 = 4 * 1024 * 1024;
        // Serialize refreshes and inspect the opened inode, not a separate
        // pathname stat. Atomic rename cannot mix metadata from one revision
        // with bytes from another. Unchanged inventories never parse again.
        let mut cached_stamp = self.inventory_stamp.lock().await;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| AppError::internal("application_refresh_inventory_1"))?;
        let stamp = InventoryStamp::from_metadata(
            &file
                .metadata()
                .await
                .map_err(|_| AppError::internal("application_refresh_inventory_2"))?,
        )?;
        if cached_stamp.as_ref() == Some(&stamp) {
            return Ok(());
        }
        if stamp.length > MAX_INVENTORY_BYTES {
            return Err(AppError::Forbidden);
        }
        let mut bytes = Vec::new();
        #[cfg(test)]
        self.inventory_reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        file.take(MAX_INVENTORY_BYTES + 1)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| AppError::internal("application_refresh_inventory_3"))?;
        if bytes.len() as u64 > MAX_INVENTORY_BYTES {
            return Err(AppError::Forbidden);
        }
        let incoming: BTreeMap<String, PreinstalledInventory> =
            serde_json::from_slice(&bytes).map_err(|_| AppError::Forbidden)?;
        let mut inventory = self.inventory.write().await;
        for (id, entry) in &incoming {
            validate_inventory_id(id)?;
            if !entry.root.is_absolute() {
                return Err(AppError::Forbidden);
            }
        }
        // Retain every historical root for restart-safe rollback. Existing IDs
        // are immutable, including grants; changing a contract needs a new ID.
        for (id, old) in inventory.iter() {
            let new = incoming.get(id).ok_or(AppError::Forbidden)?;
            if serde_json::to_value(old)
                .map_err(|_| AppError::internal("application_refresh_inventory_4"))?
                != serde_json::to_value(new)
                    .map_err(|_| AppError::internal("application_refresh_inventory_5"))?
            {
                return Err(AppError::Forbidden);
            }
        }
        *inventory = incoming;
        *cached_stamp = Some(stamp);
        Ok(())
    }

    async fn load(
        &self,
        id: &str,
        revision: i64,
        reason: &str,
    ) -> Result<ApplicationPluginSnapshot, AppError> {
        validate_inventory_id(id)?;
        self.refresh_inventory().await?;
        let entry = self
            .inventory
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or(AppError::Forbidden)?;
        self.load_entry(id, entry, revision, reason).await
    }

    async fn load_entry(
        &self,
        id: &str,
        entry: PreinstalledInventory,
        revision: i64,
        reason: &str,
    ) -> Result<ApplicationPluginSnapshot, AppError> {
        let db = self.db.clone();
        let permit =
            tokio::time::timeout(ADMISSION_WAIT, COMPILATION_PERMITS.clone().acquire_owned())
                .await
                .map_err(|_| AppError::capacity_error("plugin_compilation_capacity"))?
                .map_err(|_| AppError::internal("application_load_entry_1"))?;
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
                release
                    .recv()
                    .map_err(|_| AppError::internal("application_load_entry_2"))?;
            }
            let metadata = std::fs::symlink_metadata(&entry.root)
                .map_err(|_| AppError::internal("application_load_entry_3"))?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AppError::Forbidden);
            }
            let root = entry.root.to_str().ok_or(AppError::Forbidden)?;
            let runtime = PluginRuntime::load(Some(root), db)
                .map_err(|_| AppError::internal("application_load_entry_4"))?;
            lifecycle::validate_grants(&runtime, &entry.grants)?;
            Ok::<_, AppError>(runtime)
        });
        let runtime = tokio::time::timeout(COMPILATION_DEADLINE, task)
            .await
            .map_err(|_| AppError::capacity_error("plugin_compilation_capacity"))?
            .map_err(|_| AppError::internal("application_load_entry_5"))??;
        let contract_digest = contract_digest(&runtime)?;
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
            _ => return Err(AppError::internal("application_load_entry_1")),
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
                revision
                    .try_into()
                    .map_err(|_| AppError::internal("application_load_entry_6"))?,
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
        self.publish_as(input, key, "host").await
    }

    pub async fn publish_as(
        &self,
        input: PublishApplicationPlugin,
        key: &str,
        actor: &str,
    ) -> Result<ApplicationRevision, AppError> {
        validate_operation(input.expected_revision, key)?;
        if let Some(base) = self
            .db
            .plugin_release_base_revision(&input.inventory_id)
            .await?
            && input.expected_revision != base
        {
            return Err(AppError::Conflict(
                "release installation was prepared against another runtime revision".into(),
            ));
        }
        let reason = if input.expected_revision == 0 {
            "initial"
        } else {
            "reload"
        };
        let hash = super::plugin_configuration_schema_digest(
            &json!({ "action": "publish", "inventory_id": input.inventory_id, "expected_revision": input.expected_revision }),
        )?;
        if let Some(replay) = self
            .db
            .replay_application_plugin_operation(key, &hash)
            .await?
        {
            return Ok(replay);
        }
        self.stage(&input.inventory_id).await?;
        self.db
            .publish_application_plugin(
                &input.inventory_id,
                input.expected_revision,
                reason,
                key,
                &hash,
                actor,
            )
            .await
    }

    pub async fn rollback(
        &self,
        input: RollbackApplicationPlugin,
        key: &str,
    ) -> Result<ApplicationRevision, AppError> {
        self.rollback_as(input, key, "host").await
    }

    pub async fn rollback_as(
        &self,
        input: RollbackApplicationPlugin,
        key: &str,
        actor: &str,
    ) -> Result<ApplicationRevision, AppError> {
        validate_operation(input.expected_revision, key)?;
        if input.target_revision <= 0 || input.target_revision >= input.expected_revision {
            return Err(AppError::BadRequest(
                "rollback target must be an earlier revision".into(),
            ));
        }
        let hash = super::plugin_configuration_schema_digest(
            &json!({ "action": "rollback", "target_revision": input.target_revision, "expected_revision": input.expected_revision }),
        )?;
        if let Some(replay) = self
            .db
            .replay_application_plugin_operation(key, &hash)
            .await?
        {
            return Ok(replay);
        }
        let target = self
            .db
            .application_plugin_revision(input.target_revision)
            .await?;
        let candidate = self
            .load(&target.inventory_id, target.revision, "rollback")
            .await?;
        validate_receipt(&candidate.receipt, &target)?;
        self.db
            .publish_application_plugin(
                &target.inventory_id,
                input.expected_revision,
                "rollback",
                key,
                &hash,
                actor,
            )
            .await
    }

    pub async fn pin(self: &Arc<Self>) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
        // The cache never supplies authority. Even a warm hit must read the
        // primary head and validate the exact immutable receipt on this request.
        let head = self.db.application_plugin_head().await?;
        self.pin_revision(head).await
    }

    /// Resume durable work using its authoritative historical receipt. Never
    /// substitute the current head when its original inventory is unavailable.
    pub async fn pin_historical(
        self: &Arc<Self>,
        revision: i64,
    ) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
        if revision <= 0 {
            return Err(AppError::BadRequest("invalid plugin revision".into()));
        }
        self.pin_revision(self.db.application_plugin_revision(revision).await?)
            .await
    }

    pub async fn pin_if_published(
        self: &Arc<Self>,
    ) -> Result<Option<Arc<ApplicationPluginSnapshot>>, AppError> {
        match self.db.optional_application_plugin_head().await? {
            Some(head) => self.pin_revision(head).await.map(Some),
            None => Ok(None),
        }
    }

    pub fn current_snapshot(&self) -> Result<Option<Arc<ApplicationPluginSnapshot>>, AppError> {
        let current = self
            .current
            .read()
            .map_err(|_| AppError::internal("application_current_snapshot_1"))?;
        if !current.initialized {
            return Err(AppError::capacity_error("plugin_snapshot_unavailable"));
        }
        Ok(current.snapshot.clone())
    }

    pub fn refresh_status(&self) -> serde_json::Value {
        let Ok(current) = self.current.read() else {
            return json!({"ready": false, "error": "snapshot_lock_failed"});
        };
        json!({
            "ready": current.initialized,
            "revision": current.snapshot.as_ref().map(|snapshot| &snapshot.receipt),
            "error": current.error,
        })
    }

    pub async fn refresh_current(self: &Arc<Self>) -> Result<(), AppError> {
        let _refresh = self.refresh.lock().await;
        let mut attempt = RefreshAttempt {
            current: &self.current,
            completed: false,
        };
        let result = tokio::time::timeout(PIN_DEADLINE, async {
            let head = self.db.optional_application_plugin_head().await?;
            {
                let current = self
                    .current
                    .read()
                    .map_err(|_| AppError::internal("application_refresh_current_1"))?;
                if head.is_none() && current.snapshot.is_some() {
                    return Err(AppError::Forbidden);
                }
            }
            if head.is_none() && !self.db.application_plugin_history(None).await?.is_empty() {
                return Err(AppError::Forbidden);
            }
            let snapshot = match head.as_ref() {
                Some(head) => Some(self.pin_revision(head.clone()).await?),
                None => None,
            };
            if self.db.optional_application_plugin_head().await? != head {
                return Err(AppError::Conflict(
                    "plugin head changed during refresh".into(),
                ));
            }
            let mut current = self
                .current
                .write()
                .map_err(|_| AppError::internal("application_refresh_current_2"))?;
            let previous = std::mem::replace(&mut current.snapshot, snapshot);
            current.initialized = true;
            current.error = None;
            drop(current);
            drop(previous);
            Ok(())
        })
        .await
        .unwrap_or(Err(AppError::capacity_error(
            "plugin_snapshot_refresh_timeout",
        )));
        if let Err(error) = &result {
            self.current
                .write()
                .map_err(|_| AppError::internal("application_refresh_current_3"))?
                .error = Some("snapshot_refresh_failed");
            tracing::error!(%error, "application plugin snapshot refresh failed");
        }
        attempt.completed = true;
        result
    }

    async fn pin_revision(
        self: &Arc<Self>,
        head: ApplicationRevision,
    ) -> Result<Arc<ApplicationPluginSnapshot>, AppError> {
        {
            let cache = self.snapshots.lock().await;
            if let Some(snapshot) = cache
                .snapshots
                .iter()
                .find(|snapshot| snapshot.receipt.revision == head.revision)
            {
                if snapshot.receipt != head {
                    return Err(AppError::Forbidden);
                }
                return Ok(snapshot.clone());
            }
        }
        self.refresh_inventory().await?;
        let entry = self
            .inventory
            .read()
            .await
            .get(&head.inventory_id)
            .cloned()
            .ok_or(AppError::Forbidden)?;
        #[cfg(test)]
        self.pin_root_inspections
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let metadata = tokio::fs::symlink_metadata(&entry.root)
            .await
            .map_err(|_| AppError::internal("application_pin_revision_1"))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(AppError::Forbidden);
        }
        tokio::time::timeout(PIN_DEADLINE, self.pin_head(head))
            .await
            .map_err(|_| AppError::capacity_error("plugin_pin_timeout"))?
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
                    if snapshot.receipt != head {
                        return Err(AppError::Forbidden);
                    }
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
                        .unwrap_or(Err(AppError::capacity_error("plugin_pin_timeout")))
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
                    return Err(AppError::internal("application_pin_head_1"));
                }
            };
            if flight.revision == head.revision {
                let snapshot = result.map_err(LoadFailure::into_error)?;
                if snapshot.receipt != head {
                    return Err(AppError::Forbidden);
                }
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
mod hot_snapshot_tests;
#[cfg(test)]
mod tests;
