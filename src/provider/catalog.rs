use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::error::AppError;

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

#[derive(Clone, Debug)]
pub(crate) struct ResolvedManagedOAuthAdapter {
    backend: ManagedOAuthAdapterBackend,
}

/// The administrator-reviewed implementation selected by the server catalog.
/// Builtins never synthesize an HTTP contribution or accept a client URL.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum ManagedOAuthAdapterBackend {
    Kimi,
    Codex,
}

impl ResolvedManagedOAuthAdapter {
    pub(crate) fn backend(&self) -> &ManagedOAuthAdapterBackend {
        &self.backend
    }

    pub(crate) fn refresh_url(&self) -> &str {
        match &self.backend {
            ManagedOAuthAdapterBackend::Kimi => crate::oauth::managed::kimi::TOKEN_ENDPOINT,
            ManagedOAuthAdapterBackend::Codex => crate::oauth::managed::codex::TOKEN_ENDPOINT,
        }
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
    pub component_adapter: Option<ComponentAdapterContribution>,
    #[serde(default)]
    pub source: String,
}

#[derive(Clone)]
pub struct ProviderCatalog {
    pub(super) types: Arc<Vec<ProviderType>>,
    pub(super) builtin_managed_oauth: Arc<Vec<BuiltinManagedOAuthRegistration>>,
}

#[derive(Clone)]
pub(super) struct BuiltinManagedOAuthRegistration {
    provider_driver: &'static str,
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
                "stream_usage_contract": {
                    "title": "Streaming usage contract",
                    "type": "string",
                    "enum": ["none", "openai-chat-usage-only"],
                    "default": "none",
                    "description": "Require the OpenAI Chat include_usage terminal chunk for this compatible upstream."
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
                "video_api": {
                    "title": "Video generation API",
                    "type": "string",
                    "enum": ["siliconflow-v1"],
                    "description": "Enable SiliconFlow's fixed /v1/video/submit and /v1/video/status asynchronous video contract."
                },
                "video_models": {
                    "title": "Video generation models",
                    "type": "array",
                    "maxItems": 100,
                    "uniqueItems": true,
                    "items": {"type": "string", "minLength": 1, "maxLength": 500},
                    "description": "Exact upstream model IDs that use the configured video API; at least one is required when a video API is enabled. Other routes on this account retain their normal text/image capabilities."
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
                        "proxy_url": {"type": "string", "pattern": "^socks5h?://", "minLength": 1, "maxLength": 2048, "writeOnly": true, "title": "Proxy URL"},
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
                        "proxy_url": {"type": "string", "pattern": "^socks5h?://", "minLength": 1, "maxLength": 2048, "writeOnly": true},
                        "proxy_network_scope": {"type": "string", "const": "private"},
                        "adapter_state": {
                            "description": "Opaque encrypted state for a server-owned OAuth driver.",
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
            component_adapter: None,
            source: "builtin".to_owned(),
        }];
        types.push(crate::provider::cbcnx::provider_type(
            credential_schema.clone(),
        ));
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
        codex.config_schema["properties"]["transport_policy"] = json!({
            "type": "object",
            "additionalProperties": false,
            "default": {},
            "description": "Runtime-adjustable recovery policy for this account and its encrypted SOCKS5H binding. Changes apply to newly prepared requests without a service release.",
            "properties": {
                "version": {
                    "type": "integer",
                    "enum": [1],
                    "default": 1
                },
                "connect_timeout_millis": {
                    "type": "integer", "minimum": 100, "maximum": 60000, "default": 5000
                },
                "read_timeout_millis": {
                    "type": "integer", "minimum": 1000, "maximum": 1260000, "default": 600000
                },
                "request_timeout_millis": {
                    "type": "integer", "minimum": 1000, "maximum": 1260000, "default": 1260000
                },
                "candidate_attempts": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 8,
                    "default": 3
                },
                "failover_deadline_millis": {
                    "type": "integer",
                    "minimum": 1000,
                    "maximum": 300000,
                    "default": 300000
                },
                "connect_attempts": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 4,
                    "default": 2
                },
                "connect_retry_delay_millis": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 2000,
                    "default": 150
                },
                "shared_probe_attempts": {
                    "type": "integer",
                    "minimum": 0,
                    "maximum": 4,
                    "default": 1
                }
            }
        });
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
        let mut kimi = builtin_managed_oauth_provider(
            crate::oauth::managed::kimi::PROVIDER_DRIVER,
            "Kimi Code OAuth",
            crate::oauth::managed::kimi::BASE_URL,
            true,
        );
        kimi.protocols = vec!["openai".to_owned(), "anthropic".to_owned()];
        kimi.credential_schema["properties"]["expires_at"] = json!({"type": ["integer", "null"], "description": "Unix milliseconds, absent source expiry remains unknown"});
        types.push(kimi);
        Self {
            types: Arc::new(types),
            builtin_managed_oauth: Arc::new(vec![
                BuiltinManagedOAuthRegistration {
                    provider_driver: crate::oauth::managed::kimi::PROVIDER_DRIVER,
                    backend: ManagedOAuthAdapterBackend::Kimi,
                },
                BuiltinManagedOAuthRegistration {
                    // New authorization uses the native driver. Historical
                    // rows are handled by the explicit database upgrade path.
                    provider_driver: "openai-codex",
                    backend: ManagedOAuthAdapterBackend::Codex,
                },
            ]),
        }
    }

    pub fn list(&self) -> &[ProviderType] {
        &self.types
    }

    /// Public provider types are the only drivers accepted for new accounts,
    /// OAuth sessions, and proxy routing.
    pub fn is_public(&self, driver: &str) -> bool {
        self.types.iter().any(|provider| provider.id == driver)
    }

    /// Interactive OAuth material must be provisioned through the server-owned
    /// authorization flow. Providers may still declare API-key or unauthenticated
    /// credentials alongside OAuth; those remain equal direct connection methods.
    pub fn supports_direct_credential(&self, driver: &str, credential_kind: &str) -> bool {
        self.get(driver).is_some_and(|provider| {
            self.is_public(driver)
                && driver != crate::oauth::managed::kimi::PROVIDER_DRIVER
                && (provider.oauth_adapter.is_none() || credential_kind != "oauth")
        })
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
            Arc::make_mut(&mut self.types).push(contribution);
        }
        Ok(())
    }

    pub fn contains(&self, driver: &str) -> bool {
        self.get(driver).is_some()
    }

    pub fn get(&self, driver: &str) -> Option<&ProviderType> {
        self.types.iter().find(|provider| provider.id == driver)
    }

    pub(crate) fn managed_oauth_adapter_for_driver(
        &self,
        driver: &str,
    ) -> Result<ResolvedManagedOAuthAdapter, AppError> {
        if driver == crate::oauth::codex_device::PROVIDER_DRIVER {
            return Ok(ResolvedManagedOAuthAdapter {
                backend: ManagedOAuthAdapterBackend::Codex,
            });
        }
        if let Some(registration) = self
            .builtin_managed_oauth
            .iter()
            .find(|registration| registration.provider_driver == driver)
        {
            return Ok(ResolvedManagedOAuthAdapter {
                backend: registration.backend.clone(),
            });
        }
        Err(AppError::BadRequest(
            "managed OAuth provider driver is unavailable".into(),
        ))
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
                "proxy_url": {"type": "string", "pattern": "^socks5h?://", "minLength": 1, "maxLength": 2048, "writeOnly": true},
                "proxy_network_scope": {"type": "string", "const": "private"},
                "adapter_state": {"type": "object", "writeOnly": true}
            }
        }),
        oauth_adapter: None,
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
        component_adapter: None,
        source: "builtin".to_owned(),
    }
}
