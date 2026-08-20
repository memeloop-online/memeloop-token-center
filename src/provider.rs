use std::sync::Arc;

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use getrandom::fill;
use serde::{Deserialize, Deserializer, Serialize, de};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::error::AppError;

const ENVELOPE_VERSION: &str = "v1";
const ENVELOPE_AAD: &[u8] = b"memeloop-token-center/upstream-credential/v1";

const MAX_ADAPTER_STATE_BYTES: usize = 16 * 1024;
const MAX_ADAPTER_STATE_DEPTH: usize = 8;
const MAX_ADAPTER_STATE_NODES: usize = 256;
pub const MANAGED_OAUTH_ADAPTER_API_VERSION: &str = "cpa-managed-oauth-adapter-v1";

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UpstreamCredential {
    None,
    SubscriptionBridge {
        handle: String,
        secret: Option<String>,
    },
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
            Self::SubscriptionBridge { secret, .. } => formatter
                .debug_struct("UpstreamCredential::SubscriptionBridge")
                .field("has_secret", &secret.is_some())
                .finish(),
            Self::ApiKey { header, prefix, .. } => formatter
                .debug_struct("UpstreamCredential::ApiKey")
                .field("header", header)
                .field("prefix", prefix)
                .field("value", &"[redacted]")
                .finish(),
            Self::OAuth {
                refresh_token,
                expires_at,
                header,
                prefix,
                adapter_state,
                ..
            } => formatter
                .debug_struct("UpstreamCredential::OAuth")
                .field("access_token", &"[redacted]")
                .field("has_refresh_token", &refresh_token.is_some())
                .field("expires_at", expires_at)
                .field("header", header)
                .field("prefix", prefix)
                .field("has_adapter_state", &adapter_state.is_some())
                .finish(),
        }
    }
}

