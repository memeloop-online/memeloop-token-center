use std::{
    collections::BTreeSet,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    time::Duration,
};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use url::{Host, Url};

use crate::error::AppError;

/// The network authority attached to an administrator-owned outbound target.
/// Public targets are resolved and pinned for every outbound operation. Private
/// targets may use cluster DNS, but can only enter persisted configuration via
/// a global-operator authorization check in the control API.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutboundScope {
    #[default]
    Public,
    Private,
}

pub fn scope_from_config(config: &Value) -> OutboundScope {
    if config
        .get("network_scope")
        .and_then(Value::as_str)
        .is_some_and(|value| value == "private")
    {
        OutboundScope::Private
    } else {
        OutboundScope::Public
    }
}

/// Return a client suitable for exactly one outbound operation.
///
/// Public DNS answers are resolved once, rejected unless every address is
/// globally routable, and pinned into reqwest. The URL itself is left intact,
/// so HTTP Host and TLS SNI/certificate verification continue to use the
/// original hostname. Pinned clients explicitly bypass environment proxies;
/// otherwise a proxy could resolve the hostname again and undo this boundary.
pub async fn client_for_url(
    shared_http: &reqwest::Client,
    value: &str,
    scope: OutboundScope,
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    let url = checked_http_url(value)?;
    let host = url
        .host()
        .ok_or_else(|| AppError::BadRequest("outbound URL must include a host".into()))?;
    let host_name = match &host {
        Host::Domain(host) => *host,
        Host::Ipv4(address) => {
            return validated_literal_client(
                shared_http,
                IpAddr::V4(*address),
                url.scheme(),
                scope,
                allow_test_loopback,
            );
        }
        Host::Ipv6(address) => {
            return validated_literal_client(
                shared_http,
                IpAddr::V6(*address),
                url.scheme(),
                scope,
                allow_test_loopback,
            );
        }
    };
    let loopback_name = is_loopback_name(host_name);
    let port = url.port_or_known_default().ok_or_else(|| {
        AppError::BadRequest("outbound URL must use a known or explicit port".into())
    })?;
    let addresses = resolve_once(host_name, port).await?;
    let test_loopback = allow_test_loopback
        && loopback_name
        && addresses.iter().all(|address| address.ip().is_loopback());
    validate_addresses(&addresses, scope, test_loopback)?;
    validate_transport_security(url.scheme(), &addresses, scope, test_loopback)?;
    crate::build_pinned_http_client(host_name, &addresses).map_err(|_| AppError::Internal)
}

fn validated_literal_client(
    shared_http: &reqwest::Client,
    address: IpAddr,
    scheme: &str,
    scope: OutboundScope,
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    let test_loopback = allow_test_loopback && address.is_loopback();
    let addresses = [SocketAddr::new(address, 0)];
    validate_addresses(&addresses, scope, test_loopback)?;
    validate_transport_security(scheme, &addresses, scope, test_loopback)?;
    Ok(shared_http.clone())
}

fn validate_addresses(
    addresses: &[SocketAddr],
    scope: OutboundScope,
    allow_all_loopback: bool,
) -> Result<(), AppError> {
    if addresses.is_empty() {
        return Err(AppError::BadRequest(
            "outbound host returned no addresses".into(),
        ));
    }
    if allow_all_loopback && addresses.iter().all(|address| address.ip().is_loopback()) {
        return Ok(());
    }
    if addresses.iter().all(|address| is_public_ip(address.ip())) {
        return Ok(());
    }
    let all_safe_private = addresses
        .iter()
        .all(|address| is_safe_private_upstream_ip(address.ip()));
    if scope == OutboundScope::Private && all_safe_private {
        return Ok(());
    }
    if addresses
        .iter()
        .any(|address| is_forbidden_outbound_ip(address.ip()))
    {
        return Err(AppError::BadRequest(
            "outbound host resolved to a forbidden address".into(),
        ));
    }
    Err(AppError::BadRequest(
        "outbound host returned mixed public and private addresses".into(),
    ))
}

