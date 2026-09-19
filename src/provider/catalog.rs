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
    /// Host-owned standard authorization code flow with S256 PKCE.
    AuthorizationCodePkce,
    /// Cursor-compatible redirect/PKCE login and polling contract.
    CursorPkce,
    /// OpenAI's server-owned Codex device authorization flow.
    OpenaiDevice,
    /// Claude Code's browser PKCE flow completed by pasting code#state.
    ClaudeManualPkce,
    /// GitHub device authorization followed by a Copilot token exchange.
    GithubDeviceCopilot,
    /// Native Kimi Code RFC 8628 device authorization.
    KimiDevice,
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

/// Versioned generation transport guarantees declared by a provider type.
/// Unknown and omitted guarantees always fail closed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GenerationAdapterContribution {
    pub api_version: String,
    #[serde(default)]
    pub provable_submit_idempotency: bool,
    #[serde(default)]
    pub provider_asset_reads_repeatable: bool,
}

/// Fixed reasoning metadata accepted by the Codex model-directory contract.
/// Codex 0.154 requires a non-empty description for every advertised effort.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodexReasoningLevel {
    pub effort: String,
    pub description: String,
}

/// Versioned, non-secret model-directory capabilities contributed by a
/// provider. A capability is never inferred from a model name or URL.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CodexModelCapabilities {
    pub version: String,
    /// A bounded strategy identifier. Providers select a gateway-owned
    /// instruction template; they never inject arbitrary prompt text.
    #[serde(default = "default_codex_agent_instructions_template")]
    pub agent_instructions_template: String,
    /// Codex's model-directory value for the shell tool.  Keep this an
    /// explicit provider declaration rather than inferring it from a model
    /// slug or upstream URL.
    pub shell_type: String,
    #[serde(default)]
    pub apply_patch_tool_type: Option<String>,
    #[serde(default)]
    pub fallback_context_window: Option<u64>,
    #[serde(default)]
    pub input_modalities: Vec<String>,
    #[serde(default)]
    pub supports_image_detail_original: bool,
    #[serde(default)]
    pub include_skills_usage_instructions: bool,
    #[serde(default)]
    pub include_plugin_usage_instructions: bool,
    #[serde(default)]
    pub include_apps_usage_instructions: bool,
    #[serde(default)]
    pub supported_reasoning_levels: Vec<CodexReasoningLevel>,
    #[serde(default)]
    pub default_reasoning_level: Option<String>,
}

pub(crate) const CODEX_MODEL_CAPABILITIES_VERSION: &str = "codex-model-capabilities-v1";
pub(crate) const CODEX_AGENT_INSTRUCTIONS_TEMPLATE_V1: &str = "codex-generic-agent-v1";
// Codex 0.154 bundled slugs. The reserved gpt-/codex- namespaces below also
// protect future bundled additions until this fixture is refreshed.
const BUNDLED_CODEX_MODEL_SLUGS: &[&str] = &[
    "codex-auto-review",
    "gpt-6-astra",
    "gpt-daybreak-blue-latest",
    "gpt-daybreak-red-latest",
    "gpt-5.2",
    "gpt-5.4",
    "gpt-5.4-mini",
    "gpt-5.5",
    "gpt-5.6-luna",
    "gpt-5.6-sol",
    "gpt-5.6-terra",
];

pub(crate) fn is_bundled_codex_model_slug(model: &str) -> bool {
    BUNDLED_CODEX_MODEL_SLUGS.contains(&model)
        || model.starts_with("gpt-")
        || model.starts_with("codex-")
}

fn default_codex_agent_instructions_template() -> String {
    CODEX_AGENT_INSTRUCTIONS_TEMPLATE_V1.to_owned()
}

