use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use getrandom::fill;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::AppError;
use crate::network::{OutboundScope, has_safe_private_ip_literal_host};

const CURRENT_ENVELOPE_VERSION: &str = "v2";
pub(super) const LEGACY_ENVELOPE_VERSION: &str = "v1";
pub(super) const ENVELOPE_AAD: &[u8] = b"memeloop-token-center/upstream-credential/v1";
const PROXY_FINGERPRINT_DOMAIN: &[u8] = b"memeloop-token-center/upstream-proxy-fingerprint/v1";
const PROVIDER_ADAPTER_SECRET_PATCH_KEY: &str = "__mtc_provider_config_secret_patch_v1";
const MAX_PROVIDER_ADAPTER_SECRET_PATCH_BYTES: usize = 48 * 1024;

pub(super) const MAX_ADAPTER_STATE_BYTES: usize = 16 * 1024;
pub(super) const MAX_ADAPTER_STATE_DEPTH: usize = 8;
pub(super) const MAX_ADAPTER_STATE_NODES: usize = 256;
#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpstreamCredential {
    None,
    ApiKey {
        value: String,
        #[serde(default = "authorization_header")]
        header: String,
        #[serde(default = "bearer_prefix")]
        prefix: String,
    },
    /// An API credential whose account is intentionally routed through one
    /// operator-approved proxy. The complete proxy URL stays inside the same
    /// encrypted envelope as the API key, because it may contain proxy
    /// authentication and private topology.
    #[serde(rename = "api_key_proxy")]
    ProxiedApiKey {
        value: String,
        #[serde(default = "authorization_header")]
        header: String,
        #[serde(default = "bearer_prefix")]
        prefix: String,
        proxy_url: String,
        proxy_network_scope: OutboundScope,
    },
    #[serde(rename = "oauth")]
    OAuth {
        access_token: String,
        refresh_token: Option<String>,
        expires_at: Option<i64>,
        #[serde(default = "authorization_header")]
        header: String,
        #[serde(default = "bearer_prefix")]
        prefix: String,
        /// Adapter-owned refresh material. It is part of the encrypted
        /// credential envelope and is never copied into account config or a
        /// response view.
        #[serde(default, deserialize_with = "deserialize_adapter_state")]
        adapter_state: Option<Value>,
        /// Optional operator-approved SOCKS5 transport for this OAuth account.
        /// Proxy authentication and private topology stay encrypted alongside
        /// the OAuth tokens and never enter the public account configuration.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proxy_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        proxy_network_scope: Option<OutboundScope>,
    },
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct UpstreamProxyMetadata {
    pub(crate) has_proxy: bool,
    pub(crate) scheme: Option<String>,
    pub(crate) remote_dns: bool,
    pub(crate) label: Option<String>,
    pub(crate) fingerprint: Option<String>,
}

impl std::fmt::Debug for UpstreamCredential {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::None => formatter.write_str("UpstreamCredential::None"),
            Self::ApiKey { .. } => formatter
                .debug_struct("UpstreamCredential::ApiKey")
                .field("credential_material", &"[redacted]")
                .finish(),
            Self::ProxiedApiKey {
                proxy_network_scope,
                ..
            } => formatter
                .debug_struct("UpstreamCredential::ProxiedApiKey")
                .field("credential_material", &"[redacted]")
                .field("proxy_url", &"[redacted]")
                .field("proxy_network_scope", proxy_network_scope)
                .finish(),
            Self::OAuth {
                refresh_token,
                expires_at,
                adapter_state,
                proxy_url,
                ..
            } => formatter
                .debug_struct("UpstreamCredential::OAuth")
                .field("access_token", &"[redacted]")
                .field("has_refresh_token", &refresh_token.is_some())
                .field("expires_at", expires_at)
                .field("has_adapter_state", &adapter_state.is_some())
                .field("has_proxy", &proxy_url.is_some())
                .finish(),
        }
    }
}

