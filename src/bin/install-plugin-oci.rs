use std::{collections::BTreeSet, path::PathBuf};

use clap::Parser;
use memeloop_token_center::plugin_distribution::{
    InstallPluginOptions, RegistryCredentials, install_plugin_oci,
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

    /// Exact allowed registry/repository, for example ghcr.io/memeloop/plugins.
    #[arg(
        long = "allowed-source",
        env = "MTC_PLUGIN_ALLOWED_SOURCES",
        value_delimiter = ',',
        required = true
    )]
    allowed_sources: Vec<String>,

    /// Cosign public key PEM. Repeat during signing-key rotation.
    #[arg(long = "cosign-public-key", required = true, num_args = 1..=8)]
    cosign_public_keys: Vec<PathBuf>,

    /// Basic-auth username. The password must come from a mounted file.
    #[arg(long, env = "MTC_PLUGIN_REGISTRY_USERNAME")]
    registry_username: Option<String>,

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
    let public_keys = arguments
        .cosign_public_keys
        .iter()
        .map(|path| read_bounded_file(path, "Cosign public key"))
        .collect::<Result<Vec<_>, _>>()?;
    let installed = install_plugin_oci(&InstallPluginOptions {
        reference: arguments.reference,
        plugin_root: arguments.plugin_dir,
        allowed_sources: arguments
            .allowed_sources
            .into_iter()
            .collect::<BTreeSet<_>>(),
        credentials,
        cosign_public_keys: public_keys,
    })
    .await?;
    println!("{}", serde_json::to_string(&installed)?);
    Ok(())
}

fn credentials(
    arguments: &Arguments,
) -> Result<RegistryCredentials, Box<dyn std::error::Error + Send + Sync>> {
    if let Some(path) = &arguments.registry_bearer_token_file {
        if arguments.registry_username.is_some() || arguments.registry_password_file.is_some() {
            return Err(
                "bearer-token and Basic registry authentication are mutually exclusive".into(),
            );
        }
        return Ok(RegistryCredentials::Bearer(read_secret(path)?));
    }
    match (
        arguments.registry_username.as_deref(),
        arguments.registry_password_file.as_ref(),
    ) {
        (None, None) => Ok(RegistryCredentials::Anonymous),
        (Some(username), Some(path)) if !username.is_empty() => Ok(RegistryCredentials::Basic {
            username: username.to_owned(),
            password: read_secret(path)?,
        }),
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
