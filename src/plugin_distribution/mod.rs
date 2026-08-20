use std::{
    collections::BTreeSet,
    ffi::OsString,
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    pin::Pin,
    process::Stdio,
    task::{Context, Poll},
    time::Duration,
};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64_STANDARD};
use oci_client::{
    Reference,
    client::{Client, ClientConfig, ClientProtocol},
    manifest::{OciDescriptor, OciManifest},
    secrets::RegistryAuth,
};
use serde::{Deserialize, Serialize};
use tempfile::{Builder as TempDirBuilder, TempDir};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use uuid::Uuid;

use crate::plugin::{PluginManifest, validate_plugin_package};

pub const PLUGIN_ARTIFACT_MEDIA_TYPE: &str = "application/vnd.memeloop.token-center.plugin.v1";
pub const PLUGIN_CONFIG_MEDIA_TYPE: &str =
    "application/vnd.memeloop.token-center.plugin.config.v1+json";
pub const PLUGIN_MANIFEST_MEDIA_TYPE: &str =
    "application/vnd.memeloop.token-center.plugin.manifest.v1+json";
pub const PLUGIN_WASM_MEDIA_TYPE: &str = "application/vnd.wasm.content.layer.v1+wasm";
pub const PLUGIN_ASSET_MEDIA_TYPE: &str = "application/vnd.memeloop.token-center.plugin.asset.v1";
const OCI_TITLE_ANNOTATION: &str = "org.opencontainers.image.title";
const MAX_FILES: usize = 64;
const MAX_PATH_BYTES: usize = 240;
const MAX_MANIFEST_BYTES: u64 = 1024 * 1024;
const MAX_WASM_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ASSET_BYTES: u64 = 8 * 1024 * 1024;
const MAX_CONFIG_BYTES: u64 = 16 * 1024;
const MAX_TOTAL_BYTES: u64 = 80 * 1024 * 1024;
const MAX_PUBLIC_KEYS: usize = 8;
const MAX_PUBLIC_KEY_BYTES: u64 = 64 * 1024;
const MAX_COSIGN_OUTPUT_BYTES: u64 = 64 * 1024;
const COSIGN_TIMEOUT: Duration = Duration::from_secs(60);
pub const COSIGN_VERIFIER_PATH: &str = "/usr/local/bin/cosign";
pub const COSIGN_VERIFIER_VERSION: &str = "v3.1.3";

#[derive(Clone, Default)]
pub enum RegistryCredentials {
    #[default]
    Anonymous,
    Basic {
        username: String,
        password: String,
    },
    Bearer(String),
}

impl RegistryCredentials {
    fn oci_auth(&self) -> RegistryAuth {
        match self {
            Self::Anonymous => RegistryAuth::Anonymous,
            Self::Basic { username, password } => {
                RegistryAuth::Basic(username.clone(), password.clone())
            }
            Self::Bearer(token) => RegistryAuth::Bearer(token.clone()),
        }
    }
}