impl UpstreamCredential {
    pub fn auth_kind(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SubscriptionBridge { .. } | Self::OAuth { .. } => "oauth",
            Self::ApiKey { .. } => "api_key",
        }
    }

    pub fn expires_at(&self) -> Option<i64> {
        match self {
            Self::None | Self::SubscriptionBridge { .. } | Self::ApiKey { .. } => None,
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
            Self::SubscriptionBridge { secret, .. } => {
                return Ok(match secret {
                    Some(secret) => request.bearer_auth(secret),
                    None => request,
                });
            }
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
            Self::SubscriptionBridge { handle, secret } => {
                validate_bridge_handle(handle)?;
                let Some(secret) = secret.as_ref() else {
                    return Ok(());
                };
                if secret.is_empty() {
                    return Err(AppError::BadRequest(
                        "subscription bridge secret cannot be empty".into(),
                    ));
                }
                reqwest::header::HeaderValue::from_str(&format!("Bearer {secret}")).map_err(
                    |_| AppError::BadRequest("invalid subscription bridge secret".into()),
                )?;
                return Ok(());
            }
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

    pub fn subscription_bridge_handle(&self) -> Option<&str> {
        match self {
            Self::SubscriptionBridge { handle, .. } => Some(handle),
            _ => None,
        }
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

fn validate_bridge_handle(handle: &str) -> Result<(), AppError> {
    if handle.is_empty()
        || handle.len() > 80
        || !handle
            .chars()
            .all(|character| character.is_ascii_alphanumeric())
    {
        return Err(AppError::BadRequest(
            "subscription bridge handle must be 1-80 ASCII alphanumeric characters".into(),
        ));
    }
    Ok(())
}

fn authorization_header() -> String {
    "authorization".to_owned()
}

fn bearer_prefix() -> String {
    "Bearer ".to_owned()
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthAdapterContribution {
    /// Versioned host/adapter contract. Keeping this explicit lets future
    /// device-code and callback flows coexist without guessing from URLs.
    pub api_version: String,
    pub flow_kind: OAuthFlowKind,
    pub login_url: String,
    pub poll_url: String,
    pub refresh_url: String,
}

/// A non-interactive adapter for normalizing and refreshing CPA managed OAuth
/// documents. This is deliberately separate from the interactive PKCE flow.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedOAuthAdapterContribution {
    pub api_version: String,
    pub source_types: Vec<String>,
    pub normalize_url: String,
    pub refresh_url: String,
}

#[derive(Clone, Debug)]
pub struct ResolvedManagedOAuthAdapter {
    provider_driver: String,
    source_type: String,
    api_version: String,
    backend: ManagedOAuthAdapterBackend,
}

/// The administrator-reviewed implementation selected by the server catalog.
/// Builtins never synthesize an HTTP contribution or accept a client URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedOAuthAdapterBackend {
    BuiltinCodex,
    BuiltinLegacyGemini,
    ReviewedHttp {
        normalize_url: String,
        refresh_url: String,
    },
}

impl ResolvedManagedOAuthAdapter {
    #[cfg(test)]
    pub(crate) fn for_test(
        provider_driver: &str,
        source_type: &str,
        normalize_url: String,
        refresh_url: String,
    ) -> Self {
        Self {
            provider_driver: provider_driver.to_owned(),
            source_type: source_type.to_owned(),
            api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.to_owned(),
            backend: ManagedOAuthAdapterBackend::ReviewedHttp {
                normalize_url,
                refresh_url,
            },
        }
    }

    pub fn provider_driver(&self) -> &str {
        &self.provider_driver
    }

    pub fn source_type(&self) -> &str {
        &self.source_type
    }

    pub fn api_version(&self) -> &str {
        &self.api_version
    }

    pub fn backend(&self) -> &ManagedOAuthAdapterBackend {
        &self.backend
    }

    pub fn normalize_url(&self) -> Option<&str> {
        match &self.backend {
            ManagedOAuthAdapterBackend::ReviewedHttp { normalize_url, .. } => Some(normalize_url),
            ManagedOAuthAdapterBackend::BuiltinCodex
            | ManagedOAuthAdapterBackend::BuiltinLegacyGemini => None,
        }
    }

    pub fn refresh_url(&self) -> &str {
        match &self.backend {
            ManagedOAuthAdapterBackend::BuiltinCodex => {
                crate::oauth::managed::codex::TOKEN_ENDPOINT
            }
            ManagedOAuthAdapterBackend::BuiltinLegacyGemini => {
                crate::oauth::managed::legacy_gemini::TOKEN_ENDPOINT
            }
            ManagedOAuthAdapterBackend::ReviewedHttp { refresh_url, .. } => refresh_url,
        }
    }

    pub fn can_refresh(&self) -> bool {
        !matches!(
            self.backend(),
            ManagedOAuthAdapterBackend::BuiltinLegacyGemini
        )
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthFlowKind {
    /// Cursor-compatible redirect/PKCE login and polling contract.
    CursorPkce,
}

/// Explicit opt-in to the executable provider ABI. Component providers are
/// buffered-only in this contract; streaming requests fail closed rather than
/// silently falling back to the built-in HTTP JSON driver.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ComponentAdapterContribution {
    pub api_version: String,
    pub max_response_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderType {
    pub id: String,
    pub display_name: String,
    pub protocols: Vec<String>,
    pub modalities: Vec<String>,
    pub config_schema: Value,
    pub credential_schema: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth_adapter: Option<OAuthAdapterContribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub managed_oauth_adapter: Option<ManagedOAuthAdapterContribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_adapter: Option<ComponentAdapterContribution>,
    #[serde(default)]
    pub source: String,
}

#[derive(Clone)]
pub struct ProviderCatalog {
    types: Arc<Vec<ProviderType>>,
    builtin_managed_oauth: Arc<Vec<BuiltinManagedOAuthRegistration>>,
}

#[derive(Clone)]
struct BuiltinManagedOAuthRegistration {
    provider_driver: &'static str,
    source_type: &'static str,
    backend: ManagedOAuthAdapterBackend,
}

impl ProviderCatalog {
    pub fn builtins() -> Self {
        let config_schema = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["base_url"],
            "properties": {
                "base_url": {"type": "string", "format": "uri", "title": "Base URL"},
                "network_scope": {
                    "title": "Network scope",
                    "type": "string",
                    "enum": ["public", "private"],
                    "default": "public",
                    "description": "Private destinations require a global operator credential."
                },
                "timeout_seconds": {"type": "integer", "minimum": 1, "maximum": 600, "default": 120},
                "input_token_overhead_ceiling": {
                    "title": "Input token overhead ceiling",
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 1000000,
                    "default": 0,
                    "description": "Trusted reservation allowance for input tokens added by a compatible upstream outside the forwarded request body."
                },
                "image_api_mode": {
                    "title": "Image generation API",
                    "type": "string",
                    "enum": ["images", "responses-tool"],
                    "default": "images",
                    "description": "Use responses-tool when a Codex-compatible upstream exposes image_generation through /v1/responses."
                },
                "image_main_model": {
                    "title": "Image generation model",
                    "type": "string",
                    "minLength": 1,
                    "description": "Responses model used to invoke image_generation when image_api_mode is responses-tool."
                },
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
                    "title": "No authentication",
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type"],
                    "properties": {
                        "type": {"const": "none", "title": "Credential type"}
                    }
                },
                {
                    "title": "API key",
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "value"],
                    "properties": {
                        "type": {"const": "api_key", "title": "Credential type"},
                        "value": {"type": "string", "minLength": 1, "writeOnly": true, "title": "Credential value"},
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
                        "prefix": {"type": "string", "default": "Bearer "},
                        "adapter_state": {
                            "description": "Opaque encrypted state for a server-installed managed OAuth adapter.",
                            "writeOnly": true
                        }
                    }
                }
            ]
        });
        let mut types = vec![ProviderType {
            id: "http-json".to_owned(),
            display_name: "HTTP JSON upstream".to_owned(),
            protocols: vec![
                "openai".to_owned(),
                "anthropic".to_owned(),
                "generation".to_owned(),
            ],
            modalities: vec![
                "text".to_owned(),
                "embedding".to_owned(),
                "image".to_owned(),
                "video".to_owned(),
            ],
            config_schema,
            credential_schema: credential_schema.clone(),
            oauth_adapter: None,
            managed_oauth_adapter: None,
            component_adapter: None,
            source: "builtin".to_owned(),
        }];
        types.push(ProviderType {
            id: "volcengine-seedance".to_owned(),
            display_name: "Volcengine Seedance".to_owned(),
            protocols: vec!["generation".to_owned()],
            modalities: vec!["video".to_owned()],
            config_schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "required": ["base_url"],
                "properties": {
                    "base_url": {"type": "string", "format": "uri", "default": "https://ark.cn-beijing.volces.com"},
                    "network_scope": {
                        "title": "Network scope",
                        "type": "string",
                        "enum": ["public", "private"],
                        "default": "public",
                        "description": "Private Seedance destinations require a global operator credential."
                    },
                    "result_origins": {
                        "type": "array",
                        "uniqueItems": true,
                        "items": {"type": "string", "format": "uri"},
                        "description": "Exact origins allowed for generated asset archival."
                    }
                }
            }),
            credential_schema: credential_schema.clone(),
            oauth_adapter: None,
            managed_oauth_adapter: None,
            component_adapter: None,
            source: "builtin".to_owned(),
        });
        types.push(ProviderType {
            id: "cpa-subscription-bridge".to_owned(),
            display_name: "CPA Copilot/Cursor subscription bridge".to_owned(),
            protocols: vec!["openai".to_owned()],
            modalities: vec!["text".to_owned()],
            config_schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "required": ["base_url", "provider"],
                "properties": {
                    "base_url": {"type": "string", "format": "uri"},
                    "provider": {"type": "string", "enum": ["copilot", "cursor"]},
                    "network_scope": {"const": "private", "readOnly": true}
                }
            }),
            credential_schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "required": ["type", "handle"],
                "properties": {
                    "type": {"const": "subscription_bridge"},
                    "handle": {"type": "string", "pattern": "^[A-Za-z0-9]{1,80}$", "writeOnly": true},
                    "secret": {"type": "string", "minLength": 1, "writeOnly": true}
                }
            }),
            oauth_adapter: None,
            managed_oauth_adapter: None,
            component_adapter: None,
            source: "builtin".to_owned(),
        });
        types.push(ProviderType {
            id: "comfyui".to_owned(),
            display_name: "ComfyUI".to_owned(),
            protocols: vec!["generation".to_owned()],
            modalities: vec!["image".to_owned(), "video".to_owned()],
            config_schema: json!({
                "$schema": "https://json-schema.org/draft/2020-12/schema",
                "type": "object",
                "additionalProperties": false,
                "required": ["base_url", "workflow_id", "workflow_template"],
                "properties": {
                    "base_url": {"type": "string", "format": "uri"},
                    "network_scope": {
                        "title": "Network scope",
                        "type": "string",
                        "enum": ["public", "private"],
                        "default": "private",
                        "description": "Private ComfyUI destinations require a global operator credential."
                    },
                    "api_prefix": {"type": "string", "enum": ["", "/api"], "default": ""},
                    "workflow_id": {"type": "string", "minLength": 1},
                    "workflow_template": {
                        "type": "object",
                        "description": "Versioned administrator-owned graph. Use {\"$mtc_param\":\"name\"} placeholders for downstream scalar parameters."
                    }
                }
            }),
            credential_schema,
            oauth_adapter: None,
            managed_oauth_adapter: None,
            component_adapter: None,
            source: "builtin".to_owned(),
        });
        types.push(builtin_managed_oauth_provider(
            "cpa-codex-oauth",
            "CPA Codex OAuth (managed)",
            "https://chatgpt.com/backend-api/codex",
            true,
        ));
        types.push(builtin_managed_oauth_provider(
            "cpa-gemini-oauth-legacy",
            "CPA legacy Gemini OAuth (managed)",
            "https://cloudcode-pa.googleapis.com",
            false,
        ));
        Self {
            types: Arc::new(types),
            builtin_managed_oauth: Arc::new(vec![
                BuiltinManagedOAuthRegistration {
                    provider_driver: "cpa-codex-oauth",
                    source_type: "codex",
                    backend: ManagedOAuthAdapterBackend::BuiltinCodex,
                },
                BuiltinManagedOAuthRegistration {
                    provider_driver: "cpa-gemini-oauth-legacy",
                    source_type: "gemini-legacy",
                    backend: ManagedOAuthAdapterBackend::BuiltinLegacyGemini,
                },
            ]),
        }
    }

    pub fn list(&self) -> &[ProviderType] {
        &self.types
    }

    /// Return only the controlled source identifiers accepted by the current
    /// server catalog. Backend identity and destinations remain server-private.
    pub fn managed_oauth_source_types(&self) -> Vec<String> {
        self.builtin_managed_oauth
            .iter()
            .map(|registration| registration.source_type.to_owned())
            .chain(
                self.types
                    .iter()
                    .filter_map(|provider| provider.managed_oauth_adapter.as_ref())
                    .flat_map(|adapter| adapter.source_types.iter().cloned()),
            )
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
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
            crate::schema::validate_definition(&contribution.config_schema)?;
            crate::schema::validate_definition(&contribution.credential_schema)?;
            if let Some(adapter) = &contribution.managed_oauth_adapter {
                validate_managed_oauth_adapter_contribution(adapter)?;
                for source_type in &adapter.source_types {
                    if self
                        .managed_oauth_source_types()
                        .iter()
                        .any(|existing| existing == source_type)
                    {
                        return Err(AppError::BadRequest(
                            "duplicate managed OAuth source type contribution".into(),
                        ));
                    }
                }
            }
            Arc::make_mut(&mut self.types).push(contribution);
        }
        Ok(())
    }

    pub fn contains(&self, driver: &str) -> bool {
        self.types.iter().any(|provider| provider.id == driver)
    }

    pub fn get(&self, driver: &str) -> Option<&ProviderType> {
        self.types.iter().find(|provider| provider.id == driver)
    }

    pub fn managed_oauth_adapter_for_source(
        &self,
        source_type: &str,
    ) -> Result<ResolvedManagedOAuthAdapter, AppError> {
        validate_managed_oauth_source_type(source_type)?;
        let mut builtin_matches = self
            .builtin_managed_oauth
            .iter()
            .filter(|registration| registration.source_type == source_type);
        if let Some(registration) = builtin_matches.next() {
            if builtin_matches.next().is_some() {
                return Err(AppError::BadRequest(
                    "managed OAuth source type is ambiguous".into(),
                ));
            }
            return Ok(ResolvedManagedOAuthAdapter {
                provider_driver: registration.provider_driver.to_owned(),
                source_type: source_type.to_owned(),
                api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.to_owned(),
                backend: registration.backend.clone(),
            });
        }
        let mut matches = self.types.iter().filter_map(|provider| {
            let adapter = provider.managed_oauth_adapter.as_ref()?;
            adapter
                .source_types
                .iter()
                .any(|candidate| candidate == source_type)
                .then_some((provider, adapter))
        });
        let Some((provider, adapter)) = matches.next() else {
            return Err(AppError::BadRequest(
                "managed OAuth source type is unsupported".into(),
            ));
        };
        if matches.next().is_some() {
            return Err(AppError::BadRequest(
                "managed OAuth source type is ambiguous".into(),
            ));
        }
        Ok(ResolvedManagedOAuthAdapter {
            provider_driver: provider.id.clone(),
            source_type: source_type.to_owned(),
            api_version: adapter.api_version.clone(),
            backend: ManagedOAuthAdapterBackend::ReviewedHttp {
                normalize_url: adapter.normalize_url.clone(),
                refresh_url: adapter.refresh_url.clone(),
            },
        })
    }

    pub fn managed_oauth_adapter_for_driver(
        &self,
        driver: &str,
    ) -> Result<ResolvedManagedOAuthAdapter, AppError> {
        if let Some(registration) = self
            .builtin_managed_oauth
            .iter()
            .find(|registration| registration.provider_driver == driver)
        {
            return Ok(ResolvedManagedOAuthAdapter {
                provider_driver: registration.provider_driver.to_owned(),
                source_type: registration.source_type.to_owned(),
                api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.to_owned(),
                backend: registration.backend.clone(),
            });
        }
        let provider = self.get(driver).ok_or_else(|| {
            AppError::BadRequest("managed OAuth provider driver is unavailable".into())
        })?;
        let adapter = provider.managed_oauth_adapter.as_ref().ok_or_else(|| {
            AppError::BadRequest("managed OAuth provider adapter is unavailable".into())
        })?;
        validate_managed_oauth_adapter_contribution(adapter)?;
        Ok(ResolvedManagedOAuthAdapter {
            provider_driver: provider.id.clone(),
            source_type: adapter.source_types[0].clone(),
            api_version: adapter.api_version.clone(),
            backend: ManagedOAuthAdapterBackend::ReviewedHttp {
                normalize_url: adapter.normalize_url.clone(),
                refresh_url: adapter.refresh_url.clone(),
            },
        })
    }
}

