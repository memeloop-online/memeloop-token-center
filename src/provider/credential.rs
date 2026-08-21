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
            Self::OAuth {
                refresh_token,
                expires_at,
                adapter_state,
                ..
            } => formatter
                .debug_struct("UpstreamCredential::OAuth")
                .field("access_token", &"[redacted]")
                .field("has_refresh_token", &refresh_token.is_some())
                .field("expires_at", expires_at)
                .field("has_adapter_state", &adapter_state.is_some())
                .finish(),
        }
    }
}

impl UpstreamCredential {
    pub fn auth_kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::OAuth { .. } => "oauth",
            Self::ApiKey { .. } => "api_key",
        }
    }

    pub fn expires_at(&self) -> Option<i64> {
        match self {
            Self::None | Self::ApiKey { .. } => None,
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
            Self::OAuth {
                access_token,
                expires_at,
                header,
                prefix,
                adapter_state,
                ..
            } => {
                if let Some(state) = adapter_state {
                    validate_adapter_state(state)?;
                }
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
