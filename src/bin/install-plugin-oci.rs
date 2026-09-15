use std::{collections::BTreeSet, path::PathBuf};

use clap::Parser;
use memeloop_token_center::plugin_distribution::{
    CosignKeylessIdentity, InstallPluginOptions, RegistryCredentials, install_plugin_oci,
};

const MAX_SECRET_FILE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Parser)]
#[command(
    name = "install-plugin-oci",
    about = "Verify and atomically install a digest-pinned Token Center OCI plugin"
)]
struct Arguments {
    /// OCI reference. Tags are rejected; @sha256:<digest> is mandatory.
    reference: String,

    /// Read-only-at-runtime plugin root populated by this installer/init container.
    #[arg(long, env = "MTC_PLUGIN_DIR")]
    plugin_dir: PathBuf,

    /// Install into a revision-specific root. Existing package IDs are never
    /// overwritten; use a new inventory ID for each complete rollout inventory.
    #[arg(long)]
    inventory_id: Option<String>,

    /// Internal unpublished revision attempt. Enables the NFS-safe portable
    /// protocol only in a fresh physical root that no runtime can reference.
    #[arg(long, conflicts_with = "inventory_id", hide = true)]
    publication_attempt_id: Option<String>,

    /// Atomically append the host-reviewed complete inventory after installation.
    /// Requires experimental-plugin-revisions; existing IDs cannot be changed.
    #[cfg(feature = "experimental-plugin-revisions")]
    #[arg(long, requires_all = ["inventory_id", "inventory_entry_file"])]
    inventory_file: Option<PathBuf>,

    /// JSON PreinstalledInventory with independently reviewed roots and grants.
    #[cfg(feature = "experimental-plugin-revisions")]
    #[arg(long, requires = "inventory_file")]
    inventory_entry_file: Option<PathBuf>,

    /// Exact allowed registry/repository, for example ghcr.io/memeloop/plugins.
    #[arg(
        long = "allowed-source",
        env = "MTC_PLUGIN_ALLOWED_SOURCES",
        value_delimiter = ',',
        required = true
    )]
    allowed_sources: Vec<String>,

    /// Cosign public key PEM. Repeat during signing-key rotation.
    #[arg(long = "cosign-public-key", required_unless_present = "cosign_certificate_identity", conflicts_with = "cosign_certificate_identity", num_args = 1..=8)]
    cosign_public_keys: Vec<PathBuf>,

    /// Exact GitHub Actions workflow identity (not a regular expression).
    #[arg(
        long,
        requires = "cosign_certificate_oidc_issuer",
        conflicts_with = "cosign_public_keys"
    )]
    cosign_certificate_identity: Option<String>,

    #[arg(long, requires = "cosign_certificate_identity")]
    cosign_certificate_oidc_issuer: Option<String>,

    /// Mounted file containing the Basic-auth username.
    #[arg(long, env = "MTC_PLUGIN_REGISTRY_USERNAME_FILE")]
    registry_username_file: Option<PathBuf>,

    /// Mounted file containing the Basic-auth password/PAT.
    #[arg(long, env = "MTC_PLUGIN_REGISTRY_PASSWORD_FILE")]
    registry_password_file: Option<PathBuf>,

    /// Mounted file containing a registry bearer token; conflicts with Basic auth.
    #[arg(long, env = "MTC_PLUGIN_REGISTRY_BEARER_TOKEN_FILE")]
    registry_bearer_token_file: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let arguments = Arguments::parse();
    let credentials = credentials(&arguments)?;
    let plugin_root = match &arguments.publication_attempt_id {
        Some(attempt) => {
            let attempt = uuid::Uuid::parse_str(attempt)?;
            if attempt.to_string() != *attempt {
                return Err("invalid publication attempt ID".into());
            }
            arguments.plugin_dir.join(format!("mtc-attempt-{attempt}"))
        }
        None => match &arguments.inventory_id {
            Some(id) => {
                if id.is_empty()
                    || id.len() > 64
                    || !id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
                {
                    return Err("invalid inventory ID".into());
                }
                arguments.plugin_dir.join(id)
            }
            None => arguments.plugin_dir.clone(),
        },
    };
    let public_keys = arguments
        .cosign_public_keys
        .iter()
        .map(|path| read_bounded_file(path, "Cosign public key"))
        .collect::<Result<Vec<_>, _>>()?;
    let installed = install_plugin_oci(&InstallPluginOptions {
        reference: arguments.reference,
        plugin_root: plugin_root.clone(),
        allowed_sources: arguments
            .allowed_sources
            .into_iter()
            .collect::<BTreeSet<_>>(),
        credentials,
        cosign_public_keys: public_keys,
        cosign_keyless: arguments
            .cosign_certificate_identity
            .zip(arguments.cosign_certificate_oidc_issuer)
            .map(|(identity, issuer)| CosignKeylessIdentity { issuer, identity }),
        allow_portable_publication: arguments.publication_attempt_id.is_some(),
    })
    .await.map_err(|error| {
        eprintln!("{}", serde_json::json!({"mtc_plugin_install":1,"stage":"install","category":error.diagnostic_category()}));
        "plugin installation failed (see safe diagnostic category)"
    })?;
    #[cfg(feature = "experimental-plugin-revisions")]
    if let (Some(path), Some(entry), Some(id)) = (
        &arguments.inventory_file,
        &arguments.inventory_entry_file,
        &arguments.inventory_id,
    ) {
        register_inventory(path, entry, id, &plugin_root, &installed)?;
    }
    println!("{}", serde_json::to_string(&installed)?);
    Ok(())
}