#[derive(Clone)]
pub struct InstallPluginOptions {
    pub reference: String,
    pub plugin_root: PathBuf,
    pub allowed_sources: BTreeSet<String>,
    pub credentials: RegistryCredentials,
    pub cosign_public_keys: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledPlugin {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub source: String,
    pub path: PathBuf,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginDistributionError {
    #[error("plugin reference must be pinned by a sha256 digest")]
    DigestPinRequired,
    #[error("plugin source is not allowed by policy")]
    SourceDenied,
    #[error("plugin signature verification failed")]
    SignatureVerification,
    #[error("OCI registry operation failed")]
    Registry,
    #[error("plugin OCI artifact is invalid: {0}")]
    InvalidArtifact(String),
    #[error("plugin package is invalid: {0}")]
    InvalidPackage(String),
    #[error("plugin installation target already exists")]
    TargetExists,
    #[error("plugin installation storage operation failed")]
    Storage,
}

#[async_trait]
trait SignatureVerifier: Send + Sync {
    async fn verify(
        &self,
        reference: &str,
        expected_digest: &str,
        credentials: &RegistryCredentials,
    ) -> Result<(), PluginDistributionError>;
}

struct CosignPublicKeySignatureVerifier<'a> {
    keys: &'a [Vec<u8>],
    runner: &'a dyn CosignRunner,
}

#[async_trait]
impl SignatureVerifier for CosignPublicKeySignatureVerifier<'_> {
    async fn verify(
        &self,
        reference: &str,
        expected_digest: &str,
        credentials: &RegistryCredentials,
    ) -> Result<(), PluginDistributionError> {
        if self.keys.is_empty() || self.keys.len() > MAX_PUBLIC_KEYS {
            return Err(PluginDistributionError::SignatureVerification);
        }
        if self
            .keys
            .iter()
            .any(|key| key.is_empty() || key.len() as u64 > MAX_PUBLIC_KEY_BYTES)
        {
            return Err(PluginDistributionError::SignatureVerification);
        }
        let image: Reference = reference
            .parse()
            .map_err(|_| PluginDistributionError::SignatureVerification)?;
        if image.digest() != Some(expected_digest) {
            return Err(PluginDistributionError::SignatureVerification);
        }
        let source = format!("{}/{}", image.registry(), image.repository());
        let verified_reference = format!("{source}@{expected_digest}");
        let workspace = CosignWorkspace::new(image.registry(), credentials, self.keys)
            .map_err(|_| PluginDistributionError::SignatureVerification)?;

        let version = self
            .runner
            .run(&cosign_version_command(&workspace))
            .await
            .map_err(|_| PluginDistributionError::SignatureVerification)?;
        if !version.success || !cosign_version_matches(&version.stdout) {
            return Err(PluginDistributionError::SignatureVerification);
        }
        for key_path in &workspace.key_paths {
            let outcome = self
                .runner
                .run(&cosign_verify_command(
                    &workspace,
                    key_path,
                    &verified_reference,
                ))
                .await
                .map_err(|_| PluginDistributionError::SignatureVerification)?;
            if outcome.success {
                return Ok(());
            }
        }
        Err(PluginDistributionError::SignatureVerification)
    }
}

struct CosignWorkspace {
    directory: TempDir,
    docker_config: PathBuf,
    key_paths: Vec<PathBuf>,
}

impl CosignWorkspace {
    fn new(
        registry: &str,
        credentials: &RegistryCredentials,
        keys: &[Vec<u8>],
    ) -> io::Result<Self> {
        let directory = TempDirBuilder::new()
            .prefix(".mtc-cosign-")
            .tempdir_in("/tmp")?;
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700))?;
        let docker_config = directory.path().join("docker");
        std::fs::create_dir(&docker_config)?;
        std::fs::set_permissions(&docker_config, std::fs::Permissions::from_mode(0o700))?;
        let auth = match credentials {
            RegistryCredentials::Anonymous => serde_json::json!({}),
            RegistryCredentials::Basic { username, password } => serde_json::json!({
                "auth": BASE64_STANDARD.encode(format!("{username}:{password}").as_bytes())
            }),
            RegistryCredentials::Bearer(token) => serde_json::json!({"registrytoken": token}),
        };
        let config = serde_json::to_vec(&serde_json::json!({
            "auths": {registry: auth}
        }))?;
        write_private_file(&docker_config.join("config.json"), &config)?;

        let mut key_paths = Vec::with_capacity(keys.len());
        for (index, key) in keys.iter().enumerate() {
            let path = directory.path().join(format!("cosign-{index}.pub"));
            write_private_file(&path, key)?;
            key_paths.push(path);
        }
        Ok(Self {
            directory,
            docker_config,
            key_paths,
        })
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

struct CosignCommandSpec {
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    current_directory: PathBuf,
}

fn cosign_base_command(workspace: &CosignWorkspace) -> CosignCommandSpec {
    CosignCommandSpec {
        arguments: Vec::new(),
        environment: vec![
            (
                OsString::from("DOCKER_CONFIG"),
                workspace.docker_config.clone().into_os_string(),
            ),
            (
                OsString::from("HOME"),
                workspace.directory.path().as_os_str().to_owned(),
            ),
            (
                OsString::from("XDG_CACHE_HOME"),
                workspace.directory.path().as_os_str().to_owned(),
            ),
        ],
        current_directory: workspace.directory.path().to_owned(),
    }
}

fn cosign_version_command(workspace: &CosignWorkspace) -> CosignCommandSpec {
    let mut command = cosign_base_command(workspace);
    command.arguments = vec![OsString::from("version"), OsString::from("--json")];
    command
}

