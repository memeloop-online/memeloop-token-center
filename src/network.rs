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
    allow_test_loopback: bool,
) -> Result<reqwest::Client, AppError> {
    client_for_url(
        shared_private_client,
        value,
        scope_from_config(config),
        allow_test_loopback,
    )
    .await
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
        IpAddr::V4(address) => address.is_private(),
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
        for endpoint in [
            "https://169.254.169.254/latest/meta-data",
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

    #[test]
    fn private_scope_requires_uniform_safe_private_answers() {
        let private = [
            "10.0.0.1:443".parse().unwrap(),
            "192.168.1.10:443".parse().unwrap(),
        ];
        assert!(validate_addresses(&private, OutboundScope::Private, false).is_ok());
        assert!(validate_addresses(&private, OutboundScope::Public, false).is_err());

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
