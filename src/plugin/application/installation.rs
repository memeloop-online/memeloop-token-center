//! Global operator installation. HTTP supplies artifact references and an exact
//! review receipt, never filesystem paths, registry credentials or signing keys.
use super::*;
use std::{collections::BTreeSet, process::Stdio};
use tokio::io::AsyncReadExt;

const INSTALLER: &str = "/usr/local/bin/install-plugin-oci";
// A complete inventory can contain sixteen packages. Each package can contain
// 64 layers plus config/manifest and up to eight signature-key attempts. Bound
// each package, not the aggregate; committed packages survive interrupted work.
const PACKAGE_DEADLINE: Duration = Duration::from_secs(3 * 60 * 60);
const STORAGE_DEADLINE: Duration = Duration::from_secs(60);
const LEASE_RENEWAL: Duration = Duration::from_secs(30);
static INSTALL_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));
// An uninterruptible filesystem read retains this permit until it actually
// returns. Never share it with request pinning/staging compilation admission.
static INSTALL_STORAGE_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallPolicy {
    plugin_root: PathBuf,
    allowed_sources: BTreeSet<String>,
    cosign_public_keys: Vec<PathBuf>,
    #[serde(default)]
    source_credentials: BTreeMap<String, CredentialFiles>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFiles {
    registry_username_file: Option<PathBuf>,
    registry_password_file: Option<PathBuf>,
    registry_bearer_token_file: Option<PathBuf>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallPluginRequest {
    pub inventory_id: String,
    pub packages: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApproveInstallationRequest {
    pub review_digest: String,
}

#[derive(Serialize)]
pub struct InstallationRecord {
    pub id: String,
    pub inventory_id: String,
    pub actor: String,
    #[serde(skip)]
    pub(crate) attempt_id: String,
    #[serde(skip)]
    pub(crate) idempotency_hash: String,
    pub status: String,
    pub packages: Vec<String>,
    pub review_digest: Option<String>,
    pub review: Option<serde_json::Value>,
    pub failure_category: Option<String>,
    #[serde(skip)]
    pub lease_until: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_packages: usize,
    #[serde(skip)]
    pub(crate) checkpoints: BTreeMap<String, PackageCheckpoint>,
}

#[derive(Clone, Deserialize, Serialize)]
pub(crate) struct PackageCheckpoint {
    trust_digest: String,
    plugin_id: String,
    tree_digest: String,
}

#[derive(Serialize)]
pub struct PluginAuditEntry {
    pub id: String,
    pub actor: String,
    pub action: String,
    pub inventory_id: Option<String>,
    pub revision: Option<i64>,
    pub outcome: String,
    pub created_at: i64,
}

impl ApplicationPlugins {
    pub fn with_installation_policy_file(
        mut self,
        path: Option<PathBuf>,
    ) -> Result<Self, AppError> {
        if path.as_ref().is_some_and(|path| !path.is_absolute())
            || (path.is_some() && self.inventory_file.is_none())
        {
            return Err(AppError::Forbidden);
        }
        self.installation_policy_file = path;
        Ok(self)
    }

    pub fn installation_enabled(&self) -> bool {
        self.installation_policy_file.is_some()
    }

    async fn install_policy(&self) -> Result<InstallPolicy, AppError> {
        let path = self
            .installation_policy_file
            .as_ref()
            .ok_or(AppError::NotFound)?;
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| AppError::Internal)?;
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| AppError::Internal)?;
        if bytes.len() > 65536 {
            return Err(AppError::Forbidden);
        }
        let policy: InstallPolicy =
            serde_json::from_slice(&bytes).map_err(|_| AppError::Forbidden)?;
        if !policy.plugin_root.is_absolute()
            || policy.allowed_sources.is_empty()
            || policy.cosign_public_keys.is_empty()
            || policy.cosign_public_keys.len() > 8
            || policy
                .cosign_public_keys
                .iter()
                .any(|path| !path.is_absolute())
            || policy
                .source_credentials
                .iter()
                .any(|(source, credentials)| {
                    !policy.allowed_sources.contains(source) || !credentials.valid()
                })
        {
            return Err(AppError::Forbidden);
        }
        Ok(policy)
    }

    pub async fn install(
        self: &Arc<Self>,
        input: InstallPluginRequest,
        key: &str,
        actor: &str,
    ) -> Result<InstallationRecord, AppError> {
        self.install_internal(input, key, actor, None).await
    }

    pub async fn retry_installation(
        self: &Arc<Self>,
        id: &str,
        actor: &str,
    ) -> Result<InstallationRecord, AppError> {
        let record = self.db.plugin_installation(id).await?;
        if !matches!(record.status.as_str(), "failed" | "interrupted") {
            return Ok(record);
        }
        let result = self
            .install_internal(
                InstallPluginRequest {
                    inventory_id: record.inventory_id.clone(),
                    packages: record.packages,
                },
                "retry",
                &record.actor,
                Some(&record.idempotency_hash),
            )
            .await?;
        self.db
            .append_plugin_audit(
                actor,
                "retry",
                Some(&record.inventory_id),
                None,
                "started",
                &format!("retry:{}:{}", record.id, result.attempt_id),
            )
            .await?;
        Ok(result)
    }

    async fn install_internal(
        self: &Arc<Self>,
        input: InstallPluginRequest,
        key: &str,
        actor: &str,
        existing_hash: Option<&str>,
    ) -> Result<InstallationRecord, AppError> {
        validate_inventory_id(&input.inventory_id)?;
        validate_operation(0, key)?;
        if input.packages.is_empty() || input.packages.len() > 16 {
            return Err(AppError::BadRequest(
                "install one to sixteen packages per complete inventory".into(),
            ));
        }
        let mut unique = BTreeSet::new();
        for reference in &input.packages {
            let (source, digest) = reference.rsplit_once("@sha256:").ok_or_else(|| {
                AppError::BadRequest("digest-pinned OCI reference required".into())
            })?;
            if reference.len() > 2048
                || source.contains('@')
                || source.contains("://")
                || source.chars().any(char::is_whitespace)
                || digest.len() != 64
                || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
                || !unique.insert(reference)
            {
                return Err(AppError::BadRequest(
                    "invalid or duplicate OCI reference".into(),
                ));
            }
        }
        let hash = super::super::plugin_configuration_schema_digest(&json!(&input))?;
        let key_hash = match existing_hash {
            Some(hash) => hash.to_owned(),
            None => {
                super::super::plugin_configuration_schema_digest(&json!({"actor":actor,"key":key}))?
            }
        };
        if let Some(replay) = self
            .db
            .replay_plugin_installation(&key_hash, &hash, actor)
            .await?
        {
            return Ok(replay);
        }
        let policy = self.install_policy().await?;
        self.refresh_inventory().await?;
        if self
            .inventory
            .read()
            .await
            .contains_key(&input.inventory_id)
            || self
                .db
                .staged_application_plugin_ids()
                .await?
                .contains(&input.inventory_id)
        {
            return Err(AppError::Conflict(
                "an existing inventory cannot be modified by installation".into(),
            ));
        }
        if input.packages.iter().any(|reference| {
            reference
                .rsplit_once("@sha256:")
                .is_none_or(|(source, _)| !policy.allowed_sources.contains(source))
        }) {
            return Err(AppError::Forbidden);
        }
        let permit = INSTALL_PERMITS
            .clone()
            .try_acquire_owned()
            .map_err(|_| AppError::Overloaded)?;
        let (record, run) = self
            .db
            .begin_plugin_installation(
                &input.inventory_id,
                &json!(&input.packages),
                &hash,
                &key_hash,
                actor,
                crate::db::unix_millis() + 270_000,
            )
            .await?;
        if run {
            let authority = self.clone();
            let id = record.id.clone();
            let attempt_id = record.attempt_id.clone();
            tokio::spawn(async move {
                let _permit = permit;
                let attempt_deadline =
                    PACKAGE_DEADLINE * input.packages.len() as u32 + STORAGE_DEADLINE * 2;
                let work = bounded_install_phase(
                    attempt_deadline,
                    authority.perform_install(&id, &attempt_id, &input, &policy),
                );
                let result = authority
                    .with_installation_lease(&id, &attempt_id, work)
                    .await;
                let review = result.ok();
                let _ = tokio::time::timeout(
                    Duration::from_secs(10),
                    authority.db.finish_plugin_installation(
                        &id,
                        &attempt_id,
                        review
                            .as_ref()
                            .map(|(digest, value)| (digest.as_str(), value)),
                    ),
                )
                .await;
            });
        }
        Ok(record)
    }

    async fn perform_install(
        &self,
        operation_id: &str,
        attempt_id: &str,
        input: &InstallPluginRequest,
        policy: &InstallPolicy,
    ) -> Result<(String, serde_json::Value), AppError> {
        let inventory_root = policy.plugin_root.join(&input.inventory_id);
        let (trust, mut checkpoints) = bounded_install_phase(STORAGE_DEADLINE, async {
            let trust = trust_digest(policy).await?;
            tokio::fs::create_dir_all(&policy.plugin_root)
                .await
                .map_err(|_| AppError::Internal)?;
            let parent = tokio::fs::symlink_metadata(&policy.plugin_root)
                .await
                .map_err(|_| AppError::Internal)?;
            if !parent.is_dir() || parent.file_type().is_symlink() {
                return Err(AppError::Forbidden);
            }
            let claim_root = inventory_root.clone();
            let owner = operation_id.to_owned();
            let permit = tokio::time::timeout(
                ADMISSION_WAIT,
                INSTALL_STORAGE_PERMITS.clone().acquire_owned(),
            )
            .await
            .map_err(|_| AppError::Overloaded)?
            .map_err(|_| AppError::Internal)?;
            tokio::task::spawn_blocking(move || {
                let _permit = permit;
                claim_inventory_root(&claim_root, &owner)
            })
            .await
            .map_err(|_| AppError::Internal)??;
            let checkpoints = self.db.plugin_installation(operation_id).await?.checkpoints;
            Ok((trust, checkpoints))
        })
        .await?;
        for reference in &input.packages {
            bounded_install_phase(PACKAGE_DEADLINE, async {
                if let Some(checkpoint) = checkpoints.get(reference)
                    && checkpoint_matches(&inventory_root, reference, &trust, checkpoint).await?
                {
                    return Ok(());
                }
                let mut command = tokio::process::Command::new(INSTALLER);
                command
                    .arg(reference)
                    .arg("--plugin-dir")
                    .arg(&policy.plugin_root)
                    .arg("--inventory-id")
                    .arg(&input.inventory_id)
                    .env_clear()
                    // Only fixed runtime loader/search paths cross the process
                    // boundary; service tokens and unrelated host settings do not.
                    .env("LD_LIBRARY_PATH", "/usr/local/lib")
                    .env("PATH", "/usr/local/bin:/usr/bin:/bin")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                for source in &policy.allowed_sources {
                    command.arg("--allowed-source").arg(source);
                }
                for key in &policy.cosign_public_keys {
                    command.arg("--cosign-public-key").arg(key);
                }
                if let Some(credentials) = policy.credentials_for(reference) {
                    credentials.apply(&mut command);
                }
                if !command
                    .status()
                    .await
                    .map_err(|_| AppError::Internal)?
                    .success()
                {
                    return Err(AppError::Internal);
                }
                let checkpoint = package_checkpoint(&inventory_root, reference, &trust).await?;
                checkpoints.insert(reference.clone(), checkpoint);
                self.db
                    .checkpoint_plugin_installation(operation_id, attempt_id, &checkpoints)
                    .await?;
                Ok(())
            })
            .await?;
        }
        bounded_install_phase(STORAGE_DEADLINE, async {
            let runtime = self
                .review_runtime(policy.plugin_root.join(&input.inventory_id))
                .await?;
            let digest = review_digest(&runtime, &trust)?;
            let review = json!({"plugins":runtime.manifests()});
            if serde_json::to_vec(&review)
                .map_err(|_| AppError::Internal)?
                .len()
                > 4 * 1024 * 1024
            {
                return Err(AppError::Forbidden);
            }
            Ok((digest, review))
        })
        .await
    }

    async fn with_installation_lease<T>(
        &self,
        id: &str,
        attempt: &str,
        work: impl std::future::Future<Output = Result<T, AppError>>,
    ) -> Result<T, AppError> {
        run_with_lease(work, || self.db.renew_plugin_installation(id, attempt)).await
    }

    async fn review_runtime(&self, root: PathBuf) -> Result<PluginRuntime, AppError> {
        let db = self.db.clone();
        let permit = tokio::time::timeout(
            ADMISSION_WAIT,
            INSTALL_STORAGE_PERMITS.clone().acquire_owned(),
        )
        .await
        .map_err(|_| AppError::Overloaded)?
        .map_err(|_| AppError::Internal)?;
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            let metadata = std::fs::symlink_metadata(&root).map_err(|_| AppError::Internal)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AppError::Forbidden);
            }
            PluginRuntime::load(Some(root.to_str().ok_or(AppError::Forbidden)?), db)
        });
        tokio::time::timeout(COMPILATION_DEADLINE, task)
            .await
            .map_err(|_| AppError::Overloaded)?
            .map_err(|_| AppError::Internal)?
    }

    pub async fn approve_installation(
        &self,
        id: &str,
        digest: &str,
        actor: &str,
    ) -> Result<(), AppError> {
        let record = self.db.plugin_installation(id).await?;
        if !matches!(record.status.as_str(), "review" | "registered")
            || record.review_digest.as_deref() != Some(digest)
        {
            return Err(AppError::Conflict("installation review changed".into()));
        }
        if record.status == "registered" {
            return Ok(());
        }
        let policy = self.install_policy().await?;
        // Recheck source policy on approval; installing before a trust-policy
        // change does not grandfather an artifact into execution authority.
        if record.packages.iter().any(|reference| {
            reference
                .rsplit_once("@sha256:")
                .is_none_or(|(source, _)| !policy.allowed_sources.contains(source))
        }) {
            return Err(AppError::Forbidden);
        }
        let root = policy.plugin_root.join(&record.inventory_id);
        let runtime = self.review_runtime(root.clone()).await?;
        let trust = trust_digest(&policy).await?;
        if review_digest(&runtime, &trust)? != digest {
            return Err(AppError::Conflict(
                "installed bytes or signing trust changed after review; install a new inventory"
                    .into(),
            ));
        }
        for (reference, checkpoint) in &record.checkpoints {
            if !checkpoint_matches(&root, reference, &trust, checkpoint).await? {
                return Err(AppError::Conflict(
                    "installed package changed after verification".into(),
                ));
            }
        }
        runtime.validate_stored_configurations().await?;
        let identities = runtime.package_identities();
        let grants = runtime
            .manifests()
            .into_iter()
            .map(|manifest| {
                let identity = identities
                    .get(&manifest.id)
                    .cloned()
                    .ok_or(AppError::Internal)?;
                Ok((
                    manifest.id.clone(),
                    vec![PluginGrant {
                        version: manifest.version.clone(),
                        capabilities: manifest.capabilities.clone(),
                        manifest_digest: lifecycle::manifest_digest(&manifest)?,
                        identity,
                    }],
                ))
            })
            .collect::<Result<BTreeMap<_, _>, AppError>>()?;
        let entry = PreinstalledInventory { root, grants };
        let path = self.inventory_file.clone().ok_or(AppError::Forbidden)?;
        append_inventory_file(&path, &record.inventory_id, &entry).await?;
        self.stage(&record.inventory_id).await?;
        self.db
            .register_plugin_installation(id, digest, actor)
            .await
    }
}

