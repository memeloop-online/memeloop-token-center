use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Component as PathComponent, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config as WasmtimeConfig, Engine, Store, StoreLimits, StoreLimitsBuilder};

use self::memeloop::token_center::types;
use crate::{
    db::Database,
    error::AppError,
    network::{self, OutboundScope},
    provider::ProviderType,
};

const PLUGIN_FUEL: u64 = 5_000_000;
const PLUGIN_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const PLUGIN_TABLE_ELEMENTS: usize = 100_000;
const PLUGIN_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;
const PLUGIN_HTTP_HEADER_COUNT: usize = 64;
const PLUGIN_HTTP_HEADER_NAME_BYTES: usize = 256;
const PLUGIN_HTTP_HEADER_VALUE_BYTES: usize = 8 * 1024;
const PLUGIN_HTTP_HEADER_TOTAL_BYTES: usize = 16 * 1024;
const PLUGIN_HTTP_HEADERS_JSON_BYTES: usize = 128 * 1024;
const PLUGIN_MANIFEST_BYTES: u64 = 1024 * 1024;
const PLUGIN_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_COMPONENT_PROVIDER_BODY: usize = 4 * 1024 * 1024;
const MAX_PLUGIN_ID_BYTES: usize = 64;
const MAX_TRAFFIC_REASON_BYTES: usize = 256;
const MAX_TRAFFIC_MODEL_BYTES: usize = 200;
const MAX_TRAFFIC_ACCOUNT_ID_BYTES: usize = 64;
const MAX_TRAFFIC_REQUEST_JSON_BYTES: usize = 16 * 1024 * 1024;
const PLUGIN_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const PLUGIN_EPOCH_TICK: Duration = Duration::from_millis(10);
const PLUGIN_CONFIGURATION_CACHE_TTL: Duration = Duration::from_secs(5);
const PLUGIN_CONFIGURATION_CACHE_ENTRIES: usize = 64;
const PLUGIN_CONFIGURATION_CACHE_BYTES: usize = 16 * 1024 * 1024;
const SUPPORTED_WIT_REQUIREMENT: &str = ">=0.2.0, <0.3.0";

