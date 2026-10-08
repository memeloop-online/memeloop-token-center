use std::{
    collections::{BTreeMap, BTreeSet},
    fs::File,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::ffi::OsStrExt},
    path::{Component, Path, PathBuf},
};

use rustix::fs::{Mode, OFlags, mkdirat, openat};
use serde::{Deserialize, Serialize};

use super::{DescriptorInventory, PluginInventoryDescriptor, PreinstalledInventory};
use crate::{
    error::AppError,
    plugin::{PluginRuntime, lifecycle, plugin_configuration_schema_digest},
    plugin_distribution::{InstallPluginOptions, RegistryCredentials, install_plugin_oci},
    provider::ProviderCatalog,
};

const MAX_BYTES: usize = PluginInventoryDescriptor::MAX_PARSE_BYTES * 2;
const INVENTORY_FILE: &str = "inventory.json";
const RECEIPT_FILE: &str = "receipt.json";

pub struct DescriptorImportOptions {
    pub descriptor_file: PathBuf,
    pub expected_digest: String,
    pub plugin_dir: PathBuf,
    pub output_dir: PathBuf,
    pub previous_bundle_dir: Option<PathBuf>,
    pub previous_inventory_file: Option<PathBuf>,
    pub installation: InstallPluginOptions,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorImportReceipt {
    pub format_version: u8,
    pub descriptor_digest: String,
    pub descriptor: PluginInventoryDescriptor,
    pub verified: BTreeMap<String, InventoryDigests>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryDigests {
    pub identity_digest: String,
    pub contract_digest: String,
}

pub async fn import_plugin_descriptor(
    options: &DescriptorImportOptions,
) -> Result<DescriptorImportReceipt, AppError> {
    let bytes = read_path(
        &options.descriptor_file,
        PluginInventoryDescriptor::MAX_PARSE_BYTES,
    )?;
    let descriptor = PluginInventoryDescriptor::parse_expected(&bytes, &options.expected_digest)?;
    validate_credential_scope(&descriptor, &options.installation.credentials)?;
    validate_paths(&descriptor, &options.plugin_dir, &options.output_dir)?;
    let directory = storage(crate::plugin_publication::directory_fd(&options.plugin_dir))?;
    let lock = File::from(storage(openat(
        &directory,
        ".mtc-descriptor-import.lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::from_bits_truncate(0o600),
    ))?);
    if !storage(lock.metadata())?.is_file() {
        return Err(AppError::Forbidden);
    }
    storage(lock.try_lock())?;
    if let Some(previous) = &options.previous_bundle_dir {
        direct_child(&options.plugin_dir, previous)?;
        let prior_directory = storage(crate::plugin_publication::directory_fd(previous))?;
        let (prior, inventory) = read_bundle(&prior_directory)?;
        descriptor.validate_extension_of(&prior.descriptor)?;
        descriptor.validate_retains_inventory(&inventory)?;
    }
    if let Some(previous) = &options.previous_inventory_file {
        let inventory = serde_json::from_slice(&read_path(previous, MAX_BYTES)?)
            .map_err(|_| AppError::Forbidden)?;
        descriptor.validate_retains_inventory(&inventory)?;
    }
    let mut verified = BTreeMap::new();
    for (inventory_id, entry) in &descriptor.inventories {
        let name = direct_child(&options.plugin_dir, &entry.inventory.root)?;
        match mkdirat(&directory, name, Mode::from_bits_truncate(0o755)) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(_) => return Err(AppError::Internal),
        }
        let root = directory_at(&directory, name)?;
        for (plugin_id, reference) in entry.package_references()? {
            same_directory(&directory, &options.plugin_dir)?;
            same_directory(&root, &entry.inventory.root)?;
            let mut installation = options.installation.clone();
            installation.reference = reference;
            let workspace = format!(".mtc-plugin-staging-import-{}", uuid::Uuid::now_v7());
            storage(mkdirat(
                &root,
                workspace.as_str(),
                Mode::from_bits_truncate(0o700),
            ))?;
            let staging = directory_at(&root, std::ffi::OsStr::new(&workspace))?;
            installation.plugin_root =
                PathBuf::from(format!("/proc/self/fd/{}/.", staging.as_raw_fd()));
            installation.allow_portable_publication = false;
            let installed = install_plugin_oci(&installation)
                .await
                .map_err(|_| AppError::Forbidden)?;
            if installed.id != plugin_id
                || installed.path != installation.plugin_root.join(&plugin_id)
            {
                return Err(AppError::Forbidden);
            }
            promote_verified_package(&staging, &root, &plugin_id)?;
            storage(rustix::fs::unlinkat(
                &root,
                workspace.as_str(),
                rustix::fs::AtFlags::REMOVEDIR,
            ))?;
            same_directory(&root, &entry.inventory.root)?;
        }
        same_directory(&root, &entry.inventory.root)?;
        let expected = entry.clone();
        let digests = tokio::task::spawn_blocking(move || {
            let path = format!("/proc/self/fd/{}/.", root.as_raw_fd());
            validate_inventory(&expected, &path)
        })
        .await
        .map_err(|_| AppError::Internal)??;
        verified.insert(inventory_id.clone(), digests);
    }
    same_directory(&directory, &options.plugin_dir)?;
    let receipt = DescriptorImportReceipt {
        format_version: 1,
        descriptor_digest: options.expected_digest.clone(),
        descriptor,
        verified,
    };
    publish_bundle(
        &directory,
        &options.plugin_dir,
        &options.output_dir,
        &receipt,
    )?;
    Ok(receipt)
}

fn validate_credential_scope(
    descriptor: &PluginInventoryDescriptor,
    credentials: &RegistryCredentials,
) -> Result<(), AppError> {
    if matches!(credentials, RegistryCredentials::Anonymous) {
        return Ok(());
    }
    let mut registries = BTreeSet::new();
    for entry in descriptor.inventories.values() {
        for reference in entry.package_references()?.values() {
            let reference: oci_client::Reference =
                reference.parse().map_err(|_| AppError::Forbidden)?;
            registries.insert(reference.registry().to_owned());
            if registries.len() > 1 {
                return Err(AppError::Forbidden);
            }
        }
    }
    Ok(())
}

fn validate_inventory(
    entry: &DescriptorInventory,
    root: &str,
) -> Result<InventoryDigests, AppError> {
    let runtime = PluginRuntime::load_for_inventory(root)?;
    lifecycle::validate_grants(&runtime, &entry.inventory.grants)?;
    if runtime.package_identities() != entry.packages {
        return Err(AppError::Forbidden);
    }
    let mut providers = ProviderCatalog::builtins();
    providers.extend(runtime.provider_types())?;
    let actual = InventoryDigests {
        identity_digest: plugin_configuration_schema_digest(&serde_json::json!({
            "manifests": runtime.manifests(), "identities": runtime.package_identities()
        }))?,
        contract_digest: super::contract_digest(&runtime)?,
    };
    if actual.identity_digest != entry.identity_digest
        || actual.contract_digest != entry.contract_digest
    {
        return Err(AppError::Forbidden);
    }
    Ok(actual)
}

fn package_tree(directory: &rustix::fd::OwnedFd) -> Result<BTreeMap<PathBuf, Vec<u8>>, AppError> {
    let mut pending = vec![(
        PathBuf::new(),
        File::from(directory_at(directory, std::ffi::OsStr::new("."))?),
    )];
    let mut result = BTreeMap::new();
    let mut total = 0usize;
    let mut entries = 0usize;
    while let Some((relative, file)) = pending.pop() {
        if relative.as_os_str().len() > 240 || entries > 16384 {
            return Err(AppError::Forbidden);
        }
        if storage(file.metadata())?.is_dir() {
            for child in storage(rustix::fs::Dir::read_from(&file))? {
                let child = storage(child)?;
                let name = child.file_name().to_bytes();
                if name == b"." || name == b".." {
                    continue;
                }
                entries += 1;
                let name = std::ffi::OsStr::from_bytes(name);
                let opened = File::from(storage(openat(
                    &file,
                    name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                    Mode::empty(),
                ))?);
                pending.push((relative.join(name), opened));
            }
            result.insert(relative, Vec::new());
        } else {
            let bytes = bounded_read(file, (81 * 1024 * 1024usize).saturating_sub(total))?;
            total += bytes.len();
            let mut tagged = vec![1];
            tagged.extend(bytes);
            result.insert(relative, tagged);
        }
    }
    Ok(result)
}

fn promote_verified_package(
    staging: &rustix::fd::OwnedFd,
    root: &rustix::fd::OwnedFd,
    plugin_id: &str,
) -> Result<(), AppError> {
    #[cfg(target_os = "linux")]
    match rustix::fs::renameat_with(
        staging,
        plugin_id,
        root,
        plugin_id,
        rustix::fs::RenameFlags::NOREPLACE,
    ) {
        Ok(()) => {}
        Err(rustix::io::Errno::EXIST) => {
            let verified = directory_at(staging, std::ffi::OsStr::new(plugin_id))?;
            let existing = directory_at(root, std::ffi::OsStr::new(plugin_id))?;
            let bytes = package_tree(&verified)?;
            if bytes != package_tree(&existing)? {
                return Err(AppError::Forbidden);
            }
            for (path, bytes) in bytes.iter().rev() {
                if path.as_os_str().is_empty() {
                    continue;
                }
                storage(rustix::fs::unlinkat(
                    &verified,
                    path,
                    if bytes.is_empty() {
                        rustix::fs::AtFlags::REMOVEDIR
                    } else {
                        rustix::fs::AtFlags::empty()
                    },
                ))?;
            }
            storage(rustix::fs::unlinkat(
                staging,
                plugin_id,
                rustix::fs::AtFlags::REMOVEDIR,
            ))?;
        }
        Err(_) => return Err(AppError::Internal),
    }
    #[cfg(not(target_os = "linux"))]
    return Err(AppError::Forbidden);
    storage(rustix::fs::fsync(root))?;
    storage(rustix::fs::fsync(staging))
}

fn validate_paths(
    descriptor: &PluginInventoryDescriptor,
    plugin_dir: &Path,
    output: &Path,
) -> Result<(), AppError> {
    if !plugin_dir.is_absolute()
        || plugin_dir
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(AppError::Forbidden);
    }
    direct_child(plugin_dir, output)?;
    for entry in descriptor.inventories.values() {
        let name = direct_child(plugin_dir, &entry.inventory.root)?;
        if entry.inventory.root == output || name.to_string_lossy().starts_with('.') {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

fn direct_child<'path>(
    parent: &Path,
    child: &'path Path,
) -> Result<&'path std::ffi::OsStr, AppError> {
    if child.parent() != Some(parent)
        || child
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(AppError::Forbidden);
    }
    let name = child.file_name().ok_or(AppError::Forbidden)?;
    if name.to_string_lossy().starts_with('.') {
        return Err(AppError::Forbidden);
    }
    Ok(name)
}

