use std::{ffi::OsString, os::unix::fs::PermissionsExt, sync::Mutex};

use sha2::{Digest, Sha256};
use tempfile::TempDir;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

use super::*;

struct AcceptSignature;

#[async_trait]
impl SignatureVerifier for AcceptSignature {
    async fn verify(
        &self,
        _reference: &str,
        _expected_digest: &str,
        _credentials: &RegistryCredentials,
    ) -> Result<(), PluginDistributionError> {
        Ok(())
    }
}

struct RejectSignature;

#[async_trait]
impl SignatureVerifier for RejectSignature {
    async fn verify(
        &self,
        _reference: &str,
        _expected_digest: &str,
        _credentials: &RegistryCredentials,
    ) -> Result<(), PluginDistributionError> {
        Err(PluginDistributionError::SignatureVerification)
    }
}

#[derive(Clone)]
struct ObservedCosignInvocation {
    arguments: Vec<OsString>,
    environment: Vec<(OsString, OsString)>,
    current_directory: PathBuf,
    current_directory_mode: u32,
    docker_config: Vec<u8>,
    docker_config_mode: u32,
    key_path: Option<PathBuf>,
    key_mode: Option<u32>,
}

struct PolicyCosignRunner {
    accepted_key: Vec<u8>,
    accepted_reference: String,
    version: &'static str,
    invocations: Mutex<Vec<ObservedCosignInvocation>>,
}

impl PolicyCosignRunner {
    fn new(accepted_key: &[u8], accepted_reference: String) -> Self {
        Self {
            accepted_key: accepted_key.to_vec(),
            accepted_reference,
            version: COSIGN_VERIFIER_VERSION,
            invocations: Mutex::new(Vec::new()),
        }
    }

    fn with_version(mut self, version: &'static str) -> Self {
        self.version = version;
        self
    }
}

#[async_trait]
impl CosignRunner for PolicyCosignRunner {
    async fn run(&self, command: &CosignCommandSpec) -> io::Result<CosignProcessOutput> {
        let docker_config_directory = command
            .environment
            .iter()
            .find(|(name, _)| name == "DOCKER_CONFIG")
            .map(|(_, value)| PathBuf::from(value))
            .expect("DOCKER_CONFIG");
        let docker_config_path = docker_config_directory.join("config.json");
        let key_path = command
            .arguments
            .iter()
            .position(|argument| argument == "--key")
            .map(|position| PathBuf::from(&command.arguments[position + 1]));
        let observed = ObservedCosignInvocation {
            arguments: command.arguments.clone(),
            environment: command.environment.clone(),
            current_directory: command.current_directory.clone(),
            current_directory_mode: std::fs::metadata(&command.current_directory)?
                .permissions()
                .mode()
                & 0o777,
            docker_config: std::fs::read(&docker_config_path)?,
            docker_config_mode: std::fs::metadata(&docker_config_path)?.permissions().mode()
                & 0o777,
            key_mode: key_path
                .as_ref()
                .map(|path| {
                    std::fs::metadata(path).map(|metadata| metadata.permissions().mode() & 0o777)
                })
                .transpose()?,
            key_path: key_path.clone(),
        };
        self.invocations
            .lock()
            .expect("invocation lock")
            .push(observed);

        if command.arguments.first() == Some(&OsString::from("version")) {
            return Ok(CosignProcessOutput {
                success: true,
                stdout: serde_json::to_vec(&serde_json::json!({
                    "gitVersion": self.version
                }))?,
            });
        }
        let key = key_path
            .as_ref()
            .map(std::fs::read)
            .transpose()?
            .unwrap_or_default();
        let reference = command
            .arguments
            .last()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        Ok(CosignProcessOutput {
            success: key == self.accepted_key && reference == self.accepted_reference,
            stdout: Vec::new(),
        })
    }
}

struct ErrorCosignRunner;

#[async_trait]
impl CosignRunner for ErrorCosignRunner {
    async fn run(&self, _command: &CosignCommandSpec) -> io::Result<CosignProcessOutput> {
        Err(io::Error::other(
            "registry-password-must-not-escape-through-errors",
        ))
    }
}

struct MockArtifact {
    options: InstallPluginOptions,
    plugin_json: Vec<u8>,
    plugin_root: PathBuf,
}