wasmtime::component::bindgen!({
    world: "plugin",
    path: "wit/token-center.wit",
});

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginManifest {
    pub id: String,
    pub version: String,
    pub wit_version: String,
    #[serde(default = "default_wasm_file")]
    pub wasm: Option<String>,
    #[serde(default)]
    pub capabilities: Vec<PluginCapability>,
    #[serde(default)]
    pub contributions: PluginContributions,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginContributions {
    #[serde(default)]
    pub traffic_policy: bool,
    /// Runs the same post-auth component hook but explicitly declares that the
    /// package contributes request/model/route rewriting. Keeping this
    /// separate in the manifest lets operators audit installed capabilities;
    /// the versioned WIT hook remains combined so policy and rewrite can share
    /// one bounded execution without exposing credentials.
    #[serde(default)]
    pub request_rewrite: bool,
    #[serde(default)]
    pub configuration: Option<PluginConfigurationContribution>,
    #[serde(default)]
    pub providers: Vec<ProviderType>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginConfigurationContribution {
    pub schema: Value,
    #[serde(default = "empty_json_object")]
    pub default: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct StoredPluginConfiguration {
    pub plugin_id: String,
    pub tenant_id: Option<Uuid>,
    pub value: Value,
    pub schema_digest: String,
    pub version: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct PutPluginConfigurationInput {
    pub plugin_id: String,
    pub tenant_id: Option<Uuid>,
    pub value: Value,
    pub schema_digest: String,
    pub expected_version: i64,
    pub idempotency_key: String,
    pub request_hash: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginConfigurationView {
    pub plugin_id: String,
    pub tenant_external_id: Option<String>,
    pub value: Value,
    pub source: String,
    pub scope_version: i64,
    pub updated_at: Option<i64>,
    pub schema_digest: String,
}

fn empty_json_object() -> Value {
    serde_json::json!({})
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginCapability {
    Log,
    Kv,
    Http { allowed_origins: Vec<String> },
}

#[derive(Clone)]
struct LoadedPlugin {
    manifest: PluginManifest,
    component: Option<Component>,
    configuration_validator: Option<crate::schema::CompiledSchema>,
}

#[derive(Clone)]
struct CachedPluginConfigurations {
    loaded_at: Instant,
    values: BTreeMap<String, Value>,
    estimated_bytes: usize,
}

#[derive(Clone, Default)]
pub struct TrafficDecision {
    pub allow: bool,
    /// The manifest-validated identity of the policy which denied the request.
    /// Guest-provided reason text is deliberately never retained here.
    denied_by_plugin_id: Option<String>,
    /// A host-owned, opaque code suitable for metrics and structured logs.
    decision_code: Option<&'static str>,
    pub model: Option<String>,
    pub upstream_account_id: Option<String>,
    pub request_json: Option<Value>,
}

impl TrafficDecision {
    pub(crate) fn log_denial(&self) {
        debug_assert!(!self.allow);
        tracing::warn!(
            plugin_id = self.denied_by_plugin_id.as_deref().unwrap_or("unknown"),
            decision_code = self.decision_code.unwrap_or("policy_denied"),
            "traffic policy plugin denied request"
        );
    }
}

impl std::fmt::Debug for TrafficDecision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TrafficDecision")
            .field("allow", &self.allow)
            .field("denied_by_plugin_id", &self.denied_by_plugin_id)
            .field("decision_code", &self.decision_code)
            .field("has_model_rewrite", &self.model.is_some())
            .field(
                "has_upstream_account_hint",
                &self.upstream_account_id.is_some(),
            )
            .field("has_request_rewrite", &self.request_json.is_some())
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedProviderRequest {
    pub method: reqwest::Method,
    pub path: String,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedProviderResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub estimated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedProviderEnvelope {
    method: String,
    path: String,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body_base64: String,
    streaming: bool,
}

#[derive(Debug, Serialize)]
struct UpstreamResponseEnvelope<'a> {
    status: u16,
    headers: &'a BTreeMap<String, String>,
    body_base64: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NormalizedProviderEnvelope {
    status: u16,
    #[serde(default)]
    headers: BTreeMap<String, String>,
    body_base64: String,
    usage: ComponentProviderUsage,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ComponentProviderUsage {
    input_tokens: u64,
    output_tokens: u64,
    estimated: bool,
}

#[derive(Clone, Default)]
pub struct PluginRuntime {
    engine: Option<Engine>,
    http: Option<reqwest::Client>,
    runtime: Option<tokio::runtime::Handle>,
    kv: Option<PluginKv>,
    plugins: Arc<Vec<LoadedPlugin>>,
    providers: Arc<Vec<ProviderType>>,
    configuration_cache: Arc<tokio::sync::RwLock<BTreeMap<Uuid, CachedPluginConfigurations>>>,
    execution_timeout: Duration,
    fuel: u64,
}

#[derive(Clone)]
struct PluginKv {
    database: Database,
}

struct HostState {
    plugin_id: String,
    capabilities: Vec<PluginCapability>,
    http: reqwest::Client,
    runtime: tokio::runtime::Handle,
    kv: Option<PluginKv>,
    limits: StoreLimits,
    deadline: Instant,
}

impl PluginRuntime {
    pub fn load(root: Option<&str>, database: Database) -> Result<Self, AppError> {
        let Some(root) = root else {
            return Ok(Self::default());
        };
        let root = Path::new(root);
        if !root.is_dir() {
            return Err(AppError::BadRequest(
                "plugin directory does not exist".into(),
            ));
        }

        let mut engine_config = WasmtimeConfig::new();
        engine_config.wasm_component_model(true);
        engine_config.consume_fuel(true);
        engine_config.epoch_interruption(true);
        let engine = Engine::new(&engine_config)
            .map_err(|_| plugin_runtime_failure("engine_initialization"))?;
        let epoch_engine = engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(PLUGIN_EPOCH_TICK);
            loop {
                interval.tick().await;
                epoch_engine.increment_epoch();
            }
        });
        let http = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| plugin_runtime_failure("http_initialization"))?;
        let canonical_root =
            fs::canonicalize(root).map_err(|_| plugin_runtime_failure("directory_resolution"))?;
        let directories = plugin_directories(&canonical_root)?;

        let mut plugins = Vec::new();
        let mut providers = Vec::new();
        for directory in directories {
            let manifest = validate_plugin_package(&directory)?;
            if plugins
                .iter()
                .any(|loaded: &LoadedPlugin| loaded.manifest.id == manifest.id)
            {
                return Err(AppError::BadRequest(format!(
                    "duplicate plugin id: {}",
                    manifest.id
                )));
            }
            for provider in &manifest.contributions.providers {
                let mut provider = provider.clone();
                provider.source = format!("plugin:{}@{}", manifest.id, manifest.version);
                if providers
                    .iter()
                    .any(|existing: &ProviderType| existing.id == provider.id)
                {
                    return Err(AppError::BadRequest(format!(
                        "duplicate plugin provider type: {}",
                        provider.id
                    )));
                }
                providers.push(provider);
            }
            let component = manifest
                .wasm
                .as_deref()
                .map(|wasm| {
                    let wasm_path = safe_child(&directory, wasm)?;
                    require_file_size(&wasm_path, PLUGIN_COMPONENT_BYTES, "plugin component")?;
                    Component::from_file(&engine, &wasm_path).map_err(|_| {
                        AppError::BadRequest("plugin component cannot be compiled".into())
                    })
                })
                .transpose()?;
            let configuration_validator = manifest
                .contributions
                .configuration
                .as_ref()
                .map(|configuration| crate::schema::compile(&configuration.schema))
                .transpose()?;
            plugins.push(LoadedPlugin {
                manifest,
                component,
                configuration_validator,
            });
        }

        Ok(Self {
            engine: Some(engine),
            http: Some(http),
            runtime: Some(tokio::runtime::Handle::current()),
            kv: Some(PluginKv { database }),
            plugins: Arc::new(plugins),
            providers: Arc::new(providers),
            configuration_cache: Arc::default(),
            execution_timeout: PLUGIN_EXECUTION_TIMEOUT,
            fuel: PLUGIN_FUEL,
        })
    }

    pub fn provider_types(&self) -> Vec<ProviderType> {
        self.providers.as_ref().clone()
    }

    pub fn manifests(&self) -> Vec<PluginManifest> {
        self.plugins
            .iter()
            .map(|plugin| plugin.manifest.clone())
            .collect()
    }

    pub fn configuration_contribution(
        &self,
        plugin_id: &str,
    ) -> Option<PluginConfigurationContribution> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .and_then(|plugin| plugin.manifest.contributions.configuration.clone())
    }

    pub async fn resolved_traffic_configurations(
        &self,
        tenant_id: Uuid,
    ) -> Result<BTreeMap<String, Value>, AppError> {
        let configurable: Vec<_> = self
            .plugins
            .iter()
            .filter(|plugin| {
                (plugin.manifest.contributions.traffic_policy
                    || plugin.manifest.contributions.request_rewrite)
                    && plugin.manifest.contributions.configuration.is_some()
            })
            .collect();
        if configurable.is_empty() {
            return Ok(BTreeMap::new());
        }
        if let Some(cached) = self.configuration_cache.read().await.get(&tenant_id)
            && cached.loaded_at.elapsed() < PLUGIN_CONFIGURATION_CACHE_TTL
        {
            return Ok(cached.values.clone());
        }
        let database = &self.kv.as_ref().ok_or(AppError::Internal)?.database;
        let layers = database.plugin_configuration_layers(tenant_id).await?;
        let mut resolved = BTreeMap::new();
        for plugin in configurable {
            let contribution = plugin
                .manifest
                .contributions
                .configuration
                .as_ref()
                .expect("filtered configurable plugin");
            let stored = layers
                .iter()
                .find(|layer| {
                    layer.plugin_id == plugin.manifest.id && layer.tenant_id == Some(tenant_id)
                })
                .or_else(|| {
                    layers.iter().find(|layer| {
                        layer.plugin_id == plugin.manifest.id && layer.tenant_id.is_none()
                    })
                });
            let value = stored
                .map(|configuration| configuration.value.clone())
                .unwrap_or_else(|| contribution.default.clone());
            plugin
                .configuration_validator
                .as_ref()
                .ok_or(AppError::Internal)?
                .validate(&value)?;
            resolved.insert(plugin.manifest.id.clone(), value);
        }
        self.cache_resolved_configurations(tenant_id, resolved.clone())
            .await;
        Ok(resolved)
    }

    async fn cache_resolved_configurations(
        &self,
        tenant_id: Uuid,
        values: BTreeMap<String, Value>,
    ) {
        let estimated_bytes = values.iter().fold(0usize, |total, (plugin_id, value)| {
            total
                .saturating_add(plugin_id.len())
                .saturating_add(estimated_json_bytes(value))
        });
        if estimated_bytes > PLUGIN_CONFIGURATION_CACHE_BYTES {
            return;
        }
        let now = Instant::now();
        let mut cache = self.configuration_cache.write().await;
        cache.retain(|_, entry| {
            now.duration_since(entry.loaded_at) <= PLUGIN_CONFIGURATION_CACHE_TTL
        });
        cache.remove(&tenant_id);
        loop {
            let current_bytes = cache.values().fold(0usize, |total, entry| {
                total.saturating_add(entry.estimated_bytes)
            });
            if cache.len() < PLUGIN_CONFIGURATION_CACHE_ENTRIES
                && current_bytes.saturating_add(estimated_bytes) <= PLUGIN_CONFIGURATION_CACHE_BYTES
            {
                break;
            }
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.loaded_at)
                .map(|(id, _)| *id)
            else {
                break;
            };
            cache.remove(&oldest);
        }
        cache.insert(
            tenant_id,
            CachedPluginConfigurations {
                loaded_at: now,
                values,
                estimated_bytes,
            },
        );
    }

    pub async fn invalidate_configuration_cache(&self, tenant_id: Option<Uuid>) {
        let mut cache = self.configuration_cache.write().await;
        if let Some(tenant_id) = tenant_id {
            cache.remove(&tenant_id);
        } else {
            // A global write may affect every tenant without an override.
            cache.clear();
        }
    }

    pub async fn validate_stored_configurations(&self) -> Result<(), AppError> {
        if self.plugins.is_empty() {
            return Ok(());
        }
        let database = &self.kv.as_ref().ok_or(AppError::Internal)?.database;
        database
            .visit_plugin_configurations(|stored| {
                let Some(plugin) = self
                    .plugins
                    .iter()
                    .find(|plugin| plugin.manifest.id == stored.plugin_id)
                else {
                    // A disabled plugin may leave configuration behind so that a
                    // later re-enable is non-destructive. It has no execution path.
                    return Ok(());
                };
                let _configuration = plugin
                    .manifest
                    .contributions
                    .configuration
                    .as_ref()
                    .ok_or_else(|| {
                        AppError::BadRequest(format!(
                            "plugin {} has stored configuration but no configuration schema",
                            stored.plugin_id
                        ))
                    })?;
                plugin
                    .configuration_validator
                    .as_ref()
                    .ok_or(AppError::Internal)?
                    .validate(&stored.value)
            })
            .await
    }

    pub fn apply_traffic(
        &self,
        context: types::RequestContext,
        request_json: &Value,
    ) -> Result<TrafficDecision, AppError> {
        self.apply_traffic_with_config(context, request_json, &BTreeMap::new())
    }

    pub fn apply_traffic_with_config(
        &self,
        context: types::RequestContext,
        request_json: &Value,
        configurations: &BTreeMap<String, Value>,
    ) -> Result<TrafficDecision, AppError> {
        let Some(engine) = &self.engine else {
            return Ok(TrafficDecision {
                allow: true,
                ..TrafficDecision::default()
            });
        };
        let http = self.http.as_ref().ok_or(AppError::Internal)?;
        let runtime = self.runtime.as_ref().ok_or(AppError::Internal)?;
        let mut current = request_json.clone();
        let mut decision = TrafficDecision {
            allow: true,
            ..TrafficDecision::default()
        };
        for plugin in self.plugins.iter().filter(|plugin| {
            plugin.manifest.contributions.traffic_policy
                || plugin.manifest.contributions.request_rewrite
        }) {
            let component = plugin.component.as_ref().ok_or_else(|| {
                AppError::Storage(format!(
                    "plugin {} contributes a traffic policy without a component",
                    plugin.manifest.id
                ))
            })?;
            let limits = StoreLimitsBuilder::new()
                .memory_size(PLUGIN_MEMORY_BYTES)
                .table_elements(PLUGIN_TABLE_ELEMENTS)
                // Component-model instantiation uses more than one internal
                // instance. Keep it functional but tightly bounded.
                .instances(8)
                .tables(2)
                .memories(2)
                .build();
            let mut store = Store::new(
                engine,
                HostState {
                    plugin_id: plugin.manifest.id.clone(),
                    capabilities: plugin.manifest.capabilities.clone(),
                    http: http.clone(),
                    runtime: runtime.clone(),
                    kv: self.kv.clone(),
                    limits,
                    deadline: Instant::now() + self.execution_timeout,
                },
            );
            store.limiter(|state| &mut state.limits);
            store.set_epoch_deadline(epoch_deadline_ticks(self.execution_timeout));
            store
                .set_fuel(self.fuel)
                .map_err(|_| plugin_runtime_failure("fuel_configuration"))?;
            let mut linker = Linker::new(engine);
            Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(|_| plugin_runtime_failure("linker_configuration"))?;
            let bindings = Plugin::instantiate(&mut store, component, &linker)
                .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
            let request = serde_json::to_string(&current).map_err(|_| AppError::Internal)?;
            let mut plugin_context = context.clone();
            let configuration = configurations
                .get(&plugin.manifest.id)
                .cloned()
                .or_else(|| {
                    plugin
                        .manifest
                        .contributions
                        .configuration
                        .as_ref()
                        .map(|contribution| contribution.default.clone())
                })
                .unwrap_or_else(empty_json_object);
            plugin_context.config_json =
                serde_json::to_string(&configuration).map_err(|_| AppError::Internal)?;
            let result = bindings
                .memeloop_token_center_traffic_policy()
                .call_post_auth(&mut store, &plugin_context, &request)
                .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
                .map_err(|error| {
                    plugin_reported_error(&plugin.manifest.id, "traffic-policy", &error)
                })?;
            let reason_is_valid = result.reason.as_deref().is_none_or(|reason| {
                validate_plugin_text(reason, MAX_TRAFFIC_REASON_BYTES, false).is_ok()
            });
            let model_is_valid = result.model.as_deref().is_none_or(|model| {
                validate_plugin_text(model, MAX_TRAFFIC_MODEL_BYTES, false).is_ok()
            });
            let account_id_is_valid =
                result
                    .upstream_account_id
                    .as_deref()
                    .is_none_or(|account_id| {
                        validate_plugin_text(account_id, MAX_TRAFFIC_ACCOUNT_ID_BYTES, false)
                            .is_ok()
                    });
            let validated_request = result
                .request_json
                .as_deref()
                .map(validate_traffic_request_json)
                .transpose();
            if !result.allow {
                return Ok(TrafficDecision {
                    allow: false,
                    denied_by_plugin_id: Some(plugin.manifest.id.clone()),
                    decision_code: Some(
                        if reason_is_valid
                            && model_is_valid
                            && account_id_is_valid
                            && validated_request.is_ok()
                        {
                            "policy_denied"
                        } else {
                            "policy_denied_invalid_metadata"
                        },
                    ),
                    ..decision
                });
            }
            if !reason_is_valid {
                return Err(invalid_plugin_result(
                    &plugin.manifest.id,
                    "traffic policy reason",
                ));
            }
            if !model_is_valid {
                return Err(invalid_plugin_result(
                    &plugin.manifest.id,
                    "traffic policy model",
                ));
            }
            if !account_id_is_valid {
                return Err(invalid_plugin_result(
                    &plugin.manifest.id,
                    "traffic policy upstream account id",
                ));
            }
            if let Some(request) = validated_request.map_err(|_| {
                invalid_plugin_result(&plugin.manifest.id, "traffic policy request JSON")
            })? {
                current = request;
                decision.request_json = Some(current.clone());
            }
            if let Some(model) = result.model {
                decision.model = Some(model);
            }
            if let Some(account_id) = result.upstream_account_id {
                decision.upstream_account_id = Some(account_id);
            }
        }
        Ok(decision)
    }

    /// Invokes the component-side provider discovery contribution, when the
    /// declaring plugin ships executable provider logic. Manifest-only HTTP
    /// providers deliberately return `None` and use the core HTTP driver.
    pub fn list_provider_models(
        &self,
        provider_id: &str,
        config: &Value,
    ) -> Result<Option<Value>, AppError> {
        let Some(plugin) = self.plugins.iter().find(|plugin| {
            plugin
                .manifest
                .contributions
                .providers
                .iter()
                .any(|provider| provider.id == provider_id)
        }) else {
            return Ok(None);
        };
        let Some(component) = plugin.component.as_ref() else {
            return Ok(None);
        };
        let engine = self.engine.as_ref().ok_or(AppError::Internal)?;
        let http = self.http.as_ref().ok_or(AppError::Internal)?;
        let runtime = self.runtime.as_ref().ok_or(AppError::Internal)?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(PLUGIN_MEMORY_BYTES)
            .table_elements(PLUGIN_TABLE_ELEMENTS)
            .instances(8)
            .tables(2)
            .memories(2)
            .build();
        let mut store = Store::new(
            engine,
            HostState {
                plugin_id: plugin.manifest.id.clone(),
                capabilities: plugin.manifest.capabilities.clone(),
                http: http.clone(),
                runtime: runtime.clone(),
                kv: self.kv.clone(),
                limits,
                deadline: Instant::now() + self.execution_timeout,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(self.execution_timeout));
        store
            .set_fuel(self.fuel)
            .map_err(|_| plugin_runtime_failure("fuel_configuration"))?;
        let mut linker = Linker::new(engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|_| plugin_runtime_failure("linker_configuration"))?;
        let bindings = Plugin::instantiate(&mut store, component, &linker)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
        let config_json = serde_json::to_string(config).map_err(|_| AppError::Internal)?;
        let models_json = bindings
            .memeloop_token_center_upstream_provider()
            .call_list_models(&mut store, &config_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| plugin_reported_error(&plugin.manifest.id, "list-models", &error))?;
        let models: Value = serde_json::from_str(&models_json).map_err(|_| {
            AppError::Upstream(format!(
                "plugin {} returned invalid models JSON",
                plugin.manifest.id
            ))
        })?;
        if !models.is_array() {
            return Err(AppError::Upstream(format!(
                "plugin {} models response must be a JSON array",
                plugin.manifest.id
            )));
        }
        Ok(Some(models))
    }

    /// Calls an explicitly declared buffered component provider. The caller
    /// supplies only administrator-owned configuration and canonical request
    /// JSON; encrypted API/OAuth credentials remain in the core.
    pub fn prepare_provider_request(
        &self,
        provider_id: &str,
        context: types::RequestContext,
        config: &Value,
        request: &Value,
    ) -> Result<Option<PreparedProviderRequest>, AppError> {
        let Some((plugin, provider)) = self.component_provider(provider_id)? else {
            return Ok(None);
        };
        let component = plugin.component.as_ref().ok_or_else(|| {
            AppError::Storage(format!(
                "plugin {} component provider is unavailable",
                plugin.manifest.id
            ))
        })?;
        let engine = self.engine.as_ref().ok_or(AppError::Internal)?;
        let (mut store, bindings) = self.instantiate(plugin, component, engine)?;
        let config_json = serde_json::to_string(config).map_err(|_| AppError::Internal)?;
        let request_json = serde_json::to_string(request).map_err(|_| AppError::Internal)?;
        if config_json.len().saturating_add(request_json.len()) > MAX_COMPONENT_PROVIDER_BODY {
            return Err(AppError::Upstream(
                "component provider request exceeds the 4 MiB ABI limit".into(),
            ));
        }
        let result = bindings
            .memeloop_token_center_upstream_provider()
            .call_prepare(&mut store, &context, &config_json, &request_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| plugin_reported_error(&plugin.manifest.id, "prepare", &error))?;
        if result.len() > encoded_body_limit(MAX_COMPONENT_PROVIDER_BODY) {
            return Err(AppError::Upstream(format!(
                "plugin {} prepared request exceeds the 4 MiB ABI limit",
                plugin.manifest.id
            )));
        }
        let envelope: PreparedProviderEnvelope = serde_json::from_str(&result).map_err(|_| {
            AppError::Upstream(format!(
                "plugin {} returned an invalid prepared request",
                plugin.manifest.id
            ))
        })?;
        if envelope.streaming {
            return Err(AppError::Upstream(format!(
                "plugin {} requested unsupported streaming transport",
                plugin.manifest.id
            )));
        }
        let method = validate_provider_method(&envelope.method)?;
        validate_provider_path(&envelope.path)?;
        validate_provider_headers(&envelope.headers, true)?;
        let body = STANDARD.decode(envelope.body_base64).map_err(|_| {
            AppError::Upstream(format!(
                "plugin {} returned invalid request body encoding",
                plugin.manifest.id
            ))
        })?;
        if body.len() > MAX_COMPONENT_PROVIDER_BODY {
            return Err(AppError::Upstream(format!(
                "plugin {} prepared request body exceeds 4 MiB",
                plugin.manifest.id
            )));
        }
        debug_assert_eq!(
            provider.component_adapter.as_ref().unwrap().api_version,
            "buffered-v1"
        );
        Ok(Some(PreparedProviderRequest {
            method,
            path: envelope.path,
            headers: envelope.headers,
            body,
        }))
    }

    pub fn normalize_provider_response(
        &self,
        provider_id: &str,
        context: types::RequestContext,
        upstream_status: u16,
        upstream_headers: &BTreeMap<String, String>,
        upstream_body: &[u8],
    ) -> Result<Option<NormalizedProviderResponse>, AppError> {
        let Some((plugin, provider)) = self.component_provider(provider_id)? else {
            return Ok(None);
        };
        let adapter = provider
            .component_adapter
            .as_ref()
            .ok_or(AppError::Internal)?;
        if upstream_body.len() > adapter.max_response_bytes
            || upstream_body.len() > MAX_COMPONENT_PROVIDER_BODY
        {
            return Err(AppError::Upstream(format!(
                "plugin {} upstream response exceeds its declared limit",
                plugin.manifest.id
            )));
        }
        validate_provider_headers(upstream_headers, false)?;
        let response_json = serde_json::to_string(&UpstreamResponseEnvelope {
            status: upstream_status,
            headers: upstream_headers,
            body_base64: STANDARD.encode(upstream_body),
        })
        .map_err(|_| AppError::Internal)?;
        if response_json.len() > encoded_body_limit(adapter.max_response_bytes) {
            return Err(AppError::Upstream(format!(
                "plugin {} upstream response exceeds its declared ABI limit",
                plugin.manifest.id
            )));
        }
        let component = plugin.component.as_ref().ok_or_else(|| {
            AppError::Storage(format!(
                "plugin {} component provider is unavailable",
                plugin.manifest.id
            ))
        })?;
        let engine = self.engine.as_ref().ok_or(AppError::Internal)?;
        let (mut store, bindings) = self.instantiate(plugin, component, engine)?;
        let result = bindings
            .memeloop_token_center_upstream_provider()
            .call_normalize(&mut store, &context, &response_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| plugin_reported_error(&plugin.manifest.id, "normalize", &error))?;
        if result.len() > encoded_body_limit(adapter.max_response_bytes) {
            return Err(AppError::Upstream(format!(
                "plugin {} normalized response exceeds its declared limit",
                plugin.manifest.id
            )));
        }
        let envelope: NormalizedProviderEnvelope = serde_json::from_str(&result).map_err(|_| {
            AppError::Upstream(format!(
                "plugin {} returned an invalid normalized response",
                plugin.manifest.id
            ))
        })?;
        if !(200..=599).contains(&envelope.status) {
            return Err(AppError::Upstream(format!(
                "plugin {} returned an invalid downstream status",
                plugin.manifest.id
            )));
        }
        validate_provider_headers(&envelope.headers, false)?;
        let body = STANDARD.decode(envelope.body_base64).map_err(|_| {
            AppError::Upstream(format!(
                "plugin {} returned invalid normalized body encoding",
                plugin.manifest.id
            ))
        })?;
        if body.len() > adapter.max_response_bytes || body.len() > MAX_COMPONENT_PROVIDER_BODY {
            return Err(AppError::Upstream(format!(
                "plugin {} normalized response body exceeds its declared limit",
                plugin.manifest.id
            )));
        }
        Ok(Some(NormalizedProviderResponse {
            status: envelope.status,
            headers: envelope.headers,
            body,
            input_tokens: envelope.usage.input_tokens,
            output_tokens: envelope.usage.output_tokens,
            estimated: envelope.usage.estimated,
        }))
    }

    fn component_provider(
        &self,
        provider_id: &str,
    ) -> Result<Option<(&LoadedPlugin, &ProviderType)>, AppError> {
        let Some(plugin) = self.plugins.iter().find(|plugin| {
            plugin
                .manifest
                .contributions
                .providers
                .iter()
                .any(|provider| provider.id == provider_id)
        }) else {
            return Ok(None);
        };
        let provider = plugin
            .manifest
            .contributions
            .providers
            .iter()
            .find(|provider| provider.id == provider_id)
            .ok_or(AppError::Internal)?;
        if provider.component_adapter.is_none() {
            return Ok(None);
        }
        if plugin.component.is_none() {
            return Err(AppError::Storage(format!(
                "plugin {} declared a component provider without a component",
                plugin.manifest.id
            )));
        }
        Ok(Some((plugin, provider)))
    }

    fn instantiate(
        &self,
        plugin: &LoadedPlugin,
        component: &Component,
        engine: &Engine,
    ) -> Result<(Store<HostState>, Plugin), AppError> {
        let http = self.http.as_ref().ok_or(AppError::Internal)?;
        let runtime = self.runtime.as_ref().ok_or(AppError::Internal)?;
        let limits = StoreLimitsBuilder::new()
            .memory_size(PLUGIN_MEMORY_BYTES)
            .table_elements(PLUGIN_TABLE_ELEMENTS)
            .instances(8)
            .tables(2)
            .memories(2)
            .build();
        let mut store = Store::new(
            engine,
            HostState {
                plugin_id: plugin.manifest.id.clone(),
                capabilities: plugin.manifest.capabilities.clone(),
                http: http.clone(),
                runtime: runtime.clone(),
                kv: self.kv.clone(),
                limits,
                deadline: Instant::now() + self.execution_timeout,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(self.execution_timeout));
        store
            .set_fuel(self.fuel)
            .map_err(|_| plugin_runtime_failure("fuel_configuration"))?;
        let mut linker = Linker::new(engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|_| plugin_runtime_failure("linker_configuration"))?;
        let bindings = Plugin::instantiate(&mut store, component, &linker)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
        Ok((store, bindings))
    }

    #[doc(hidden)]
    pub fn set_execution_limits_for_tests(&mut self, timeout: Duration, fuel: u64) {
        self.execution_timeout = timeout;
        self.fuel = fuel;
    }
}

fn encoded_body_limit(body_limit: usize) -> usize {
    body_limit
        .saturating_mul(4)
        .div_ceil(3)
        .saturating_add(64 * 1024)
}

fn validate_provider_method(value: &str) -> Result<reqwest::Method, AppError> {
    let method = reqwest::Method::from_bytes(value.as_bytes())
        .map_err(|_| AppError::Upstream("component provider returned an invalid method".into()))?;
    if !matches!(
        method,
        reqwest::Method::GET
            | reqwest::Method::POST
            | reqwest::Method::PUT
            | reqwest::Method::PATCH
            | reqwest::Method::DELETE
    ) {
        return Err(AppError::Upstream(
            "component provider method is not allowed".into(),
        ));
    }
    Ok(method)
}

fn validate_provider_path(value: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > 4_096
        || !value.starts_with('/')
        || value.starts_with("//")
        || value.contains('#')
        || value.chars().any(char::is_control)
    {
        return Err(AppError::Upstream(
            "component provider returned an unsafe relative path".into(),
        ));
    }
    Ok(())
}

fn validate_provider_headers(
    headers: &BTreeMap<String, String>,
    reject_credentials: bool,
) -> Result<(), AppError> {
    if headers.len() > 64 {
        return Err(AppError::Upstream(
            "component provider returned too many headers".into(),
        ));
    }
    let mut total = 0_usize;
    for (name, value) in headers {
        let parsed_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AppError::Upstream("component provider header name is invalid".into()))?;
        reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| AppError::Upstream("component provider header value is invalid".into()))?;
        total = total.saturating_add(name.len()).saturating_add(value.len());
        let forbidden = matches!(
            parsed_name.as_str(),
            "transfer-encoding"
                | "connection"
                | "upgrade"
                | "proxy-authorization"
                | "proxy-authenticate"
                | "te"
                | "trailer"
                | "set-cookie"
        ) || (reject_credentials
            && matches!(
                parsed_name.as_str(),
                "authorization" | "cookie" | "x-api-key" | "host" | "content-length"
            ));
        if forbidden {
            return Err(AppError::Upstream(
                "component provider returned a forbidden header".into(),
            ));
        }
    }
    if total > 16 * 1024 {
        return Err(AppError::Upstream(
            "component provider headers exceed 16 KiB".into(),
        ));
    }
    Ok(())
}

fn validate_plugin_http_method(value: &str) -> Result<reqwest::Method, String> {
    let method = reqwest::Method::from_bytes(value.as_bytes())
        .map_err(|_| "plugin HTTP method is invalid".to_owned())?;
    if !matches!(
        method,
        reqwest::Method::GET
            | reqwest::Method::HEAD
            | reqwest::Method::POST
            | reqwest::Method::PUT
            | reqwest::Method::PATCH
            | reqwest::Method::DELETE
    ) {
        return Err("plugin HTTP method is not allowed".to_owned());
    }
    Ok(method)
}

fn validate_plugin_http_headers(
    headers_json: &str,
) -> Result<Vec<(reqwest::header::HeaderName, reqwest::header::HeaderValue)>, String> {
    if headers_json.len() > PLUGIN_HTTP_HEADERS_JSON_BYTES {
        return Err("plugin HTTP headers JSON exceeds 128 KiB".to_owned());
    }
    let headers: BTreeMap<String, String> = serde_json::from_str(headers_json)
        .map_err(|_| "plugin HTTP headers must be a string map".to_owned())?;
    if headers.len() > PLUGIN_HTTP_HEADER_COUNT {
        return Err("plugin HTTP request has too many headers".to_owned());
    }

    let mut total = 0_usize;
    let mut normalized_names = BTreeSet::new();
    let mut validated = Vec::with_capacity(headers.len());
    for (name, value) in headers {
        if name.len() > PLUGIN_HTTP_HEADER_NAME_BYTES {
            return Err("plugin HTTP header name exceeds 256 bytes".to_owned());
        }
        if value.len() > PLUGIN_HTTP_HEADER_VALUE_BYTES {
            return Err("plugin HTTP header value exceeds 8 KiB".to_owned());
        }
        total = total.saturating_add(name.len()).saturating_add(value.len());
        if total > PLUGIN_HTTP_HEADER_TOTAL_BYTES {
            return Err("plugin HTTP headers exceed 16 KiB".to_owned());
        }

        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| "plugin HTTP header name is invalid".to_owned())?;
        let normalized = name.as_str();
        if !normalized_names.insert(normalized.to_owned()) {
            return Err("plugin HTTP request contains a duplicate header".to_owned());
        }
        let forbidden = matches!(
            normalized,
            "host"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "http2-settings"
                | "proxy"
                | "forwarded"
                | "via"
                | "max-forwards"
                | "x-real-ip"
                | "x-original-url"
                | "x-rewrite-url"
                | "x-http-method"
                | "x-http-method-override"
                | "x-method-override"
        ) || normalized.starts_with("proxy-")
            || normalized.starts_with("x-proxy-")
            || normalized.starts_with("x-forwarded-");
        if forbidden {
            return Err("plugin HTTP request contains a forbidden header".to_owned());
        }

        let value = reqwest::header::HeaderValue::from_str(&value)
            .map_err(|_| "plugin HTTP header value is invalid".to_owned())?;
        validated.push((name, value));
    }
    Ok(validated)
}

fn epoch_deadline_ticks(timeout: Duration) -> u64 {
    let tick_millis = PLUGIN_EPOCH_TICK.as_millis();
    u64::try_from(timeout.as_millis().div_ceil(tick_millis))
        .unwrap_or(u64::MAX)
        .max(1)
}

fn plugin_directories(root: &Path) -> Result<Vec<PathBuf>, AppError> {
    if root.join("plugin.json").is_file() {
        return Ok(vec![root.to_path_buf()]);
    }
    let entries = fs::read_dir(root)
        .map_err(|_| plugin_runtime_failure("directory_read"))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| plugin_runtime_failure("directory_entry_read"))?;
    let mut directories = Vec::new();
    for entry in entries {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|_| plugin_runtime_failure("directory_entry_type"))?;
        if file_type.is_symlink() {
            return Err(AppError::BadRequest(
                "plugin package directory cannot be a symlink".into(),
            ));
        }
        if file_type.is_dir() {
            directories.push(
                fs::canonicalize(entry.path())
                    .map_err(|_| plugin_runtime_failure("package_directory_resolution"))?,
            );
        }
    }
    directories.sort();
    Ok(directories)
}

