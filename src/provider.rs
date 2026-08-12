use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use getrandom::fill;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::AppError;

const ENVELOPE_VERSION: &str = "v1";
const ENVELOPE_AAD: &[u8] = b"memeloop-token-center/upstream-credential/v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpstreamCredential {
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
    },
}

impl UpstreamCredential {
    pub fn auth_kind(&self) -> &'static str {
        match self {
            Self::ApiKey { .. } => "api_key",
            Self::OAuth { .. } => "oauth",
        }
    }

    pub fn expires_at(&self) -> Option<i64> {
        match self {
            Self::ApiKey { .. } => None,
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
                ..
            } => {
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
}

fn authorization_header() -> String {
    "authorization".to_owned()
}

fn bearer_prefix() -> String {
    "Bearer ".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderType {
    pub id: String,
    pub display_name: String,
    pub protocols: Vec<String>,
    pub modalities: Vec<String>,
    pub config_schema: Value,
    pub credential_schema: Value,
    pub source: String,
}

#[derive(Clone)]
pub struct ProviderCatalog {
    types: Vec<ProviderType>,
}

impl ProviderCatalog {
    pub fn builtins() -> Self {
        let config_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["base_url"],
            "properties": {
                "base_url": {"type": "string", "format": "uri"},
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600, "default": 120},
                "oauth": {
                    "type": "object",
                    "readOnly": true,
                    "additionalProperties": false,
                    "required": ["driver", "refresh_url"],
                    "properties": {
                        "driver": {"type": "string"},
                        "refresh_url": {"type": "string", "format": "uri"}
                    }
                }
            }
        });
        let credential_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "oneOf": [
                {
                    "title": "API key",
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "value"],
                    "properties": {
                        "type": {"const": "api_key"},
                        "value": {"type": "string", "minLength": 1, "writeOnly": true},
                        "header": {"type": "string", "default": "authorization"},
                        "prefix": {"type": "string", "default": "Bearer "}
                    }
                },
                {
                    "title": "OAuth",
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "access_token"],
                    "properties": {
                        "type": {"const": "oauth"},
                        "access_token": {"type": "string", "minLength": 1, "writeOnly": true},
                        "refresh_token": {"type": "string", "writeOnly": true},
                        "expires_at": {"type": "integer", "description": "Unix milliseconds"},
                        "header": {"type": "string", "default": "authorization"},
                        "prefix": {"type": "string", "default": "Bearer "}
                    }
                }
            ]
        });
        Self {
            types: vec![ProviderType {
                id: "http-json".to_owned(),
                display_name: "HTTP JSON upstream".to_owned(),
                protocols: vec!["openai".to_owned(), "anthropic".to_owned()],
                modalities: vec![
                    "text".to_owned(),
                    "embedding".to_owned(),
                    "image".to_owned(),
                    "video".to_owned(),
                ],
                config_schema,
                credential_schema,
                source: "builtin".to_owned(),
            }],
        }
    }

    pub fn list(&self) -> &[ProviderType] {
        &self.types
    }

    pub fn extend(
        &mut self,
        contributions: impl IntoIterator<Item = ProviderType>,
    ) -> Result<(), AppError> {
        for contribution in contributions {
            if contribution.id.trim().is_empty()
                || self
                    .types
                    .iter()
                    .any(|provider| provider.id == contribution.id)
            {
                return Err(AppError::BadRequest(format!(
                    "duplicate or empty provider type: {}",
                    contribution.id
                )));
            }
            self.types.push(contribution);
        }
        Ok(())
    }

    pub fn contains(&self, driver: &str) -> bool {
        self.types.iter().any(|provider| provider.id == driver)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct UpstreamAccountView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub name: String,
    pub driver: String,
    pub auth_kind: String,
    pub credential_generation: i64,
    pub status: String,
    pub config: Value,
    pub credential_expires_at: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelRouteView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct ResolvedUpstream {
    pub route_id: Uuid,
    pub account_id: Uuid,
    pub driver: String,
    pub base_url: String,
    pub upstream_model: String,
    pub credential: UpstreamCredential,
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
    if parsed.username() != "" || parsed.password().is_some() || parsed.query().is_some() {
        return Err(AppError::BadRequest(
            "upstream base_url cannot contain credentials or a query".into(),
        ));
    }
    Ok(base_url.trim_end_matches('/').to_owned())
}

pub fn seal_credential(
    credential: &UpstreamCredential,
    key_material: &[u8],
) -> Result<String, AppError> {
    seal_private_json(credential, key_material, ENVELOPE_AAD)
}

pub(crate) fn seal_private_json<T: Serialize>(
    value: &T,
    key_material: &[u8],
    aad: &[u8],
) -> Result<String, AppError> {
    let plaintext = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&encryption_key(key_material))
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
        "{ENVELOPE_VERSION}.{}.{}",
        URL_SAFE_NO_PAD.encode(nonce),
        URL_SAFE_NO_PAD.encode(ciphertext)
    ))
}

pub fn open_credential(
    envelope: &str,
    key_material: &[u8],
) -> Result<UpstreamCredential, AppError> {
    open_private_json(envelope, key_material, ENVELOPE_AAD)
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
    if version != Some(ENVELOPE_VERSION) || parts.next().is_some() {
        return Err(AppError::Internal);
    }
    let nonce = URL_SAFE_NO_PAD
        .decode(nonce.ok_or(AppError::Internal)?)
        .map_err(|_| AppError::Internal)?;
    let nonce: [u8; 12] = nonce.try_into().map_err(|_| AppError::Internal)?;
    let ciphertext = URL_SAFE_NO_PAD
        .decode(ciphertext.ok_or(AppError::Internal)?)
        .map_err(|_| AppError::Internal)?;
    let cipher = ChaCha20Poly1305::new_from_slice(&encryption_key(key_material))
        .map_err(|_| AppError::Internal)?;
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

fn encryption_key(key_material: &[u8]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(b"memeloop-token-center/upstream-encryption-key/v1\0");
    hash.update(key_material);
    hash.finalize().into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credential_envelope_round_trips_without_plaintext() {
        let credential = UpstreamCredential::OAuth {
            access_token: "secret-access".to_owned(),
            refresh_token: Some("secret-refresh".to_owned()),
            expires_at: Some(42),
            header: authorization_header(),
            prefix: bearer_prefix(),
        };
        let envelope =
            seal_credential(&credential, b"a key material with at least 32 bytes").unwrap();
        assert!(!envelope.contains("secret"));
        let opened = open_credential(&envelope, b"a key material with at least 32 bytes").unwrap();
        assert_eq!(opened.auth_kind(), "oauth");
        assert_eq!(opened.expires_at(), Some(42));
    }
}