async fn mock_artifact(title: &str, plugin_json: &[u8]) -> (MockServer, TempDir, MockArtifact) {
    let server = MockServer::start().await;
    let temporary = TempDir::new().expect("temporary directory");
    let plugin_root = temporary.path().join("plugins");
    let config = br#"{"format_version":1}"#;
    let config_digest = digest(config);
    let layer_digest = digest(plugin_json);
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "artifactType": PLUGIN_ARTIFACT_MEDIA_TYPE,
        "config": {"mediaType": PLUGIN_CONFIG_MEDIA_TYPE, "digest": config_digest, "size": config.len()},
        "layers": [{
            "mediaType": PLUGIN_MANIFEST_MEDIA_TYPE,
            "digest": layer_digest,
            "size": plugin_json.len(),
            "annotations": { OCI_TITLE_ANNOTATION: title }
        }]
    });
    let manifest_bytes = serde_json::to_vec(&manifest).expect("manifest JSON");
    let manifest_digest = digest(&manifest_bytes);
    mount_blob(&server, &config_digest, config).await;
    mount_blob(&server, &layer_digest, plugin_json).await;
    for verb in ["GET", "HEAD"] {
        let response = ResponseTemplate::new(200)
            .insert_header("content-type", "application/vnd.oci.image.manifest.v1+json")
            .insert_header("docker-content-digest", manifest_digest.as_str());
        Mock::given(method(verb))
            .and(path(format!("/v2/test/plugin/manifests/{manifest_digest}")))
            .respond_with(if verb == "GET" {
                response.set_body_bytes(manifest_bytes.clone())
            } else {
                response
            })
            .mount(&server)
            .await;
    }
    let source = format!("{}/test/plugin", server.address());
    let reference = format!("{source}@{manifest_digest}");
    (
        server,
        temporary,
        MockArtifact {
            options: InstallPluginOptions {
                reference,
                plugin_root: plugin_root.clone(),
                allowed_sources: BTreeSet::from([source]),
                credentials: RegistryCredentials::Anonymous,
                cosign_public_keys: vec![],
            },
            plugin_json: plugin_json.to_vec(),
            plugin_root,
        },
    )
}

async fn mount_blob(server: &MockServer, digest: &str, body: &[u8]) {
    Mock::given(method("GET"))
        .and(path(format!("/v2/test/plugin/blobs/{digest}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("docker-content-digest", digest)
                .set_body_bytes(body.to_vec()),
        )
        .mount(server)
        .await;
}

fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn plugin_json() -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "id": "oci-test", "version": "1.0.0", "wit_version": "0.2.0",
        "wasm": null, "capabilities": [], "contributions": {"providers": []}
    }))
    .expect("plugin JSON")
}

#[test]
fn digest_and_paths_are_fail_closed() {
    assert!(valid_sha256_digest(&format!("sha256:{}", "a".repeat(64))));
    assert!(!valid_sha256_digest(&format!("sha512:{}", "a".repeat(64))));
    assert!(!valid_sha256_digest(&format!("sha256:{}", "A".repeat(64))));
    for path in [
        "",
        "/plugin.json",
        "../plugin.json",
        "a/./b",
        "a//b",
        "a\\b",
        ".mtc-oci-install.json",
    ] {
        assert!(validate_relative_path(path).is_err(), "accepted {path}");
    }
    assert_eq!(
        validate_relative_path("schemas/config.json").expect("safe path"),
        PathBuf::from("schemas/config.json")
    );
}

#[tokio::test]
async fn pulls_from_mock_registry_and_atomically_installs() {
    let (_server, _temporary, artifact) = mock_artifact("plugin.json", &plugin_json()).await;
    let installed = install_plugin_oci_with_verifier(&artifact.options, &AcceptSignature, true)
        .await
        .expect("install plugin");
    assert_eq!(
        (installed.id.as_str(), installed.version.as_str()),
        ("oci-test", "1.0.0")
    );
    assert_eq!(
        tokio::fs::read(installed.path.join("plugin.json"))
            .await
            .expect("installed plugin"),
        artifact.plugin_json
    );
    let receipt: serde_json::Value = serde_json::from_slice(
        &tokio::fs::read(installed.path.join(".mtc-oci-install.json"))
            .await
            .expect("receipt"),
    )
    .expect("receipt JSON");
    assert_eq!(receipt["digest"], installed.digest);
    assert!(matches!(
        install_plugin_oci_with_verifier(&artifact.options, &AcceptSignature, true)
            .await
            .expect_err("no replacement"),
        PluginDistributionError::TargetExists
    ));
}