impl memeloop::token_center::host::Host for HostState {
    fn log(&mut self, level: String, _message: String) {
        if !self
            .capabilities
            .iter()
            .any(|capability| matches!(capability, PluginCapability::Log))
        {
            return;
        }
        match level.as_str() {
            "error" => tracing::error!(
                plugin_id = %self.plugin_id,
                decision_code = "plugin_log_emitted",
                "plugin emitted a log event"
            ),
            "warn" => tracing::warn!(
                plugin_id = %self.plugin_id,
                decision_code = "plugin_log_emitted",
                "plugin emitted a log event"
            ),
            "debug" => tracing::debug!(
                plugin_id = %self.plugin_id,
                decision_code = "plugin_log_emitted",
                "plugin emitted a log event"
            ),
            _ => tracing::info!(
                plugin_id = %self.plugin_id,
                decision_code = "plugin_log_emitted",
                "plugin emitted a log event"
            ),
        }
    }

    fn kv_get(&mut self, key: String) -> Result<Option<Vec<u8>>, String> {
        if !self
            .capabilities
            .iter()
            .any(|capability| matches!(capability, PluginCapability::Kv))
        {
            return Err("plugin did not declare the KV capability".to_owned());
        }
        let kv = self
            .kv
            .as_ref()
            .ok_or_else(|| "plugin KV runtime is unavailable".to_owned())?;
        self.runtime
            .block_on(kv.database.plugin_kv_get(&self.plugin_id, &key))
            .map_err(|_| "plugin KV get failed".to_owned())
    }