async fn bounded_install_phase<T>(
    deadline: Duration,
    work: impl std::future::Future<Output = Result<T, AppError>>,
) -> Result<T, AppError> {
    tokio::time::timeout(deadline, work)
        .await
        .map_err(|_| AppError::Overloaded)?
}

async fn run_with_lease<T, R: std::future::Future<Output = Result<(), AppError>>>(
    work: impl std::future::Future<Output = Result<T, AppError>>,
    mut renew: impl FnMut() -> R,
) -> Result<T, AppError> {
    tokio::pin!(work);
    let mut renewal = tokio::time::interval(LEASE_RENEWAL);
    renewal.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            result = &mut work => return result,
            _ = renewal.tick() => {
                tokio::time::timeout(Duration::from_secs(10), renew())
                    .await.map_err(|_| AppError::Overloaded)??;
            }
        }
    }
}

impl CredentialFiles {
    fn valid(&self) -> bool {
        let basic = self.registry_username_file.is_some() && self.registry_password_file.is_some();
        let partial =
            self.registry_username_file.is_some() != self.registry_password_file.is_some();
        !(partial || basic && self.registry_bearer_token_file.is_some())
            && [
                &self.registry_username_file,
                &self.registry_password_file,
                &self.registry_bearer_token_file,
            ]
            .iter()
            .all(|path| path.as_ref().is_none_or(|path| path.is_absolute()))
    }

