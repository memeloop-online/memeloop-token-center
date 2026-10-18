//! GitHub release metadata is untrusted discovery data. A resolved reference
//! still needs explicit operator trust confirmation and ordinary OCI signature
//! verification before it can become an installation or execution authority.
use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{error::AppError, plugin::CosignKeylessIdentity};

const MAX_RELEASE_BYTES: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 4;
const RELEASE_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginReleaseDescriptor {
    pub format_version: u8,
    pub reference: String,
    pub signer: CosignKeylessIdentity,
}

#[derive(Deserialize, Serialize)]
pub struct ResolvedPluginRelease {
    pub release_url: String,
    pub descriptor_sha256: String,
    pub source: String,
    pub descriptor: PluginReleaseDescriptor,
}

/// Borrowed credentials must never be copied into responses, receipts or argv.
pub struct ReleaseProxy<'a> {
    pub url: &'a str,
    pub username: Option<&'a str>,
    pub password: Option<&'a str>,
    pub host: Option<&'a str>,
    pub addresses: &'a [SocketAddr],
}

fn invalid_release() -> AppError {
    AppError::BadRequest("invalid GitHub plugin release asset".into())
}

fn secure_url(url: &Url) -> bool {
    url.scheme() == "https"
        && url.port().is_none()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none()
}

/// Accept explicit release assets; repository pages and arbitrary URLs are not
/// install sources. Latest links are resolved once and pinned by descriptor
/// hash plus the contained immutable OCI digest during review.
pub fn validate_github_release_url(value: &str) -> Result<Url, AppError> {
    let url = Url::parse(value).map_err(|_| invalid_release())?;
    if value.len() > 2048
        || !secure_url(&url)
        || url.host_str() != Some("github.com")
        || url.query().is_some()
    {
        return Err(invalid_release());
    }
    let segments: Vec<_> = url.path_segments().ok_or_else(invalid_release)?.collect();
    let asset = match segments.as_slice() {
        [owner, repository, "releases", "download", tag, asset]
            if safe_segment(owner) && safe_segment(repository) && safe_segment(tag) =>
        {
            *asset
        }
        [owner, repository, "releases", "latest", "download", asset]
            if safe_segment(owner) && safe_segment(repository) =>
        {
            *asset
        }
        _ => return Err(invalid_release()),
    };
    if !safe_segment(asset) || !asset.ends_with(".json") {
        return Err(invalid_release());
    }
    Ok(url)
}

fn safe_segment(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
}

fn allowed_redirect(url: &Url) -> bool {
    if !secure_url(url) {
        return false;
    }
    match url.host_str() {
        Some("github.com") => validate_github_release_url(url.as_str()).is_ok(),
        Some("release-assets.githubusercontent.com" | "objects.githubusercontent.com") => true,
        _ => false,
    }
}

fn parse_release(bytes: &[u8], original: &Url) -> Result<ResolvedPluginRelease, AppError> {
    if bytes.len() > MAX_RELEASE_BYTES {
        return Err(invalid_release());
    }
    let descriptor: PluginReleaseDescriptor =
        serde_json::from_slice(bytes).map_err(|_| invalid_release())?;
    let (source, digest) = descriptor
        .reference
        .rsplit_once("@sha256:")
        .ok_or_else(invalid_release)?;
    if descriptor.format_version != 1
        || !descriptor.signer.valid()
        || source.len() > 1024
        || !source.starts_with("ghcr.io/")
        || source.contains('@')
        || source.contains(':')
        || source.split('/').any(|part| !safe_segment(part))
        || digest.len() != 64
        || !digest.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(invalid_release());
    }
    // A release can propose only a signing workflow belonging to its own
    // repository. This remains a claim for the operator to confirm, not trust.
    let segments: Vec<_> = original
        .path_segments()
        .ok_or_else(invalid_release)?
        .collect();
    let prefix = format!(
        "https://github.com/{}/{}/.github/workflows/",
        segments[0], segments[1]
    );
    if !descriptor.signer.identity.starts_with(&prefix) {
        return Err(invalid_release());
    }
    Ok(ResolvedPluginRelease {
        release_url: original.to_string(),
        descriptor_sha256: format!("sha256:{:x}", Sha256::digest(bytes)),
        source: source.to_owned(),
        descriptor,
    })
}