fn cosign_verify_command(
    workspace: &CosignWorkspace,
    key_path: &Path,
    verified_reference: &str,
) -> CosignCommandSpec {
    let mut command = cosign_base_command(workspace);
    command.arguments = vec![
        OsString::from("verify"),
        OsString::from("--key"),
        key_path.as_os_str().to_owned(),
        // Plugin packages use an explicitly provisioned public-key trust root.
        // Do not require a Rekor entry for this offline verification policy.
        OsString::from("--insecure-ignore-tlog"),
        OsString::from(verified_reference),
    ];
    command
}

fn cosign_version_matches(stdout: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(stdout) else {
        return false;
    };
    value.get("gitVersion").and_then(serde_json::Value::as_str) == Some(COSIGN_VERIFIER_VERSION)
}

struct CosignProcessOutput {
    success: bool,
    stdout: Vec<u8>,
}

#[async_trait]
trait CosignRunner: Send + Sync {
    async fn run(&self, command: &CosignCommandSpec) -> io::Result<CosignProcessOutput>;
}

struct SystemCosignRunner;

#[async_trait]
impl CosignRunner for SystemCosignRunner {
    async fn run(&self, command: &CosignCommandSpec) -> io::Result<CosignProcessOutput> {
        run_cosign_process(Path::new(COSIGN_VERIFIER_PATH), command, COSIGN_TIMEOUT).await
    }
}

async fn run_cosign_process(
    program: &Path,
    command: &CosignCommandSpec,
    timeout: Duration,
) -> io::Result<CosignProcessOutput> {
    let mut child = tokio::process::Command::new(program);
    child
        .args(&command.arguments)
        .env_clear()
        .envs(command.environment.iter().cloned())
        .current_dir(&command.current_directory)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = child.spawn()?;
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| io::Error::other("cosign stdout unavailable"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| io::Error::other("cosign stderr unavailable"))?;
    let stdout_task = tokio::spawn(read_bounded_output(stdout));
    let stderr_task = tokio::spawn(read_bounded_output(stderr));
    let status = match tokio::time::timeout(timeout, child.wait()).await {
        Ok(status) => status?,
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            // Deterministically join both readers after the child closes
            // its pipe ends. No detached task may retain a verifier pipe
            // or delay temporary workspace cleanup.
            let _ = stdout_task.await;
            let _ = stderr_task.await;
            return Err(io::Error::new(io::ErrorKind::TimedOut, "cosign timed out"));
        }
    };
    let stdout = stdout_task
        .await
        .map_err(|_| io::Error::other("cosign stdout task failed"))??;
    // Drain and bound stderr, but never return it to callers or logs: registry
    // implementations have historically echoed credential-bearing errors.
    stderr_task
        .await
        .map_err(|_| io::Error::other("cosign stderr task failed"))??;
    Ok(CosignProcessOutput {
        success: status.success(),
        stdout,
    })
}

async fn read_bounded_output<R: AsyncRead + Unpin>(reader: R) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_COSIGN_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_COSIGN_OUTPUT_BYTES {
        return Err(io::Error::other("cosign output exceeded limit"));
    }
    Ok(bytes)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArtifactConfig {
    format_version: u8,
}

#[derive(Debug)]
struct PlannedFile {
    path: PathBuf,
    descriptor: OciDescriptor,
    maximum: u64,
}

#[derive(Debug, Serialize)]
struct InstallReceipt<'a> {
    format_version: u8,
    source: &'a str,
    digest: &'a str,
    signature_policy: &'static str,
}

pub async fn install_plugin_oci(
    options: &InstallPluginOptions,
) -> Result<InstalledPlugin, PluginDistributionError> {
    let runner = SystemCosignRunner;
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &options.cosign_public_keys,
        runner: &runner,
    };
    install_plugin_oci_with_verifier(options, &verifier, false).await
}