#[tokio::test]
async fn official_cosign_contract_accepts_rotated_key_and_keeps_private_auth_out_of_arguments() {
    let (_server, _temporary, mut artifact) = mock_artifact("plugin.json", &plugin_json()).await;
    let registry_password = "registry-password-must-never-appear-in-process-metadata";
    artifact.options.credentials = RegistryCredentials::Basic {
        username: "installer".to_owned(),
        password: registry_password.to_owned(),
    };
    artifact.options.cosign_public_keys = vec![b"wrong-key".to_vec(), b"trusted-key".to_vec()];
    let runner = PolicyCosignRunner::new(b"trusted-key", artifact.options.reference.clone());
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &artifact.options.cosign_public_keys,
        runner: &runner,
    };
    let expected_digest = artifact
        .options
        .reference
        .split_once('@')
        .expect("pinned reference")
        .1;
    verifier
        .verify(
            &artifact.options.reference,
            expected_digest,
            &artifact.options.credentials,
        )
        .await
        .expect("official Cosign command contract");
    artifact.options.credentials = RegistryCredentials::Anonymous;
    assert_eq!(
        install_plugin_oci_with_verifier(&artifact.options, &AcceptSignature, true)
            .await
            .expect("verified install")
            .id,
        "oci-test"
    );

    let invocations = runner.invocations.lock().expect("invocation lock").clone();
    assert_eq!(invocations.len(), 3, "version, wrong key, trusted key");
    let mut temporary_paths = Vec::new();
    for invocation in &invocations {
        assert_eq!(invocation.docker_config_mode, 0o600);
        assert_eq!(invocation.current_directory_mode, 0o700);
        assert!(
            !format!("{:?}{:?}", invocation.arguments, invocation.environment)
                .contains(registry_password)
        );
        let config: serde_json::Value =
            serde_json::from_slice(&invocation.docker_config).expect("Docker config JSON");
        assert!(
            config["auths"]
                .as_object()
                .is_some_and(|auths| auths.len() == 1)
        );
        let config_text = std::str::from_utf8(&invocation.docker_config).expect("UTF-8 config");
        assert!(!config_text.contains("installer"));
        assert!(!config_text.contains(registry_password));
        if let Some(path) = &invocation.key_path {
            assert_eq!(invocation.key_mode, Some(0o600));
            temporary_paths.push(path.clone());
        }
        temporary_paths.push(invocation.current_directory.clone());
    }
    drop(invocations);
    // The verifier owns the workspace only for the subprocess lifetime. It is
    // synchronously removed on success, failure, or future cancellation.
    for path in temporary_paths {
        assert!(!path.exists(), "temporary verifier path leaked: {path:?}");
    }
    let invocations = runner.invocations.lock().expect("invocation lock");
    let verify_arguments = &invocations.last().expect("verify invocation").arguments;
    assert!(verify_arguments.contains(&OsString::from("--insecure-ignore-tlog")));
    assert!(!verify_arguments.contains(&OsString::from("--offline")));
    assert!(!verify_arguments.contains(&OsString::from("--private-infrastructure")));
    assert!(!verify_arguments.contains(&OsString::from("--insecure-ignore-sct")));
}

#[test]
fn bearer_registry_auth_uses_registrytoken_not_identitytoken() {
    let keys = vec![b"trusted-key".to_vec()];
    let workspace = CosignWorkspace::new(
        "registry.example",
        &RegistryCredentials::Bearer("short-lived-bearer".to_owned()),
        &keys,
    )
    .expect("Cosign workspace");
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(workspace.docker_config.join("config.json")).expect("Docker config"),
    )
    .expect("Docker config JSON");
    let registry = &config["auths"]["registry.example"];
    assert_eq!(registry["registrytoken"], "short-lived-bearer");
    assert!(registry.get("identitytoken").is_none());
}

