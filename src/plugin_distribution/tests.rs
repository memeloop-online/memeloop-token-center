use sha2::{Digest, Sha256};
use sigstore::{
    cosign::{Constraint, SignatureLayer, constraint::PrivateKeySigner},
    crypto::SigningScheme,
    registry::OciReference,
};
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

async fn mount_cosign_signature(
    server: &MockServer,
    reference: &str,
    manifest_digest: &str,
) -> Vec<u8> {
    let image: OciReference = reference.parse().expect("OCI reference");
    let mut layer =
        SignatureLayer::new_unsigned(&image, manifest_digest).expect("unsigned signature layer");
    let signer = SigningScheme::ECDSA_P256_SHA256_ASN1
        .create_signer()
        .expect("test signer");
    let public_key = signer
        .to_sigstore_keypair()
        .expect("test keypair")
        .public_key_to_pem()
        .expect("public key")
        .into_bytes();
    assert!(
        PrivateKeySigner::new_with_signer(signer)
            .add_constraint(&mut layer)
            .expect("sign layer")
    );
    let payload = serde_json::to_vec(&layer.simple_signing).expect("simple-signing payload");
    let payload_digest = digest(&payload);
    let config = b"{}";
    let config_digest = digest(config);
    let signature_manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": config_digest, "size": config.len()},
        "layers": [{
            "mediaType": "application/vnd.dev.cosign.simplesigning.v1+json",
            "digest": payload_digest,
            "size": payload.len(),
            "annotations": {"dev.cosignproject.cosign/signature": layer.signature.expect("signature")}
        }]
    });
    let bytes = serde_json::to_vec(&signature_manifest).expect("signature manifest");
    let signature_manifest_digest = digest(&bytes);
    let signature_tag = format!("{}.sig", manifest_digest.replace(':', "-"));
    mount_blob(server, &config_digest, config).await;
    mount_blob(server, &payload_digest, &payload).await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/test/plugin/manifests/{signature_tag}")))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/vnd.oci.image.manifest.v1+json")
                .insert_header("docker-content-digest", signature_manifest_digest.as_str())
                .set_body_bytes(bytes),
        )
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!("/v2/test/plugin/referrers/{manifest_digest}")))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path(format!(
            "/v2/test/plugin/manifests/{}",
            manifest_digest.replace(':', "-")
        )))
        .respond_with(ResponseTemplate::new(404))
        .mount(server)
        .await;
    public_key
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
async fn standard_sigstore_verifier_accepts_cosign_signature_from_mock_registry() {
    let (server, _temporary, mut artifact) = mock_artifact("plugin.json", &plugin_json()).await;
    let manifest_digest = artifact
        .options
        .reference
        .split_once('@')
        .expect("pinned")
        .1
        .to_owned();
    artifact.options.cosign_public_keys =
        vec![mount_cosign_signature(&server, &artifact.options.reference, &manifest_digest).await];
    let verifier = CosignPublicKeySignatureVerifier {
        keys: &artifact.options.cosign_public_keys,
        allow_plain_http: true,
    };
    assert_eq!(
        install_plugin_oci_with_verifier(&artifact.options, &verifier, true)
            .await
            .expect("verified install")
            .id,
        "oci-test"
    );
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