fn validate_transport_security(
    scheme: &str,
    addresses: &[SocketAddr],
    scope: OutboundScope,
    allow_all_loopback: bool,
) -> Result<(), AppError> {
    let cleartext_is_private = scope == OutboundScope::Private
        && addresses
            .iter()
            .all(|address| is_safe_private_upstream_ip(address.ip()));
    if scheme == "https" || allow_all_loopback || cleartext_is_private {
        return Ok(());
    }
    Err(AppError::BadRequest(
        "outbound URLs must use HTTPS unless every pinned address is explicitly private".into(),
    ))
}

pub async fn client_for_config_url(
    shared_private_client: &reqwest::Client,
    value: &str,
    config: &Value,
    proxy: Option<(&str, OutboundScope)>,
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    let target_scope = scope_from_config(config);
    let Some((proxy_url, proxy_scope)) = proxy else {
        return client_for_url(
            shared_private_client,
            value,
            target_scope,
            allow_test_loopback,
        )
        .await;
    };

    // A proxy is an explicit global-operator trust boundary. Only `socks5`
    // (never `socks5h`) is accepted: reqwest resolves its destination through
    // this client's pinned resolver before the SOCKS handshake. Validate the
    // final target exactly as for a direct request, then independently resolve,
    // classify and pin the proxy endpoint. Environment proxy inheritance is
    // disabled by the base builder.
    let target = checked_http_url(value)?;
    let (target_host, target_addresses, target_test_loopback) =
        validated_endpoint(&target, target_scope, allow_test_loopback).await?;
    validate_transport_security(
        target.scheme(),
        &target_addresses,
        target_scope,
        target_test_loopback,
    )?;

    let proxy = checked_proxy_url(proxy_url)?;
    let (proxy_host, proxy_addresses, proxy_test_loopback) =
        validated_endpoint(&proxy, proxy_scope, allow_test_loopback).await?;
    let private_proxy = proxy_scope == OutboundScope::Private
        && proxy_addresses
            .iter()
            .all(|address| is_safe_private_upstream_ip(address.ip()));
    if !private_proxy && !proxy_test_loopback {
        return Err(AppError::BadRequest(
            "upstream SOCKS5 proxies must use an explicitly private endpoint".into(),
        ));
    }

    let mut pins: Vec<(&str, &[SocketAddr])> = Vec::new();
    if let Some(host) = target_host.as_deref() {
        pins.push((host, &target_addresses));
    }
    if let Some(host) = proxy_host.as_deref() {
        pins.push((host, &proxy_addresses));
    }
    crate::build_explicit_proxy_http_client(proxy_url, &pins).map_err(|_| AppError::Internal)
}

async fn validated_endpoint(
    url: &Url,
    scope: OutboundScope,
    allow_test_loopback: bool,
) -> Result<(Option<String>, Vec<SocketAddr>, bool), AppError> {
    let host = url
        .host()
        .ok_or_else(|| AppError::BadRequest("outbound URL must include a host".into()))?;
    let port = url
        .port_or_known_default()
        .or_else(|| (url.scheme() == "socks5").then_some(1080))
        .ok_or_else(|| {
            AppError::BadRequest("outbound URL must use a known or explicit port".into())
        })?;
    let (host_name, addresses, loopback_name) = match host {
        Host::Domain(name) => {
            let values = resolve_once(name, port).await?;
            (Some(name.to_owned()), values, is_loopback_name(name))
        }
        Host::Ipv4(address) => (
            None,
            vec![SocketAddr::new(IpAddr::V4(address), port)],
            address.is_loopback(),
        ),
        Host::Ipv6(address) => (
            None,
            vec![SocketAddr::new(IpAddr::V6(address), port)],
            address.is_loopback(),
        ),
    };
    let test_loopback = allow_test_loopback
        && loopback_name
        && addresses.iter().all(|address| address.ip().is_loopback());
    validate_addresses(&addresses, scope, test_loopback)?;
    Ok((host_name, addresses, test_loopback))
}