impl UpstreamCredential {
    pub fn auth_kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::OAuth { .. } => "oauth",
            Self::ApiKey { .. } | Self::ProxiedApiKey { .. } => "api_key",
        }
    }

    pub fn expires_at(&self) -> Option<i64> {
        match self {
            Self::None | Self::ApiKey { .. } | Self::ProxiedApiKey { .. } => None,
            Self::OAuth { expires_at, .. } => *expires_at,
        }
    }

    pub fn apply(
        &self,
        request: reqwest::RequestBuilder,
        now: i64,
    ) -> Result<reqwest::RequestBuilder, AppError> {
        let Some((header_name, value)) = self.request_header(now)? else {
            return Ok(request);
        };
        Ok(request.header(header_name, value))
    }

    /// Return the validated credential header without coupling callers to a
    /// specific HTTP client. Native Codex uses a fingerprinted client while
    /// every other provider continues to use reqwest.
    pub(crate) fn request_header(
        &self,
        now: i64,
    ) -> Result<Option<(http::HeaderName, http::HeaderValue)>, AppError> {
        self.validate(now)?;
        let (secret, header, prefix) = match self {
            Self::None => return Ok(None),
            Self::ApiKey {
                value,
                header,
                prefix,
            } => (value, header, prefix),
            Self::ProxiedApiKey {
                value,
                header,
                prefix,
                ..
            } => (value, header, prefix),
            Self::OAuth {
                access_token,
                header,
                prefix,
                ..
            } => (access_token, header, prefix),
        };
        let header_name = http::HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| AppError::BadRequest("invalid upstream credential header".into()))?;
        let mut value = http::HeaderValue::from_str(&format!("{prefix}{secret}"))
            .map_err(|_| AppError::BadRequest("invalid upstream credential value".into()))?;
        value.set_sensitive(true);
        Ok(Some((header_name, value)))
    }

    pub fn validate(&self, now: i64) -> Result<(), AppError> {
        let (secret, header, prefix) = match self {
            Self::None => return Ok(()),
            Self::ApiKey {
                value,
                header,
                prefix,
            } => (value, header, prefix),
            Self::ProxiedApiKey {
                value,
                header,
                prefix,
                proxy_url,
                proxy_network_scope,
            } => {
                validate_proxy_url(proxy_url)?;
                if *proxy_network_scope != OutboundScope::Private {
                    return Err(AppError::BadRequest(
                        "upstream SOCKS5 proxy must use private network scope".into(),
                    ));
                }
                (value, header, prefix)
            }
            Self::OAuth {
                access_token,
                expires_at,
                header,
                prefix,
                adapter_state,
                proxy_url,
                proxy_network_scope,
                ..
            } => {
                if let Some(state) = adapter_state {
                    validate_adapter_state(state)?;
                }
                validate_optional_private_proxy(proxy_url.as_deref(), *proxy_network_scope)?;
                if expires_at.is_some_and(|expires_at| expires_at <= now) {
                    return Err(AppError::Upstream(
                        "upstream OAuth credential is expired and must be refreshed".into(),
                    ));
                }
                (access_token, header, prefix)
            }
        };
        if secret.is_empty() {
            return Err(AppError::BadRequest(
                "upstream credential secret is required".into(),
            ));
        }
        reqwest::header::HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| AppError::BadRequest("invalid upstream credential header".into()))?;
        reqwest::header::HeaderValue::from_str(&format!("{prefix}{secret}"))
            .map_err(|_| AppError::BadRequest("invalid upstream credential value".into()))?;
        Ok(())
    }

    pub fn has_oauth_refresh_state(&self) -> bool {
        matches!(
            self,
            Self::OAuth {
                refresh_token: Some(token),
                ..
            } if !token.is_empty()
        ) || matches!(
            self,
            Self::OAuth {
                adapter_state: Some(_),
                ..
            }
        )
    }

    pub fn adapter_state(&self) -> Option<&Value> {
        match self {
            Self::OAuth { adapter_state, .. } => adapter_state.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn with_provider_adapter_secret_patch(
        mut self,
        patch: &Value,
    ) -> Result<Self, AppError> {
        let patch = validate_provider_adapter_secret_patch(patch)?;
        let encoded = URL_SAFE_NO_PAD.encode(patch);
        match &mut self {
            Self::OAuth { adapter_state, .. } => {
                let state = adapter_state
                    .get_or_insert_with(|| Value::Object(serde_json::Map::new()))
                    .as_object_mut()
                    .ok_or_else(|| {
                        AppError::Conflict(
                            "provider adapter returned unsupported credential state".into(),
                        )
                    })?;
                state.insert(
                    PROVIDER_ADAPTER_SECRET_PATCH_KEY.into(),
                    Value::String(encoded),
                );
                validate_adapter_state(adapter_state.as_ref().ok_or(AppError::Internal)?)?;
                Ok(self)
            }
            _ => Err(AppError::Internal),
        }
    }

    pub(crate) fn provider_adapter_secret_patch(&self) -> Result<Option<Value>, AppError> {
        let Some(state) = self.adapter_state() else {
            return Ok(None);
        };
        decode_provider_adapter_secret_patch(state)
    }

    pub(crate) fn hydrate_provider_adapter_config(
        &self,
        persisted: Value,
    ) -> Result<Value, AppError> {
        let Some(patch) = self.provider_adapter_secret_patch()? else {
            return Ok(persisted);
        };
        apply_provider_adapter_secret_patch(persisted, &patch)
    }

    pub fn proxy(&self) -> Option<(&str, OutboundScope)> {
        match self {
            Self::ProxiedApiKey {
                proxy_url,
                proxy_network_scope,
                ..
            } => Some((proxy_url.as_str(), *proxy_network_scope)),
            Self::OAuth {
                proxy_url: Some(proxy_url),
                proxy_network_scope: Some(proxy_network_scope),
                ..
            } => Some((proxy_url.as_str(), *proxy_network_scope)),
            _ => None,
        }
    }

    pub(crate) fn proxy_metadata(
        &self,
        key_material: &[u8],
    ) -> Result<UpstreamProxyMetadata, AppError> {
        self.proxy_metadata_for_driver(key_material, false)
    }

    pub(crate) fn codex_proxy_metadata(
        &self,
        key_material: &[u8],
    ) -> Result<UpstreamProxyMetadata, AppError> {
        self.proxy_metadata_for_driver(key_material, true)
    }

    fn proxy_metadata_for_driver(
        &self,
        key_material: &[u8],
        codex: bool,
    ) -> Result<UpstreamProxyMetadata, AppError> {
        let Some((proxy_url, proxy_scope)) = self.proxy() else {
            return Ok(UpstreamProxyMetadata::default());
        };
        let parsed = url::Url::parse(proxy_url).ok();
        let valid = if codex {
            proxy_scope == OutboundScope::Private && validate_codex_proxy_url(proxy_url).is_ok()
        } else {
            proxy_scope == OutboundScope::Private && validate_proxy_url(proxy_url).is_ok()
        };
        let scheme = parsed
            .as_ref()
            .map(url::Url::scheme)
            .filter(|scheme| matches!(*scheme, "socks5" | "socks5h"))
            .map(str::to_owned);
        let remote_dns = valid && scheme.as_deref() == Some("socks5h");
        let mut hasher = Sha256::new();
        hasher.update(PROXY_FINGERPRINT_DOMAIN);
        hasher.update([0]);
        hasher.update(key_material);
        hasher.update([0]);
        hasher.update(proxy_url.as_bytes());
        let digest = format!("{:x}", hasher.finalize());
        Ok(UpstreamProxyMetadata {
            has_proxy: true,
            scheme: valid.then_some(scheme).flatten(),
            remote_dns,
            label: Some(if !valid {
                "Configured proxy requires update".to_owned()
            } else if remote_dns {
                "SOCKS5H private proxy".to_owned()
            } else {
                "SOCKS5 private proxy".to_owned()
            }),
            fingerprint: Some(format!("proxy_{}", &digest[..16])),
        })
    }

    pub(crate) fn supports_transport_proxy(&self) -> bool {
        matches!(self, Self::OAuth { .. } | Self::ProxiedApiKey { .. })
    }

    /// Replace only a proxy container, preserving all other credential fields.
    pub(crate) fn with_transport_proxy(self, proxy_url: String) -> Result<Self, AppError> {
        validate_proxy_url(&proxy_url)?;
        match self {
            Self::ProxiedApiKey {
                value,
                header,
                prefix,
                ..
            } => Ok(Self::ProxiedApiKey {
                value,
                header,
                prefix,
                proxy_url,
                proxy_network_scope: OutboundScope::Private,
            }),
            Self::OAuth {
                access_token,
                refresh_token,
                expires_at,
                header,
                prefix,
                adapter_state,
                ..
            } => Ok(Self::OAuth {
                access_token,
                refresh_token,
                expires_at,
                header,
                prefix,
                adapter_state,
                proxy_url: Some(proxy_url),
                proxy_network_scope: Some(OutboundScope::Private),
            }),
            _ => Err(AppError::BadRequest(
                "this credential type does not support transport proxy updates".into(),
            )),
        }
    }

    /// Preserve an imported account proxy when an ordinary API-key rotation
    /// supplies only replacement key material. A caller that needs to change
    /// the proxy must use the explicit proxied credential form. Removing it
    /// requires a future dedicated transport operation, so a routine rotation
    /// cannot silently bypass required egress routing.
    pub fn preserve_proxy_from(self, current: &Self) -> Self {
        match (self, current) {
            (
                Self::ApiKey {
                    value,
                    header,
                    prefix,
                },
                Self::ProxiedApiKey {
                    proxy_url,
                    proxy_network_scope,
                    ..
                },
            ) => Self::ProxiedApiKey {
                value,
                header,
                prefix,
                proxy_url: proxy_url.clone(),
                proxy_network_scope: *proxy_network_scope,
            },
            (
                Self::OAuth {
                    access_token,
                    refresh_token,
                    expires_at,
                    header,
                    prefix,
                    adapter_state,
                    proxy_url: None,
                    proxy_network_scope: None,
                },
                Self::OAuth {
                    proxy_url: Some(proxy_url),
                    proxy_network_scope: Some(proxy_network_scope),
                    ..
                },
            ) => Self::OAuth {
                access_token,
                refresh_token,
                expires_at,
                header,
                prefix,
                adapter_state,
                proxy_url: Some(proxy_url.clone()),
                proxy_network_scope: Some(*proxy_network_scope),
            },
            (replacement, _) => replacement,
        }
    }
}

pub(crate) fn validate_provider_adapter_secret_patch(patch: &Value) -> Result<Vec<u8>, AppError> {
    let patch = serde_json::to_vec(patch).map_err(|_| AppError::Internal)?;
    if patch.len() > MAX_PROVIDER_ADAPTER_SECRET_PATCH_BYTES {
        return Err(AppError::BadRequest(
            "OAuth provider secret configuration exceeds its storage limit".into(),
        ));
    }
    Ok(patch)
}

fn validate_optional_private_proxy(
    proxy_url: Option<&str>,
    proxy_network_scope: Option<OutboundScope>,
) -> Result<(), AppError> {
    match (proxy_url, proxy_network_scope) {
        (None, None) => Ok(()),
        (Some(proxy_url), Some(OutboundScope::Private)) => validate_proxy_url(proxy_url),
        _ => Err(AppError::BadRequest(
            "upstream SOCKS5 proxy must use private network scope".into(),
        )),
    }
}

pub(crate) fn validate_proxy_url(value: &str) -> Result<(), AppError> {
    if value.len() > 2_048 || value.trim() != value || value.bytes().any(|byte| byte < 0x20) {
        return Err(AppError::BadRequest("upstream proxy URL is invalid".into()));
    }
    let parsed = url::Url::parse(value)
        .map_err(|_| AppError::BadRequest("upstream proxy URL is invalid".into()))?;
    if !matches!(parsed.scheme(), "socks5" | "socks5h")
        || parsed.host_str().is_none()
        || parsed.port() == Some(0)
        || (parsed.path() != "" && parsed.path() != "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(AppError::BadRequest("upstream proxy URL is invalid".into()));
    }
    Ok(())
}

pub(crate) fn validate_codex_proxy_url(value: &str) -> Result<(), AppError> {
    validate_proxy_url(value)?;
    let parsed = url::Url::parse(value)
        .map_err(|_| AppError::BadRequest("upstream proxy URL is invalid".into()))?;
    if parsed.scheme() != "socks5h" || !has_safe_private_ip_literal_host(&parsed) {
        return Err(AppError::BadRequest(
            "OpenAI Codex requires a private IP-literal socks5h proxy with remote DNS".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_oauth_remote_dns_proxy_url(
    value: &str,
    allow_test_loopback: bool,
) -> Result<(), AppError> {
    validate_proxy_url(value)?;
    let parsed = url::Url::parse(value)
        .map_err(|_| AppError::BadRequest("upstream proxy URL is invalid".into()))?;
    let test_loopback = allow_test_loopback
        && parsed
            .host_str()
            .and_then(|host| host.parse::<std::net::IpAddr>().ok())
            .is_some_and(|address| address.is_loopback());
    if parsed.scheme() != "socks5h"
        || (!has_safe_private_ip_literal_host(&parsed) && !test_loopback)
    {
        return Err(AppError::BadRequest(
            "OAuth authorization requires a private IP-literal socks5h proxy with remote DNS"
                .into(),
        ));
    }
    Ok(())
}

fn deserialize_adapter_state<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: Deserializer<'de>,
{
    let state = Option::<Value>::deserialize(deserializer)?;
    if let Some(value) = &state {
        validate_adapter_state(value).map_err(de::Error::custom)?;
    }
    Ok(state)
}

pub fn validate_adapter_state(state: &Value) -> Result<(), AppError> {
    let mut provider_state = state.clone();
    if let Some(object) = provider_state.as_object_mut()
        && object.remove(PROVIDER_ADAPTER_SECRET_PATCH_KEY).is_some()
    {
        decode_provider_adapter_secret_patch(state)?.ok_or(AppError::Internal)?;
    }
    let encoded = serde_json::to_vec(&provider_state).map_err(|_| AppError::Internal)?;
    if encoded.len() > MAX_ADAPTER_STATE_BYTES {
        return Err(AppError::BadRequest(
            "managed OAuth adapter state exceeds its size limit".into(),
        ));
    }
    fn visit(value: &Value, depth: usize, nodes: &mut usize) -> bool {
        *nodes = nodes.saturating_add(1);
        if *nodes > MAX_ADAPTER_STATE_NODES || depth > MAX_ADAPTER_STATE_DEPTH {
            return false;
        }
        match value {
            Value::Array(values) => values
                .iter()
                .all(|value| visit(value, depth.saturating_add(1), nodes)),
            Value::Object(values) => values
                .values()
                .all(|value| visit(value, depth.saturating_add(1), nodes)),
            _ => true,
        }
    }
    let mut nodes = 0;
    if !visit(&provider_state, 0, &mut nodes) {
        return Err(AppError::BadRequest(
            "managed OAuth adapter state exceeds its structural limit".into(),
        ));
    }
    Ok(())
}

fn decode_provider_adapter_secret_patch(state: &Value) -> Result<Option<Value>, AppError> {
    let Some(encoded) = state
        .as_object()
        .and_then(|object| object.get(PROVIDER_ADAPTER_SECRET_PATCH_KEY))
        .and_then(Value::as_str)
    else {
        return Ok(None);
    };
    let decoded = URL_SAFE_NO_PAD
        .decode(encoded)
        .map_err(|_| AppError::Internal)?;
    if decoded.len() > MAX_PROVIDER_ADAPTER_SECRET_PATCH_BYTES {
        return Err(AppError::BadRequest(
            "OAuth provider secret configuration exceeds its storage limit".into(),
        ));
    }
    serde_json::from_slice(&decoded)
        .map(Some)
        .map_err(|_| AppError::Internal)
}

fn apply_provider_adapter_secret_patch(
    mut config: Value,
    patch: &Value,
) -> Result<Value, AppError> {
    let entries = patch.as_array().ok_or(AppError::Internal)?;
    for entry in entries {
        let entry = entry.as_object().ok_or(AppError::Internal)?;
        if entry.len() != 2 {
            return Err(AppError::Internal);
        }
        let path = entry
            .get("path")
            .and_then(Value::as_array)
            .ok_or(AppError::Internal)?
            .iter()
            .map(|segment| {
                segment
                    .as_str()
                    .map(str::to_owned)
                    .ok_or(AppError::Internal)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let value = entry.get("value").cloned().ok_or(AppError::Internal)?;
        if path.is_empty() {
            config = value;
            continue;
        }
        let mut target = &mut config;
        for segment in &path[..path.len() - 1] {
            target = target
                .as_object_mut()
                .ok_or(AppError::Internal)?
                .entry(segment.clone())
                .or_insert_with(|| Value::Object(serde_json::Map::new()));
        }
        target
            .as_object_mut()
            .ok_or(AppError::Internal)?
            .insert(path.last().cloned().ok_or(AppError::Internal)?, value);
    }
    Ok(config)
}

pub(super) fn authorization_header() -> String {
    "authorization".to_owned()
}

pub(super) fn bearer_prefix() -> String {
    "Bearer ".to_owned()
}

pub fn validate_config(config: &Value) -> Result<String, AppError> {
    let base_url = config
        .get("base_url")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::BadRequest("upstream config.base_url is required".into()))?;
    let parsed = url::Url::parse(base_url)
        .map_err(|_| AppError::BadRequest("upstream base_url must be a URL".into()))?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err(AppError::BadRequest(
            "upstream base_url must be an HTTP(S) origin".into(),
        ));
    }
    if parsed.username() != ""
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(AppError::BadRequest(
            "upstream base_url cannot contain credentials, a query, or a fragment".into(),
        ));
    }
    Ok(base_url.trim_end_matches('/').to_owned())
}

pub fn seal_credential(
    credential: &UpstreamCredential,
    key_material: &[u8],
) -> Result<String, AppError> {
    if let UpstreamCredential::OAuth {
        adapter_state: Some(state),
        ..
    } = credential
    {
        validate_adapter_state(state)?;
    }
    seal_private_json(credential, key_material, ENVELOPE_AAD)
}

pub(crate) fn seal_private_json<T: Serialize>(
    value: &T,
    key_material: &[u8],
    aad: &[u8],
) -> Result<String, AppError> {
    let mut nonce = [0_u8; 12];
    fill(&mut nonce).map_err(|_| AppError::Internal)?;
    seal_private_json_with_nonce(value, key_material, aad, nonce)
}

/// Seals a private value with a caller-provided nonce. This is restricted to
/// durable records whose caller can prove nonce uniqueness and needs an exact
/// ciphertext replay after an unknown database COMMIT acknowledgement.
pub(crate) fn seal_private_json_with_nonce<T: Serialize>(
    value: &T,
    key_material: &[u8],
    aad: &[u8],
    nonce: [u8; 12],
) -> Result<String, AppError> {
    let plaintext = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&current_encryption_key(key_material)?)
        .map_err(|_| AppError::Internal)?;
    let ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plaintext,
                aad,
            },
        )
        .map_err(|_| AppError::Internal)?;
    Ok(format!(
        "{CURRENT_ENVELOPE_VERSION}.{}.{}",
        URL_SAFE_NO_PAD.encode(nonce),
        URL_SAFE_NO_PAD.encode(ciphertext)
    ))
}

pub fn open_credential(
    envelope: &str,
    key_material: &[u8],
) -> Result<UpstreamCredential, AppError> {
    let credential = open_private_json(envelope, key_material, ENVELOPE_AAD)?;
    if let UpstreamCredential::OAuth {
        adapter_state: Some(state),
        ..
    } = &credential
    {
        validate_adapter_state(state)?;
    }
    Ok(credential)
}

pub(crate) fn open_private_json<T: for<'de> Deserialize<'de>>(
    envelope: &str,
    key_material: &[u8],
    aad: &[u8],
) -> Result<T, AppError> {
    let mut parts = envelope.split('.');
    let version = parts.next();
    let nonce = parts.next();
    let ciphertext = parts.next();
    let key = match version {
        Some(CURRENT_ENVELOPE_VERSION) => current_encryption_key(key_material)?,
        // Existing deployments wrote v1 envelopes with the historical
        // SHA-256 derivation. Keep that format read-only so an upgrade never
        // strands credentials; every new write uses RustCrypto HKDF below.
        Some(LEGACY_ENVELOPE_VERSION) => legacy_encryption_key(key_material),
        _ => return Err(AppError::Internal),
    };
    if parts.next().is_some() {
        return Err(AppError::Internal);
    }
    let nonce = URL_SAFE_NO_PAD
        .decode(nonce.ok_or(AppError::Internal)?)
        .map_err(|_| AppError::Internal)?;
    let nonce: [u8; 12] = nonce.try_into().map_err(|_| AppError::Internal)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(ciphertext.ok_or(AppError::Internal)?)
        .map_err(|_| AppError::Internal)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&key).map_err(|_| AppError::Internal)?;
    let plaintext = cipher
        .decrypt(
            (&nonce).into(),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| AppError::Internal)?;
    serde_json::from_slice(&plaintext).map_err(|_| AppError::Internal)
}

