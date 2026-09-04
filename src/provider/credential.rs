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
        self.validate(now)?;
        let (secret, header, prefix) = match self {
            Self::None => return Ok(request),
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
        let header_name = reqwest::header::HeaderName::from_bytes(header.as_bytes())
            .map_err(|_| AppError::BadRequest("invalid upstream credential header".into()))?;
        let value = reqwest::header::HeaderValue::from_str(&format!("{prefix}{secret}"))
            .map_err(|_| AppError::BadRequest("invalid upstream credential value".into()))?;
        Ok(request.header(header_name, value))
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

fn validate_proxy_url(value: &str) -> Result<(), AppError> {
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
    if parsed.scheme() == "socks5h" && !has_safe_private_ip_literal_host(&parsed) {
        return Err(AppError::BadRequest("upstream proxy URL is invalid".into()));
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
    let encoded = serde_json::to_vec(state).map_err(|_| AppError::Internal)?;
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
    if !visit(state, 0, &mut nodes) {
        return Err(AppError::BadRequest(
            "managed OAuth adapter state exceeds its structural limit".into(),
        ));
    }
    Ok(())
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
    let plaintext = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&current_encryption_key(key_material)?)
        .map_err(|_| AppError::Internal)?;
    let mut nonce = [0_u8; 12];
    fill(&mut nonce).map_err(|_| AppError::Internal)?;
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
            "socks5h://proxy.internal:1080",
            "socks5h://8.8.8.8:1080",
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
}