fn checked_proxy_url(value: &str) -> Result<Url, AppError> {
    if value.len() > 2_048 || value.trim() != value || value.bytes().any(|byte| byte < 0x20) {
        return Err(AppError::BadRequest("upstream proxy URL is invalid".into()));
    }
    let url = Url::parse(value)
        .map_err(|_| AppError::BadRequest("upstream proxy URL is invalid".into()))?;
    if url.scheme() != "socks5"
        || url.host_str().is_none()
        || url.port() == Some(0)
        || (url.path() != "" && url.path() != "/")
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(AppError::BadRequest("upstream proxy URL is invalid".into()));
    }
    Ok(url)
}

pub fn checked_http_url(value: &str) -> Result<Url, AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::BadRequest("outbound target must be a URL".into()))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(AppError::BadRequest(
            "outbound target must be an HTTP(S) URL".into(),
        ));
    }
    if !url.username().is_empty() || url.password().is_some() || url.fragment().is_some() {
        return Err(AppError::BadRequest(
            "outbound URL cannot contain credentials or a fragment".into(),
        ));
    }
    Ok(url)
}

/// Append an OpenAI-compatible API path to either an origin-style base URL
/// (`https://provider.example`) or an API-base URL whose path already ends in
/// `/v1` (`https://provider.example/openai/v1`). Provider exports commonly use
/// both forms; blindly appending `/v1/...` to the latter produces a valid but
/// incorrect `/v1/v1/...` request.
pub(crate) fn upstream_api_url(base_url: &str, api_path: &str) -> String {
    // Both values have already crossed controlled boundaries: provider base
    // URLs are schema/destination validated before persistence and API paths
    // are static protocol constants. Avoid reparsing the same base URL for
    // every inference request; the hot path should allocate only its result.
    debug_assert!(api_path.starts_with('/'));
    debug_assert!(!api_path.contains(['?', '#']));
    let base = base_url.trim_end_matches('/');
    let suffix = if base.ends_with("/v1") {
        api_path.strip_prefix("/v1").unwrap_or(api_path)
    } else {
        api_path
    };
    format!("{base}{suffix}")
}

async fn resolve_once(host: &str, port: u16) -> Result<Vec<SocketAddr>, AppError> {
    let resolved = tokio::time::timeout(
        Duration::from_secs(3),
        tokio::net::lookup_host((host, port)),
    )
    .await
    .map_err(|_| AppError::BadRequest("outbound DNS lookup timed out".into()))?
    .map_err(|_| AppError::BadRequest("outbound host could not be resolved".into()))?;
    let addresses = resolved
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if addresses.is_empty() {
        return Err(AppError::BadRequest(
            "outbound host returned no addresses".into(),
        ));
    }
    Ok(addresses)
}

fn is_loopback_name(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    normalized == "localhost"
        || normalized.ends_with(".localhost")
        || normalized
            .parse::<IpAddr>()
            .is_ok_and(|address| address.is_loopback())
}

pub fn is_public_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_ipv4(address),
        IpAddr::V6(address) => is_public_ipv6(address),
    }
}