    fn kv_put(&mut self, key: String, value: Vec<u8>) -> Result<(), String> {
        if !self
            .capabilities
            .iter()
            .any(|capability| matches!(capability, PluginCapability::Kv))
        {
            return Err("plugin did not declare the KV capability".to_owned());
        }
        let kv = self
            .kv
            .as_ref()
            .ok_or_else(|| "plugin KV runtime is unavailable".to_owned())?;
        self.runtime
            .block_on(kv.database.plugin_kv_put(&self.plugin_id, &key, &value))
            .map_err(|_| "plugin KV put failed".to_owned())
    }

    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers_json: String,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        if body.len() > PLUGIN_HTTP_BODY_BYTES {
            return Err("plugin HTTP request exceeds 16 MiB".to_owned());
        }
        let url = url::Url::parse(&url).map_err(|_| "plugin HTTP URL is invalid".to_owned())?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
        {
            return Err(
                "plugin HTTP URL must be an HTTP(S) URL without credentials or a fragment"
                    .to_owned(),
            );
        }
        let origin = url.origin().ascii_serialization();
        let allowed = self.capabilities.iter().any(|capability| {
            let PluginCapability::Http { allowed_origins } = capability else {
                return false;
            };
            allowed_origins.iter().any(|allowed| allowed == &origin)
        });
        if !allowed {
            return Err(format!("plugin HTTP origin is not allowed: {origin}"));
        }
        // Validate the complete request metadata before DNS or network access.
        // The allowlisted URL remains the request URL so reqwest derives Host
        // and TLS SNI from its original hostname; plugins cannot override it.
        let method = validate_plugin_http_method(&method)?;
        let headers = validate_plugin_http_headers(&headers_json)?;
        // Plugin packages currently have no global-operator approval metadata
        // for private destinations. Until that exists, HTTP capability is
        // deliberately public-only and its DNS result is pinned per call.
        let outbound_http = self
            .runtime
            .block_on(network::client_for_url(
                &self.http,
                url.as_str(),
                OutboundScope::Public,
                false,
            ))
            .map_err(|_| "plugin HTTP destination is unavailable or unsafe".to_owned())?;
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "plugin execution deadline exceeded".to_owned())?;
        let mut request = outbound_http
            .request(method, url)
            .timeout(remaining)
            .body(body);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        let (status, response_headers, response_body) = self.runtime.block_on(async move {
            let response = request
                .send()
                .await
                .map_err(|_| "plugin HTTP request failed".to_owned())?;
            let status = response.status().as_u16();
            let response_headers = response
                .headers()
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .to_str()
                        .ok()
                        .map(|value| (name.to_string(), value.to_owned()))
                })
                .collect::<BTreeMap<_, _>>();
            let mut response_body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| "plugin HTTP response read failed".to_owned())?;
                if response_body.len().saturating_add(chunk.len()) > PLUGIN_HTTP_BODY_BYTES {
                    return Err("plugin HTTP response exceeds 16 MiB".to_owned());
                }
                response_body.extend_from_slice(&chunk);
            }
            Ok::<_, String>((status, response_headers, response_body))
        })?;
        serde_json::to_vec(&serde_json::json!({
            "status": status,
            "headers": response_headers,
            "body_base64": STANDARD.encode(response_body)
        }))
        .map_err(|_| "plugin HTTP response encoding failed".to_owned())
    }
}

