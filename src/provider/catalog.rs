use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::AppError;

pub const MANAGED_OAUTH_ADAPTER_API_VERSION: &str = "cpa-managed-oauth-adapter-v1";

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
    /// OpenAI's server-owned Codex device authorization flow.
    OpenaiDevice,
    /// Claude Code's browser PKCE flow completed by pasting code#state.
    ClaudeManualPkce,
    /// GitHub device authorization followed by a Copilot token exchange.
    GithubDeviceCopilot,
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
    pub(super) types: Arc<Vec<ProviderType>>,
    legacy_types: Arc<Vec<ProviderType>>,
    pub(super) builtin_managed_oauth: Arc<Vec<BuiltinManagedOAuthRegistration>>,
}

#[derive(Clone)]
pub(super) struct BuiltinManagedOAuthRegistration {
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
                "result_origins": {
                    "title": "Generated asset origins",
                    "type": "array",
                    "uniqueItems": true,
                    "items": {"type": "string", "format": "uri"},
                    "description": "Exact origins allowed for generated asset archival."
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
                    "title": "API key through an account proxy",
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["type", "value", "proxy_url", "proxy_network_scope"],
                    "properties": {
                        "type": {"const": "api_key_proxy", "title": "Credential type"},
                        "value": {"type": "string", "minLength": 1, "writeOnly": true, "title": "Credential value"},
                        "header": {"type": "string", "default": "authorization"},
                        "prefix": {"type": "string", "default": "Bearer "},
                        "proxy_url": {"type": "string", "pattern": "^socks5://", "minLength": 1, "maxLength": 2048, "writeOnly": true, "title": "Proxy URL"},
                        "proxy_network_scope": {"type": "string", "const": "private"}
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
                    },
                    "parameter_schema": {
                        "type": "object",
                        "description": "Optional closed scalar JSON Schema. properties and required must exactly match workflow placeholders; unsafe keywords such as $ref are rejected."
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
            crate::oauth::codex_device::PROVIDER_DRIVER,
            "OpenAI Codex",
            crate::oauth::codex_device::BASE_URL,
            true,
        ));
        let codex = types
            .last_mut()
            .expect("OpenAI Codex provider was just inserted");
        codex.oauth_adapter = Some(OAuthAdapterContribution {
            api_version: "oauth-adapter-v1".to_owned(),
            flow_kind: OAuthFlowKind::OpenaiDevice,
            login_url: "https://auth.openai.com/codex/device".to_owned(),
            poll_url: "https://auth.openai.com/api/accounts/deviceauth/token".to_owned(),
            refresh_url: crate::oauth::codex_device::TOKEN_ENDPOINT.to_owned(),
        });
        types.push(builtin_interactive_oauth_provider(
            "anthropic-claude",
            "Anthropic Claude",
            vec!["anthropic"],
            "https://api.anthropic.com",
            InteractiveOAuthDefinition {
                flow_kind: OAuthFlowKind::ClaudeManualPkce,
                login_url: "https://claude.com/cai/oauth/authorize",
                poll_url: "https://platform.claude.com/v1/oauth/token",
                refresh_url: "https://platform.claude.com/v1/oauth/token",
            },
        ));
        let mut copilot = builtin_interactive_oauth_provider(
            "github-copilot",
            "GitHub Copilot",
            vec!["openai"],
            "https://api.githubcopilot.com",
            InteractiveOAuthDefinition {
                flow_kind: OAuthFlowKind::GithubDeviceCopilot,
                login_url: "https://github.com/login/device/code",
                poll_url: "https://github.com/login/oauth/access_token",
                refresh_url: "https://api.github.com/copilot_internal/v2/token",
            },
        );
        copilot.config_schema["properties"]["base_url"] = json!({
            "type": "string",
            "format": "uri",
            "readOnly": true
        });
        types.push(copilot);
        types.push(builtin_interactive_oauth_provider(
            "cursor",
            "Cursor",
            vec!["openai"],
            "https://api2.cursor.sh",
            InteractiveOAuthDefinition {
                flow_kind: OAuthFlowKind::CursorPkce,
                login_url: crate::oauth::DEFAULT_CURSOR_LOGIN_URL,
                poll_url: crate::oauth::DEFAULT_CURSOR_POLL_URL,
                refresh_url: crate::oauth::DEFAULT_CURSOR_REFRESH_URL,
            },
        ));
        let legacy_types = vec![
            builtin_managed_oauth_provider(
                "cpa-codex-oauth",
                "Legacy Codex OAuth import",
                "https://chatgpt.com/backend-api/codex",
                true,
            ),
            builtin_managed_oauth_provider(
                "cpa-gemini-oauth-legacy",
                "Legacy Gemini OAuth import",
                "https://cloudcode-pa.googleapis.com",
                false,
            ),
        ];
        Self {
            types: Arc::new(types),
            legacy_types: Arc::new(legacy_types),
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

    /// Public provider types are the only drivers accepted for newly created
    /// accounts. Legacy drivers remain resolvable for imported rows and routes,
    /// but are never advertised as product capabilities.
    pub fn is_public(&self, driver: &str) -> bool {
        self.types.iter().any(|provider| provider.id == driver)
    }

    /// Interactive OAuth material must be provisioned through the server-owned
    /// authorization flow. Providers may still declare API-key or unauthenticated
    /// credentials alongside OAuth; those remain equal direct connection methods.
    pub fn supports_direct_credential(&self, driver: &str, credential_kind: &str) -> bool {
        self.get(driver).is_some_and(|provider| {
            self.is_public(driver)
                && (provider.oauth_adapter.is_none() || credential_kind != "oauth")
        })
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
        self.extend_with_endpoint_policy(contributions, false)
    }

    /// Integration-test hook for mock adapters bound to loopback. Release
    /// builds do not expose this method, and normal catalog extension remains
    /// fail-closed.
    #[cfg(debug_assertions)]
    #[doc(hidden)]
    pub fn extend_for_test(
        &mut self,
        contributions: impl IntoIterator<Item = ProviderType>,
    ) -> Result<(), AppError> {
        self.extend_with_endpoint_policy(contributions, true)
    }

    fn extend_with_endpoint_policy(
        &mut self,
        contributions: impl IntoIterator<Item = ProviderType>,
        allow_test_loopback: bool,
    ) -> Result<(), AppError> {
        for contribution in contributions {
            if contribution.id.trim().is_empty()
                || self
                    .types
                    .iter()
                    .any(|provider| provider.id == contribution.id)
                || self
                    .legacy_types
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
                validate_managed_oauth_adapter_contribution_with_policy(
                    adapter,
                    allow_test_loopback,
                )?;
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
        self.get(driver).is_some()
    }

    pub fn get(&self, driver: &str) -> Option<&ProviderType> {
        self.types
            .iter()
            .chain(self.legacy_types.iter())
            .find(|provider| provider.id == driver)
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
        if driver == crate::oauth::codex_device::PROVIDER_DRIVER {
            return Ok(ResolvedManagedOAuthAdapter {
                provider_driver: driver.to_owned(),
                source_type: "codex".to_owned(),
                api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.to_owned(),
                backend: ManagedOAuthAdapterBackend::BuiltinCodex,
            });
        }
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
                json!(["base_url", "network_scope", "reservation_token_bounds"])
            } else {
                json!(["base_url"])
            },
            "properties": {
                "base_url": {"const": base_url, "readOnly": true},
                "network_scope": {"const": "public", "readOnly": true},
                "reservation_token_bounds": {
                    "type": "object",
                    "default": {},
                    "propertyNames": {"minLength": 1, "maxLength": 500},
                    "additionalProperties": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 1000000000
                    },
                    "description": "Conservative token reservation bounds keyed by exact upstream model. Values come from trusted synchronized model metadata and prevent under-reservation; they are not advertised provider output limits."
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

struct InteractiveOAuthDefinition<'a> {
    flow_kind: OAuthFlowKind,
    login_url: &'a str,
    poll_url: &'a str,
    refresh_url: &'a str,
}

fn builtin_interactive_oauth_provider(
    id: &str,
    display_name: &str,
    protocols: Vec<&str>,
    base_url: &str,
    oauth: InteractiveOAuthDefinition<'_>,
) -> ProviderType {
    ProviderType {
        id: id.to_owned(),
        display_name: display_name.to_owned(),
        protocols: protocols.into_iter().map(str::to_owned).collect(),
        modalities: vec!["text".to_owned()],
        config_schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["base_url", "network_scope"],
            "properties": {
                "base_url": {"const": base_url, "readOnly": true},
                "network_scope": {"const": "public", "readOnly": true}
            }
        }),
        credential_schema: json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "type": "object",
            "additionalProperties": false,
            "required": ["type", "access_token", "expires_at", "adapter_state"],
            "properties": {
                "type": {"const": "oauth"},
                "access_token": {"type": "string", "minLength": 1, "writeOnly": true},
                "refresh_token": {"type": "string", "writeOnly": true},
                "expires_at": {"type": "integer", "description": "Unix milliseconds"},
                "header": {"const": "authorization"},
                "prefix": {"const": "Bearer "},
                "adapter_state": {"type": "object", "writeOnly": true}
            }
        }),
        oauth_adapter: Some(OAuthAdapterContribution {
            api_version: "oauth-adapter-v1".to_owned(),
            flow_kind: oauth.flow_kind,
            login_url: oauth.login_url.to_owned(),
            poll_url: oauth.poll_url.to_owned(),
            refresh_url: oauth.refresh_url.to_owned(),
        }),
        managed_oauth_adapter: None,
        component_adapter: None,
        source: "builtin".to_owned(),
    }
}

pub(crate) fn validate_managed_oauth_adapter_contribution(
    adapter: &ManagedOAuthAdapterContribution,
) -> Result<(), AppError> {
    validate_managed_oauth_adapter_contribution_with_policy(adapter, false)
}

fn validate_managed_oauth_adapter_contribution_with_policy(
    adapter: &ManagedOAuthAdapterContribution,
    allow_test_loopback: bool,
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
    crate::oauth::validate_managed_oauth_adapter_endpoint_with_policy(
        &adapter.normalize_url,
        "normalize_url",
        allow_test_loopback,
    )?;
    crate::oauth::validate_managed_oauth_adapter_endpoint_with_policy(
        &adapter.refresh_url,
        "refresh_url",
        allow_test_loopback,
    )?;
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