fn builtin_managed_oauth_provider(
    id: &str,
    display_name: &str,
    base_url: &str,
    routes_openai_responses: bool,
) -> ProviderType {
    ProviderType {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        // The provider protocol vocabulary is coarse-grained. The dedicated
        // transport still rejects chat completions and embeddings before
        // reservation, archive creation, or an upstream request.
        protocols: if routes_openai_responses {
            vec!["openai".to_owned()]
        } else {
            Vec::new()
        },
        modalities: vec!["text".to_owned()],
        config_schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": if routes_openai_responses {
                json!(["base_url", "network_scope", "output_token_limits"])
            } else {
                json!(["base_url"])
            },
            "properties": {
                "base_url": {"const": base_url, "readOnly": true},
                "network_scope": {"const": "public", "readOnly": true},
                "output_token_limits": {
                    "type": "object",
                    "default": {},
                    "propertyNames": {"minLength": 1, "maxLength": 500},
                    "additionalProperties": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000000000
                    },
                    "description": "Hard output-token ceilings keyed by exact upstream model. Values must come from reviewed official limits or trusted model metadata; clients cannot supply or override them."
                }
            }
        }),
        credential_schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["type", "access_token", "refresh_token", "expires_at", "adapter_state"],
            "properties": {
                "type": {"const": "oauth"},
                "access_token": {"type": "string", "minLength": 1, "writeOnly": true},
                "refresh_token": {"type": "string", "minLength": 1, "writeOnly": true},
                "expires_at": {"type": "integer", "description": "Unix milliseconds"},
                "header": {"const": "authorization"},
                "prefix": {"const": "Bearer "},
                "adapter_state": {"type": "object", "writeOnly": true}
            }
        }),
        oauth_adapter: None,
        managed_oauth_adapter: None,
        component_adapter: None,
        source: "builtin".to_owned(),
    }
}