impl memeloop::token_center::types::Host for HostState {}

/// Validates one unpacked plugin package with the same authoritative rules the
/// runtime uses at startup. Distribution installers call this before making a
/// staged package visible.
pub fn validate_plugin_package(directory: &Path) -> Result<PluginManifest, AppError> {
    let manifest_path = directory.join("plugin.json");
    if !manifest_path.is_file() {
        return Err(AppError::BadRequest(
            "plugin package has no plugin.json".into(),
        ));
    }
    require_file_size(&manifest_path, PLUGIN_MANIFEST_BYTES, "plugin manifest")?;
    let manifest_bytes =
        fs::read(&manifest_path).map_err(|_| plugin_runtime_failure("manifest_read"))?;
    let manifest_value: Value = serde_json::from_slice(&manifest_bytes)
        .map_err(|_| AppError::BadRequest("plugin manifest is invalid JSON".into()))?;
    let manifest_schema: Value =
        serde_json::from_str(include_str!("../schemas/plugin-manifest.schema.json"))
            .map_err(|_| AppError::Internal)?;
    // Validate the source document before Serde applies defaults. The checked-in
    // schema is authoritative for required and unknown fields.
    crate::schema::validate_instance(&manifest_schema, &manifest_value)?;
    let manifest: PluginManifest = serde_json::from_value(manifest_value)
        .map_err(|_| AppError::BadRequest("plugin manifest shape is invalid".into()))?;
    validate_manifest(&manifest)?;
    for provider in &manifest.contributions.providers {
        let mut provider = provider.clone();
        provider.source = format!("plugin:{}@{}", manifest.id, manifest.version);
        validate_provider_contribution(&manifest.id, &provider)?;
    }
    if let Some(wasm) = manifest.wasm.as_deref() {
        let wasm_path = safe_child(directory, wasm)?;
        require_file_size(&wasm_path, PLUGIN_COMPONENT_BYTES, "plugin component")?;
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), AppError> {
    if manifest.id.is_empty()
        || manifest.id.len() > MAX_PLUGIN_ID_BYTES
        || !manifest
            .id
            .chars()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == '-')
    {
        return Err(AppError::BadRequest(
            "plugin id must contain lowercase ASCII letters, digits, or hyphens".into(),
        ));
    }
    if semver::Version::parse(&manifest.version).is_err()
        || !semver::VersionReq::parse(SUPPORTED_WIT_REQUIREMENT)
            .map_err(|_| AppError::Internal)?
            .matches(&semver::Version::parse(&manifest.wit_version).map_err(|_| {
                AppError::BadRequest(format!("plugin {} has an invalid WIT version", manifest.id))
            })?)
    {
        return Err(AppError::BadRequest(format!(
            "plugin {} has an unsupported version or WIT version",
            manifest.id
        )));
    }
    if (manifest.contributions.traffic_policy || manifest.contributions.request_rewrite)
        && manifest.wasm.is_none()
    {
        return Err(AppError::BadRequest(format!(
            "plugin {} needs a component for its traffic or request-rewrite contribution",
            manifest.id
        )));
    }
    if manifest.wasm.is_none()
        && manifest
            .contributions
            .providers
            .iter()
            .any(|provider| provider.component_adapter.is_some())
    {
        return Err(AppError::BadRequest(format!(
            "plugin {} needs a component for its executable provider adapter",
            manifest.id
        )));
    }
    if let Some(configuration) = &manifest.contributions.configuration {
        if configuration.schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err(AppError::BadRequest(format!(
                "plugin {} configuration schema must have an object root",
                manifest.id
            )));
        }
        crate::schema::validate_definition(&configuration.schema)?;
        if schema_contains_write_only(&configuration.schema) {
            return Err(AppError::BadRequest(format!(
                "plugin {} configuration schema cannot contain writeOnly fields; credentials belong to provider credential_schema",
                manifest.id
            )));
        }
        crate::schema::validate_instance(&configuration.schema, &configuration.default)?;
    }
    for capability in &manifest.capabilities {
        if let PluginCapability::Http { allowed_origins } = capability {
            if allowed_origins.is_empty() {
                return Err(AppError::BadRequest(format!(
                    "plugin {} HTTP capability needs at least one allowed origin",
                    manifest.id
                )));
            }
            for origin in allowed_origins {
                let parsed = url::Url::parse(origin).map_err(|_| {
                    AppError::BadRequest(format!(
                        "plugin {} has an invalid HTTP origin",
                        manifest.id
                    ))
                })?;
                if parsed.scheme() != "https"
                    || parsed.host_str().is_none()
                    || parsed.origin().ascii_serialization() != *origin
                    || parsed.path() != "/"
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} HTTP allowlist entries must be exact public HTTPS origins",
                        manifest.id
                    )));
                }
            }
        }
    }
    for provider in &manifest.contributions.providers {
        validate_provider_contribution(&manifest.id, provider)?;
    }
    Ok(())
}