pub(super) fn current_encryption_key(key_material: &[u8]) -> Result<[u8; 32], AppError> {
    let hkdf = hkdf::Hkdf::<Sha256>::new(
        Some(b"memeloop-token-center/private-envelope/hkdf-sha256/v2"),
        key_material,
    );
    let mut key = [0_u8; 32];
    hkdf.expand(b"chacha20poly1305-key", &mut key)
        .map_err(|_| AppError::Internal)?;
    Ok(key)
}

pub(super) fn legacy_encryption_key(key_material: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"memeloop-token-center/upstream-encryption-key/v1\0");
    hash.update(key_material);
    hash.finalize().into()
}

#[cfg(test)]
mod proxy_tests {
    use super::*;

    fn proxied() -> UpstreamCredential {
        UpstreamCredential::ProxiedApiKey {
            value: "api-secret".into(),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            proxy_url: "socks5://proxy-user:proxy-secret@10.20.30.40:1080".into(),
            proxy_network_scope: OutboundScope::Private,
        }
    }

    fn proxied_oauth() -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "oauth-access-secret".into(),
            refresh_token: Some("oauth-refresh-secret".into()),
            expires_at: Some(i64::MAX),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(serde_json::json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "account-123"
            })),
            proxy_url: Some("socks5://proxy-user:proxy-secret@100.64.0.16:1080".into()),
            proxy_network_scope: Some(OutboundScope::Private),
        }
    }

    #[test]
    fn proxied_api_key_is_encrypted_redacted_and_round_trips() {
        let credential = proxied();
        credential.validate(0).unwrap();
        let debug = format!("{credential:?}");
        assert!(!debug.contains("api-secret"));
        assert!(!debug.contains("proxy-secret"));
        assert!(!debug.contains("10.20.30.40"));

        let envelope = seal_credential(&credential, b"test-key-material").unwrap();
        assert!(!envelope.contains("api-secret"));
        assert!(!envelope.contains("proxy-secret"));
        let opened = open_credential(&envelope, b"test-key-material").unwrap();
        assert_eq!(
            opened.proxy(),
            Some((
                "socks5://proxy-user:proxy-secret@10.20.30.40:1080",
                OutboundScope::Private
            ))
        );
    }

    #[test]
    fn ordinary_rotation_preserves_proxy_and_invalid_proxy_shapes_fail() {
        let rotated = UpstreamCredential::ApiKey {
            value: "replacement".into(),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
        }
        .preserve_proxy_from(&proxied());
        assert_eq!(
            rotated.proxy(),
            Some((
                "socks5://proxy-user:proxy-secret@10.20.30.40:1080",
                OutboundScope::Private
            ))
        );
        for proxy_url in [
            "https://10.20.30.40:8443",
            "socks5://10.20.30.40:1080/path",
            "socks5://10.20.30.40:1080?secret=value",
            "socks5://10.20.30.40:0",
            "file:///tmp/proxy",
        ] {
            let mut credential = proxied();
            if let UpstreamCredential::ProxiedApiKey {
                proxy_url: value, ..
            } = &mut credential
            {
                *value = proxy_url.into();
            }
            assert!(credential.validate(0).is_err(), "{proxy_url}");
        }
        for proxy_url in [
            "socks5h://proxy.example.test:1080",
            "socks5h://8.8.8.8:1080",
        ] {
            let mut credential = proxied();
            if let UpstreamCredential::ProxiedApiKey {
                proxy_url: value, ..
            } = &mut credential
            {
                *value = proxy_url.into();
            }
            credential.validate(0).unwrap();
        }
        let mut remote_dns = proxied();
        if let UpstreamCredential::ProxiedApiKey {
            proxy_url: value, ..
        } = &mut remote_dns
        {
            *value = "socks5h://proxy-user:proxy-secret@10.20.30.40:1080".into();
        }
        remote_dns.validate(0).unwrap();
        assert_eq!(
            remote_dns.proxy(),
            Some((
                "socks5h://proxy-user:proxy-secret@10.20.30.40:1080",
                OutboundScope::Private
            ))
        );
        let mut public_scope = proxied();
        if let UpstreamCredential::ProxiedApiKey {
            proxy_network_scope,
            ..
        } = &mut public_scope
        {
            *proxy_network_scope = OutboundScope::Public;
        }
        assert!(public_scope.validate(0).is_err());
    }

    #[test]
    fn oauth_proxy_is_encrypted_redacted_optional_and_preserved() {
        let credential = proxied_oauth();
        credential.validate(0).unwrap();
        let debug = format!("{credential:?}");
        for secret in [
            "oauth-access-secret",
            "oauth-refresh-secret",
            "proxy-secret",
            "100.64.0.16",
        ] {
            assert!(!debug.contains(secret));
        }
        let envelope = seal_credential(&credential, b"test-key-material").unwrap();
        assert!(!envelope.contains("proxy-secret"));
        let opened = open_credential(&envelope, b"test-key-material").unwrap();
        assert_eq!(opened.proxy(), credential.proxy());

        let legacy_json = serde_json::json!({
            "type": "oauth",
            "access_token": "old-access",
            "refresh_token": "old-refresh",
            "expires_at": i64::MAX,
            "header": "authorization",
            "prefix": "Bearer ",
            "adapter_state": null
        });
        let legacy: UpstreamCredential = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy.proxy(), None);
        legacy.validate(0).unwrap();

        let replacement = UpstreamCredential::OAuth {
            access_token: "replacement-access".into(),
            refresh_token: Some("replacement-refresh".into()),
            expires_at: Some(i64::MAX),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        }
        .preserve_proxy_from(&credential);
        assert_eq!(replacement.proxy(), credential.proxy());
    }

    #[test]
    fn provider_adapter_secret_patch_is_carried_only_inside_the_encrypted_credential() {
        let secret = "synthetic-provider-config-secret";
        let patch = serde_json::json!([{"path":["client_secret"],"value":secret}]);
        let credential = UpstreamCredential::OAuth {
            access_token: "synthetic-access".into(),
            refresh_token: Some("synthetic-refresh".into()),
            expires_at: Some(i64::MAX),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        }
        .with_provider_adapter_secret_patch(&patch)
        .unwrap();
        let envelope = seal_credential(&credential, b"test-key-material").unwrap();
        assert!(!envelope.contains(secret));
        let opened = open_credential(&envelope, b"test-key-material").unwrap();
        assert_eq!(
            opened
                .hydrate_provider_adapter_config(serde_json::json!({
                    "base_url": "https://provider.example/api"
                }))
                .unwrap(),
            serde_json::json!({
                "base_url": "https://provider.example/api",
                "client_secret": secret
            })
        );
    }

    #[test]
    fn oauth_proxy_fields_must_be_paired_and_private() {
        for (proxy_url, scope) in [
            (Some("socks5://100.64.0.16:1080".into()), None),
            (None, Some(OutboundScope::Private)),
            (
                Some("socks5://100.64.0.16:1080".into()),
                Some(OutboundScope::Public),
            ),
            (
                Some("https://100.64.0.16:1080".into()),
                Some(OutboundScope::Private),
            ),
        ] {
            let mut credential = proxied_oauth();
            if let UpstreamCredential::OAuth {
                proxy_url: current_url,
                proxy_network_scope: current_scope,
                ..
            } = &mut credential
            {
                *current_url = proxy_url;
                *current_scope = scope;
            }
            assert!(credential.validate(0).is_err());
        }
    }

    #[test]
    fn native_oauth_remote_dns_proxy_requires_socks5h_private_literal() {
        validate_oauth_remote_dns_proxy_url("socks5h://100.64.0.16:1080", false).unwrap();
        validate_oauth_remote_dns_proxy_url("socks5h://127.0.0.1:1080", true).unwrap();
        for proxy in [
            "socks5://100.64.0.16:1080",
            "socks5h://proxy.internal:1080",
            "socks5h://8.8.8.8:1080",
            "socks5h://127.0.0.1:1080",
        ] {
            assert!(
                validate_oauth_remote_dns_proxy_url(proxy, false).is_err(),
                "{proxy}"
            );
        }
    }
}