async fn install_plugin_oci_with_verifier(
    options: &InstallPluginOptions,
    verifier: &dyn SignatureVerifier,
    allow_plain_http: bool,
) -> Result<InstalledPlugin, PluginDistributionError> {
    let supplied_reference: Reference = options
        .reference
        .parse()
        .map_err(|_| PluginDistributionError::DigestPinRequired)?;
    let expected_digest = supplied_reference
        .digest()
        .filter(|digest| valid_sha256_digest(digest))
        .ok_or(PluginDistributionError::DigestPinRequired)?;
    let source = format!(
        "{}/{}",
        supplied_reference.registry(),
        supplied_reference.repository()
    );
    if !options.allowed_sources.contains(&source) {
        return Err(PluginDistributionError::SourceDenied);
    }
    // Discard any supplied tag before both verification and pulling. A tag may
    // coexist syntactically with a digest, but it must never influence either
    // operation after the digest policy check succeeds.
    let verified_reference = format!("{source}@{expected_digest}");
    let reference: Reference = verified_reference
        .parse()
        .map_err(|_| PluginDistributionError::DigestPinRequired)?;

    verifier
        .verify(&verified_reference, expected_digest, &options.credentials)
        .await?;

    let client = Client::new(ClientConfig {
        protocol: if allow_plain_http {
            ClientProtocol::Http
        } else {
            ClientProtocol::Https
        },
        max_concurrent_download: 1,
        read_timeout: Some(Duration::from_secs(60)),
        connect_timeout: Some(Duration::from_secs(10)),
        ..Default::default()
    });
    let auth = options.credentials.oci_auth();
    let (manifest, resolved_digest) = client
        .pull_manifest(&reference, &auth)
        .await
        .map_err(|_| PluginDistributionError::Registry)?;
    if resolved_digest != expected_digest {
        return Err(PluginDistributionError::InvalidArtifact(
            "registry returned a different manifest digest".to_owned(),
        ));
    }
    let OciManifest::Image(manifest) = manifest else {
        return Err(PluginDistributionError::InvalidArtifact(
            "image indexes are not plugin packages".to_owned(),
        ));
    };
    let files = validate_artifact_manifest(&manifest)?;

    prepare_plugin_root(&options.plugin_root).await?;
    let staging_path = options
        .plugin_root
        .join(format!(".mtc-plugin-staging-{}", Uuid::now_v7()));
    tokio::fs::create_dir(&staging_path)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    let mut staging = StagingGuard::new(staging_path.clone());

    let config = pull_config(&client, &reference, &manifest.config).await?;
    if config.format_version != 1 {
        return Err(PluginDistributionError::InvalidArtifact(
            "unsupported plugin artifact format version".to_owned(),
        ));
    }
    for file in &files {
        pull_file(&client, &reference, file, &staging_path).await?;
    }

    let package = validate_plugin_package(&staging_path)
        .map_err(|error| PluginDistributionError::InvalidPackage(error.to_string()))?;
    validate_manifest_layer_relationships(&package, &files)?;
    write_receipt(&staging_path, &source, expected_digest).await?;
    sync_directory(&staging_path).await?;

    let target = options.plugin_root.join(&package.id);
    atomic_noreplace_rename(&options.plugin_root, &staging_path, &package.id)?;
    staging.disarm();
    sync_directory(&options.plugin_root).await?;

    Ok(InstalledPlugin {
        id: package.id,
        version: package.version,
        digest: expected_digest.to_owned(),
        source,
        path: target,
    })
}

#[cfg(target_os = "linux")]
fn atomic_noreplace_rename(
    root: &Path,
    staging: &Path,
    plugin_id: &str,
) -> Result<(), PluginDistributionError> {
    use rustix::fs::{Mode, OFlags, RenameFlags, open, renameat_with};

    let staging_name = staging
        .file_name()
        .ok_or(PluginDistributionError::Storage)?;
    let root = open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| PluginDistributionError::Storage)?;
    renameat_with(
        &root,
        staging_name,
        &root,
        plugin_id,
        RenameFlags::NOREPLACE,
    )
    .map_err(|error| {
        if error == rustix::io::Errno::EXIST {
            PluginDistributionError::TargetExists
        } else {
            PluginDistributionError::Storage
        }
    })
}

#[cfg(not(target_os = "linux"))]
fn atomic_noreplace_rename(
    _root: &Path,
    _staging: &Path,
    _plugin_id: &str,
) -> Result<(), PluginDistributionError> {
    // The production target is K8s/Linux. Refuse an emulated check-then-rename
    // on platforms without Linux renameat2 instead of silently permitting a
    // target-replacement race.
    Err(PluginDistributionError::Storage)
}