/// Explicit request-shape compatibility declarations owned by the provider
/// catalog.  A capability is never inferred from a model name or URL: a
/// third-party upstream must opt in before the gateway rewrites Codex
/// collaboration payloads for it.
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub enum ResponsesViaChatDialect {
    #[serde(rename = "openai_chat_v1")]
    OpenAiChatV1,
    #[serde(rename = "kimi_v1")]
    KimiV1,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RequestCompatibility {
    #[serde(default)]
    pub third_party: bool,
    /// The provider translates OpenAI Responses requests through the host's
    /// Chat transport. This is an explicit hook for future DeepSeek/GLM-style
    /// adapters; it is not inferred from a URL or model name.
    #[serde(default)]
    pub responses_via_chat_v1: bool,
    /// Closed, versioned dialect implemented by the Responses-via-Chat bridge.
    /// This is required whenever `responses_via_chat_v1` is enabled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responses_via_chat_dialect: Option<ResponsesViaChatDialect>,
    /// The provider accepts OpenAI Responses traffic through the host's
    /// versioned Anthropic Messages adapter.  This capability is explicit so
    /// an Anthropic-looking URL or model name can never enable translation.
    #[serde(default)]
    pub responses_via_anthropic_messages_v1: bool,
    #[serde(default)]
    pub codex_multi_agent_v2: bool,
}

impl RequestCompatibility {
    pub(crate) fn is_default(&self) -> bool {
        !self.third_party
            && !self.responses_via_chat_v1
            && self.responses_via_chat_dialect.is_none()
            && !self.responses_via_anthropic_messages_v1
            && !self.codex_multi_agent_v2
    }

    pub fn supports_codex_multi_agent_v2(&self) -> bool {
        // Third-party MultiAgentV2 is executable only through an explicitly
        // declared, versioned Responses transport. Native Codex has its own
        // upstream Responses transport and does not use this predicate.
        self.third_party
            && self.codex_multi_agent_v2
            && ((self.responses_via_chat_v1 && self.responses_via_chat_dialect.is_some())
                || self.responses_via_anthropic_messages_v1)
    }
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_adapter: Option<GenerationAdapterContribution>,
    #[serde(default, skip_serializing_if = "RequestCompatibility::is_default")]
    pub request_compatibility: RequestCompatibility,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_model_capabilities: Option<CodexModelCapabilities>,
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
                "provider_asset_reads_repeatable": {
                    "type": "boolean",
                    "default": false,
                    "description": "Whether generated-asset GET URLs remain usable after a bounded validation GET. Set false for one-use URLs."
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
                "openai-audio".to_owned(),
                "generation".to_owned(),
            ],
            modalities: vec![
                "text".to_owned(),
                "embedding".to_owned(),
                "audio".to_owned(),
                "image".to_owned(),
                "video".to_owned(),
            ],
            config_schema,
            credential_schema: credential_schema.clone(),
            oauth_adapter: None,
            component_adapter: None,
            generation_adapter: Some(GenerationAdapterContribution {
                api_version: "generation-adapter-v1".to_owned(),
                provable_submit_idempotency: false,
                provider_asset_reads_repeatable: false,
            }),
            request_compatibility: Default::default(),
            codex_model_capabilities: None,
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
            generation_adapter: Some(GenerationAdapterContribution {
                api_version: "generation-adapter-v1".to_owned(),
                provable_submit_idempotency: false,
                provider_asset_reads_repeatable: true,
            }),
            request_compatibility: Default::default(),
            codex_model_capabilities: None,
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
            generation_adapter: Some(GenerationAdapterContribution {
                api_version: "generation-adapter-v1".to_owned(),
                provable_submit_idempotency: false,
                provider_asset_reads_repeatable: true,
            }),
            request_compatibility: Default::default(),
            codex_model_capabilities: None,
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
        codex.protocols.push("generation".to_owned());
        codex.modalities.push("image".to_owned());
        codex.generation_adapter = Some(GenerationAdapterContribution {
            api_version: "generation-adapter-v1".to_owned(),
            provable_submit_idempotency: false,
            provider_asset_reads_repeatable: false,
        });
        codex.config_schema["properties"]["image_main_model"] = json!({
            "title": "Image generation main model",
            "type": "string",
            "minLength": 1,
            "maxLength": 200,
            "description": "Codex Responses model that invokes the image_generation tool. The verified image tool model is gpt-image-2; the public model name remains configurable on the generation route."
        });
        codex.config_schema["properties"]["transport_policy"] = json!({
            "type": "object",
            "additionalProperties": false,
            "default": {},
            "description": "Runtime-adjustable transport policy for this account and its encrypted SOCKS5H binding. Changes apply to newly prepared requests without a service release.",
            "properties": {
                "version": {
                    "type": "integer",
                    "enum": [1],
                    "default": 1
                },
                "connect_timeout_millis": {
                    "type": "integer", "minimum": 100, "maximum": 60000, "default": 5000,
                    "description": "Pre-delivery connection deadline; must be lower than the total request timeout."
                },
                "read_timeout_millis": {
                    "type": "integer", "minimum": 1000, "maximum": 1260000, "default": 600000,
                    "description": "Maximum inactivity from response headers to the first body read and between later body reads."
                },
                "request_timeout_millis": {
                    "type": "integer", "minimum": 1000, "maximum": 1260000, "default": 1260000,
                    "description": "One absolute budget from the first send through the complete response body, including the sole permitted classified HTTP 400 replay."
                },
                "memory_admission_wait_millis": {
                    "type": "integer", "minimum": 100, "maximum": 300000, "default": 30000,
                    "title": "Memory queue timeout (ms)",
                    "description": "Maximum wait for gateway memory capacity within the request deadline."
                },
                "dispatch_max_in_flight": {
                    "type": "integer", "minimum": 1, "maximum": 64, "default": 4,
                    "description": "Concurrent Codex requests per account and proxy endpoint, including retries and response streams. Runtime decreases drain existing requests."
                },
                "dispatch_max_queued": {
                    "type": "integer", "minimum": 0, "maximum": 1024, "default": 32,
                    "description": "Maximum FIFO waiters before durable request admission. Zero rejects immediately when busy."
                },
                "dispatch_queue_timeout_millis": {
                    "type": "integer", "minimum": 1, "maximum": 300000, "default": 30000,
                    "description": "Maximum dispatch queue wait before returning 503 with Retry-After, without request or billing admission."
                },
                "max_sse_event_bytes": {
                    "type": "integer", "minimum": 262144, "maximum": 16777216, "default": 8388608,
                    "title": "Maximum SSE event bytes",
                    "description": "Maximum bytes retained for one upstream SSE event. Responses terminal events may contain the complete response object."
                },
                "max_sse_framed_bytes": {
                    "type": "integer", "minimum": 262144, "maximum": 16842752, "default": 8454144,
                    "title": "Maximum framed bytes per network chunk",
                    "description": "Maximum completed SSE bytes materialized from one upstream network chunk. Must be at least max_sse_event_bytes."
                },
                "max_sse_terminal_hold_bytes": {
                    "type": "integer", "minimum": 262144, "maximum": 16842752, "default": 8454144,
                    "title": "Maximum terminal hold bytes",
                    "description": "Maximum validated Responses terminal bytes held until EOF. Must be between max_sse_event_bytes and max_sse_framed_bytes."
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
                },
                "chat_controls": {
                    "type": "string",
                    "enum": ["provider_default", "strict"],
                    "default": "strict",
                    "title": "Chat controls",
                    "description": "Choose how Chat Completions sampling and output-limit controls are adapted to the Codex Responses transport. Provider default validates and removes controls the upstream cannot represent; strict accepts neutral values only."
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
        let mut claude = builtin_interactive_oauth_provider(
            "anthropic-claude",
            "Anthropic Claude",
            vec!["anthropic", "openai"],
            "https://api.anthropic.com",
            InteractiveOAuthDefinition {
                flow_kind: OAuthFlowKind::ClaudeManualPkce,
                login_url: "https://claude.com/cai/oauth/authorize",
                poll_url: "https://platform.claude.com/v1/oauth/token",
                refresh_url: "https://platform.claude.com/v1/oauth/token",
            },
        );
        claude.request_compatibility = RequestCompatibility {
            third_party: true,
            responses_via_anthropic_messages_v1: true,
            codex_multi_agent_v2: true,
            ..Default::default()
        };
        claude.codex_model_capabilities = Some(CodexModelCapabilities {
            version: CODEX_MODEL_CAPABILITIES_VERSION.to_owned(),
            agent_instructions_template: CODEX_AGENT_INSTRUCTIONS_TEMPLATE_V1.to_owned(),
            shell_type: "unified_exec".to_owned(),
            apply_patch_tool_type: Some("freeform".to_owned()),
            fallback_context_window: Some(200_000),
            input_modalities: vec!["text".to_owned(), "image".to_owned()],
            supports_image_detail_original: false,
            include_skills_usage_instructions: false,
            include_plugin_usage_instructions: false,
            include_apps_usage_instructions: false,
            supported_reasoning_levels: Vec::new(),
            default_reasoning_level: None,
        });
        types.push(claude);
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
        kimi.oauth_adapter = Some(OAuthAdapterContribution {
            api_version: "oauth-adapter-v1".to_owned(),
            flow_kind: OAuthFlowKind::KimiDevice,
            login_url: crate::oauth::kimi_device::DEVICE_ENDPOINT.to_owned(),
            poll_url: crate::oauth::managed::kimi::TOKEN_ENDPOINT.to_owned(),
            refresh_url: crate::oauth::managed::kimi::TOKEN_ENDPOINT.to_owned(),
        });
        kimi.request_compatibility = RequestCompatibility {
            third_party: true,
            responses_via_chat_v1: true,
            responses_via_chat_dialect: Some(ResponsesViaChatDialect::KimiV1),
            responses_via_anthropic_messages_v1: false,
            codex_multi_agent_v2: true,
        };
        kimi.codex_model_capabilities = Some(CodexModelCapabilities {
            version: CODEX_MODEL_CAPABILITIES_VERSION.to_owned(),
            agent_instructions_template: CODEX_AGENT_INSTRUCTIONS_TEMPLATE_V1.to_owned(),
            shell_type: "unified_exec".to_owned(),
            apply_patch_tool_type: Some("freeform".to_owned()),
            fallback_context_window: Some(256 * 1024),
            input_modalities: vec!["text".to_owned(), "image".to_owned()],
            supports_image_detail_original: false,
            include_skills_usage_instructions: true,
            include_plugin_usage_instructions: true,
            include_apps_usage_instructions: true,
            supported_reasoning_levels: vec![
                CodexReasoningLevel {
                    effort: "low".to_owned(),
                    description: "Fast responses with lighter reasoning".to_owned(),
                },
                CodexReasoningLevel {
                    effort: "medium".to_owned(),
                    description: "Balances speed and reasoning depth for everyday tasks".to_owned(),
                },
                CodexReasoningLevel {
                    effort: "high".to_owned(),
                    description: "Greater reasoning depth for complex problems".to_owned(),
                },
                CodexReasoningLevel {
                    effort: "xhigh".to_owned(),
                    description: "Extra high reasoning depth for complex problems".to_owned(),
                },
                CodexReasoningLevel {
                    effort: "max".to_owned(),
                    description: "Maximum reasoning depth for the hardest problems".to_owned(),
                },
            ],
            default_reasoning_level: Some("medium".to_owned()),
        });
        kimi.credential_schema["properties"]["expires_at"] = json!({"type": ["integer", "null"], "description": "Unix milliseconds, absent source expiry remains unknown"});
        types.push(kimi);
        for provider in &mut types {
            if matches!(provider.id.as_str(), "openai-codex" | "kimi-oauth") {
                provider.config_schema["properties"]["quota_read_policy"] = json!({
                    "type": "object", "additionalProperties": false, "default": {},
                    "description": "Runtime-adjustable read-only quota retry policy. Does not affect inference, reset consumption or OAuth; max_attempts includes the first request.",
                    "properties": {
                        "max_attempts": {"type":"integer", "minimum":1, "maximum":4, "default":3},
                        "initial_delay_millis": {"type":"integer", "minimum":50, "maximum":2000, "default":200},
                        "total_timeout_millis": {"type":"integer", "minimum":1000, "maximum":30000, "default":20000}
                    }
                });
            }
        }
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
            if contribution
                .generation_adapter
                .as_ref()
                .is_some_and(|adapter| adapter.api_version != "generation-adapter-v1")
            {
                return Err(AppError::BadRequest(
                    "unsupported provider generation adapter version".into(),
                ));
            }
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

    pub fn supports_codex_multi_agent_v2(&self, driver: &str) -> bool {
        self.get(driver).is_some_and(|provider| {
            provider
                .request_compatibility
                .supports_codex_multi_agent_v2()
        })
    }

    pub fn supports_responses_via_chat_v1(&self, driver: &str) -> bool {
        self.get(driver).is_some_and(|provider| {
            provider.request_compatibility.third_party
                && provider.request_compatibility.responses_via_chat_v1
                && provider
                    .request_compatibility
                    .responses_via_chat_dialect
                    .is_some()
        })
    }

    pub fn responses_via_chat_dialect(&self, driver: &str) -> Option<ResponsesViaChatDialect> {
        self.get(driver).and_then(|provider| {
            let compatibility = &provider.request_compatibility;
            if compatibility.third_party
                && compatibility.responses_via_chat_v1
                && compatibility.responses_via_chat_dialect.is_some()
            {
                compatibility.responses_via_chat_dialect
            } else {
                None
            }
        })
    }

    pub fn supports_responses_via_anthropic_messages_v1(&self, driver: &str) -> bool {
        self.get(driver).is_some_and(|provider| {
            provider.request_compatibility.third_party
                && provider
                    .request_compatibility
                    .responses_via_anthropic_messages_v1
        })
    }

    /// Model catalogs also include native Codex routes, whose transport reads
    /// MultiAgentV2 without the third-party normalization hook.
    pub fn supports_codex_multi_agent_v2_model_catalog(&self, driver: &str) -> bool {
        driver == crate::oauth::codex_device::PROVIDER_DRIVER
            || self.supports_codex_multi_agent_v2(driver)
    }

    pub(crate) fn codex_model_capabilities_for_catalog(
        &self,
        driver: &str,
    ) -> Option<CodexModelCapabilities> {
        self.get(driver)
            .filter(|provider| {
                provider
                    .request_compatibility
                    .supports_codex_multi_agent_v2()
            })
            .filter(|provider| {
                provider.request_compatibility.responses_via_chat_dialect
                    != Some(ResponsesViaChatDialect::OpenAiChatV1)
                    || provider
                        .codex_model_capabilities
                        .as_ref()
                        .is_none_or(|capabilities| {
                            capabilities.supported_reasoning_levels.is_empty()
                                && capabilities.default_reasoning_level.is_none()
                        })
            })
            .and_then(|provider| provider.codex_model_capabilities.clone())
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
                "proxy_url": {"type": "string", "pattern": "^socks5h://", "minLength": 1, "maxLength": 2048, "writeOnly": true},
                "proxy_network_scope": {"type": "string", "const": "private"},
                "adapter_state": {"type": "object", "writeOnly": true}
            }
        }),
        oauth_adapter: None,
        component_adapter: None,
        generation_adapter: None,
        request_compatibility: Default::default(),
        codex_model_capabilities: None,
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
                "proxy_url": {"type": "string", "pattern": "^socks5h://", "minLength": 1, "maxLength": 2048, "writeOnly": true},
                "proxy_network_scope": {"type": "string", "const": "private"},
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
        generation_adapter: None,
        request_compatibility: Default::default(),
        codex_model_capabilities: None,
        source: "builtin".to_owned(),
    }
}