pub(crate) fn is_safe_private_upstream_ip(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => {
            let octets = address.octets();
            let is_tailnet = octets[0] == 100
                && (64..=127).contains(&octets[1])
                // The Tailscale local API / metadata address is never a
                // provider or operator proxy endpoint.
                && octets != [100, 100, 100, 200];
            address.is_private() || is_tailnet
        }
        IpAddr::V6(address) => {
            address.to_ipv4_mapped().is_none() && (address.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

fn is_forbidden_outbound_ip(address: IpAddr) -> bool {
    !is_public_ip(address) && !is_safe_private_upstream_ip(address)
}

fn is_public_ipv4(address: Ipv4Addr) -> bool {
    let octets = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_documentation()
        || address.is_unspecified()
        || octets[0] == 0
        || (octets[0] == 100 && (64..=127).contains(&octets[1]))
        || (octets[0] == 192 && octets[1] == 0 && octets[2] == 0)
        || (octets[0] == 192 && octets[1] == 88 && octets[2] == 99)
        || (octets[0] == 198 && matches!(octets[1], 18 | 19))
        || octets[0] >= 224)
}

fn is_public_ipv6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if address.is_loopback()
        || address.is_unspecified()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] & 0xffc0) == 0xfec0
        || (segments[0] & 0xff00) == 0xff00
        || (segments[0] == 0x2001 && segments[1] == 0x0db8)
        || (segments[0] == 0x2001 && segments[1] == 0x0002 && segments[2] == 0)
        || (segments[0] == 0x2001 && (0x0010..=0x002f).contains(&segments[1]))
        || (segments[0] == 0x3fff && segments[1] <= 0x0fff)
    {
        return false;
    }
    if let Some(address) = address.to_ipv4_mapped() {
        return is_public_ipv4(address);
    }
    // Well-known NAT64 and 6to4 embed an IPv4 destination. Do not let either
    // representation tunnel a private/reserved address through the IPv6 check.
    if segments[..6] == [0x0064, 0xff9b, 0, 0, 0, 0] {
        return is_public_ipv4(embedded_ipv4(segments[6], segments[7]));
    }
    if segments[0] == 0x2002 {
        return is_public_ipv4(embedded_ipv4(segments[1], segments[2]));
    }
    // All other currently routable global-unicast IPv6 space is in 2000::/3.
    // Staying conservative here makes future/special allocations opt-in after
    // review instead of silently turning them into an SSRF bypass.
    (segments[0] & 0xe000) == 0x2000
}