fn valid_sha256_digest(digest: &str) -> bool {
    let Some(value) = digest.strip_prefix("sha256:") else {
        return false;
    };
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn validate_artifact_manifest(
    manifest: &oci_client::manifest::OciImageManifest,
) -> Result<Vec<PlannedFile>, PluginDistributionError> {
    if manifest.schema_version != 2
        || manifest.artifact_type.as_deref() != Some(PLUGIN_ARTIFACT_MEDIA_TYPE)
        || manifest.config.media_type != PLUGIN_CONFIG_MEDIA_TYPE
        || manifest.config.size < 0
        || manifest.config.size as u64 > MAX_CONFIG_BYTES
        || manifest.subject.is_some()
    {
        return Err(PluginDistributionError::InvalidArtifact(
            "unsupported artifact manifest or config".to_owned(),
        ));
    }
    if manifest.layers.is_empty() || manifest.layers.len() > MAX_FILES {
        return Err(PluginDistributionError::InvalidArtifact(format!(
            "plugin packages must contain 1 to {MAX_FILES} files"
        )));
    }
    let mut paths = BTreeSet::new();
    let mut manifest_count = 0;
    let mut wasm_count = 0;
    let mut total = manifest.config.size as u64;
    let mut files = Vec::with_capacity(manifest.layers.len());
    for descriptor in &manifest.layers {
        let title = descriptor
            .annotations
            .as_ref()
            .and_then(|annotations| annotations.get(OCI_TITLE_ANNOTATION))
            .ok_or_else(|| {
                PluginDistributionError::InvalidArtifact(
                    "every layer needs an OCI title annotation".to_owned(),
                )
            })?;
        let path = validate_relative_path(title)?;
        if !paths.insert(path.clone()) {
            return Err(PluginDistributionError::InvalidArtifact(
                "duplicate package file path".to_owned(),
            ));
        }
        let maximum = match descriptor.media_type.as_str() {
            PLUGIN_MANIFEST_MEDIA_TYPE if title == "plugin.json" => {
                manifest_count += 1;
                MAX_MANIFEST_BYTES
            }
            PLUGIN_WASM_MEDIA_TYPE if title.ends_with(".wasm") => {
                wasm_count += 1;
                MAX_WASM_BYTES
            }
            PLUGIN_ASSET_MEDIA_TYPE if title != "plugin.json" && !title.ends_with(".wasm") => {
                MAX_ASSET_BYTES
            }
            _ => {
                return Err(PluginDistributionError::InvalidArtifact(
                    "layer media type does not match its package file".to_owned(),
                ));
            }
        };
        if descriptor.size < 0 || descriptor.size as u64 > maximum {
            return Err(PluginDistributionError::InvalidArtifact(
                "package file exceeds its size limit".to_owned(),
            ));
        }
        total = total.checked_add(descriptor.size as u64).ok_or_else(|| {
            PluginDistributionError::InvalidArtifact("package is too large".into())
        })?;
        files.push(PlannedFile {
            path,
            descriptor: descriptor.clone(),
            // Enforce the signed descriptor size while streaming, not only the
            // wider media-type ceiling. A registry cannot consume extra disk
            // and fail only after the complete oversized response arrives.
            maximum: descriptor.size as u64,
        });
    }
    if manifest_count != 1 || wasm_count > 1 || total > MAX_TOTAL_BYTES {
        return Err(PluginDistributionError::InvalidArtifact(
            "plugin package file set is invalid or too large".to_owned(),
        ));
    }
    Ok(files)
}

fn validate_relative_path(value: &str) -> Result<PathBuf, PluginDistributionError> {
    if value.is_empty()
        || value.len() > MAX_PATH_BYTES
        || value == ".mtc-oci-install.json"
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains('\0')
        || value.contains('\\')
        || value
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        return Err(PluginDistributionError::InvalidArtifact(
            "package file path is unsafe".to_owned(),
        ));
    }
    Ok(PathBuf::from(value))
}

async fn pull_config(
    client: &Client,
    reference: &Reference,
    descriptor: &OciDescriptor,
) -> Result<ArtifactConfig, PluginDistributionError> {
    let mut buffer = BoundedBuffer::new(descriptor.size as u64);
    client
        .pull_blob(reference, descriptor, &mut buffer)
        .await
        .map_err(|_| PluginDistributionError::Registry)?;
    if buffer.bytes.len() as i64 != descriptor.size {
        return Err(PluginDistributionError::InvalidArtifact(
            "config size does not match its descriptor".to_owned(),
        ));
    }
    serde_json::from_slice(&buffer.bytes).map_err(|_| {
        PluginDistributionError::InvalidArtifact("plugin artifact config is invalid".to_owned())
    })
}