    fn apply(&self, command: &mut tokio::process::Command) {
        for (flag, path) in [
            ("--registry-username-file", &self.registry_username_file),
            ("--registry-password-file", &self.registry_password_file),
            (
                "--registry-bearer-token-file",
                &self.registry_bearer_token_file,
            ),
        ] {
            if let Some(path) = path {
                command.arg(flag).arg(path);
            }
        }
    }
}

impl InstallPolicy {
    fn credentials_for(&self, reference: &str) -> Option<&CredentialFiles> {
        let (source, _) = reference.rsplit_once("@sha256:")?;
        self.source_credentials.get(source)
    }
}

// The final name never exists without a complete, durable owner receipt. A
// killed creator leaves only an unreferenced hidden temporary directory; retries
// can claim the final name without deleting or adopting arbitrary existing roots.
#[cfg(target_os = "linux")]
fn claim_inventory_root(root: &std::path::Path, owner: &str) -> Result<(), AppError> {
    use rustix::fs::{Mode, OFlags, RenameFlags, open, renameat_with};
    use std::io::{Read, Write};
    let parent = root.parent().ok_or(AppError::Forbidden)?;
    let name = root.file_name().ok_or(AppError::Forbidden)?;
    let temporary = parent.join(format!(".mtc-inventory-claim-{}", uuid::Uuid::now_v7()));
    std::fs::create_dir(&temporary).map_err(|_| AppError::Internal)?;
    let marker = temporary.join(".mtc-install-owner");
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&marker)
            .map_err(|_| AppError::Internal)?;
        file.write_all(owner.as_bytes())
            .map_err(|_| AppError::Internal)?;
        file.sync_all().map_err(|_| AppError::Internal)?;
        std::fs::File::open(&temporary)
            .and_then(|file| file.sync_all())
            .map_err(|_| AppError::Internal)?;
        let directory = open(
            parent,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|_| AppError::Internal)?;
        match renameat_with(
            &directory,
            temporary.file_name().ok_or(AppError::Internal)?,
            &directory,
            name,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::EXIST => {
                let metadata = std::fs::symlink_metadata(root).map_err(|_| AppError::Forbidden)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(AppError::Forbidden);
                }
                let existing_marker = root.join(".mtc-install-owner");
                let metadata =
                    std::fs::symlink_metadata(&existing_marker).map_err(|_| AppError::Forbidden)?;
                if !metadata.is_file() || metadata.file_type().is_symlink() {
                    return Err(AppError::Forbidden);
                }
                let mut existing = Vec::new();
                std::fs::File::open(existing_marker)
                    .map_err(|_| AppError::Forbidden)?
                    .take(65)
                    .read_to_end(&mut existing)
                    .map_err(|_| AppError::Internal)?;
                if existing != owner.as_bytes() {
                    return Err(AppError::Forbidden);
                }
            }
            Err(_) => return Err(AppError::Internal),
        }
        std::fs::File::open(parent)
            .and_then(|file| file.sync_all())
            .map_err(|_| AppError::Internal)
    })();
    // Only files created by this invocation, never an existing inventory.
    let _ = std::fs::remove_file(marker);
    let _ = std::fs::remove_dir(temporary);
    result
}