#[cfg(feature = "experimental-plugin-revisions")]
fn register_inventory(
    path: &std::path::Path,
    entry_path: &std::path::Path,
    id: &str,
    root: &std::path::Path,
    installed: &memeloop_token_center::plugin_distribution::InstalledPlugin,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    use memeloop_token_center::plugin::application::PreinstalledInventory;
    use std::{
        collections::BTreeMap,
        io::{Read, Write},
    };
    const MAX_BYTES: u64 = 4 * 1024 * 1024;
    fn read_json(
        path: &std::path::Path,
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)?
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_BYTES {
            return Err("inventory file too large".into());
        }
        Ok(bytes)
    }
    if !path.is_absolute() || !root.is_absolute() {
        return Err("absolute inventory paths required".into());
    }
    let entry: PreinstalledInventory = serde_json::from_slice(&read_json(entry_path)?)?;
    let manifest = memeloop_token_center::plugin::validate_plugin_package(&installed.path)?;
    let manifest_digest = memeloop_token_center::plugin::lifecycle::manifest_digest(&manifest)?;
    let provenance: memeloop_token_center::plugin::PluginInstallProvenance =
        serde_json::from_slice(&read_json(&installed.path.join(".mtc-oci-install.json"))?)?;
    let component_sha256 = match &manifest.wasm {
        Some(wasm) => {
            use sha2::{Digest, Sha256};
            let mut bytes = Vec::new();
            std::fs::File::open(installed.path.join(wasm))?
                .take(64 * 1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            if bytes.len() > 64 * 1024 * 1024 {
                return Err("plugin component too large".into());
            }
            Some(format!("sha256:{:x}", Sha256::digest(&bytes)))
        }
        None => None,
    };
    if entry.root != root
        || installed.path != root.join(&manifest.id)
        || manifest.id != installed.id
        || manifest.version != installed.version
        || !entry.grants.get(&installed.id).is_some_and(|grants| {
            grants.iter().any(|grant| {
                grant.version == installed.version
                    && grant.manifest_digest == manifest_digest
                    && grant.capabilities == manifest.capabilities
                    && grant.identity.component_sha256 == component_sha256
                    && grant.identity.provenance.as_ref() == Some(&provenance)
                    && grant.identity.provenance.as_ref().is_some_and(|receipt| {
                        receipt.source == installed.source
                            && receipt.digest == installed.digest
                            && matches!(
                                receipt.signature_policy.as_str(),
                                "cosign-public-key" | "cosign-keyless"
                            )
                    })
            })
        })
    {
        return Err("installed package does not match reviewed inventory".into());
    }
    // Serialize appenders with a stable sibling lock inode across atomic rename.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path.with_extension("lock"))?;
    lock.try_lock()?;
    let mut inventory: BTreeMap<String, PreinstalledInventory> = match read_json(path) {
        Ok(bytes) => serde_json::from_slice(&bytes)?,
        Err(error) => return Err(error),
    };
    if let Some(existing) = inventory.get(id) {
        if serde_json::to_value(existing)? == serde_json::to_value(&entry)? {
            return Ok(());
        }
        return Err("inventory ID already registered with different contents".into());
    }
    inventory.insert(id.to_owned(), entry);
    let bytes = serde_json::to_vec_pretty(&inventory)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err("inventory file too large".into());
    }
    let parent = path.parent().ok_or("invalid inventory path")?;
    let permissions = std::fs::metadata(path)?.permissions();
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(&bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path)?;
    std::fs::File::open(parent)?.sync_all()?;
    Ok(())
}