#[tokio::test]
async fn wrong_key_and_tampered_payload_fail_before_registry_or_storage_access() {
    let temporary = TempDir::new().expect("temporary directory");
    let digest = format!("sha256:{}", "a".repeat(64));
    let reference = format!("127.0.0.1:1/test/plugin@{digest}");
    let options = InstallPluginOptions {
        reference: reference.clone(),
        plugin_root: temporary.path().join("plugins"),
        allowed_sources: BTreeSet::from(["127.0.0.1:1/test/plugin".to_owned()]),
        credentials: RegistryCredentials::Anonymous,
        cosign_public_keys: vec![b"wrong-key".to_vec()],
    };
    // The runner models official Cosign's fail-closed exit status: neither the
    // supplied key nor the digest-bound payload matches the trusted signature.
    let runner = PolicyCosignRunner::new(
        b"trusted-key",
        format!("127.0.0.1:1/test/plugin@sha256:{}", "b".repeat(64)),
    );
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &options.cosign_public_keys,
        runner: &runner,
    };
    assert!(matches!(
        install_plugin_oci_with_verifier(&options, &verifier, true)
            .await
            .expect_err("wrong key and tampered payload must fail"),
        PluginDistributionError::SignatureVerification
    ));
    assert!(!options.plugin_root.exists());
    assert!(
        runner
            .invocations
            .lock()
            .expect("invocation lock")
            .iter()
            .all(|invocation| !invocation.current_directory.exists())
    );
}

#[tokio::test]
async fn verifier_rejects_expected_digest_mismatch_without_spawning_cosign() {
    let keys = vec![b"trusted-key".to_vec()];
    let reference_digest = format!("sha256:{}", "a".repeat(64));
    let reference = format!("registry.example/test/plugin@{reference_digest}");
    let runner = PolicyCosignRunner::new(b"trusted-key", reference.clone());
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &keys,
        runner: &runner,
    };
    assert!(matches!(
        verifier
            .verify(
                &reference,
                &format!("sha256:{}", "b".repeat(64)),
                &RegistryCredentials::Anonymous,
            )
            .await
            .expect_err("wrong expected digest"),
        PluginDistributionError::SignatureVerification
    ));
    assert!(
        runner
            .invocations
            .lock()
            .expect("invocation lock")
            .is_empty()
    );
}

#[tokio::test]
async fn subprocess_errors_are_redacted_to_the_public_signature_failure() {
    let keys = vec![b"trusted-key".to_vec()];
    let digest = format!("sha256:{}", "a".repeat(64));
    let reference = format!("registry.example/test/plugin@{digest}");
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &keys,
        runner: &ErrorCosignRunner,
    };
    let error = verifier
        .verify(
            &reference,
            &digest,
            &RegistryCredentials::Basic {
                username: "installer".to_owned(),
                password: "registry-password-must-not-escape-through-errors".to_owned(),
            },
        )
        .await
        .expect_err("runner error must fail closed");
    assert_eq!(error.to_string(), "plugin signature verification failed");
    assert!(!error.to_string().contains("registry-password"));
}

#[tokio::test]
async fn verifier_rejects_unexpected_cosign_version_and_bounds_process_output() {
    let keys = vec![b"trusted-key".to_vec()];
    let digest = format!("sha256:{}", "a".repeat(64));
    let reference = format!("registry.example/test/plugin@{digest}");
    let runner = PolicyCosignRunner::new(b"trusted-key", reference.clone()).with_version("v3.1.2");
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &keys,
        runner: &runner,
    };
    assert!(matches!(
        verifier
            .verify(&reference, &digest, &RegistryCredentials::Anonymous)
            .await
            .expect_err("outdated cosign"),
        PluginDistributionError::SignatureVerification
    ));
    assert!(
        read_bounded_output(std::io::Cursor::new(vec![
            0;
            MAX_COSIGN_OUTPUT_BYTES as usize
                + 1
        ]))
        .await
        .is_err()
    );
}

#[tokio::test]
async fn digest_only_reference_is_passed_to_cosign_when_input_also_has_a_tag() {
    let (_server, _temporary, mut artifact) = mock_artifact("plugin.json", &plugin_json()).await;
    let (source, digest) = artifact
        .options
        .reference
        .split_once('@')
        .expect("pinned reference");
    let source = source.to_owned();
    let digest = digest.to_owned();
    artifact.options.reference = format!("{source}:mutable-tag@{digest}");
    artifact.options.cosign_public_keys = vec![b"trusted-key".to_vec()];
    let digest_only_reference = format!("{source}@{digest}");
    let runner = PolicyCosignRunner::new(b"trusted-key", digest_only_reference.clone());
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &artifact.options.cosign_public_keys,
        runner: &runner,
    };
    install_plugin_oci_with_verifier(&artifact.options, &verifier, true)
        .await
        .expect("digest-only verification and pull");
    let invocations = runner.invocations.lock().expect("invocation lock");
    assert_eq!(
        invocations
            .last()
            .and_then(|invocation| invocation.arguments.last())
            .and_then(|value| value.to_str()),
        Some(digest_only_reference.as_str())
    );
    assert!(invocations.iter().all(|invocation| {
        !invocation
            .arguments
            .iter()
            .any(|argument| argument.to_string_lossy().contains("mutable-tag"))
    }));
}