#[cfg(not(target_os = "linux"))]
fn claim_inventory_root(_root: &std::path::Path, _owner: &str) -> Result<(), AppError> {
    Err(AppError::Forbidden)
}

async fn package_checkpoint(
    root: &std::path::Path,
    reference: &str,
    trust: &str,
) -> Result<PackageCheckpoint, AppError> {
    let root = root.to_owned();
    let reference = reference.to_owned();
    let trust = trust.to_owned();
    let permit = tokio::time::timeout(
        ADMISSION_WAIT,
        INSTALL_STORAGE_PERMITS.clone().acquire_owned(),
    )
    .await
    .map_err(|_| AppError::Overloaded)?
    .map_err(|_| AppError::Internal)?;
    let task = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        use std::io::Read;
        let (source, digest) = reference.rsplit_once('@').ok_or(AppError::Forbidden)?;
        let mut found = None;
        let mut count = 0;
        for entry in std::fs::read_dir(&root).map_err(|_| AppError::Internal)? {
            let entry = entry.map_err(|_| AppError::Internal)?;
            if entry.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            count += 1;
            if count > 16 {
                return Err(AppError::Forbidden);
            }
            let path = entry.path();
            let metadata = std::fs::symlink_metadata(&path).map_err(|_| AppError::Internal)?;
            if !metadata.is_dir() || metadata.file_type().is_symlink() {
                return Err(AppError::Forbidden);
            }
            let mut receipt = Vec::new();
            std::fs::File::open(path.join(".mtc-oci-install.json"))
                .map_err(|_| AppError::Forbidden)?
                .take(16385)
                .read_to_end(&mut receipt)
                .map_err(|_| AppError::Internal)?;
            if receipt.len() > 16384 {
                return Err(AppError::Forbidden);
            }
            let receipt: super::super::PluginInstallProvenance =
                serde_json::from_slice(&receipt).map_err(|_| AppError::Forbidden)?;
            if receipt.source != source
                || receipt.digest != digest
                || receipt.signature_policy != "cosign-public-key"
            {
                continue;
            }
            let manifest = super::super::validate_plugin_package(&path)?;
            if entry.file_name() != std::ffi::OsStr::new(&manifest.id) || found.is_some() {
                return Err(AppError::Forbidden);
            }
            found = Some(PackageCheckpoint {
                trust_digest: trust.clone(),
                plugin_id: manifest.id,
                tree_digest: package_tree_digest(&path)?,
            });
        }
        found.ok_or(AppError::Forbidden)
    });
    tokio::time::timeout(STORAGE_DEADLINE, task)
        .await
        .map_err(|_| AppError::Overloaded)?
        .map_err(|_| AppError::Internal)?
}

async fn checkpoint_matches(
    root: &std::path::Path,
    reference: &str,
    trust: &str,
    checkpoint: &PackageCheckpoint,
) -> Result<bool, AppError> {
    if checkpoint.trust_digest != trust {
        return Ok(false);
    }
    let actual = package_checkpoint(root, reference, trust).await?;
    Ok(actual.plugin_id == checkpoint.plugin_id && actual.tree_digest == checkpoint.tree_digest)
}