fn same_directory(directory: &impl std::os::fd::AsFd, path: &Path) -> Result<(), AppError> {
    let current = storage(crate::plugin_publication::directory_fd(path))?;
    let expected = storage(rustix::fs::fstat(directory))?;
    let actual = storage(rustix::fs::fstat(&current))?;
    if expected.st_dev != actual.st_dev || expected.st_ino != actual.st_ino {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn directory_at(
    parent: &impl std::os::fd::AsFd,
    name: &std::ffi::OsStr,
) -> Result<rustix::fd::OwnedFd, AppError> {
    storage(openat(
        parent,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ))
}

fn read_path(path: &Path, maximum: usize) -> Result<Vec<u8>, AppError> {
    bounded_read(storage(File::open(path))?, maximum)
}

fn bounded_read(file: File, maximum: usize) -> Result<Vec<u8>, AppError> {
    if !storage(file.metadata())?.is_file() {
        return Err(AppError::Forbidden);
    }
    let mut bytes = Vec::new();
    storage(file.take(maximum as u64 + 1).read_to_end(&mut bytes))?;
    if bytes.len() > maximum {
        return Err(AppError::Forbidden);
    }
    Ok(bytes)
}

fn read_at(directory: &impl std::os::fd::AsFd, name: &str) -> Result<Vec<u8>, AppError> {
    bounded_read(
        File::from(storage(openat(
            directory,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
            Mode::empty(),
        ))?),
        MAX_BYTES,
    )
}

fn inventory_map(
    descriptor: &PluginInventoryDescriptor,
) -> BTreeMap<String, PreinstalledInventory> {
    descriptor
        .inventories
        .iter()
        .map(|(id, entry)| (id.clone(), entry.inventory.clone()))
        .collect()
}

fn read_bundle(
    directory: &impl std::os::fd::AsFd,
) -> Result<
    (
        DescriptorImportReceipt,
        BTreeMap<String, PreinstalledInventory>,
    ),
    AppError,
> {
    let receipt: DescriptorImportReceipt =
        serde_json::from_slice(&read_at(directory, RECEIPT_FILE)?)
            .map_err(|_| AppError::Forbidden)?;
    let inventory: BTreeMap<String, PreinstalledInventory> =
        serde_json::from_slice(&read_at(directory, INVENTORY_FILE)?)
            .map_err(|_| AppError::Forbidden)?;
    if receipt.format_version != 1
        || receipt.descriptor.digest()? != receipt.descriptor_digest
        || encoded(&inventory)? != encoded(&inventory_map(&receipt.descriptor))?
        || receipt.verified.len() != receipt.descriptor.inventories.len()
        || receipt.descriptor.inventories.iter().any(|(id, entry)| {
            receipt.verified.get(id).is_none_or(|actual| {
                actual.identity_digest != entry.identity_digest
                    || actual.contract_digest != entry.contract_digest
            })
        })
    {
        return Err(AppError::Forbidden);
    }
    Ok((receipt, inventory))
}

fn encoded(value: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let bytes = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    if bytes.len() > MAX_BYTES {
        return Err(AppError::Forbidden);
    }
    Ok(bytes)
}

fn write_at(directory: &impl std::os::fd::AsFd, name: &str, bytes: &[u8]) -> Result<(), AppError> {
    let mut file = File::from(storage(openat(
        directory,
        name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o644),
    ))?);
    storage(file.write_all(bytes))?;
    storage(file.sync_all())
}

fn publish_bundle(
    directory: &impl std::os::fd::AsFd,
    plugin_dir: &Path,
    output: &Path,
    receipt: &DescriptorImportReceipt,
) -> Result<(), AppError> {
    let name = direct_child(plugin_dir, output)?;
    let inventory = encoded(&inventory_map(&receipt.descriptor))?;
    let receipt_bytes = encoded(receipt)?;
    match openat(
        directory,
        name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(existing) => {
            let (prior, _) = read_bundle(&existing)?;
            if encoded(&prior)? != receipt_bytes {
                return Err(AppError::Forbidden);
            }
            return Ok(());
        }
        Err(rustix::io::Errno::NOENT) => {}
        Err(_) => return Err(AppError::Forbidden),
    }
    let staging_name = format!(".mtc-descriptor-partial-{}", uuid::Uuid::now_v7());
    storage(mkdirat(
        directory,
        staging_name.as_str(),
        Mode::from_bits_truncate(0o755),
    ))?;
    let staging = directory_at(directory, std::ffi::OsStr::new(&staging_name))?;
    write_at(&staging, INVENTORY_FILE, &inventory)?;
    write_at(&staging, RECEIPT_FILE, &receipt_bytes)?;
    storage(rustix::fs::fsync(&staging))?;
    #[cfg(target_os = "linux")]
    storage(rustix::fs::renameat_with(
        directory,
        staging_name.as_str(),
        directory,
        name,
        rustix::fs::RenameFlags::NOREPLACE,
    ))?;
    #[cfg(not(target_os = "linux"))]
    return Err(AppError::Forbidden);
    storage(rustix::fs::fsync(directory))
}

fn storage<T, Error>(result: Result<T, Error>) -> Result<T, AppError> {
    result.map_err(|_| AppError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_descriptor(root: PathBuf) -> PluginInventoryDescriptor {
        PluginInventoryDescriptor {
            format_version: 1,
            inventories: BTreeMap::from([(
                "empty".into(),
                DescriptorInventory {
                    inventory: PreinstalledInventory {
                        root,
                        grants: BTreeMap::new(),
                    },
                    packages: BTreeMap::new(),
                    identity_digest: plugin_configuration_schema_digest(&serde_json::json!({
                        "manifests": [], "identities": {}
                    }))
                    .unwrap(),
                    contract_digest: plugin_configuration_schema_digest(&serde_json::json!({}))
                        .unwrap(),
                },
            )]),
        }
    }

    fn options(directory: &Path) -> DescriptorImportOptions {
        let descriptor = empty_descriptor(directory.join("empty"));
        let descriptor_file = directory.join("input.json");
        std::fs::write(&descriptor_file, encoded(&descriptor).unwrap()).unwrap();
        DescriptorImportOptions {
            descriptor_file,
            expected_digest: descriptor.digest().unwrap(),
            plugin_dir: directory.into(),
            output_dir: directory.join("bundle"),
            previous_bundle_dir: None,
            previous_inventory_file: None,
            installation: InstallPluginOptions {
                reference: String::new(),
                plugin_root: directory.into(),
                allowed_sources: Default::default(),
                credentials: Default::default(),
                cosign_public_keys: Vec::new(),
                cosign_keyless: None,
                allow_portable_publication: false,
            },
        }
    }

    fn registry_descriptor(root: &Path, sources: [&str; 2]) -> PluginInventoryDescriptor {
        let mut descriptor = PluginInventoryDescriptor {
            format_version: 1,
            inventories: BTreeMap::new(),
        };
        for (index, source) in sources.into_iter().enumerate() {
            let inventory_id = format!("inventory-{index}");
            let identity = crate::plugin::PluginPackageIdentity {
                component_sha256: None,
                provenance: Some(crate::plugin::PluginInstallProvenance {
                    format_version: 1,
                    source: source.into(),
                    digest: format!("sha256:{}", "a".repeat(64)),
                    signature_policy: "cosign-public-key".into(),
                }),
            };
            descriptor.inventories.insert(
                inventory_id.clone(),
                DescriptorInventory {
                    inventory: PreinstalledInventory {
                        root: root.join(inventory_id),
                        grants: BTreeMap::from([(
                            "package".into(),
                            vec![lifecycle::PluginGrant {
                                version: "1.0.0".into(),
                                capabilities: Vec::new(),
                                manifest_digest: "b".repeat(64),
                                identity: identity.clone(),
                            }],
                        )]),
                    },
                    packages: BTreeMap::from([("package".into(), identity)]),
                    identity_digest: "c".repeat(64),
                    contract_digest: "d".repeat(64),
                },
            );
        }
        descriptor
    }

    #[tokio::test]
    async fn authenticated_multi_registry_descriptor_fails_before_install_or_lock() {
        for credentials in [
            RegistryCredentials::Basic {
                username: "fixture-user".into(),
                password: "fixture-only".into(),
            },
            RegistryCredentials::Bearer("fixture-only".into()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let mut options = options(root.path());
            let descriptor = registry_descriptor(
                root.path(),
                ["first.invalid/package", "second.invalid/package"],
            );
            options.expected_digest = descriptor.digest().unwrap();
            options.installation.credentials = credentials;
            std::fs::write(&options.descriptor_file, encoded(&descriptor).unwrap()).unwrap();
            assert!(matches!(
                import_plugin_descriptor(&options).await,
                Err(AppError::Forbidden)
            ));
            assert!(!root.path().join(".mtc-descriptor-import.lock").exists());
            assert!(!root.path().join("inventory-0").exists());
            assert!(!options.output_dir.exists());
        }
    }

    #[test]
    fn anonymous_multi_registry_and_authenticated_single_registry_preserve_policy() {
        let multiple = registry_descriptor(
            Path::new("/plugins"),
            ["first.invalid/package", "second.invalid/package"],
        );
        validate_credential_scope(&multiple, &RegistryCredentials::Anonymous).unwrap();
        let same = registry_descriptor(
            Path::new("/plugins"),
            ["first.invalid/package", "first.invalid/other-package"],
        );
        validate_credential_scope(&same, &RegistryCredentials::Bearer("fixture-only".into()))
            .unwrap();
        let ports = registry_descriptor(
            Path::new("/plugins"),
            ["first.invalid:443/package", "first.invalid:8443/package"],
        );
        assert!(
            validate_credential_scope(&ports, &RegistryCredentials::Bearer("fixture-only".into()))
                .is_err()
        );
    }

    #[tokio::test]
    #[ignore = "Existing GHA signed-Claude step provides its pinned image, real Cosign and fixture"]
    async fn privacy_wire_shim_release_component_descriptor_import_roundtrip() {
        const CHILD_MARKER: &str = "MTC_ID64_SIGNED_IMPORT_CHILD";
        if std::env::var_os(CHILD_MARKER).is_none() {
            run_signed_import_with_existing_cosign(CHILD_MARKER).await;
            return;
        }
        let fixture =
            PathBuf::from(std::env::var("MTC_CLAUDE_WIRE_FIXTURE").expect("GHA signed fixture"));
        let fixture_runtime =
            PluginRuntime::load_for_inventory(fixture.parent().unwrap().to_str().unwrap()).unwrap();
        let manifests = fixture_runtime.manifests();
        assert_eq!(manifests.len(), 1);
        assert_eq!(manifests[0].id, "claude-code-wire");
        let identities = fixture_runtime.package_identities();
        let identity = identities["claude-code-wire"].clone();
        assert!(identity.component_sha256.is_some());
        let provenance = identity.provenance.as_ref().unwrap();
        assert_eq!(provenance.source, std::env::var("PLUGIN_SOURCE").unwrap());
        assert_eq!(provenance.digest, std::env::var("PLUGIN_DIGEST").unwrap());
        assert_eq!(provenance.signature_policy, "cosign-keyless");
        let root = tempfile::tempdir().unwrap();
        let mut options = options(root.path());
        let entry = DescriptorInventory {
            inventory: PreinstalledInventory {
                root: root.path().join("signed"),
                grants: BTreeMap::from([("claude-code-wire".into(), vec![lifecycle::PluginGrant {
                    version: manifests[0].version.clone(),
                    capabilities: manifests[0].capabilities.clone(),
                    manifest_digest: lifecycle::manifest_digest(&manifests[0]).unwrap(),
                    identity: identity.clone(),
                }])]),
            },
            packages: identities,
            identity_digest: plugin_configuration_schema_digest(&serde_json::json!({
                "manifests": fixture_runtime.manifests(), "identities": fixture_runtime.package_identities()
            })).unwrap(),
            contract_digest: super::super::contract_digest(&fixture_runtime).unwrap(),
        };
        let descriptor = PluginInventoryDescriptor {
            format_version: 1,
            inventories: BTreeMap::from([("signed".into(), entry)]),
        };
        options.expected_digest = descriptor.digest().unwrap();
        options
            .installation
            .allowed_sources
            .insert(provenance.source.clone());
        options.installation.cosign_keyless = Some(crate::plugin::CosignKeylessIdentity {
            identity: std::env::var("SIGNING_IDENTITY").unwrap(),
            issuer: std::env::var("SIGNING_ISSUER").unwrap(),
        });
        std::fs::write(&options.descriptor_file, encoded(&descriptor).unwrap()).unwrap();
        let first = import_plugin_descriptor(&options)
            .await
            .expect("real signed nonempty first import");
        let first_bytes = std::fs::read(options.output_dir.join(RECEIPT_FILE)).unwrap();
        assert_eq!(first.verified.len(), 1);
        assert_eq!(
            first.verified["signed"].identity_digest,
            descriptor.inventories["signed"].identity_digest
        );
        let second = import_plugin_descriptor(&options)
            .await
            .expect("real signed nonempty authoritative reimport");
        assert_eq!(encoded(&first).unwrap(), encoded(&second).unwrap());
        assert_eq!(
            first_bytes,
            std::fs::read(options.output_dir.join(RECEIPT_FILE)).unwrap()
        );
        let installed = root.path().join("signed/claude-code-wire");
        for filename in ["plugin.json", "plugin.wasm", ".mtc-oci-install.json"] {
            assert_eq!(
                std::fs::read(installed.join(filename)).unwrap(),
                std::fs::read(fixture.join(filename)).unwrap()
            );
        }
        std::fs::write(
            root.path().join("signed/plugin.json"),
            std::fs::read(installed.join("plugin.json")).unwrap(),
        )
        .unwrap();
        assert!(
            validate_inventory(
                &descriptor.inventories["signed"],
                root.path().join("signed").to_str().unwrap()
            )
            .is_err()
        );
    }

    async fn run_signed_import_with_existing_cosign(marker: &str) {
        use std::process::Stdio;
        use std::time::Duration;
        let directory = tempfile::tempdir().unwrap();
        let image = format!(
            "{}@{}",
            std::env::var("INSTALLER_SOURCE").expect("existing GHA pinned installer source"),
            std::env::var("INSTALLER_DIGEST").expect("existing GHA pinned installer digest")
        );
        let container = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new("docker")
                .args([
                    "create",
                    "--entrypoint",
                    "/usr/local/bin/cosign",
                    &image,
                    "version",
                    "--json",
                ])
                .stdin(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            container.status.success(),
            "create existing pinned CI fixture container"
        );
        let container = String::from_utf8(container.stdout).unwrap();
        let container = container.trim();
        assert!(!container.is_empty() && container.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let copy = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new("docker")
                .args(["cp", &format!("{container}:/usr/local/bin/cosign")])
                .arg(directory.path().join("cosign"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await;
        let removed = tokio::time::timeout(
            Duration::from_secs(60),
            tokio::process::Command::new("docker")
                .args(["rm", container])
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .status(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            removed.success(),
            "remove only this test's created container"
        );
        assert!(
            copy.unwrap().unwrap().success(),
            "copy real fixed Cosign companion"
        );
        let executable = directory.path().join("descriptor-import-tests");
        let current = std::env::current_exe().unwrap();
        if std::fs::hard_link(&current, &executable).is_err() {
            std::fs::copy(&current, &executable).unwrap();
        }
        let status = tokio::time::timeout(Duration::from_secs(600), tokio::process::Command::new(executable)
            .args(["--ignored", "--exact", "plugin::application::descriptor_import::tests::privacy_wire_shim_release_component_descriptor_import_roundtrip", "--nocapture"])
            .env(marker, "1").stdin(Stdio::null()).kill_on_drop(true).status()).await.unwrap().unwrap();
        assert!(
            status.success(),
            "real signed descriptor import/reimport subprocess"
        );
    }

    #[tokio::test]
    async fn empty_bundle_is_atomic_and_identical_init_is_idempotent() {
        let directory = tempfile::tempdir().unwrap();
        let options = options(directory.path());
        let first = import_plugin_descriptor(&options).await.unwrap();
        let original = std::fs::read(options.output_dir.join(RECEIPT_FILE)).unwrap();
        let second = import_plugin_descriptor(&options).await.unwrap();
        assert_eq!(encoded(&first).unwrap(), encoded(&second).unwrap());
        assert_eq!(
            original,
            std::fs::read(options.output_dir.join(RECEIPT_FILE)).unwrap()
        );
        let published = crate::plugin_publication::directory_fd(&options.output_dir).unwrap();
        read_bundle(&published).unwrap();
    }

    #[tokio::test]
    async fn wrong_digest_and_runtime_receipt_never_publish_and_retry_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let mut options = options(directory.path());
        let expected = options.expected_digest.clone();
        options.expected_digest = "a".repeat(64);
        assert!(import_plugin_descriptor(&options).await.is_err());
        assert!(!options.output_dir.exists());
        let mut wrong = empty_descriptor(directory.path().join("empty"));
        wrong.inventories.get_mut("empty").unwrap().identity_digest = "b".repeat(64);
        options.expected_digest = wrong.digest().unwrap();
        std::fs::write(&options.descriptor_file, encoded(&wrong).unwrap()).unwrap();
        assert!(import_plugin_descriptor(&options).await.is_err());
        assert!(!options.output_dir.exists());
        std::fs::write(
            &options.descriptor_file,
            encoded(&empty_descriptor(directory.path().join("empty"))).unwrap(),
        )
        .unwrap();
        options.expected_digest = expected;
        import_plugin_descriptor(&options).await.unwrap();
    }

    #[tokio::test]
    async fn symlinks_partial_bundle_and_extra_packages_are_not_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let options = options(directory.path());
        std::os::unix::fs::symlink(external.path(), directory.path().join("empty")).unwrap();
        assert!(import_plugin_descriptor(&options).await.is_err());
        assert!(!options.output_dir.exists());
        std::fs::remove_file(directory.path().join("empty")).unwrap();
        std::fs::create_dir(&options.output_dir).unwrap();
        std::fs::write(options.output_dir.join(INVENTORY_FILE), b"{}").unwrap();
        assert!(import_plugin_descriptor(&options).await.is_err());
        assert!(!options.output_dir.join(RECEIPT_FILE).exists());
        std::fs::remove_dir_all(&options.output_dir).unwrap();
        std::fs::create_dir(directory.path().join("empty").join("extra")).unwrap();
        assert!(import_plugin_descriptor(&options).await.is_err());
        assert!(!options.output_dir.exists());
    }

    #[test]
    fn roots_cannot_escape_overlap_or_use_partial_names() {
        let descriptor = empty_descriptor("/plugins/empty".into());
        for output in [
            "/elsewhere/bundle",
            "/plugins/empty",
            "/plugins/.partial",
            "/plugins/deep/bundle",
        ] {
            assert!(validate_paths(&descriptor, Path::new("/plugins"), Path::new(output)).is_err());
        }
        assert!(direct_child(Path::new("/plugins"), Path::new("/plugins/../escape")).is_err());
    }

    #[test]
    fn promotion_compares_whole_verified_tree_not_only_receipt() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        let target = temporary.path().join("target");
        for root in [&source, &target] {
            std::fs::create_dir_all(root.join("package")).unwrap();
            std::fs::write(root.join("package/receipt"), b"same receipt").unwrap();
            std::fs::write(root.join("package/asset"), b"verified bytes").unwrap();
        }
        let source_fd = crate::plugin_publication::directory_fd(&source).unwrap();
        let target_fd = crate::plugin_publication::directory_fd(&target).unwrap();
        std::fs::write(target.join("package/asset"), b"different bytes").unwrap();
        assert!(promote_verified_package(&source_fd, &target_fd, "package").is_err());
        assert!(source.join("package").exists());
        std::fs::write(target.join("package/asset"), b"verified bytes").unwrap();
        promote_verified_package(&source_fd, &target_fd, "package").unwrap();
        assert!(!source.join("package").exists());
        assert_eq!(
            std::fs::read(target.join("package/asset")).unwrap(),
            b"verified bytes"
        );
    }
}