pub fn plugin_configuration_schema_digest(schema: &Value) -> Result<String, AppError> {
    let canonical = canonical_json(schema);
    let bytes = serde_json::to_vec(&canonical).map_err(|_| AppError::Internal)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

pub fn plugin_configuration_request_hash(
    plugin_id: &str,
    scope: &str,
    expected_version: i64,
    schema_digest: &str,
    value: &Value,
) -> Result<String, AppError> {
    let payload = serde_json::json!({
        "plugin_id": plugin_id,
        "scope": scope,
        "expected_version": expected_version,
        "schema_digest": schema_digest,
        "value": canonical_json(value)
    });
    let bytes = serde_json::to_vec(&payload).map_err(|_| AppError::Internal)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Array(values) => Value::Array(values.iter().map(canonical_json).collect()),
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|left, right| left.0.cmp(right.0));
            Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key.clone(), canonical_json(value)))
                    .collect(),
            )
        }
        value => value.clone(),
    }
}

fn estimated_json_bytes(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(number) => number.to_string().len(),
        // Every input byte can expand to at most one six-byte JSON escape
        // (for example `\u0000`), so this remains a conservative budget.
        Value::String(value) => value.len().saturating_mul(6).saturating_add(2),
        Value::Array(values) => values.iter().fold(2usize, |total, value| {
            total
                .saturating_add(1)
                .saturating_add(estimated_json_bytes(value))
        }),
        Value::Object(values) => values.iter().fold(2usize, |total, (key, value)| {
            total
                .saturating_add(key.len().saturating_mul(6))
                .saturating_add(4)
                .saturating_add(estimated_json_bytes(value))
        }),
    }
}

fn schema_contains_write_only(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(schema_contains_write_only),
        Value::Object(values) => {
            values.get("writeOnly").and_then(Value::as_bool) == Some(true)
                || values.values().any(schema_contains_write_only)
        }
        _ => false,
    }
}

fn validate_provider_contribution(
    plugin_id: &str,
    provider: &ProviderType,
) -> Result<(), AppError> {
    const PROTOCOLS: &[&str] = &["openai", "anthropic", "generation"];
    const MODALITIES: &[&str] = &["text", "embedding", "image", "video", "audio"];
    if provider.id.is_empty()
        || provider.id.len() > MAX_PLUGIN_ID_BYTES
        || !provider
            .id
            .chars()
            .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == '-')
        || provider.display_name.trim().is_empty()
        || provider.protocols.is_empty()
        || provider
            .protocols
            .iter()
            .any(|protocol| !PROTOCOLS.contains(&protocol.as_str()))
        || provider.modalities.is_empty()
        || provider
            .modalities
            .iter()
            .any(|modality| !MODALITIES.contains(&modality.as_str()))
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} contributes an invalid provider"
        )));
    }
    crate::schema::validate_definition(&provider.config_schema)?;
    crate::schema::validate_definition(&provider.credential_schema)?;
    let supported_credentials = [
        serde_json::json!({"type": "none"}),
        serde_json::json!({"type": "api_key", "value": "contract-probe"}),
        serde_json::json!({
            "type": "oauth",
            "access_token": "contract-probe",
            "refresh_token": "contract-probe",
            "expires_at": 4_102_444_800_000_i64
        }),
    ];
    if !supported_credentials.iter().any(|credential| {
        crate::schema::validate_instance(&provider.credential_schema, credential).is_ok()
    }) {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} credential schema accepts no supported core credential shape",
            provider.id
        )));
    }
    if let Some(adapter) = &provider.oauth_adapter {
        if adapter.api_version != "oauth-adapter-v1"
            || adapter.flow_kind != crate::provider::OAuthFlowKind::CursorPkce
        {
            return Err(AppError::BadRequest(format!(
                "plugin {plugin_id} provider {} contributes an unsupported OAuth adapter contract",
                provider.id
            )));
        }
        crate::schema::validate_instance(
            &provider.credential_schema,
            &supported_credentials[2],
        )
        .map_err(|_| {
            AppError::BadRequest(format!(
                "plugin {plugin_id} provider {} OAuth adapter credential schema rejects the OAuth result shape",
                provider.id
            ))
        })?;
        for (field, endpoint) in [
            ("login_url", &adapter.login_url),
            ("poll_url", &adapter.poll_url),
            ("refresh_url", &adapter.refresh_url),
        ] {
            crate::oauth::validate_oauth_endpoint(endpoint, field)?;
        }
    }
    if let Some(adapter) = &provider.managed_oauth_adapter {
        crate::provider::validate_managed_oauth_adapter_contribution(adapter).map_err(|_| {
            AppError::BadRequest(format!(
                "plugin {plugin_id} provider {} contributes an invalid managed OAuth adapter",
                provider.id
            ))
        })?;
        crate::schema::validate_instance(
            &provider.credential_schema,
            &serde_json::json!({
                "type": "oauth",
                "access_token": "contract-probe",
                "refresh_token": "contract-probe",
                "expires_at": 4_102_444_800_000_i64,
                "adapter_state": {"probe": true}
            }),
        )
        .map_err(|_| {
            AppError::BadRequest(format!(
                "plugin {plugin_id} provider {} managed OAuth credential schema rejects the adapter result shape",
                provider.id
            ))
        })?;
    }
    if let Some(adapter) = &provider.component_adapter
        && (adapter.api_version != "buffered-v1"
            || adapter.max_response_bytes == 0
            || adapter.max_response_bytes > MAX_COMPONENT_PROVIDER_BODY
            || provider
                .protocols
                .iter()
                .any(|protocol| !matches!(protocol.as_str(), "openai" | "anthropic")))
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} contributes an unsupported component provider adapter"
        )));
    }
    Ok(())
}

fn safe_child(root: &Path, child: &str) -> Result<PathBuf, AppError> {
    let child = Path::new(child);
    if child.is_absolute()
        || child.components().any(|component| {
            matches!(
                component,
                PathComponent::ParentDir | PathComponent::RootDir | PathComponent::Prefix(_)
            )
        })
    {
        return Err(AppError::BadRequest(
            "plugin wasm path must stay inside its package".into(),
        ));
    }
    let canonical_root =
        fs::canonicalize(root).map_err(|_| plugin_runtime_failure("package_resolution"))?;
    let canonical_child = fs::canonicalize(root.join(child))
        .map_err(|_| plugin_runtime_failure("component_resolution"))?;
    if !canonical_child.starts_with(&canonical_root) {
        return Err(AppError::BadRequest(
            "plugin wasm path must stay inside its package".into(),
        ));
    }
    Ok(canonical_child)
}

fn require_file_size(path: &Path, maximum: u64, kind: &str) -> Result<(), AppError> {
    let length = fs::metadata(path)
        .map_err(|_| plugin_runtime_failure("file_metadata"))?
        .len();
    if length > maximum {
        return Err(AppError::BadRequest(format!(
            "{kind} exceeds the {maximum} byte limit"
        )));
    }
    Ok(())
}

fn default_wasm_file() -> Option<String> {
    Some("plugin.wasm".to_owned())
}

fn validate_plugin_text(value: &str, maximum: usize, allow_empty: bool) -> Result<(), ()> {
    if value.len() > maximum
        || (!allow_empty && value.trim().is_empty())
        || value.chars().any(char::is_control)
    {
        return Err(());
    }
    Ok(())
}

fn validate_traffic_request_json(value: &str) -> Result<Value, ()> {
    if value.len() > MAX_TRAFFIC_REQUEST_JSON_BYTES {
        return Err(());
    }
    serde_json::from_str(value).map_err(|_| ())
}

fn invalid_plugin_result(plugin_id: &str, field: &str) -> AppError {
    AppError::Upstream(format!("plugin {plugin_id} returned an invalid {field}"))
}

fn plugin_runtime_failure(code: &'static str) -> AppError {
    // The fixed low-cardinality code is safe for logs and metrics. Never retain
    // the underlying fs/Wasmtime/HTTP error: those strings may include mount
    // paths, URLs, configuration fragments, or guest-controlled custom text.
    AppError::Storage(format!("plugin_runtime_{code}"))
}