fn package_tree_digest(root: &std::path::Path) -> Result<String, AppError> {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let mut pending = vec![PathBuf::new()];
    let mut files = BTreeMap::new();
    let mut entries = 0;
    let mut total = 0;
    while let Some(relative) = pending.pop() {
        let path = root.join(&relative);
        let metadata = std::fs::symlink_metadata(&path).map_err(|_| AppError::Internal)?;
        if metadata.file_type().is_symlink() || relative.as_os_str().len() > 240 {
            return Err(AppError::Forbidden);
        }
        if metadata.is_dir() {
            for entry in std::fs::read_dir(path).map_err(|_| AppError::Internal)? {
                let entry = entry.map_err(|_| AppError::Internal)?;
                entries += 1;
                if entries > 64 * 240 {
                    return Err(AppError::Forbidden);
                }
                pending.push(relative.join(entry.file_name()));
            }
        } else if metadata.is_file() {
            if files.len() >= 65 || metadata.len() > 64 * 1024 * 1024 {
                return Err(AppError::Forbidden);
            }
            let mut file = std::fs::File::open(path)
                .map_err(|_| AppError::Internal)?
                .take(64 * 1024 * 1024 + 1);
            let mut hash = Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let count = file.read(&mut buffer).map_err(|_| AppError::Internal)?;
                if count == 0 {
                    break;
                }
                total += count;
                if total > 80 * 1024 * 1024 + 16384 {
                    return Err(AppError::Forbidden);
                }
                hash.update(&buffer[..count]);
            }
            files.insert(relative, format!("{:x}", hash.finalize()));
        } else {
            return Err(AppError::Forbidden);
        }
    }
    super::super::plugin_configuration_schema_digest(&json!(files))
}

fn review_digest(runtime: &PluginRuntime, trust: &str) -> Result<String, AppError> {
    super::super::plugin_configuration_schema_digest(
        &json!({"manifests":runtime.manifests(),"identities":runtime.package_identities(),"trust":trust}),
    )
}

async fn trust_digest(policy: &InstallPolicy) -> Result<String, AppError> {
    let mut keys = Vec::new();
    for path in &policy.cosign_public_keys {
        let file = tokio::fs::File::open(path)
            .await
            .map_err(|_| AppError::Internal)?;
        let mut bytes = Vec::new();
        file.take(65537)
            .read_to_end(&mut bytes)
            .await
            .map_err(|_| AppError::Internal)?;
        if bytes.is_empty() || bytes.len() > 65536 {
            return Err(AppError::Forbidden);
        }
        keys.push(bytes);
    }
    super::super::plugin_configuration_schema_digest(
        &json!({"sources":policy.allowed_sources,"keys":keys}),
    )
}