pub(crate) fn validate_managed_oauth_adapter_contribution(
    adapter: &ManagedOAuthAdapterContribution,
) -> Result<(), AppError> {
    if adapter.api_version != MANAGED_OAUTH_ADAPTER_API_VERSION
        || adapter.source_types.is_empty()
        || adapter.source_types.len() > 64
    {
        return Err(AppError::BadRequest(
            "unsupported managed OAuth adapter contract".into(),
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for source_type in &adapter.source_types {
        validate_managed_oauth_source_type(source_type)?;
        if !seen.insert(source_type) {
            return Err(AppError::BadRequest(
                "managed OAuth adapter source types must be unique".into(),
            ));
        }
    }
    crate::oauth::validate_managed_oauth_adapter_endpoint(&adapter.normalize_url, "normalize_url")?;
    crate::oauth::validate_managed_oauth_adapter_endpoint(&adapter.refresh_url, "refresh_url")?;
    Ok(())
}

fn validate_managed_oauth_source_type(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 64
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_' | b'.')
        })
    {
        return Err(AppError::BadRequest(
            "managed OAuth source type must contain 1-64 controlled ASCII characters".into(),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpstreamAccountView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: Option<String>,
    pub name: String,
    pub driver: String,
    pub auth_kind: String,
    /// How this provider was connected. This is presentation metadata only;
    /// API keys, OAuth and subscription bridges remain the same account model.
    pub connection_method: String,
    pub credential_generation: i64,
    pub status: String,
    pub config: Value,
    pub credential_expires_at: Option<i64>,
    /// Server-derived lifecycle capabilities. Clients must use these instead
    /// of inferring actions from `auth_kind` or `connection_method`.
    pub can_refresh: bool,
    pub can_rotate: bool,
    pub can_reauthorize: bool,
    /// Number of model routes that still reference this stable upstream
    /// identity, including disabled routes retained for audit purposes.
    pub route_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelRouteView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: Option<String>,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct ResolvedUpstream {
    pub route_id: Uuid,
    pub account_id: Uuid,
    pub driver: String,
    pub base_url: String,
    pub config: Value,
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
            adapter_state: None,
        };
        let envelope =
            seal_credential(&credential, b"a key material with at least 32 bytes").unwrap();
        assert!(!envelope.contains("secret"));
        let opened = open_credential(&envelope, b"a key material with at least 32 bytes").unwrap();
        assert_eq!(opened.auth_kind(), "oauth");
        assert_eq!(opened.expires_at(), Some(42));
    }

    #[test]
    fn unauthenticated_credential_is_valid() {
        let credential: UpstreamCredential =
            serde_json::from_value(json!({"type": "none"})).unwrap();
        assert_eq!(credential.auth_kind(), "none");
        credential.validate(42).unwrap();
    }

    #[test]
    fn subscription_bridge_credential_round_trips_without_exposing_handle_or_secret() {
        let credential = UpstreamCredential::SubscriptionBridge {
            handle: "OpaqueHandle123".to_owned(),
            secret: Some("bridge-secret".to_owned()),
        };
        credential.validate(42).unwrap();
        let envelope =
            seal_credential(&credential, b"a key material with at least 32 bytes").unwrap();
        assert!(!envelope.contains("OpaqueHandle123"));
        assert!(!envelope.contains("bridge-secret"));
        let opened = open_credential(&envelope, b"a key material with at least 32 bytes").unwrap();
        assert_eq!(opened.auth_kind(), "oauth");
        assert_eq!(opened.subscription_bridge_handle(), Some("OpaqueHandle123"));
    }

    #[test]
    fn subscription_bridge_rejects_unsafe_handles() {
        for handle in ["", "../account", "contains space", "handle_with_symbol"] {
            let credential = UpstreamCredential::SubscriptionBridge {
                handle: handle.to_owned(),
                secret: None,
            };
            assert!(credential.validate(42).is_err(), "accepted {handle:?}");
        }
    }

    #[test]
    fn oauth_adapter_state_is_bounded_redacted_and_backward_compatible() {
        let legacy: UpstreamCredential = serde_json::from_value(json!({
            "type": "oauth",
            "access_token": "legacy-secret",
            "expires_at": 42
        }))
        .unwrap();
        assert!(legacy.adapter_state().is_none());
        assert!(!format!("{legacy:?}").contains("legacy-secret"));

        let valid: UpstreamCredential = serde_json::from_value(json!({
            "type": "oauth",
            "access_token": "access-secret",
            "adapter_state": {"refresh_family": ["state-secret"]}
        }))
        .unwrap();
        assert_eq!(
            valid.adapter_state().unwrap()["refresh_family"][0],
            "state-secret"
        );
        let rendered = format!("{valid:?}");
        assert!(!rendered.contains("access-secret"));
        assert!(!rendered.contains("state-secret"));

        let oversized = json!({
            "type": "oauth",
            "access_token": "access",
            "adapter_state": "x".repeat(MAX_ADAPTER_STATE_BYTES + 1)
        });
        assert!(serde_json::from_value::<UpstreamCredential>(oversized).is_err());

        let mut nested = json!(null);
        for _ in 0..=MAX_ADAPTER_STATE_DEPTH {
            nested = json!([nested]);
        }
        assert!(
            serde_json::from_value::<UpstreamCredential>(json!({
                "type": "oauth",
                "access_token": "access",
                "adapter_state": nested
            }))
            .is_err()
        );

        assert!(
            serde_json::from_value::<UpstreamCredential>(json!({
                "type": "oauth",
                "access_token": "access",
                "adapter_state": vec![0; MAX_ADAPTER_STATE_NODES + 1]
            }))
            .is_err()
        );
    }

    fn managed_provider(id: &str, source_types: Vec<&str>) -> ProviderType {
        ProviderType {
            id: id.into(),
            display_name: id.into(),
            protocols: vec!["openai".into()],
            modalities: vec!["text".into()],
            config_schema: json!({"type": "object"}),
            credential_schema: json!({"type": "object"}),
            oauth_adapter: None,
            managed_oauth_adapter: Some(ManagedOAuthAdapterContribution {
                api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.into(),
                source_types: source_types.into_iter().map(str::to_owned).collect(),
                normalize_url: "http://adapter.default.svc/normalize".into(),
                refresh_url: "http://adapter.default.svc/refresh".into(),
            }),
            component_adapter: None,
            source: "test".into(),
        }
    }

    #[test]
    fn cloned_provider_catalog_shares_frozen_schemas_and_extends_copy_on_write() {
        let mut catalog = ProviderCatalog::builtins();
        let cloned = catalog.clone();
        let builtin_count = catalog.list().len();
        assert!(Arc::ptr_eq(&catalog.types, &cloned.types));
        assert!(Arc::ptr_eq(
            &catalog.builtin_managed_oauth,
            &cloned.builtin_managed_oauth
        ));

        catalog
            .extend([managed_provider(
                "managed-copy-on-write",
                vec!["copy-on-write"],
            )])
            .expect("extend cloned catalog");

        assert!(!Arc::ptr_eq(&catalog.types, &cloned.types));
        assert_eq!(catalog.list().len(), builtin_count + 1);
        assert_eq!(cloned.list().len(), builtin_count);
        assert!(catalog.contains("managed-copy-on-write"));
        assert!(!cloned.contains("managed-copy-on-write"));
    }

    #[test]
    fn managed_oauth_source_types_are_extensible_but_unique_and_controlled() {
        let mut catalog = ProviderCatalog::builtins();
        catalog
            .extend([managed_provider("managed-one", vec!["gemini-custom"])])
            .unwrap();
        assert_eq!(
            catalog
                .managed_oauth_adapter_for_source("gemini-custom")
                .unwrap()
                .provider_driver(),
            "managed-one"
        );
        assert!(
            catalog
                .extend([managed_provider("managed-two", vec!["codex"])])
                .is_err()
        );
        assert!(
            ProviderCatalog::builtins()
                .extend([managed_provider(
                    "managed-duplicate",
                    vec!["other-custom", "other-custom"],
                )])
                .is_err()
        );
        assert!(
            ProviderCatalog::builtins()
                .extend([managed_provider("managed-bad", vec!["Codex/../../secret"])])
                .is_err()
        );
    }

    #[test]
    fn builtin_codex_routes_openai_with_required_trusted_limits_only() {
        let catalog = ProviderCatalog::builtins();
        let codex = catalog.get("cpa-codex-oauth").unwrap();
        assert_eq!(codex.protocols, vec!["openai"]);
        assert_eq!(
            codex.config_schema.pointer("/properties/base_url/const"),
            Some(&json!("https://chatgpt.com/backend-api/codex"))
        );
        assert_eq!(
            codex
                .config_schema
                .pointer("/properties/output_token_limits/additionalProperties/minimum"),
            Some(&json!(1))
        );
        assert!(
            codex
                .config_schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.contains(&json!("output_token_limits")))
        );
        assert!(
            codex
                .config_schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| required.contains(&json!("network_scope")))
        );

        let gemini = catalog.get("cpa-gemini-oauth-legacy").unwrap();
        assert!(gemini.protocols.is_empty());

        assert!(
            catalog
                .managed_oauth_adapter_for_driver("cpa-codex-oauth")
                .unwrap()
                .can_refresh()
        );
        assert!(
            !catalog
                .managed_oauth_adapter_for_driver("cpa-gemini-oauth-legacy")
                .unwrap()
                .can_refresh()
        );
    }

    #[test]
    fn builtin_http_json_optionally_bounds_trusted_input_token_overhead() {
        let catalog = ProviderCatalog::builtins();
        let http_json = catalog.get("http-json").unwrap();
        let overhead = http_json
            .config_schema
            .pointer("/properties/input_token_overhead_ceiling")
            .unwrap();
        assert_eq!(overhead.get("minimum"), Some(&json!(0)));
        assert_eq!(overhead.get("maximum"), Some(&json!(1_000_000)));
        assert_eq!(overhead.get("default"), Some(&json!(0)));
        assert!(
            http_json
                .config_schema
                .get("required")
                .and_then(Value::as_array)
                .is_some_and(|required| !required.contains(&json!("input_token_overhead_ceiling")))
        );
        crate::schema::validate_instance(
            &http_json.config_schema,
            &json!({"base_url": "https://example.com"}),
        )
        .unwrap();
        crate::schema::validate_instance(
            &http_json.config_schema,
            &json!({
                "base_url": "https://example.com",
                "input_token_overhead_ceiling": 256
            }),
        )
        .unwrap();
        assert!(
            crate::schema::validate_instance(
                &http_json.config_schema,
                &json!({
                    "base_url": "https://example.com",
                    "input_token_overhead_ceiling": 1_000_001
                }),
            )
            .is_err()
        );
    }
}
