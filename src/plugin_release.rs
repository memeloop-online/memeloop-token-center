//! GitHub release metadata is untrusted discovery data. A resolved reference
//! still needs explicit operator trust confirmation and ordinary OCI signature
//! verification before it can become an installation or execution authority.
use std::{net::SocketAddr, time::Duration};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::Url;

use crate::{
    error::{AppError, PluginReleaseDownloadFailure},
    plugin::CosignKeylessIdentity,
};

const MAX_RELEASE_BYTES: usize = 64 * 1024;
const MAX_REDIRECTS: usize = 4;
const RELEASE_TIMEOUT: Duration = Duration::from_secs(60);
static RELEASE_FETCH_PERMITS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(4));

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
    let _permit = RELEASE_FETCH_PERMITS
        .try_acquire()
        .map_err(|_| AppError::Overloaded)?;
    download_release(original, |url| request_release(url, proxy.as_ref())).await
}

async fn request_release(
    current: Url,
    proxy: Option<&ReleaseProxy<'_>>,
) -> Result<reqwest::Response, AppError> {
    let host = current.host_str().ok_or_else(invalid_release)?;
    let addresses: Vec<SocketAddr> = tokio::net::lookup_host((host, 443))
        .await
        .map_err(|_| download_failure(PluginReleaseDownloadFailure::Dns))?
        .take(33)
        .collect();
    if addresses.is_empty() {
        return Err(download_failure(PluginReleaseDownloadFailure::Dns));
    }
    if addresses.len() > 32
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
    if let Some(proxy) = proxy {
        crate::network::checked_installer_proxy_url(proxy.url).map_err(|_| invalid_release())?;
        let mut endpoint = Url::parse(proxy.url).map_err(|_| invalid_release())?;
        let address = proxy.addresses.first().ok_or_else(invalid_release)?;
        endpoint
            .set_ip_host(address.ip())
            .map_err(|_| invalid_release())?;
        let mut configured =
            reqwest::Proxy::all(endpoint.as_str()).map_err(|_| invalid_release())?;
        match (proxy.username, proxy.password) {
            (Some(username), Some(password)) => {
                configured = configured.basic_auth(username, password)
            }
            (None, None) => (),
            _ => return Err(invalid_release()),
        }
        builder = builder.proxy(configured);
    } else {
        // Pin the address checked above; TLS still authenticates host.
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder
        .build()
        .map_err(|_| download_failure(PluginReleaseDownloadFailure::Client))?;
    let response = client
        .get(current)
        .header(
            reqwest::header::ACCEPT,
            "application/octet-stream, application/json",
        )
        .header(reqwest::header::USER_AGENT, "memeloop-plugin-installer")
        .send()
        .await
        .map_err(request_failure)?;
    Ok(response)
}

async fn download_release<F, Fut>(
    original: Url,
    mut request: F,
) -> Result<ResolvedPluginRelease, AppError>
where
    F: FnMut(Url) -> Fut,
    Fut: std::future::Future<Output = Result<reqwest::Response, AppError>>,
{
    tokio::time::timeout(RELEASE_TIMEOUT, async {
        let mut current = original.clone();
        for hop in 0..=MAX_REDIRECTS {
            let mut response = request(current.clone()).await?;
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
            if !response.status().is_success() {
                tracing::warn!(
                    status = response.status().as_u16(),
                    "GitHub plugin release server returned an unsuccessful status"
                );
                return Err(download_failure(PluginReleaseDownloadFailure::RemoteHttp));
            }
            if response
                .content_length()
                .is_some_and(|size| size > MAX_RELEASE_BYTES as u64)
            {
                return Err(invalid_release());
            }
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(|error| {
                download_failure(if error.is_timeout() {
                    PluginReleaseDownloadFailure::Timeout
                } else {
                    PluginReleaseDownloadFailure::Body
                })
            })? {
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
    .map_err(|_| download_failure(PluginReleaseDownloadFailure::Timeout))?
}

fn download_failure(failure: PluginReleaseDownloadFailure) -> AppError {
    tracing::warn!(
        stage = failure.code(),
        "GitHub plugin release download failed"
    );
    AppError::PluginReleaseDownload(failure)
}

fn request_failure(error: reqwest::Error) -> AppError {
    download_failure(if error.is_timeout() {
        PluginReleaseDownloadFailure::Timeout
    } else if error.is_connect() {
        PluginReleaseDownloadFailure::Connection
    } else {
        PluginReleaseDownloadFailure::Request
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn github_release_http_fixture_follows_only_approved_redirects_and_bounds_body() {
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{method, path},
        };
        let server = MockServer::start().await;
        let original = validate_github_release_url(
            "https://github.com/owner/repo/releases/download/v1/release.json",
        )
        .unwrap();
        let descriptor = serde_json::json!({"format_version":1,
            "reference":format!("ghcr.io/owner/repo@sha256:{}","a".repeat(64)),
            "signer":{"issuer":"https://token.actions.githubusercontent.com",
                "identity":"https://github.com/owner/repo/.github/workflows/publish.yml@refs/tags/v1"}});
        Mock::given(method("GET"))
            .and(path(original.path()))
            .respond_with(ResponseTemplate::new(302).insert_header(
                "Location",
                "https://release-assets.githubusercontent.com/asset",
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/asset"))
            .respond_with(ResponseTemplate::new(200).set_body_json(&descriptor))
            .expect(1)
            .mount(&server)
            .await;
        // Only the transport is replaced. URL/redirect policy, response status,
        // bounded body consumption and descriptor parsing are production code.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let result = download_release(original.clone(), |url| {
            let target = format!("{}{}", server.uri(), url.path());
            let client = client.clone();
            async move {
                client
                    .get(target)
                    .send()
                    .await
                    .map_err(|_| invalid_release())
            }
        })
        .await
        .unwrap();
        assert_eq!(result.descriptor.reference, descriptor["reference"]);
        assert_eq!(result.release_url, original.as_str());
        server.verify().await;
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(302)
                    .insert_header("Location", "https://169.254.169.254/latest/meta-data/"),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            download_release(original.clone(), |_| {
                let target = server.uri();
                let client = client.clone();
                async move {
                    client
                        .get(target)
                        .send()
                        .await
                        .map_err(|_| invalid_release())
                }
            })
            .await
            .is_err()
        );
        server.verify().await;
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200).set_body_bytes(vec![b' '; MAX_RELEASE_BYTES + 1]),
            )
            .expect(1)
            .mount(&server)
            .await;
        assert!(
            download_release(original, |_| {
                let target = server.uri();
                let client = client.clone();
                async move {
                    client
                        .get(target)
                        .send()
                        .await
                        .map_err(|_| invalid_release())
                }
            })
            .await
            .is_err()
        );
        server.verify().await;
    }

    #[tokio::test]
    async fn valid_release_transport_and_remote_http_failures_are_safe_dependency_errors() {
        use axum::{body::to_bytes, response::IntoResponse};
        use wiremock::{Mock, MockServer, ResponseTemplate};
        let original = validate_github_release_url(
            "https://github.com/owner/repo/releases/download/v1/release.json",
        )
        .unwrap();
        let server = MockServer::start().await;
        const SECRET: &str = "signed-redirect-token-and-proxy-password";
        Mock::given(wiremock::matchers::method("GET"))
            .respond_with(ResponseTemplate::new(503).set_body_string(SECRET))
            .mount(&server)
            .await;
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let error = download_release(original.clone(), |_| {
            let client = client.clone();
            let target = format!("{}?signature={SECRET}", server.uri());
            async move { client.get(target).send().await.map_err(request_failure) }
        })
        .await
        .err()
        .unwrap();
        assert_eq!(error.diagnostic_category(), "plugin_release_http_failed");
        assert!(!format!("{error:?}").contains(SECRET));
        let response = error.into_response();
        assert_eq!(response.status(), axum::http::StatusCode::BAD_GATEWAY);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert!(!String::from_utf8_lossy(&body).contains(SECRET));
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "plugin_release_http_failed");

        // Exercise the real reqwest error mapper with a secret-bearing URL.
        let error = client
            .get(format!("http://[::1?signature={SECRET}"))
            .send()
            .await
            .unwrap_err();
        let error = request_failure(error);
        assert_eq!(error.diagnostic_category(), "plugin_release_request_failed");
        assert!(!format!("{error:?}").contains(SECRET));
        // A refused connection for an otherwise accepted release URL stays a
        // dependency failure; the request URL can never enter the public error.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let error = download_release(original.clone(), |_| {
            let client = client.clone();
            async move {
                client
                    .get(format!("http://{address}/?signature={SECRET}"))
                    .send()
                    .await
                    .map_err(request_failure)
            }
        })
        .await
        .err()
        .unwrap();
        assert_eq!(
            error.diagnostic_category(),
            "plugin_release_connection_failed"
        );
        assert!(!format!("{error:?}").contains(SECRET));
        for failure in [
            PluginReleaseDownloadFailure::Request,
            PluginReleaseDownloadFailure::RemoteHttp,
            PluginReleaseDownloadFailure::Dns,
            PluginReleaseDownloadFailure::Connection,
            PluginReleaseDownloadFailure::Body,
            PluginReleaseDownloadFailure::Timeout,
            PluginReleaseDownloadFailure::Client,
        ] {
            let error = download_release(original.clone(), |_| {
                std::future::ready(Err(download_failure(failure)))
            })
            .await
            .err()
            .unwrap();
            assert_eq!(error.diagnostic_category(), failure.code());
            let response = error.into_response();
            assert_eq!(
                response.status(),
                if failure == PluginReleaseDownloadFailure::Timeout {
                    axum::http::StatusCode::GATEWAY_TIMEOUT
                } else {
                    axum::http::StatusCode::BAD_GATEWAY
                }
            );
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], failure.code());
        }
    }

    #[tokio::test]
    async fn release_body_transport_error_does_not_retain_remote_secrets() {
        let original = validate_github_release_url(
            "https://github.com/owner/repo/releases/download/v1/release.json",
        )
        .unwrap();
        let error = download_release(original, |_| async {
            let stream = futures_util::stream::once(async {
                Err::<bytes::Bytes, _>(std::io::Error::other("remote-secret-body"))
            });
            Ok(reqwest::Response::from(axum::http::Response::new(
                reqwest::Body::wrap_stream(stream),
            )))
        })
        .await
        .err()
        .unwrap();
        assert_eq!(error.diagnostic_category(), "plugin_release_body_failed");
        assert!(!format!("{error:?}").contains("remote-secret-body"));
    }

    #[tokio::test(start_paused = true)]
    async fn release_total_deadline_is_a_gateway_timeout() {
        use axum::response::IntoResponse;
        let original = validate_github_release_url(
            "https://github.com/owner/repo/releases/download/v1/release.json",
        )
        .unwrap();
        let error = download_release(original, |_| std::future::pending())
            .await
            .err()
            .unwrap();
        assert_eq!(error.diagnostic_category(), "plugin_release_timeout");
        assert_eq!(
            error.into_response().status(),
            axum::http::StatusCode::GATEWAY_TIMEOUT
        );
    }

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
            assert!(
                matches!(
                    validate_github_release_url(url),
                    Err(AppError::BadRequest(_))
                ),
                "{url}"
            );
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