fn credentials(
    arguments: &Arguments,
) -> Result<RegistryCredentials, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(path) = &arguments.registry_bearer_token_file {
        if arguments.registry_username_file.is_some() || arguments.registry_password_file.is_some()
        {
            return Err(
                "bearer-token and Basic registry authentication are mutually exclusive".into(),
            );
        }
        return Ok(RegistryCredentials::Bearer(read_secret(path)?));
    }
    match (
        arguments.registry_username_file.as_ref(),
        arguments.registry_password_file.as_ref(),
    ) {
        (None, None) => Ok(RegistryCredentials::Anonymous),
        (Some(username_path), Some(password_path)) => {
            let username = read_secret(username_path)?;
            if username.contains(':') {
                return Err("registry username must not contain ':'".into());
            }
            Ok(RegistryCredentials::Basic {
                username,
                password: read_secret(password_path)?,
            })
        }
        _ => Err("registry username and password file must be provided together".into()),
    }
}

fn read_secret(path: &PathBuf) -> Result<String, Box<dyn std::error::Error + Send + Sync>> {
    let bytes = read_bounded_file(path, "registry credential")?;
    let value = std::str::from_utf8(&bytes)?.trim_end_matches(['\r', '\n']);
    if value.is_empty() || value.contains('\0') {
        return Err("registry credential file is empty or invalid".into());
    }
    Ok(value.to_owned())
}

fn read_bounded_file(
    path: &PathBuf,
    kind: &str,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SECRET_FILE_BYTES {
        return Err(format!("{kind} file is empty, invalid, or too large").into());
    }
    Ok(std::fs::read(path)?)
}

#[cfg(all(test, feature = "experimental-plugin-revisions"))]
mod inventory_tests {
    use super::*;

    #[test]
    fn reviewed_registration_appends_once_and_rejects_wrong_artifact() {
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("inventory.json");
        let entry_path = temporary.path().join("reviewed.json");
        let root = temporary.path().join("new");
        std::fs::write(&path, b"{}").unwrap();
        let installed = memeloop_token_center::plugin_distribution::InstalledPlugin {
            id: "new-plugin".into(),
            version: "1.0.0".into(),
            digest: format!("sha256:{}", "a".repeat(64)),
            source: "ghcr.io/example/plugins".into(),
            path: root.join("new-plugin"),
        };
        std::fs::create_dir_all(&installed.path).unwrap();
        let manifest: memeloop_token_center::plugin::PluginManifest =
            serde_json::from_value(serde_json::json!({
                "id":"new-plugin", "version":"1.0.0", "wit_version":"0.2.0", "wasm":null,
                "capabilities":[], "contributions":{}
            }))
            .unwrap();
        std::fs::write(
            installed.path.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let manifest_digest =
            memeloop_token_center::plugin::lifecycle::manifest_digest(&manifest).unwrap();
        let entry = serde_json::json!({"root":root,"grants":{"new-plugin":[{
            "version":"1.0.0","capabilities":[],"manifest_digest":manifest_digest,
            "identity":{"component_sha256":null,"provenance":{
                "format_version":1,"source":installed.source,"digest":installed.digest,
                "signature_policy":"cosign-public-key"
            }}
        }]}});
        std::fs::write(
            installed.path.join(".mtc-oci-install.json"),
            serde_json::to_vec(&entry["grants"]["new-plugin"][0]["identity"]["provenance"])
                .unwrap(),
        )
        .unwrap();
        std::fs::write(&entry_path, serde_json::to_vec(&entry).unwrap()).unwrap();
        register_inventory(&path, &entry_path, "new", &root, &installed).unwrap();
        let original = std::fs::read(&path).unwrap();
        register_inventory(&path, &entry_path, "new", &root, &installed).unwrap();
        let mut wrong = installed;
        wrong.digest = format!("sha256:{}", "b".repeat(64));
        assert!(register_inventory(&path, &entry_path, "other", &root, &wrong).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }
}
