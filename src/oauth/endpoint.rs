use std::net::IpAddr;

use url::{Host, Url};

use crate::{
    error::AppError,
    network::{self, OutboundScope},
};

pub(crate) fn validate_oauth_endpoint(value: &str, field: &str) -> Result<Url, AppError> {
    validate_oauth_endpoint_with_scope(value, field, OutboundScope::Public)
}

pub(super) fn validate_oauth_endpoint_with_scope(
    value: &str,
    field: &str,
    scope: OutboundScope,
) -> Result<Url, AppError> {
    let url = Url::parse(value)
        .map_err(|_| AppError::BadRequest(format!("OAuth {field} must be a URL")))?;
    let private_http = url.scheme() == "http"
        && url.host().is_some_and(|host| match host {
            Host::Domain(host) => {
                is_loopback_oauth_name(host)
                    || (scope == OutboundScope::Private && is_private_cluster_service(host))
            }
            Host::Ipv4(address) => address.is_loopback(),
            Host::Ipv6(address) => address.is_loopback(),
        });
    if url.scheme() != "https" && !private_http {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} must use HTTPS unless the account is explicitly private"
        )));
    }
    if url.username() != "" || url.password().is_some() || url.fragment().is_some() {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} cannot contain credentials or a fragment"
        )));
    }
    Ok(url)
}

pub(crate) fn validate_oauth_adapter_endpoint(value: &str, field: &str) -> Result<Url, AppError> {
    // Catalog contributions are syntax only: the account's operator-approved
    // network scope is enforced again before every login, poll, and refresh.
    let url = validate_oauth_endpoint_with_scope(value, field, OutboundScope::Private)?;
    classify_oauth_endpoint(&url, field, false, OutboundScope::Private)?;
    Ok(url)
}

pub(crate) fn validate_managed_oauth_adapter_endpoint(
    value: &str,
    field: &str,
) -> Result<Url, AppError> {
    validate_managed_oauth_adapter_endpoint_inner(value, field, false)
}

fn validate_managed_oauth_adapter_endpoint_inner(
    value: &str,
    field: &str,
    allow_test_loopback: bool,
) -> Result<Url, AppError> {
    let url = validate_oauth_endpoint_with_scope(value, field, OutboundScope::Private)?;
    if url.query().is_some() {
        return Err(AppError::BadRequest(format!(
            "OAuth {field} cannot contain a query"
        )));
    }
    classify_oauth_endpoint(&url, field, allow_test_loopback, OutboundScope::Private)?;
    Ok(url)
}

pub(super) fn managed_oauth_endpoint_scope(
    value: &str,
    allow_test_loopback: bool,
) -> Result<(Url, OutboundScope), AppError> {
    let url =
        validate_managed_oauth_adapter_endpoint_inner(value, "adapter_url", allow_test_loopback)?;
    let scope = classify_oauth_endpoint(
        &url,
        "adapter_url",
        allow_test_loopback,
        OutboundScope::Private,
    )?;
    Ok((url, scope))
}

pub(crate) fn oauth_adapter_endpoint_scope(
    value: &str,
    field: &str,
    allow_test_loopback: bool,
    configured_scope: OutboundScope,
) -> Result<(Url, OutboundScope), AppError> {
    let url = validate_oauth_endpoint_with_scope(value, field, configured_scope)?;
    let scope = classify_oauth_endpoint(&url, field, allow_test_loopback, configured_scope)?;
    Ok((url, scope))
}

fn classify_oauth_endpoint(
    url: &Url,
    field: &str,
    allow_test_loopback: bool,
    configured_scope: OutboundScope,
) -> Result<OutboundScope, AppError> {
    let host = url
        .host()
        .ok_or_else(|| AppError::BadRequest(format!("OAuth {field} must include a host")))?;
    match host {
        Host::Domain(host) if is_loopback_oauth_name(host) && !allow_test_loopback => Err(
            AppError::BadRequest(format!("OAuth {field} cannot target a loopback host")),
        ),
        Host::Domain(host) if is_private_cluster_service(host) => {
            if configured_scope == OutboundScope::Private {
                Ok(OutboundScope::Private)
            } else {
                Err(AppError::BadRequest(format!(
                    "OAuth {field} cannot target a private cluster service"
                )))
            }
        }
        Host::Domain(_) => Ok(OutboundScope::Public),
        Host::Ipv4(address) => classify_oauth_ip(
            IpAddr::V4(address),
            field,
            allow_test_loopback,
            configured_scope,
        ),
        Host::Ipv6(address) => classify_oauth_ip(
            IpAddr::V6(address),
            field,
            allow_test_loopback,
            configured_scope,
        ),
    }
}

fn classify_oauth_ip(
    address: IpAddr,
    field: &str,
    allow_test_loopback: bool,
    _configured_scope: OutboundScope,
) -> Result<OutboundScope, AppError> {
    if allow_test_loopback && address.is_loopback() {
        return Ok(OutboundScope::Public);
    }
    if network::is_public_ip(address) {
        return Ok(OutboundScope::Public);
    }
    Err(AppError::BadRequest(format!(
        "OAuth {field} cannot target a private or reserved IP address"
    )))
}

fn is_loopback_oauth_name(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    normalized == "localhost" || normalized.ends_with(".localhost")
}

fn is_private_cluster_service(host: &str) -> bool {
    let normalized = host.trim_end_matches('.').to_ascii_lowercase();
    let labels = normalized.split('.').collect::<Vec<_>>();
    matches!(
        labels.as_slice(),
        [service, namespace, "svc"]
            | [service, namespace, "svc", "cluster", "local"]
            if !service.is_empty() && !namespace.is_empty()
    )
}