fn plugin_reported_error(plugin_id: &str, operation: &str, error: &str) -> AppError {
    let code = if validate_plugin_text(error, MAX_TRAFFIC_REASON_BYTES, false).is_ok() {
        "reported_error"
    } else {
        "reported_invalid_error"
    };
    AppError::Upstream(format!("plugin {plugin_id} {operation} failed ({code})"))
}

fn plugin_failure(plugin_id: &str, _error: wasmtime::Error) -> AppError {
    AppError::Upstream(format!("plugin {plugin_id} execution failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn configurable_manifest(schema: Value, default: Value) -> PluginManifest {
        PluginManifest {
            id: "configured-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            wit_version: "0.2.0".to_owned(),
            wasm: Some("plugin.wasm".to_owned()),
            capabilities: Vec::new(),
            contributions: PluginContributions {
                traffic_policy: true,
                configuration: Some(PluginConfigurationContribution { schema, default }),
                ..PluginContributions::default()
            },
        }
    }

    #[test]
    fn plugin_configuration_requires_non_secret_object_schema_and_valid_default() {
        assert!(
            validate_manifest(&configurable_manifest(
                serde_json::json!({"type": "object"}),
                serde_json::json!({}),
            ))
            .is_ok()
        );
        assert!(
            validate_manifest(&configurable_manifest(
                serde_json::json!({"type": "string"}),
                serde_json::json!("value"),
            ))
            .is_err()
        );
        assert!(
            validate_manifest(&configurable_manifest(
                serde_json::json!({
                    "type": "object",
                    "properties": {"token": {"type": "string", "writeOnly": true}}
                }),
                serde_json::json!({}),
            ))
            .is_err()
        );
        assert!(
            validate_manifest(&configurable_manifest(
                serde_json::json!({
                    "type": "object",
                    "required": ["mode"],
                    "properties": {"mode": {"type": "string"}}
                }),
                serde_json::json!({}),
            ))
            .is_err()
        );
    }

    #[test]
    fn plugin_configuration_hashes_are_canonical_across_object_key_order() {
        let first = serde_json::json!({"nested": {"z": 1, "a": 2}, "mode": "safe"});
        let second: Value =
            serde_json::from_str(r#"{"mode":"safe","nested":{"a":2,"z":1}}"#).unwrap();
        assert_eq!(
            plugin_configuration_schema_digest(&first).unwrap(),
            plugin_configuration_schema_digest(&second).unwrap()
        );
        assert_eq!(
            plugin_configuration_request_hash("plugin", "global", 3, "digest", &first).unwrap(),
            plugin_configuration_request_hash("plugin", "global", 3, "digest", &second).unwrap()
        );
    }

    #[tokio::test]
    async fn plugin_configuration_cache_has_entry_and_byte_bounds() {
        let runtime = PluginRuntime::default();
        for index in 0..(PLUGIN_CONFIGURATION_CACHE_ENTRIES + 10) {
            runtime
                .cache_resolved_configurations(
                    Uuid::from_u128(index as u128 + 1),
                    BTreeMap::from([("plugin".to_owned(), Value::String("x".repeat(512 * 1024)))]),
                )
                .await;
        }
        let cache = runtime.configuration_cache.read().await;
        assert!(cache.len() <= PLUGIN_CONFIGURATION_CACHE_ENTRIES);
        assert!(
            cache
                .values()
                .map(|entry| entry.estimated_bytes)
                .sum::<usize>()
                <= PLUGIN_CONFIGURATION_CACHE_BYTES
        );
    }

    #[test]
    fn plugin_text_boundaries_reject_empty_oversize_and_control_characters() {
        assert!(validate_plugin_text("valid", 5, false).is_ok());
        assert!(validate_plugin_text("valid!", 5, false).is_err());
        assert!(validate_plugin_text("", 5, false).is_err());
        assert!(validate_plugin_text("", 5, true).is_ok());
        assert!(validate_plugin_text("line\nbreak", 32, false).is_err());
        assert!(validate_plugin_text("nul\0byte", 32, false).is_err());
        assert!(validate_plugin_text("你好", 6, false).is_ok());
        assert!(validate_plugin_text("你好", 5, false).is_err());
    }

    #[test]
    fn rejects_plugin_paths_that_escape_the_package() {
        let parent = tempfile::tempdir().unwrap();
        let package = parent.path().join("package");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("plugin.wasm"), []).unwrap();
        fs::write(parent.path().join("secret.wasm"), []).unwrap();
        assert!(safe_child(&package, "../secret.wasm").is_err());
        assert!(safe_child(&package, "/secret.wasm").is_err());
        assert_eq!(
            safe_child(&package, "plugin.wasm").expect("safe path"),
            fs::canonicalize(package.join("plugin.wasm")).unwrap()
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(
                parent.path().join("secret.wasm"),
                package.join("link.wasm"),
            )
            .unwrap();
            assert!(safe_child(&package, "link.wasm").is_err());
        }
    }

    #[test]
    fn rejects_non_origin_http_allowlist_entries() {
        let manifest = PluginManifest {
            id: "test-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            wit_version: "0.2.0".to_owned(),
            wasm: Some("plugin.wasm".to_owned()),
            capabilities: vec![PluginCapability::Http {
                allowed_origins: vec!["https://example.com/oauth/token".to_owned()],
            }],
            contributions: PluginContributions::default(),
        };

        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn rejects_private_http_plugin_capability_without_operator_approval_metadata() {
        let manifest = PluginManifest {
            id: "test-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            wit_version: "0.2.0".to_owned(),
            wasm: Some("plugin.wasm".to_owned()),
            capabilities: vec![PluginCapability::Http {
                allowed_origins: vec!["http://metadata.internal".to_owned()],
            }],
            contributions: PluginContributions::default(),
        };

        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn component_provider_request_boundaries_reject_origin_and_secret_header_smuggling() {
        assert!(validate_provider_path("/vendor/infer?mode=one").is_ok());
        for path in [
            "https://metadata.invalid/token",
            "//metadata.invalid/token",
            "/safe#fragment",
            "/safe\nforged",
        ] {
            assert!(validate_provider_path(path).is_err(), "{path:?}");
        }
        assert!(validate_provider_method("POST").is_ok());
        assert!(validate_provider_method("CONNECT").is_err());
        for name in [
            "authorization",
            "cookie",
            "x-api-key",
            "host",
            "content-length",
            "transfer-encoding",
        ] {
            assert!(
                validate_provider_headers(
                    &BTreeMap::from([(name.to_owned(), "smuggled".to_owned())]),
                    true,
                )
                .is_err(),
                "{name}"
            );
        }
        assert!(
            validate_provider_headers(
                &BTreeMap::from([("x-vendor-version".into(), "2026-08".into())]),
                true,
            )
            .is_ok()
        );
    }

    #[test]
    fn plugin_http_methods_are_an_explicit_allowlist() {
        for method in ["GET", "HEAD", "POST", "PUT", "PATCH", "DELETE"] {
            assert!(validate_plugin_http_method(method).is_ok(), "{method}");
        }
        for method in ["CONNECT", "TRACE", "OPTIONS", "CUSTOM", "get"] {
            assert!(validate_plugin_http_method(method).is_err(), "{method}");
        }
    }

    #[test]
    fn plugin_http_headers_reject_authority_hop_proxy_and_method_smuggling() {
        for name in [
            "Host",
            "Content-Length",
            "Connection",
            "Keep-Alive",
            "Proxy-Authenticate",
            "Proxy-Authorization",
            "Proxy-Connection",
            "Proxy-Custom",
            "Proxy",
            "X-Proxy-Custom",
            "TE",
            "Trailer",
            "Transfer-Encoding",
            "Upgrade",
            "HTTP2-Settings",
            "Forwarded",
            "Via",
            "Max-Forwards",
            "X-Forwarded-Host",
            "X-Forwarded-For",
            "X-Real-IP",
            "X-Original-URL",
            "X-Rewrite-URL",
            "X-HTTP-Method",
            "X-HTTP-Method-Override",
            "X-Method-Override",
        ] {
            let encoded =
                serde_json::to_string(&BTreeMap::from([(name.to_owned(), "smuggled".to_owned())]))
                    .unwrap();
            assert!(validate_plugin_http_headers(&encoded).is_err(), "{name}");
        }

        let duplicate = r#"{"X-Vendor-Version":"one","x-vendor-version":"two"}"#;
        assert!(validate_plugin_http_headers(duplicate).is_err());
    }

    #[test]
    fn plugin_http_headers_allow_auth_and_enforce_every_size_boundary() {
        let authentication = serde_json::to_string(&BTreeMap::from([
            ("Authorization".to_owned(), "Bearer plugin-token".to_owned()),
            ("X-Api-Key".to_owned(), "vendor-key".to_owned()),
        ]))
        .unwrap();
        let headers =
            validate_plugin_http_headers(&authentication).expect("authentication headers");
        assert_eq!(headers.len(), 2);

        let at_count_limit = (0..PLUGIN_HTTP_HEADER_COUNT)
            .map(|index| (format!("x-test-{index}"), String::new()))
            .collect::<BTreeMap<_, _>>();
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&at_count_limit).unwrap()).is_ok()
        );
        let over_count_limit = (0..=PLUGIN_HTTP_HEADER_COUNT)
            .map(|index| (format!("x-test-{index}"), String::new()))
            .collect::<BTreeMap<_, _>>();
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&over_count_limit).unwrap())
                .is_err()
        );

        let at_name_limit =
            BTreeMap::from([("x".repeat(PLUGIN_HTTP_HEADER_NAME_BYTES), String::new())]);
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&at_name_limit).unwrap()).is_ok()
        );
        let over_name_limit =
            BTreeMap::from([("x".repeat(PLUGIN_HTTP_HEADER_NAME_BYTES + 1), String::new())]);
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&over_name_limit).unwrap())
                .is_err()
        );

        let at_value_limit = BTreeMap::from([(
            "x-test".to_owned(),
            "a".repeat(PLUGIN_HTTP_HEADER_VALUE_BYTES),
        )]);
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&at_value_limit).unwrap()).is_ok()
        );
        let over_value_limit = BTreeMap::from([(
            "x-test".to_owned(),
            "a".repeat(PLUGIN_HTTP_HEADER_VALUE_BYTES + 1),
        )]);
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&over_value_limit).unwrap())
                .is_err()
        );

        let at_total_limit = BTreeMap::from([
            ("x-a".to_owned(), "a".repeat(8_189)),
            ("x-b".to_owned(), "b".repeat(8_189)),
        ]);
        assert_eq!(
            at_total_limit
                .iter()
                .map(|(name, value)| name.len() + value.len())
                .sum::<usize>(),
            PLUGIN_HTTP_HEADER_TOTAL_BYTES
        );
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&at_total_limit).unwrap()).is_ok()
        );
        let mut over_total_limit = at_total_limit;
        over_total_limit
            .get_mut("x-b")
            .expect("second header")
            .push('b');
        assert!(
            validate_plugin_http_headers(&serde_json::to_string(&over_total_limit).unwrap())
                .is_err()
        );

        assert!(
            validate_plugin_http_headers(&" ".repeat(PLUGIN_HTTP_HEADERS_JSON_BYTES + 1)).is_err()
        );
    }

    #[tokio::test]
    async fn malicious_plugin_request_is_rejected_before_dns_access() {
        let mut state = HostState {
            plugin_id: "malicious-plugin".to_owned(),
            capabilities: vec![PluginCapability::Http {
                allowed_origins: vec!["https://does-not-resolve.invalid".to_owned()],
            }],
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            runtime: tokio::runtime::Handle::current(),
            kv: None,
            limits: StoreLimitsBuilder::new().build(),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        let error = memeloop::token_center::host::Host::http_request(
            &mut state,
            "POST".to_owned(),
            "https://does-not-resolve.invalid/token".to_owned(),
            r#"{"Host":"metadata.internal"}"#.to_owned(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("forbidden header"), "{error}");
    }

    #[test]
    fn manifest_only_oauth_provider_does_not_require_wasm() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "example-oauth",
            "version": "1.0.0",
            "wit_version": "0.2.0",
            "wasm": null,
            "contributions": {
                "providers": [{
                    "id": "example-provider",
                    "display_name": "Example provider",
                    "protocols": ["openai"],
                    "modalities": ["text"],
                    "config_schema": {"type": "object"},
                    "credential_schema": {"type": "object"},
                    "oauth_adapter": {
                        "api_version": "oauth-adapter-v1",
                        "flow_kind": "cursor_pkce",
                        "login_url": "http://example-oauth.default.svc/login",
                        "poll_url": "http://example-oauth.default.svc/poll",
                        "refresh_url": "http://example-oauth.default.svc/refresh"
                    }
                }]
            }
        }))
        .unwrap();
        assert!(validate_manifest(&manifest).is_ok());

        let mut invalid = manifest;
        invalid.contributions.traffic_policy = true;
        assert!(validate_manifest(&invalid).is_err());
    }

    fn managed_oauth_manifest() -> PluginManifest {
        serde_json::from_value(serde_json::json!({
            "id": "managed-oauth",
            "version": "1.0.0",
            "wit_version": "0.2.0",
            "wasm": null,
            "contributions": {"providers": [{
                "id": "managed-provider",
                "display_name": "Managed provider",
                "protocols": ["openai"],
                "modalities": ["text"],
                "config_schema": {"type": "object"},
                "credential_schema": {"type": "object"},
                "managed_oauth_adapter": {
                    "api_version": "cpa-managed-oauth-adapter-v1",
                    "source_types": ["codex-account", "gemini-account"],
                    "normalize_url": "http://managed-oauth.default.svc/normalize",
                    "refresh_url": "http://managed-oauth.default.svc/refresh"
                }
            }]}
        }))
        .unwrap()
    }

    #[test]
    fn managed_oauth_contribution_validates_version_sources_and_ssrf_boundaries() {
        let valid = managed_oauth_manifest();
        assert!(validate_manifest(&valid).is_ok());

        let mut invalid_version = valid.clone();
        invalid_version.contributions.providers[0]
            .managed_oauth_adapter
            .as_mut()
            .unwrap()
            .api_version = "cpa-managed-oauth-adapter-v2".into();
        assert!(validate_manifest(&invalid_version).is_err());

        let mut duplicate_source = valid.clone();
        duplicate_source.contributions.providers[0]
            .managed_oauth_adapter
            .as_mut()
            .unwrap()
            .source_types = vec!["codex-account".into(), "codex-account".into()];
        assert!(validate_manifest(&duplicate_source).is_err());

        let mut illegal_source = valid.clone();
        illegal_source.contributions.providers[0]
            .managed_oauth_adapter
            .as_mut()
            .unwrap()
            .source_types = vec!["Codex/account".into()];
        assert!(validate_manifest(&illegal_source).is_err());

        for endpoint in [
            "ftp://adapter.example/normalize",
            "http://example.com/normalize",
            "https://user:password@example.com/normalize",
            "http://127.0.0.1:3000/normalize",
            "https://169.254.169.254/latest/meta-data",
            "https://adapter.example/normalize?target=http://metadata.internal",
            "https://adapter.example/normalize#fragment",
        ] {
            let mut invalid_endpoint = valid.clone();
            invalid_endpoint.contributions.providers[0]
                .managed_oauth_adapter
                .as_mut()
                .unwrap()
                .normalize_url = endpoint.into();
            assert!(
                validate_manifest(&invalid_endpoint).is_err(),
                "accepted {endpoint}"
            );
        }
    }

    #[test]
    fn executable_provider_requires_component_supported_version_and_buffered_protocol() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "component-provider",
            "version": "1.0.0",
            "wit_version": "0.2.0",
            "wasm": null,
            "contributions": {"providers": [{
                "id": "component-http",
                "display_name": "Component HTTP",
                "protocols": ["openai"],
                "modalities": ["text"],
                "config_schema": {"type": "object"},
                "credential_schema": {"type": "object"},
                "component_adapter": {
                    "api_version": "buffered-v1",
                    "max_response_bytes": 1024
                }
            }]}
        }))
        .unwrap();
        assert!(validate_manifest(&manifest).is_err());

        let mut with_component = manifest;
        with_component.wasm = Some("plugin.wasm".into());
        assert!(validate_manifest(&with_component).is_ok());
        with_component.contributions.providers[0].protocols = vec!["generation".into()];
        assert!(validate_manifest(&with_component).is_err());
    }

    #[tokio::test]
    async fn config_map_style_root_manifest_is_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("plugin.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        fs::write(
            directory.path().join("plugin.json"),
            serde_json::to_vec(&serde_json::json!({
                "id": "config-map-provider",
                "version": "1.0.0",
                "wit_version": "0.2.0",
                "wasm": null,
                "contributions": {
                    "providers": [{
                        "id": "config-map-http",
                        "display_name": "ConfigMap HTTP",
                        "protocols": ["openai"],
                        "modalities": ["text"],
                        "config_schema": {"type": "object"},
                        "credential_schema": {"type": "object"}
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        fs::create_dir(directory.path().join("..data")).unwrap();
        fs::copy(
            directory.path().join("plugin.json"),
            directory.path().join("..data/plugin.json"),
        )
        .unwrap();

        let runtime = PluginRuntime::load(directory.path().to_str(), database).unwrap();
        assert_eq!(runtime.manifests().len(), 1);
        assert_eq!(runtime.provider_types()[0].id, "config-map-http");
        assert_eq!(
            runtime.provider_types()[0].source,
            "plugin:config-map-provider@1.0.0"
        );
        drop(runtime);
    }

    #[tokio::test]
    async fn undeclared_kv_and_http_capabilities_have_no_effect() {
        let mut state = HostState {
            plugin_id: "least-privilege-plugin".to_owned(),
            capabilities: Vec::new(),
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap(),
            runtime: tokio::runtime::Handle::current(),
            kv: None,
            limits: StoreLimitsBuilder::new().build(),
            deadline: Instant::now() + Duration::from_secs(1),
        };
        assert!(
            memeloop::token_center::host::Host::kv_get(&mut state, "secret".to_owned()).is_err()
        );
        assert!(
            memeloop::token_center::host::Host::http_request(
                &mut state,
                "GET".to_owned(),
                "https://example.com/".to_owned(),
                "{}".to_owned(),
                Vec::new(),
            )
            .is_err()
        );

        state.capabilities = vec![PluginCapability::Http {
            allowed_origins: vec!["https://allowed.example".to_owned()],
        }];
        let error = memeloop::token_center::host::Host::http_request(
            &mut state,
            "GET".to_owned(),
            "https://blocked.example/path".to_owned(),
            "{}".to_owned(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("not allowed"));
    }
}