pub async fn resolve_github_release(
    value: &str,
    proxy: Option<ReleaseProxy<'_>>,
) -> Result<ResolvedPluginRelease, AppError> {
    let original = validate_github_release_url(value)?;
    tokio::time::timeout(RELEASE_TIMEOUT, async {
        let mut current = original.clone();
        for hop in 0..=MAX_REDIRECTS {
            let host = current.host_str().ok_or_else(invalid_release)?;
            let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 443))
                .await
                .map_err(|_| invalid_release())?
                .take(33)
                .collect();
            if addresses.is_empty()
                || addresses.len() > 32
                || addresses
                    .iter()
                    .any(|address| !crate::network::is_public_ip(address.ip()))
            {
                return Err(invalid_release());
            }
            let mut builder = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(10))
                .timeout(RELEASE_TIMEOUT);
            if let Some(proxy) = &proxy {
                crate::network::checked_installer_proxy_url(proxy.url)
                    .map_err(|_| invalid_release())?;
                let mut configured =
                    reqwest::Proxy::all(proxy.url).map_err(|_| invalid_release())?;
                match (proxy.username, proxy.password) {
                    (Some(username), Some(password)) => {
                        configured = configured.basic_auth(username, password)
                    }
                    (None, None) => (),
                    _ => return Err(invalid_release()),
                }
                builder = builder.proxy(configured);
                if let Some(host) = proxy.host {
                    if proxy.addresses.is_empty() {
                        return Err(invalid_release());
                    }
                    builder = builder.resolve_to_addrs(host, proxy.addresses);
                }
            } else {
                // Pin the address checked above; TLS still authenticates host.
                builder = builder.resolve_to_addrs(host, &addresses);
            }
            let client = builder.build().map_err(|_| invalid_release())?;
            let mut response = client
                .get(current.clone())
                .header(
                    reqwest::header::ACCEPT,
                    "application/octet-stream, application/json",
                )
                .header(reqwest::header::USER_AGENT, "memeloop-plugin-installer")
                .send()
                .await
                .map_err(|_| invalid_release())?;
            if response.status().is_redirection() {
                if hop == MAX_REDIRECTS {
                    return Err(invalid_release());
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(invalid_release)?;
                let next = current.join(location).map_err(|_| invalid_release())?;
                if next.as_str().len() > 8192 || !allowed_redirect(&next) {
                    return Err(invalid_release());
                }
                current = next;
                continue;
            }
            if !response.status().is_success()
                || response
                    .content_length()
                    .is_some_and(|size| size > MAX_RELEASE_BYTES as u64)
            {
                return Err(invalid_release());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|_| invalid_release())? {
                if bytes.len() + chunk.len() > MAX_RELEASE_BYTES {
                    return Err(invalid_release());
                }
                bytes.extend_from_slice(&chunk);
            }
            return parse_release(&bytes, &original);
        }
        Err(invalid_release())
    })
    .await
    .map_err(|_| invalid_release())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_assets_are_exact_https_github_paths() {
        for url in [
            "https://github.com/owner/repo/releases/download/v1/release.json",
            "https://github.com/owner/repo/releases/latest/download/release.json",
        ] {
            assert!(validate_github_release_url(url).is_ok(), "{url}");
        }
        for url in [
            "http://github.com/owner/repo/releases/download/v1/release.json",
            "https://github.com/owner/repo/releases/tag/v1",
            "https://github.com/owner/repo/releases/download/v1/plugin.tar",
            "https://github.com.evil.test/owner/repo/releases/download/v1/release.json",
            "https://user@github.com/owner/repo/releases/download/v1/release.json",
            "https://github.com/owner/repo/releases/download/v1/release.json?url=http://127.0.0.1",
            "https://github.com/owner/repo/releases/download/v1/%2Frelease.json",
            "https://127.0.0.1/owner/repo/releases/download/v1/release.json",
        ] {
            assert!(validate_github_release_url(url).is_err(), "{url}");
        }
    }

    #[test]
    fn redirects_cannot_escape_github_release_hosts() {
        for url in [
            "https://127.0.0.1/x",
            "https://169.254.169.254/x",
            "http://release-assets.githubusercontent.com/x",
            "https://evil.githubusercontent.com/x",
            "https://github.com/owner/repo",
            "https://user:pass@objects.githubusercontent.com/x",
        ] {
            assert!(!allowed_redirect(&Url::parse(url).unwrap()), "{url}");
        }
        assert!(allowed_redirect(
            &Url::parse("https://release-assets.githubusercontent.com/x?signature=1").unwrap()
        ));
    }

    #[test]
    fn descriptor_requires_digest_and_same_repository_signer() {
        let original = validate_github_release_url(
            "https://github.com/owner/repo/releases/download/v1/release.json",
        )
        .unwrap();
        let mut value = serde_json::json!({"format_version":1,
            "reference":format!("ghcr.io/owner/repo@sha256:{}", "a".repeat(64)),
            "signer":{"issuer":"https://token.actions.githubusercontent.com",
            "identity":"https://github.com/owner/repo/.github/workflows/publish.yml@refs/heads/master"}});
        assert!(parse_release(&serde_json::to_vec(&value).unwrap(), &original).is_ok());
        value["reference"] = serde_json::json!("ghcr.io/owner/repo:latest");
        assert!(parse_release(&serde_json::to_vec(&value).unwrap(), &original).is_err());
        value["reference"] =
            serde_json::json!(format!("ghcr.io/owner/repo@sha256:{}", "a".repeat(64)));
        value["signer"]["identity"] = serde_json::json!(
            "https://github.com/evil/repo/.github/workflows/publish.yml@refs/heads/master"
        );
        assert!(parse_release(&serde_json::to_vec(&value).unwrap(), &original).is_err());
    }
}