async fn pull_file(
    client: &Client,
    reference: &Reference,
    file: &PlannedFile,
    staging: &Path,
) -> Result<(), PluginDistributionError> {
    let destination = staging.join(&file.path);
    if let Some(parent) = destination.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|_| PluginDistributionError::Storage)?;
    }
    let output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&destination)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    let mut output = BoundedFile::new(output, file.maximum);
    client
        .pull_blob(reference, &file.descriptor, &mut output)
        .await
        .map_err(|_| PluginDistributionError::Registry)?;
    if output.written != file.descriptor.size as u64 {
        return Err(PluginDistributionError::InvalidArtifact(
            "file size does not match its descriptor".to_owned(),
        ));
    }
    output
        .file
        .sync_all()
        .await
        .map_err(|_| PluginDistributionError::Storage)
}

fn validate_manifest_layer_relationships(
    manifest: &PluginManifest,
    files: &[PlannedFile],
) -> Result<(), PluginDistributionError> {
    let wasm_layers = files
        .iter()
        .filter(|file| file.descriptor.media_type == PLUGIN_WASM_MEDIA_TYPE)
        .map(|file| file.path.to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    match (&manifest.wasm, wasm_layers.as_slice()) {
        (None, []) => Ok(()),
        (Some(expected), [actual]) if expected == actual => Ok(()),
        _ => Err(PluginDistributionError::InvalidArtifact(
            "manifest wasm path does not match the OCI wasm layer".to_owned(),
        )),
    }
}

async fn prepare_plugin_root(root: &Path) -> Result<(), PluginDistributionError> {
    tokio::fs::create_dir_all(root)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    let metadata = tokio::fs::symlink_metadata(root)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(PluginDistributionError::Storage);
    }
    Ok(())
}

async fn write_receipt(
    staging: &Path,
    source: &str,
    digest: &str,
) -> Result<(), PluginDistributionError> {
    let bytes = serde_json::to_vec_pretty(&InstallReceipt {
        format_version: 1,
        source,
        digest,
        signature_policy: "cosign-public-key",
    })
    .map_err(|_| PluginDistributionError::Storage)?;
    let receipt = staging.join(".mtc-oci-install.json");
    let mut output = tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(receipt)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    tokio::io::AsyncWriteExt::write_all(&mut output, &bytes)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    output
        .sync_all()
        .await
        .map_err(|_| PluginDistributionError::Storage)
}

async fn sync_directory(path: &Path) -> Result<(), PluginDistributionError> {
    let directory = tokio::fs::File::open(path)
        .await
        .map_err(|_| PluginDistributionError::Storage)?;
    directory
        .sync_all()
        .await
        .map_err(|_| PluginDistributionError::Storage)
}

struct StagingGuard {
    path: Option<PathBuf>,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

struct BoundedBuffer {
    bytes: Vec<u8>,
    maximum: u64,
}

impl BoundedBuffer {
    fn new(maximum: u64) -> Self {
        Self {
            bytes: Vec::new(),
            maximum,
        }
    }
}

impl AsyncWrite for BoundedBuffer {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.bytes.len() as u64 + buffer.len() as u64 > self.maximum {
            return Poll::Ready(Err(io::Error::other("bounded buffer exceeded")));
        }
        self.bytes.extend_from_slice(buffer);
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}

struct BoundedFile {
    file: tokio::fs::File,
    maximum: u64,
    written: u64,
}

impl BoundedFile {
    fn new(file: tokio::fs::File, maximum: u64) -> Self {
        Self {
            file,
            maximum,
            written: 0,
        }
    }
}

impl AsyncWrite for BoundedFile {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        if self.written + buffer.len() as u64 > self.maximum {
            return Poll::Ready(Err(io::Error::other("bounded file exceeded")));
        }
        match Pin::new(&mut self.file).poll_write(context, buffer) {
            Poll::Ready(Ok(written)) => {
                self.written += written as u64;
                Poll::Ready(Ok(written))
            }
            other => other,
        }
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.file).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.file).poll_shutdown(context)
    }
}

#[cfg(test)]
mod tests;