#[tokio::test]
async fn system_executor_kills_on_timeout_and_rejects_oversized_output() {
    let temporary = TempDir::new().expect("temporary directory");
    let sleep = CosignCommandSpec {
        arguments: vec![OsString::from("5")],
        environment: Vec::new(),
        current_directory: temporary.path().to_owned(),
    };
    let timeout_error =
        match run_cosign_process(Path::new("/bin/sleep"), &sleep, Duration::from_millis(5)).await {
            Err(error) => error,
            Ok(_) => panic!("timeout must terminate child"),
        };
    assert_eq!(timeout_error.kind(), io::ErrorKind::TimedOut);

    let oversized = CosignCommandSpec {
        arguments: vec![
            OsString::from("-c"),
            OsString::from(format!("head -c {} /dev/zero", MAX_COSIGN_OUTPUT_BYTES + 1)),
        ],
        environment: Vec::new(),
        current_directory: temporary.path().to_owned(),
    };
    assert!(
        run_cosign_process(Path::new("/bin/sh"), &oversized, Duration::from_secs(2))
            .await
            .is_err()
    );
}

#[test]
fn cosign_binary_and_version_are_fixed_security_constants() {
    assert_eq!(COSIGN_VERIFIER_PATH, "/usr/local/bin/cosign");
    assert_eq!(COSIGN_VERIFIER_VERSION, "v3.1.3-mtc.1");
    assert!(cosign_version_matches(br#"{"gitVersion":"v3.1.3-mtc.1"}"#));
    assert!(!cosign_version_matches(br#"{"gitVersion":"v3.1.3"}"#));
    assert!(!cosign_version_matches(br#"{"gitVersion":"v3.1.2"}"#));
    assert!(!cosign_version_matches(br#"{"GitVersion":"v3.1.3"}"#));
    assert!(!cosign_version_matches(br#"{"git_version":"v3.1.3"}"#));
}

#[tokio::test]
async fn signature_policy_rejects_before_registry_or_storage_access() {
    let temporary = TempDir::new().expect("temporary directory");
    let digest = format!("sha256:{}", "a".repeat(64));
    let options = InstallPluginOptions {
        reference: format!("127.0.0.1:1/test/plugin@{digest}"),
        plugin_root: temporary.path().join("plugins"),
        allowed_sources: BTreeSet::from(["127.0.0.1:1/test/plugin".to_owned()]),
        credentials: RegistryCredentials::Anonymous,
        cosign_public_keys: vec![],
    };
    assert!(matches!(
        install_plugin_oci_with_verifier(&options, &RejectSignature, true)
            .await
            .expect_err("reject signature"),
        PluginDistributionError::SignatureVerification
    ));
    assert!(!options.plugin_root.exists());
}

#[tokio::test]
async fn traversal_and_invalid_packages_leave_no_visible_plugin() {
    let (_server, _temporary, traversal) = mock_artifact("../plugin.json", &plugin_json()).await;
    assert!(matches!(
        install_plugin_oci_with_verifier(&traversal.options, &AcceptSignature, true)
            .await
            .expect_err("traversal"),
        PluginDistributionError::InvalidArtifact(_)
    ));
    assert!(!traversal.plugin_root.exists());
    let (_server, _temporary, invalid) = mock_artifact("plugin.json", br#"{"id":"INVALID"}"#).await;
    assert!(matches!(
        install_plugin_oci_with_verifier(&invalid.options, &AcceptSignature, true)
            .await
            .expect_err("invalid package"),
        PluginDistributionError::InvalidPackage(_)
    ));
    assert!(
        std::fs::read_dir(&invalid.plugin_root)
            .expect("plugin root")
            .next()
            .is_none()
    );
}