async fn append_inventory_file(
    path: &std::path::Path,
    id: &str,
    entry: &PreinstalledInventory,
) -> Result<(), AppError> {
    let path = path.to_owned();
    let id = id.to_owned();
    let entry = entry.clone();
    tokio::task::spawn_blocking(move || {
        use std::io::{Read, Write};
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path.with_extension("lock"))
            .map_err(|_| AppError::Internal)?;
        lock.try_lock().map_err(|_| AppError::Overloaded)?;
        let mut bytes = Vec::new();
        std::fs::File::open(&path)
            .map_err(|_| AppError::Internal)?
            .take(4 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| AppError::Internal)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(AppError::Forbidden);
        }
        let mut inventory: BTreeMap<String, PreinstalledInventory> =
            serde_json::from_slice(&bytes).map_err(|_| AppError::Forbidden)?;
        if let Some(existing) = inventory.get(&id) {
            if serde_json::to_value(existing).map_err(|_| AppError::Internal)?
                == serde_json::to_value(&entry).map_err(|_| AppError::Internal)?
            {
                return Ok(());
            }
            return Err(AppError::Conflict("inventory ID is immutable".into()));
        }
        inventory.insert(id, entry);
        let bytes = serde_json::to_vec(&inventory).map_err(|_| AppError::Internal)?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(AppError::Forbidden);
        }
        let parent = path.parent().ok_or(AppError::Forbidden)?;
        let temporary = parent.join(format!(".mtc-inventory-{}", uuid::Uuid::now_v7()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|_| AppError::Internal)?;
        let result = (|| {
            file.set_permissions(
                std::fs::metadata(&path)
                    .map_err(|_| AppError::Internal)?
                    .permissions(),
            )
            .map_err(|_| AppError::Internal)?;
            file.write_all(&bytes).map_err(|_| AppError::Internal)?;
            file.sync_all().map_err(|_| AppError::Internal)?;
            std::fs::rename(&temporary, &path).map_err(|_| AppError::Internal)?;
            std::fs::File::open(parent)
                .and_then(|file| file.sync_all())
                .map_err(|_| AppError::Internal)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result
    })
    .await
    .map_err(|_| AppError::Internal)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        AppState,
        config::{Config, RuntimeRole},
    };
    use axum::{
        body::Body,
        http::{Request, StatusCode, header},
    };
    use tower::ServiceExt;

    async fn fixture() -> (tempfile::TempDir, AppState, InstallationRecord, String) {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("private-install-root");
        let baseline = root.join("baseline");
        std::fs::create_dir_all(&baseline).unwrap();
        let inventory = directory.path().join("inventory.json");
        std::fs::write(
            &inventory,
            serde_json::to_vec(&json!({"baseline":{"root":baseline,"grants":{}}})).unwrap(),
        )
        .unwrap();
        let key = directory.path().join("private-signing-key.pem");
        std::fs::write(&key, b"public-key-fixture-must-not-appear-in-api").unwrap();
        let policy_path = directory.path().join("policy.json");
        std::fs::write(&policy_path,serde_json::to_vec(&json!({"plugin_root":root,"allowed_sources":["ghcr.io/example/new"],"cosign_public_keys":[key]})).unwrap()).unwrap();
        let mut config = Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("test.db").display()
        ));
        config.plugin_inventory_file = Some(inventory.to_str().unwrap().into());
        config.plugin_install_policy_file = Some(policy_path.to_str().unwrap().into());
        let state = AppState::initialize(config).await.unwrap();
        let authority = state.application_plugins.as_ref().unwrap();
        authority
            .publish(
                PublishApplicationPlugin {
                    inventory_id: "baseline".into(),
                    expected_revision: 0,
                },
                "baseline",
            )
            .await
            .unwrap();
        // The existing distribution tests exercise real signature verification
        // and mock-registry installation. This fixture seeds its verified output
        // to exercise the independent review/approval/publication HTTP boundary.
        let package = root.join("installed/new-plugin");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("plugin.json"),serde_json::to_vec(&json!({"id":"new-plugin","version":"1.0.0","wit_version":"0.2.0","wasm":null,"capabilities":[],"contributions":{}})).unwrap()).unwrap();
        let reference = format!("ghcr.io/example/new@sha256:{}", "a".repeat(64));
        std::fs::write(package.join(".mtc-oci-install.json"),serde_json::to_vec(&json!({"format_version":1,"source":"ghcr.io/example/new","digest":format!("sha256:{}","a".repeat(64)),"signature_policy":"cosign-public-key"})).unwrap()).unwrap();
        let (record, run) = state
            .db
            .begin_plugin_installation(
                "installed",
                &json!([reference]),
                "request-hash",
                "idempotency-hash",
                "bootstrap",
                crate::db::unix_millis() + 270000,
            )
            .await
            .unwrap();
        assert!(run);
        let runtime = authority
            .review_runtime(root.join("installed"))
            .await
            .unwrap();
        let policy = authority.install_policy().await.unwrap();
        let digest = review_digest(&runtime, &trust_digest(&policy).await.unwrap()).unwrap();
        state
            .db
            .finish_plugin_installation(
                &record.id,
                &record.attempt_id,
                Some((&digest, &json!({"plugins":runtime.manifests()}))),
            )
            .await
            .unwrap();
        (directory, state, record, digest)
    }

    async fn call(
        state: &AppState,
        method: &str,
        path: &str,
        token: &str,
        body: serde_json::Value,
        key: &str,
    ) -> (StatusCode, serde_json::Value) {
        let response = crate::api::router_for_role(state.clone(), RuntimeRole::Control)
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {token}"))
                    .header(header::CONTENT_TYPE, "application/json")
                    .header("idempotency-key", key)
                    .body(if method == "GET" {
                        Body::empty()
                    } else {
                        Body::from(serde_json::to_vec(&body).unwrap())
                    })
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 8 * 1024 * 1024)
            .await
            .unwrap();
        (
            status,
            if bytes.is_empty() {
                json!({})
            } else {
                serde_json::from_slice(&bytes).unwrap()
            },
        )
    }

    #[tokio::test]
    async fn review_approval_publish_rollback_and_audit_are_real_control_operations() {
        let (_directory, state, record, digest) = fixture().await;
        let token = &state.config.service_token;
        let (status, history) = call(
            &state,
            "GET",
            "/internal/v1/plugin-runtime/history",
            token,
            json!({}),
            "read",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(history["installations"][0]["status"], "review");
        for secret in [
            "private-install-root",
            "private-signing-key",
            "public-key-fixture",
        ] {
            assert!(!history.to_string().contains(secret));
        }
        let approval = format!(
            "/internal/v1/plugin-runtime/installations/{}/approve",
            record.id
        );
        assert_eq!(
            call(
                &state,
                "POST",
                &approval,
                token,
                json!({"review_digest":"stale"}),
                "approve"
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            call(
                &state,
                "POST",
                &approval,
                token,
                json!({"review_digest":digest}),
                "approve"
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            call(
                &state,
                "POST",
                &approval,
                token,
                json!({"review_digest":digest}),
                "approve"
            )
            .await
            .0,
            StatusCode::NO_CONTENT
        );
        let (status, published) = call(
            &state,
            "POST",
            "/internal/v1/plugin-runtime/publish",
            token,
            json!({"inventory_id":"installed","expected_revision":1}),
            "publish",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{published}");
        assert_eq!(published["revision"], 2);
        let (status, manifests) = call(
            &state,
            "GET",
            "/internal/v1/plugins",
            token,
            json!({}),
            "read",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(manifests[0]["id"], "new-plugin");
        assert_eq!(
            call(
                &state,
                "POST",
                "/internal/v1/plugin-runtime/rollback",
                token,
                json!({"target_revision":1,"expected_revision":2}),
                "rollback"
            )
            .await
            .0,
            StatusCode::OK
        );
        let (_, history) = call(
            &state,
            "GET",
            "/internal/v1/plugin-runtime/history",
            token,
            json!({}),
            "read",
        )
        .await;
        assert_eq!(history["revisions"].as_array().unwrap().len(), 3);
        assert_eq!(history["installations"][0]["status"], "registered");
        let audit = history["audit"].as_array().unwrap();
        assert!(
            audit
                .iter()
                .any(|entry| entry["action"] == "publish" && entry["actor"] == "bootstrap")
        );
        assert!(
            audit
                .iter()
                .any(|entry| entry["action"] == "rollback" && entry["revision"] == 3)
        );
        assert_eq!(
            audit
                .iter()
                .filter(|entry| entry["action"] == "approve")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn unconfigured_runtime_has_readable_disabled_status_but_no_mutation_authority() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("disabled.db").display()
        )))
        .await
        .unwrap();
        assert!(state.application_plugins.is_none());
        let token = &state.config.service_token;
        let (status, value) = call(
            &state,
            "GET",
            "/internal/v1/plugin-runtime",
            token,
            json!({}),
            "status",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value, json!({"current":null,"candidates":[]}));
        let (status, history) = call(
            &state,
            "GET",
            "/internal/v1/plugin-runtime/history",
            token,
            json!({}),
            "history",
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(history["runtime_enabled"], false);
        assert_eq!(history["installation_enabled"], false);
        assert_eq!(
            call(
                &state,
                "POST",
                "/internal/v1/plugin-runtime/publish",
                token,
                json!({"inventory_id":"missing","expected_revision":0}),
                "publish"
            )
            .await
            .0,
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &state,
                "GET",
                "/internal/v1/plugin-runtime/history",
                "invalid",
                json!({}),
                "history"
            )
            .await
            .0,
            StatusCode::UNAUTHORIZED
        );
    }

    #[tokio::test]
    async fn source_authority_and_review_digest_cannot_be_bypassed() {
        let (directory, state, record, digest) = fixture().await;
        state.db.create_tenant("scoped", None).await.unwrap();
        let scoped = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: "tenant-installer".into(),
                    scopes: vec!["plugins:read".into(), "plugins:write".into()],
                    tenant_external_id: Some("scoped".into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(
            call(
                &state,
                "GET",
                "/internal/v1/plugin-runtime/history",
                &scoped.token,
                json!({}),
                "read"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let input = json!({"inventory_id":"new","packages":[format!("denied.example/repo@sha256:{}","b".repeat(64))]});
        assert_eq!(
            call(
                &state,
                "POST",
                "/internal/v1/plugin-runtime/installations",
                &scoped.token,
                input.clone(),
                "install"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(
                &state,
                "POST",
                "/internal/v1/plugin-runtime/installations",
                &state.config.service_token,
                input,
                "install"
            )
            .await
            .0,
            StatusCode::FORBIDDEN
        );
        let forged = json!({"inventory_id":"new","packages":[],"root":"/untrusted/path"});
        assert!(
            call(
                &state,
                "POST",
                "/internal/v1/plugin-runtime/installations",
                &state.config.service_token,
                forged,
                "install"
            )
            .await
            .0
            .is_client_error()
        );
        std::fs::write(
            directory.path().join("private-signing-key.pem"),
            b"rotated-trust",
        )
        .unwrap();
        assert_eq!(
            call(
                &state,
                "POST",
                &format!(
                    "/internal/v1/plugin-runtime/installations/{}/approve",
                    record.id
                ),
                &state.config.service_token,
                json!({"review_digest":digest}),
                "approve"
            )
            .await
            .0,
            StatusCode::CONFLICT
        );
        assert_eq!(
            state
                .application_plugins
                .as_ref()
                .unwrap()
                .status()
                .await
                .unwrap()
                .candidates
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn expired_install_attempt_cannot_finish_a_newer_retry() {
        let (_directory, state, _, _) = fixture().await;
        let packages = json!([format!("ghcr.io/example/new@sha256:{}", "b".repeat(64))]);
        let (old, _) = state
            .db
            .begin_plugin_installation(
                "retry",
                &packages,
                "hash",
                "key",
                "bootstrap",
                crate::db::unix_millis() - 1,
            )
            .await
            .unwrap();
        let (new, run) = state
            .db
            .begin_plugin_installation(
                "retry",
                &packages,
                "hash",
                "key",
                "bootstrap",
                crate::db::unix_millis() + 270000,
            )
            .await
            .unwrap();
        assert!(run);
        assert_eq!(old.id, new.id);
        assert_ne!(old.attempt_id, new.attempt_id);
        assert!(
            state
                .db
                .renew_plugin_installation(&old.id, &old.attempt_id)
                .await
                .is_err()
        );
        assert!(
            state
                .db
                .checkpoint_plugin_installation(&old.id, &old.attempt_id, &BTreeMap::new())
                .await
                .is_err()
        );
        state
            .db
            .renew_plugin_installation(&new.id, &new.attempt_id)
            .await
            .unwrap();
        assert!(
            state
                .db
                .finish_plugin_installation(&old.id, &old.attempt_id, None)
                .await
                .is_err()
        );
        assert_eq!(
            state.db.plugin_installation(&new.id).await.unwrap().status,
            "installing"
        );
        state
            .db
            .finish_plugin_installation(&new.id, &new.attempt_id, None)
            .await
            .unwrap();
        assert_eq!(
            state.db.plugin_installation(&new.id).await.unwrap().status,
            "failed"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn interrupted_private_claims_never_poison_final_inventory_name() {
        let directory = tempfile::tempdir().unwrap();
        // Simulate SIGKILL before marker creation, midway through writing it,
        // and after marker completion but before the atomic rename.
        for (index, contents) in [None, Some(""), Some("own"), Some("owner")]
            .into_iter()
            .enumerate()
        {
            let temporary = directory
                .path()
                .join(format!(".mtc-inventory-claim-crashed-{index}"));
            std::fs::create_dir(&temporary).unwrap();
            if let Some(contents) = contents {
                std::fs::write(temporary.join(".mtc-install-owner"), contents).unwrap();
            }
        }
        let root = directory.path().join("inventory");
        claim_inventory_root(&root, "owner").unwrap();
        assert_eq!(
            std::fs::read(root.join(".mtc-install-owner")).unwrap(),
            b"owner"
        );
        claim_inventory_root(&root, "owner").unwrap();
        assert!(claim_inventory_root(&root, "another-owner").is_err());
        // An arbitrary preexisting empty directory still must not be adopted.
        let unrelated = directory.path().join("unrelated");
        std::fs::create_dir(&unrelated).unwrap();
        assert!(claim_inventory_root(&unrelated, "owner").is_err());
    }

    #[test]
    fn registry_credentials_are_exact_source_scoped_and_unmapped_is_anonymous() {
        let policy: InstallPolicy = serde_json::from_value(json!({
            "plugin_root":"/var/lib/plugins", "cosign_public_keys":["/run/trust/pub.pem"],
            "allowed_sources":["private.example/team","vendor.example/plugin","private.example/other"],
            "source_credentials":{"private.example/team":{"registry_username_file":"/run/private/user","registry_password_file":"/run/private/password"}}
        })).unwrap();
        for (source, expected) in [
            ("private.example/team", 4),
            ("vendor.example/plugin", 0),
            ("private.example/other", 0),
        ] {
            let mut command = tokio::process::Command::new(INSTALLER);
            if let Some(credentials) =
                policy.credentials_for(&format!("{source}@sha256:{}", "a".repeat(64)))
            {
                assert!(credentials.valid());
                credentials.apply(&mut command);
            }
            assert_eq!(command.as_std().get_args().count(), expected, "{source}");
        }
        assert!(
            serde_json::from_value::<InstallPolicy>(json!({
                "plugin_root":"/var/lib/plugins", "cosign_public_keys":["/run/trust/pub.pem"],
                "allowed_sources":["private.example/team","vendor.example/plugin"],
                "registry_bearer_token_file":"/run/private/token"
            }))
            .is_err()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn legal_slow_packages_progress_beyond_old_aggregate_deadline() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let renewals = AtomicUsize::new(0);
        let complete = AtomicUsize::new(0);
        let work = async {
            for _ in 0..16 {
                // Multiple legal sub-60s operations per package; total far
                // exceeds 240s without ever exceeding the per-package bound.
                tokio::time::timeout(PACKAGE_DEADLINE, async {
                    for _ in 0..6 {
                        tokio::time::sleep(Duration::from_secs(50)).await;
                    }
                })
                .await
                .unwrap();
                complete.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        };
        run_with_lease(work, || {
            renewals.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        })
        .await
        .unwrap();
        assert_eq!(complete.load(Ordering::SeqCst), 16);
        assert!(renewals.load(Ordering::SeqCst) >= 160);
        let failed = run_with_lease(
            async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(())
            },
            || std::future::ready(Err(AppError::Forbidden)),
        )
        .await;
        assert!(failed.is_err(), "lost ownership must cancel work");
    }

    #[tokio::test]
    async fn durable_checkpoint_skips_only_exact_bytes_under_unchanged_trust() {
        let (directory, state, record, _) = fixture().await;
        let root = directory.path().join("private-install-root/installed");
        let reference = &record.packages[0];
        let checkpoint = package_checkpoint(&root, reference, "trust").await.unwrap();
        assert!(
            checkpoint_matches(&root, reference, "trust", &checkpoint)
                .await
                .unwrap()
        );
        assert!(
            !checkpoint_matches(&root, reference, "rotated", &checkpoint)
                .await
                .unwrap()
        );
        // Include non-Wasm assets; a manifest/provenance-only hash is not enough.
        std::fs::write(root.join("new-plugin/asset.txt"), b"changed bytes").unwrap();
        assert!(
            !checkpoint_matches(&root, reference, "trust", &checkpoint)
                .await
                .unwrap()
        );
        let (active, _) = state
            .db
            .begin_plugin_installation(
                "progress",
                &json!([reference]),
                "hash",
                "progress-key",
                "bootstrap",
                crate::db::unix_millis() + 270000,
            )
            .await
            .unwrap();
        let checkpoints = BTreeMap::from([(reference.clone(), checkpoint)]);
        state
            .db
            .checkpoint_plugin_installation(&active.id, &active.attempt_id, &checkpoints)
            .await
            .unwrap();
        state
            .db
            .finish_plugin_installation(&active.id, &active.attempt_id, None)
            .await
            .unwrap();
        let (retry, run) = state
            .db
            .begin_plugin_installation(
                "progress",
                &json!([reference]),
                "hash",
                "progress-key",
                "bootstrap",
                crate::db::unix_millis() + 270000,
            )
            .await
            .unwrap();
        assert!(run);
        assert_eq!(retry.completed_packages, 1);
        assert_eq!(
            retry.checkpoints[reference].tree_digest,
            checkpoints[reference].tree_digest
        );
        assert_ne!(active.attempt_id, retry.attempt_id);
    }

    #[tokio::test(start_paused = true)]
    async fn pending_checkpoint_is_bounded_and_stops_lease_renewal() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let renewals = AtomicUsize::new(0);
        // This represents a successful installer followed by a checkpoint
        // read/DB write that never resolves, not a pending subprocess.
        let package = bounded_install_phase(PACKAGE_DEADLINE, async {
            std::future::ready(Ok::<(), AppError>(())).await?;
            std::future::pending::<Result<(), AppError>>().await
        });
        let result = run_with_lease(package, || {
            renewals.fetch_add(1, Ordering::SeqCst);
            std::future::ready(Ok(()))
        })
        .await;
        assert!(result.is_err());
        let stopped = renewals.load(Ordering::SeqCst);
        assert!(stopped > 1);
        tokio::time::advance(Duration::from_secs(540)).await;
        assert_eq!(renewals.load(Ordering::SeqCst), stopped);
        assert!(
            bounded_install_phase(
                STORAGE_DEADLINE,
                std::future::pending::<Result<(), AppError>>()
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn timed_out_storage_work_releases_database_installation_claim() {
        let (_directory, state, _, _) = fixture().await;
        let packages = json!([format!("ghcr.io/example/new@sha256:{}", "c".repeat(64))]);
        let (old, _) = state
            .db
            .begin_plugin_installation(
                "hung",
                &packages,
                "hash",
                "hung-key",
                "bootstrap",
                crate::db::unix_millis() + 270000,
            )
            .await
            .unwrap();
        let authority = state.application_plugins.as_ref().unwrap();
        let result = authority
            .with_installation_lease(
                &old.id,
                &old.attempt_id,
                bounded_install_phase(
                    Duration::from_millis(1),
                    std::future::pending::<Result<(), AppError>>(),
                ),
            )
            .await;
        assert!(result.is_err());
        // The production task uses this same terminal operation after the
        // bounded work returns, without any renewal running alongside it.
        state
            .db
            .finish_plugin_installation(&old.id, &old.attempt_id, None)
            .await
            .unwrap();
        let (_, run) = state
            .db
            .begin_plugin_installation(
                "next",
                &packages,
                "next-hash",
                "next-key",
                "bootstrap",
                crate::db::unix_millis() + 270000,
            )
            .await
            .unwrap();
        assert!(
            run,
            "another installation must be able to acquire the global lock"
        );
        assert!(!Arc::ptr_eq(&INSTALL_STORAGE_PERMITS, &COMPILATION_PERMITS));
    }
}
