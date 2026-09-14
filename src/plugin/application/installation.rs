//! Global operator installation. HTTP supplies artifact references and an exact
//! review receipt, never filesystem paths, registry credentials or signing keys.
use super::*;
use std::{collections::BTreeSet, process::Stdio};
use tokio::io::AsyncReadExt;

const INSTALLER: &str = "/usr/local/bin/install-plugin-oci";
const INSTALL_DEADLINE: Duration = Duration::from_secs(240);
static INSTALL_PERMITS: LazyLock<Arc<tokio::sync::Semaphore>> =
    LazyLock::new(|| Arc::new(tokio::sync::Semaphore::new(1)));

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallPolicy {
    plugin_root: PathBuf,
    allowed_sources: BTreeSet<String>,
    cosign_public_keys: Vec<PathBuf>,
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
            || [
                &policy.registry_username_file,
                &policy.registry_password_file,
                &policy.registry_bearer_token_file,
            ]
            .iter()
            .any(|path| path.as_ref().is_some_and(|path| !path.is_absolute()))
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
                let result = tokio::time::timeout(
                    INSTALL_DEADLINE,
                    authority.perform_install(&id, &input, &policy),
                )
                .await;
                let review = match result {
                    Ok(Ok(review)) => Some(review),
                    _ => None,
                };
                let _ = authority
                    .db
                    .finish_plugin_installation(
                        &id,
                        &attempt_id,
                        review
                            .as_ref()
                            .map(|(digest, value)| (digest.as_str(), value)),
                    )
                    .await;
            });
        }
        Ok(record)
    }

    async fn perform_install(
        &self,
        operation_id: &str,
        input: &InstallPluginRequest,
        policy: &InstallPolicy,
    ) -> Result<(String, serde_json::Value), AppError> {
        let trust = trust_digest(policy).await?;
        let inventory_root = policy.plugin_root.join(&input.inventory_id);
        tokio::fs::create_dir_all(&policy.plugin_root)
            .await
            .map_err(|_| AppError::Internal)?;
        let parent = tokio::fs::symlink_metadata(&policy.plugin_root)
            .await
            .map_err(|_| AppError::Internal)?;
        if !parent.is_dir() || parent.file_type().is_symlink() {
            return Err(AppError::Forbidden);
        }
        let marker = inventory_root.join(".mtc-install-owner");
        match tokio::fs::create_dir(&inventory_root).await {
            Ok(()) => {
                use tokio::io::AsyncWriteExt;
                let mut file = tokio::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&marker)
                    .await
                    .map_err(|_| AppError::Internal)?;
                file.write_all(operation_id.as_bytes())
                    .await
                    .map_err(|_| AppError::Internal)?;
                file.sync_all().await.map_err(|_| AppError::Internal)?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let metadata = tokio::fs::symlink_metadata(&inventory_root)
                    .await
                    .map_err(|_| AppError::Internal)?;
                if !metadata.is_dir() || metadata.file_type().is_symlink() {
                    return Err(AppError::Forbidden);
                }
                let file = tokio::fs::File::open(&marker)
                    .await
                    .map_err(|_| AppError::Forbidden)?;
                let mut owner = Vec::new();
                file.take(65)
                    .read_to_end(&mut owner)
                    .await
                    .map_err(|_| AppError::Internal)?;
                if owner != operation_id.as_bytes() {
                    return Err(AppError::Forbidden);
                }
            }
            Err(_) => return Err(AppError::Internal),
        }
        for reference in &input.packages {
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
            for (flag, path) in [
                ("--registry-username-file", &policy.registry_username_file),
                ("--registry-password-file", &policy.registry_password_file),
                (
                    "--registry-bearer-token-file",
                    &policy.registry_bearer_token_file,
                ),
            ] {
                if let Some(path) = path {
                    command.arg(flag).arg(path);
                }
            }
            if !command
                .status()
                .await
                .map_err(|_| AppError::Internal)?
                .success()
            {
                return Err(AppError::Internal);
            }
        }
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
    }

    async fn review_runtime(&self, root: PathBuf) -> Result<PluginRuntime, AppError> {
        let db = self.db.clone();
        let permit =
            tokio::time::timeout(ADMISSION_WAIT, COMPILATION_PERMITS.clone().acquire_owned())
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
        if review_digest(&runtime, &trust_digest(&policy).await?)? != digest {
            return Err(AppError::Conflict(
                "installed bytes or signing trust changed after review; install a new inventory"
                    .into(),
            ));
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
}