fn embedded_ipv4(high: u16, low: u16) -> Ipv4Addr {
    Ipv4Addr::new((high >> 8) as u8, high as u8, (low >> 8) as u8, low as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_api_url_accepts_origin_and_versioned_api_bases() {
        assert_eq!(
            upstream_api_url("https://provider.example", "/v1/images/generations"),
            "https://provider.example/v1/images/generations"
        );
        assert_eq!(
            upstream_api_url("https://provider.example/v1/", "/v1/images/generations"),
            "https://provider.example/v1/images/generations"
        );
        assert_eq!(
            upstream_api_url(
                "https://provider.example/openai/v1",
                "/v1/images/generations"
            ),
            "https://provider.example/openai/v1/images/generations"
        );
    }

    #[test]
    fn private_reserved_and_embedded_addresses_are_not_public() {
        for value in [
            "127.0.0.1",
            "10.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "192.0.0.1",
            "192.88.99.1",
            "198.18.0.1",
            "::1",
            "100::1",
            "fc00::1",
            "fe80::1",
            "2001:db8::1",
            "2001:2::1",
            "2001:10::1",
            "3fff::1",
            "::a9fe:a9fe",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "64:ff9b:1::a9fe:a9fe",
            "2002:7f00:0001::",
        ] {
            assert!(!is_public_ip(value.parse().unwrap()), "{value}");
        }
        assert!(is_public_ip("1.1.1.1".parse().unwrap()));
        assert!(is_public_ip("2606:4700:4700::1111".parse().unwrap()));
        assert!(is_public_ip("64:ff9b::101:101".parse().unwrap()));
        assert!(is_public_ip("2002:0101:0101::".parse().unwrap()));
    }

    #[tokio::test]
    async fn public_http_is_rejected_but_explicit_test_loopback_is_bounded() {
        let shared = crate::build_http_client().unwrap();
        assert!(
            client_for_url(
                &shared,
                "http://127.0.0.1:1234",
                OutboundScope::Public,
                false
            )
            .await
            .is_err()
        );
        assert!(
            client_for_url(
                &shared,
                "http://127.0.0.1:1234",
                OutboundScope::Public,
                true
            )
            .await
            .is_ok()
        );
        assert!(
            client_for_url(&shared, "http://10.0.0.1:1234", OutboundScope::Public, true)
                .await
                .is_err()
        );
        assert!(
            client_for_url(
                &shared,
                "http://1.1.1.1/provider",
                OutboundScope::Private,
                false,
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn private_scope_allows_exact_safe_lan_endpoint_but_not_metadata() {
        let shared = crate::build_http_client().unwrap();
        assert!(
            client_for_url(
                &shared,
                "http://10.0.0.1:8188/provider",
                OutboundScope::Private,
                false,
            )
            .await
            .is_ok()
        );
        assert!(
            client_for_url(
                &shared,
                "https://100.64.0.16/provider",
                OutboundScope::Private,
                false,
            )
            .await
            .is_ok()
        );
        for endpoint in [
            "https://169.254.169.254/latest/meta-data",
            "https://100.100.100.200/localapi",
            "https://127.0.0.1/provider",
            "https://[::1]/provider",
            "https://[::ffff:169.254.169.254]/provider",
            "https://[64:ff9b::a9fe:a9fe]/provider",
            "https://[2002:7f00:1::]/provider",
        ] {
            assert!(
                client_for_url(&shared, endpoint, OutboundScope::Private, false)
                    .await
                    .is_err(),
                "private scope accepted {endpoint}"
            );
        }
    }

    #[tokio::test]
    async fn explicit_proxy_validates_target_and_proxy_scopes_independently() {
        let shared = crate::build_http_client().unwrap();
        let config = serde_json::json!({
            "base_url": "https://1.1.1.1/v1",
            "network_scope": "public"
        });
        assert!(
            client_for_config_url(
                &shared,
                "https://1.1.1.1/v1/models",
                &config,
                Some(("socks5://10.20.30.40:1080", OutboundScope::Private)),
                false,
            )
            .await
            .is_ok()
        );
        assert!(
            client_for_config_url(
                &shared,
                "https://1.1.1.1/v1/models",
                &config,
                Some(("socks5://8.8.8.8:1080", OutboundScope::Public)),
                false,
            )
            .await
            .is_err(),
            "a cleartext public SOCKS proxy must fail closed"
        );
        assert!(
            client_for_config_url(
                &shared,
                "https://169.254.169.254/latest/meta-data",
                &config,
                Some(("socks5://10.20.30.40:1080", OutboundScope::Private)),
                false,
            )
            .await
            .is_err(),
            "an approved proxy must not bypass final-target SSRF checks"
        );
    }

    #[test]
    fn private_scope_requires_uniform_safe_private_answers() {
        let private = [
            "10.0.0.1:443".parse().unwrap(),
            "192.168.1.10:443".parse().unwrap(),
        ];
        assert!(validate_addresses(&private, OutboundScope::Private, false).is_ok());
        assert!(validate_addresses(&private, OutboundScope::Public, false).is_err());

        let tailnet = ["100.64.0.16:443".parse().unwrap()];
        assert!(validate_addresses(&tailnet, OutboundScope::Private, false).is_ok());
        assert!(validate_addresses(&tailnet, OutboundScope::Public, false).is_err());
        let tailnet_metadata = ["100.100.100.200:443".parse().unwrap()];
        assert!(validate_addresses(&tailnet_metadata, OutboundScope::Private, false).is_err());

        let addresses = [
            "1.1.1.1:443".parse().unwrap(),
            "10.0.0.1:443".parse().unwrap(),
        ];
        assert!(validate_addresses(&addresses, OutboundScope::Private, false).is_err());
        assert!(validate_addresses(&addresses[..1], OutboundScope::Public, false).is_ok());
        assert!(validate_addresses(&[], OutboundScope::Public, false).is_err());
        assert!(
            validate_transport_security("http", &addresses[..1], OutboundScope::Private, false,)
                .is_err(),
            "private-config public DNS answers must not permit cleartext"
        );
        assert!(
            validate_transport_security("http", &private, OutboundScope::Private, false).is_ok(),
            "explicit private ComfyUI endpoints may use cleartext on the private network"
        );
    }

    #[test]
    fn private_scope_never_allows_metadata_or_transition_bypasses() {
        for value in [
            "127.0.0.1",
            "169.254.169.254",
            "::1",
            "fe80::1",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
            "2002:7f00:1::",
        ] {
            let addresses = [SocketAddr::new(value.parse().unwrap(), 443)];
            assert!(
                validate_addresses(&addresses, OutboundScope::Private, false).is_err(),
                "private scope accepted {value}"
            );
        }
    }
}
