use std::{
    borrow::Cow,
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

#[cfg(feature = "experimental-plugin-revisions")]
pub mod application;
pub(crate) mod background_actions;
mod configuration;
/// Experimental lifecycle primitives; application integration is separately
/// host-opted-in through `application` and never enabled by the production binary.
#[cfg(feature = "experimental-plugin-revisions")]
pub mod lifecycle;
pub mod routing;
pub(crate) mod service_data;
mod ui_projection;
pub use configuration::{
    ConfigurationSource, ResolvedConfigurationRevision, ResolvedTrafficSnapshot,
};

const PLUGIN_FUEL: u64 = u64::MAX;
const PLUGIN_TABLE_ELEMENTS: usize = 100_000;
const PLUGIN_HTTP_HEADER_COUNT: usize = 64;
const PLUGIN_HTTP_HEADER_NAME_BYTES: usize = 256;
const PLUGIN_HTTP_HEADER_VALUE_BYTES: usize = 8 * 1024;
const PLUGIN_HTTP_HEADER_TOTAL_BYTES: usize = 16 * 1024;
const PLUGIN_HTTP_HEADERS_JSON_BYTES: usize = 128 * 1024;
const PLUGIN_MANIFEST_BYTES: u64 = 1024 * 1024;
const PLUGIN_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const PLUGIN_UI_MODULE_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_COMPONENT_PROVIDER_BODY: usize = 4 * 1024 * 1024;
const MAX_PLUGIN_ID_BYTES: usize = 64;
const MAX_TRAFFIC_REASON_BYTES: usize = 256;
const MAX_TRAFFIC_MODEL_BYTES: usize = 200;
const MAX_TRAFFIC_ACCOUNT_ID_BYTES: usize = 64;
const PLUGIN_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const PLUGIN_EPOCH_TICK: Duration = Duration::from_millis(10);
const PLUGIN_CONFIGURATION_CACHE_TTL: Duration = Duration::from_secs(5);
const PLUGIN_CONFIGURATION_CACHE_ENTRIES: usize = 64;
const PLUGIN_CONFIGURATION_CACHE_BYTES: usize = 16 * 1024 * 1024;
const MAX_PLUGIN_SERVICE_DATA_TIMEOUT_MILLIS: u64 = 30_000;
const MAX_PLUGIN_SERVICE_DATA_BODY_BYTES: usize = 1024 * 1024;
const MAX_PLUGIN_SERVICE_DATA_ENDPOINTS: usize = 32;
const PLUGIN_SERVICE_DATA_COMPONENT_API: &str = "component-v1";
const SUPPORTED_WIT_REQUIREMENT: &str = ">=0.2.0, <0.4.0";
// These plugin_kv namespaces contain core-owned policy or its immutable
// receipt. Guest components must never acquire them through their manifest ID.
const CORE_PLUGIN_KV_NAMESPACES: &[&str] = &[
    "typed-filter",
    "filter-assistant-audit",
    "system-settings",
    "http-settings-audit",
];
const CORE_OPERATOR_ROUTES: &[&str] = &[
    "overview",
    "requests",
    "sessions",
    "usage",
    "generations",
    "providers",
    "routes",
    "pricing",
    "tenants",
    "credentials",
    "service-credentials",
    "plugins",
    "system-settings",
];
const CORE_OPERATOR_CATEGORIES: &[&str] = &["monitoring", "traffic", "identity", "system"];
const PLUGIN_DATA_SCOPES: &[&str] = &[
    "credits:read",
    "entitlements:read",
    "keys:read",
    "metrics:read",
    "plugins:read",
    "prices:read",
    "providers:read",
    "requests:read",
    "routes:read",
    "schemas:read",
    "service_tokens:read",
    "tenants:read",
];

wasmtime::component::bindgen!({
    world: "plugin",
    path: "wit/token-center.wit",
});

mod service_data_component {
    wasmtime::component::bindgen!({
        world: "service-data-plugin",
        path: "wit/token-center.wit",
        with: {
            "memeloop:token-center/host@0.2.0": super::memeloop::token_center::host,
        },
    });
}

mod wire_shim_component {
    wasmtime::component::bindgen!({
        world: "memeloop:token-center/wire-shim-plugin@0.3.0",
        path: ["wit/token-center.wit", "wit/wire-shim.wit"],
    });
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_routing: Option<routing::GroupRoutingContribution>,
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
    /// Operator tabs and cards. `typed_data_v1` uses a core projection;
    /// `component_v1` loads a digest-addressed module captured from the active
    /// installed plugin package through the versioned UI SDK.
    #[serde(default)]
    pub operator_ui: Vec<PluginOperatorUiContribution>,
    /// Named, server-side JSON feeds used by `operator_ui`. The browser can
    /// only call the core proxy route for one of these manifest entries.
    #[serde(default)]
    pub service_data: Vec<PluginServiceDataEndpoint>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub background_actions: Option<background_actions::BackgroundActionContribution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wire_shim: Option<WireShimContribution>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireShimContribution {
    pub drivers: Vec<String>,
    pub protocols: Vec<String>,
}

pub(crate) struct WireShimOutcome {
    pub(crate) request_json: String,
}

fn validate_wire_shim_output(set_headers: &[(String, String)]) -> Result<(), AppError> {
    if !set_headers.is_empty() {
        return Err(AppError::upstream_with(
            "upstream_unavailable",
            crate::error::ErrorFacts::new().stage("plugin_validate_wire_shim_output"),
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginOperatorUiSlot {
    #[serde(rename = "operator.sidebar.tab")]
    SidebarTab,
    #[serde(rename = "operator.overview.card")]
    OverviewCard,
    #[serde(rename = "operator.page.before")]
    PageBefore,
    #[serde(rename = "operator.page.after")]
    PageAfter,
    #[serde(rename = "operator.model-route.action")]
    ModelRouteAction,
}

/// Closed, core-owned visual presentation choices for declarative operator
/// contributions. A presentation selects only a renderer already compiled
/// into Token Center; it is never a URL, a component name, or plugin code.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PluginOperatorUiPresentation {
    HealthIntelligenceV1,
    ProjectionV1,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginOperatorUiCategory {
    pub id: String,
    /// Required for a category that is not owned by the core. Core categories
    /// use their localized product labels instead of plugin-supplied strings.
    #[serde(default)]
    pub label: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginOperatorUiContribution {
    pub id: String,
    pub slot: PluginOperatorUiSlot,
    #[serde(default)]
    pub category: Option<PluginOperatorUiCategory>,
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub target_route: Option<String>,
    pub label: String,
    pub icon: String,
    /// `typed_data_v1` selects a core projection. `component_v1` selects a
    /// component exported by the signed package's runtime UI module.
    pub renderer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_entry: Option<String>,
    /// Populated from the exact module bytes captured by the active runtime.
    /// Source manifests cannot provide this field because the JSON schema
    /// rejects it; clients use it to build an immutable same-origin URL.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub module_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_props: Option<Value>,
    /// An optional closed presentation selected by the core. Omitting this
    /// keeps the generic typed-data presentation for backwards compatibility.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presentation: Option<PluginOperatorUiPresentation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_endpoint: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginServiceDataEndpoint {
    pub id: String,
    /// A complete, manifest-audited HTTPS URL. Requests are GET-only, carry
    /// no caller credentials, and are DNS-pinned by `network::client_for_url`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Optional executable collector/normalizer. The worker invokes this
    /// adapter outside every inference and operator request path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component_adapter: Option<PluginServiceDataComponentAdapter>,
    pub required_scope: String,
    pub response_schema: Value,
    #[serde(default = "empty_json_object")]
    pub fallback: Value,
    #[serde(default = "default_plugin_data_cache_ttl_seconds")]
    pub cache_ttl_seconds: u64,
    #[serde(default = "default_plugin_data_timeout_millis")]
    pub timeout_millis: u64,
    #[serde(default = "default_plugin_data_max_body_bytes")]
    pub max_body_bytes: usize,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginServiceDataComponentAdapter {
    pub api_version: String,
    pub collector: String,
    pub normalizer: String,
    #[serde(default = "empty_json_object")]
    pub config: Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginServiceDataView {
    pub data: Value,
    /// `true` means a stale cached result or manifest fallback was returned
    /// after a bounded upstream failure. The endpoint remains a valid typed
    /// JSON response so local renderers can degrade without unsafe content.
    pub partial: bool,
    pub provenance: PluginServiceDataProvenance,
}

#[derive(Clone, Debug, Serialize)]
pub struct PluginServiceDataProvenance {
    pub plugin_id: String,
    pub endpoint_id: String,
    pub origin: String,
    /// Unix epoch milliseconds for the last successful collection. A value of
    /// zero preserves the published V1 contract when no collection has
    /// succeeded; `freshness` is authoritative for availability.
    pub fetched_at: i64,
    pub source: String,
    pub freshness: String,
    pub last_attempt_at: Option<i64>,
    pub next_attempt_at: Option<i64>,
    pub consecutive_failures: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

fn default_plugin_data_cache_ttl_seconds() -> u64 {
    30
}

fn default_plugin_data_timeout_millis() -> u64 {
    2_000
}

fn default_plugin_data_max_body_bytes() -> usize {
    64 * 1024
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

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PluginCapability {
    Log,
    Kv,
    Http {
        allowed_origins: Vec<String>,
    },
    /// Read-only candidate quota context for native-health group ordering.
    GroupRoutingQuota,
    /// Account-scoped reset-credit intents, authorized separately by an operator.
    QuotaResetCredit,
}

#[derive(Clone)]
struct LoadedPlugin {
    manifest: PluginManifest,
    component: Option<Component>,
    service_data_component: Option<service_data_component::ServiceDataPluginPre<HostState>>,
    background_action_component:
        Option<background_actions::bindings::BackgroundActionsPluginPre<HostState>>,
    wire_shim_component: Option<wire_shim_component::WireShimPluginPre<HostState>>,
    ui_modules: BTreeMap<String, Arc<[u8]>>,
    configuration_validator: Option<crate::schema::CompiledSchema>,
    routing_validator: Option<crate::schema::CompiledSchema>,
    routing_fingerprint: String,
    #[cfg(feature = "experimental-plugin-revisions")]
    identity: PluginPackageIdentity,
}

#[cfg(feature = "experimental-plugin-revisions")]
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginInstallProvenance {
    pub format_version: u8,
    pub source: String,
    pub digest: String,
    pub signature_policy: String,
}

/// Exact GitHub Actions signing identity. Installation callers must obtain
/// host policy or explicit operator confirmation before treating it as trust.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CosignKeylessIdentity {
    pub issuer: String,
    pub identity: String,
}

impl CosignKeylessIdentity {
    pub fn valid(&self) -> bool {
        self.issuer == "https://token.actions.githubusercontent.com"
            && self.identity.starts_with("https://github.com/")
            && self.identity.contains("/.github/workflows/")
            && (self.identity.contains("@refs/heads/") || self.identity.contains("@refs/tags/"))
            && self.identity.len() <= 512
            && !self
                .identity
                .bytes()
                .any(|b| b.is_ascii_control() || b.is_ascii_whitespace())
            && !self.identity.contains(['*', '?', '[', ']'])
    }
}

#[cfg(feature = "experimental-plugin-revisions")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PluginPackageIdentity {
    pub component_sha256: Option<String>,
    pub provenance: Option<PluginInstallProvenance>,
}

#[derive(Clone)]
struct CachedPluginConfigurations {
    loaded_at: Instant,
    snapshot: ResolvedTrafficSnapshot,
    estimated_bytes: usize,
}

#[derive(Default)]
struct ConfigurationCache {
    // An identity token cannot wrap/reuse a counter while an old read holds it.
    epoch: Arc<()>,
    read_generation: u64,
    // Fixed-size completion fences survive value eviction and uncached reads.
    // Hash collisions conservatively fence older reads for another tenant.
    published_generations: [u64; 32],
    entries: BTreeMap<Uuid, CachedPluginConfigurations>,
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
    /// Project the model the proxy would dispatch at this chain position. An
    /// explicit model decision remains authoritative across later body rewrites.
    /// Keep the JSON presented to the next guest consistent with its context,
    /// without changing the original authentication identity or returned metadata.
    fn project_context<'request>(
        &self,
        original: &types::RequestContext,
        request: &'request Value,
    ) -> (types::RequestContext, Cow<'request, Value>) {
        let model = self
            .model
            .as_deref()
            .or_else(|| {
                request
                    .get("model")
                    .and_then(Value::as_str)
                    .filter(|model| {
                        !model.trim().is_empty() && model.len() <= MAX_TRAFFIC_MODEL_BYTES
                    })
            })
            .unwrap_or(&original.model)
            .to_owned();
        // Preserve non-object rewrites so the proxy still rejects them instead
        // of accidentally turning malformed guest output into a valid request.
        let mut request = Cow::Borrowed(request);
        if request.is_object()
            && request.get("model").and_then(Value::as_str) != Some(model.as_str())
        {
            request.to_mut()["model"] = Value::String(model.clone());
        }
        let mut context = original.clone();
        context.model = model;
        (context, request)
    }

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
    http: Option<crate::http_settings::HttpClient>,
    runtime: Option<tokio::runtime::Handle>,
    kv: Option<PluginKv>,
    plugins: Arc<Vec<LoadedPlugin>>,
    providers: Arc<Vec<ProviderType>>,
    configuration_cache: Arc<tokio::sync::RwLock<ConfigurationCache>>,
    execution_timeout: Duration,
    fuel: u64,
    #[cfg(feature = "experimental-plugin-revisions")]
    _epoch_task: Option<Arc<EpochTask>>,
}

#[cfg(feature = "experimental-plugin-revisions")]
struct EpochTask(tokio::task::JoinHandle<()>);

#[cfg(feature = "experimental-plugin-revisions")]
impl Drop for EpochTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[derive(Clone)]
struct PluginKv {
    database: Database,
}

struct HostState {
    plugin_id: String,
    capabilities: Vec<PluginCapability>,
    http: crate::http_settings::HttpClient,
    runtime: tokio::runtime::Handle,
    kv: Option<PluginKv>,
    limits: StoreLimits,
    deadline: Instant,
    http_body_limit: Option<usize>,
    http_diagnostic: PluginHttpDiagnostic,
}

#[derive(Default)]
struct PluginHttpDiagnostic {
    origin_slot: Option<usize>,
    robots: bool,
    status: Option<u16>,
    stage: &'static str,
    failure_code: Option<&'static str>,
}

fn read_identity_bytes(path: &Path, maximum: u64) -> Result<Vec<u8>, AppError> {
    use std::io::Read;
    let file = fs::File::open(path).map_err(|_| plugin_runtime_failure("package_read"))?;
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| plugin_runtime_failure("package_read"))?;
    if bytes.len() as u64 > maximum {
        return Err(AppError::BadRequest(
            "plugin file exceeds size limit".into(),
        ));
    }
    Ok(bytes)
}

impl PluginRuntime {
    /// Host `http-request` is the outbound hook a plugin calls. No loaded
    /// package registers a replacement for the core Codex send, so that path
    /// keeps using the built-in client.
    pub(crate) fn codex_outbound_registered(&self) -> bool {
        false
    }

    pub(crate) fn wire_shim_matches(&self, driver: &str, protocol: &str) -> bool {
        self.plugins.iter().any(|plugin| {
            plugin
                .manifest
                .contributions
                .wire_shim
                .as_ref()
                .is_some_and(|shim| {
                    shim.drivers.iter().any(|value| value == driver)
                        && shim.protocols.iter().any(|value| value == protocol)
                })
        })
    }

    pub(crate) fn finalize_wire_shim(
        &self,
        driver: &str,
        protocol: &str,
        mut context: types::RequestContext,
        request_json: &str,
        headers_json: &str,
        configurations: &BTreeMap<String, Value>,
    ) -> Result<Option<WireShimOutcome>, AppError> {
        let Some(plugin) = self.plugins.iter().find(|plugin| {
            plugin
                .manifest
                .contributions
                .wire_shim
                .as_ref()
                .is_some_and(|shim| {
                    shim.drivers.iter().any(|value| value == driver)
                        && shim.protocols.iter().any(|value| value == protocol)
                })
        }) else {
            return Ok(None);
        };
        let pre = plugin
            .wire_shim_component
            .as_ref()
            .ok_or(AppError::internal("plugin_finalize_wire_shim_1"))?;
        let engine = self
            .engine
            .as_ref()
            .ok_or(AppError::internal("plugin_finalize_wire_shim_2"))?;
        let limits = StoreLimitsBuilder::new()
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
                http: self
                    .http
                    .as_ref()
                    .ok_or(AppError::internal("plugin_finalize_wire_shim_3"))?
                    .clone(),
                runtime: self
                    .runtime
                    .as_ref()
                    .ok_or(AppError::internal("plugin_finalize_wire_shim_4"))?
                    .clone(),
                kv: self.kv.clone(),
                limits,
                deadline: Instant::now() + self.execution_timeout,
                http_body_limit: None,
                http_diagnostic: PluginHttpDiagnostic::default(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(self.execution_timeout));
        store
            .set_fuel(self.fuel)
            .map_err(|_| plugin_runtime_failure("fuel_configuration"))?;
        context.config_json = serde_json::to_string(traffic_configuration(plugin, configurations))
            .map_err(|_| AppError::internal("plugin_finalize_wire_shim_1"))?;
        let context = wire_shim_component::memeloop::token_center0_3_0::types::RequestContext {
            tenant_id: context.tenant_id,
            principal_id: context.principal_id,
            key_id: context.key_id,
            protocol: context.protocol,
            model: context.model,
            config_json: context.config_json,
        };
        let bindings = pre
            .instantiate(&mut store)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
        let result = bindings
            .memeloop_token_center0_3_0_wire_shim_v1()
            .call_finalize(&mut store, &context, request_json, headers_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| {
                plugin_reported_error(&plugin.manifest.id, "wire-shim-finalize", &error)
            })?;
        validate_wire_shim_output(&result.set_headers)?;
        Ok(Some(WireShimOutcome {
            request_json: result.request_json,
        }))
    }

    pub fn load(root: Option<&str>, database: Database) -> Result<Self, AppError> {
        Self::load_with_database(root, Some(database))
    }

    #[cfg(all(
        feature = "experimental-plugin-revisions",
        feature = "plugin-distribution"
    ))]
    fn load_for_inventory(root: &str) -> Result<Self, AppError> {
        match fs::symlink_metadata(Path::new(root).join("plugin.json")) {
            Ok(_) => return Err(AppError::Forbidden),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => return Err(AppError::internal("plugin_load_for_inventory_1")),
        }
        Self::load_with_database(Some(root), None)
    }

    fn load_with_database(
        root: Option<&str>,
        database: Option<Database>,
    ) -> Result<Self, AppError> {
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
        let epoch_task = tokio::spawn(async move {
            let mut interval = tokio::time::interval(PLUGIN_EPOCH_TICK);
            loop {
                interval.tick().await;
                epoch_engine.increment_epoch();
            }
        });
        #[cfg(feature = "experimental-plugin-revisions")]
        let epoch_task = Arc::new(EpochTask(epoch_task));
        #[cfg(not(feature = "experimental-plugin-revisions"))]
        drop(epoch_task); // Preserve the default loader's detached epoch timer.
        let http = database
            .as_ref()
            .map(|database| database.http_clients.clone())
            .unwrap_or_default()
            .handle(crate::http_settings::HttpPurpose::Plugin);
        let canonical_root =
            fs::canonicalize(root).map_err(|_| plugin_runtime_failure("directory_resolution"))?;
        let directories = plugin_directories(&canonical_root)?;

        let mut plugins = Vec::new();
        let mut providers = Vec::new();
        for directory in directories {
            let mut manifest = validate_plugin_package(&directory)?;
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
            let ui_modules = load_operator_ui_modules(&directory, &mut manifest)?;
            let (component, component_sha256) = {
                let component_bytes = manifest
                    .wasm
                    .as_deref()
                    .map(|wasm| {
                        let wasm_path = safe_child(&directory, wasm)?;
                        require_file_size(&wasm_path, PLUGIN_COMPONENT_BYTES, "plugin component")?;
                        read_identity_bytes(&wasm_path, PLUGIN_COMPONENT_BYTES)
                    })
                    .transpose()?;
                use sha2::{Digest, Sha256};
                let component_sha256 = component_bytes
                    .as_ref()
                    .map(|bytes| format!("sha256:{:x}", Sha256::digest(bytes)));
                // Compile exactly the bytes that were hashed, never reopen a path.
                let component = component_bytes
                    .as_ref()
                    .map(|bytes| {
                        Component::new(&engine, bytes).map_err(|_| {
                            AppError::BadRequest("plugin component cannot be compiled".into())
                        })
                    })
                    .transpose()?;
                (component, component_sha256)
            };
            let routing_fingerprint = plugin_configuration_schema_digest(&serde_json::json!({
                "manifest": manifest, "component_sha256": component_sha256,
            }))?;
            let service_data_component = if manifest
                .contributions
                .service_data
                .iter()
                .any(|endpoint| endpoint.component_adapter.is_some())
            {
                let component = component.as_ref().ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "plugin {} needs a component for service data",
                        manifest.id
                    ))
                })?;
                let mut linker = Linker::new(&engine);
                service_data_component::ServiceDataPlugin::add_to_linker::<_, HasSelf<_>>(
                    &mut linker,
                    |state| state,
                )
                .map_err(|_| plugin_runtime_failure("service_data_linker_configuration"))?;
                let instance_pre = linker.instantiate_pre(component).map_err(|_| {
                    AppError::BadRequest(format!(
                        "plugin {} service data component has an incompatible ABI",
                        manifest.id
                    ))
                })?;
                Some(
                    service_data_component::ServiceDataPluginPre::new(instance_pre).map_err(
                        |_| {
                            AppError::BadRequest(format!(
                                "plugin {} service data component has an incompatible ABI",
                                manifest.id
                            ))
                        },
                    )?,
                )
            } else {
                None
            };
            let background_action_component =
                background_actions::prepare_component(&engine, &manifest, component.as_ref())?;
            let wire_shim_component = if manifest.contributions.wire_shim.is_some() {
                let component = component
                    .as_ref()
                    .ok_or(AppError::internal("plugin_site_1"))?;
                let mut linker = Linker::<HostState>::new(&engine);
                register_wire_shim_host_imports(&mut linker)
                    .map_err(|_| plugin_runtime_failure("wire_shim_linker_configuration"))?;
                let instance_pre = linker.instantiate_pre(component).map_err(|_| {
                    AppError::BadRequest(format!(
                        "plugin {} wire shim imports or ABI are incompatible",
                        manifest.id
                    ))
                })?;
                Some(
                    wire_shim_component::WireShimPluginPre::new(instance_pre).map_err(|_| {
                        AppError::BadRequest(format!(
                            "plugin {} wire shim ABI is incompatible",
                            manifest.id
                        ))
                    })?,
                )
            } else {
                None
            };
            #[cfg(feature = "experimental-plugin-revisions")]
            let identity = {
                let receipt_path = directory.join(".mtc-oci-install.json");
                let provenance = if receipt_path.exists() {
                    let path = safe_child(&directory, ".mtc-oci-install.json")?;
                    Some(
                        serde_json::from_slice(&read_identity_bytes(&path, 16 * 1024)?).map_err(
                            |_| AppError::BadRequest("invalid plugin install receipt".into()),
                        )?,
                    )
                } else {
                    None
                };
                PluginPackageIdentity {
                    component_sha256,
                    provenance,
                }
            };
            let configuration_validator = manifest
                .contributions
                .configuration
                .as_ref()
                .map(|configuration| crate::schema::compile(&configuration.schema))
                .transpose()?;
            let routing_validator = manifest
                .contributions
                .group_routing
                .as_ref()
                .map(|routing| crate::schema::compile(&routing.schema))
                .transpose()?;
            plugins.push(LoadedPlugin {
                manifest,
                component,
                service_data_component,
                background_action_component,
                wire_shim_component,
                ui_modules,
                configuration_validator,
                routing_validator,
                routing_fingerprint,
                #[cfg(feature = "experimental-plugin-revisions")]
                identity,
            });
        }
        validate_loaded_operator_ui_contributions(&plugins)?;
        // Manifest ordering belongs only to the explicit revision contract.
        // Default builds retain the existing sorted-directory precedence.
        #[cfg(feature = "experimental-plugin-revisions")]
        plugins.sort_by(|left, right| left.manifest.id.cmp(&right.manifest.id));

        Ok(Self {
            engine: Some(engine),
            http: Some(http),
            runtime: Some(tokio::runtime::Handle::current()),
            kv: database.map(|database| PluginKv { database }),
            plugins: Arc::new(plugins),
            providers: Arc::new(providers),
            configuration_cache: Arc::default(),
            execution_timeout: PLUGIN_EXECUTION_TIMEOUT,
            fuel: PLUGIN_FUEL,
            #[cfg(feature = "experimental-plugin-revisions")]
            _epoch_task: Some(epoch_task),
        })
    }

    pub fn provider_types(&self) -> Vec<ProviderType> {
        self.providers.as_ref().clone()
    }

    /// Cached manifest + actual compiled-byte identity for durable group work.
    /// Available in default builds too; never reopens packages per request.
    pub fn group_routing_fingerprints(&self) -> BTreeMap<String, String> {
        self.plugins
            .iter()
            .filter(|plugin| plugin.manifest.contributions.group_routing.is_some())
            .map(|plugin| {
                (
                    plugin.manifest.id.clone(),
                    plugin.routing_fingerprint.clone(),
                )
            })
            .collect()
    }

    pub fn manifests(&self) -> Vec<PluginManifest> {
        self.plugins
            .iter()
            .map(|plugin| plugin.manifest.clone())
            .collect()
    }

    pub(crate) fn operator_ui_module(
        &self,
        plugin_id: &str,
        version: &str,
        sha256: &str,
        entry: &str,
    ) -> Option<Arc<[u8]>> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id && plugin.manifest.version == version)?;
        plugin
            .manifest
            .contributions
            .operator_ui
            .iter()
            .any(|contribution| {
                contribution.renderer == "component_v1"
                    && contribution.module_entry.as_deref() == Some(entry)
                    && contribution.module_sha256.as_deref() == Some(sha256)
            })
            .then_some(())?;
        plugin.ui_modules.get(entry).cloned()
    }

    #[cfg(feature = "experimental-plugin-revisions")]
    pub fn package_identities(&self) -> BTreeMap<String, PluginPackageIdentity> {
        self.plugins
            .iter()
            .map(|plugin| (plugin.manifest.id.clone(), plugin.identity.clone()))
            .collect()
    }

    /// A bounded, aggregate-only snapshot for operational metrics. Tenant and
    /// plugin identifiers never leave the cache through this interface.
    pub async fn runtime_metrics(&self) -> PluginRuntimeMetrics {
        let configuration_cache = self.configuration_cache.read().await;
        PluginRuntimeMetrics {
            loaded_plugins: self.plugins.len(),
            cache_entries: configuration_cache.entries.len(),
            cache_bytes: configuration_cache
                .entries
                .values()
                .fold(0usize, |total, entry| {
                    total.saturating_add(entry.estimated_bytes)
                }),
        }
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
        Ok(self.resolved_traffic_snapshot(tenant_id).await?.values)
    }

    pub async fn invalidate_configuration_cache(&self, tenant_id: Option<Uuid>) {
        let mut cache = self.configuration_cache.write().await;
        // Publish the fence and remove entries under the same lock as refill.
        // A tenant write also fences other in-flight reads, conservatively.
        cache.epoch = Arc::new(());
        if let Some(tenant_id) = tenant_id {
            cache.entries.remove(&tenant_id);
        } else {
            // A global write may affect every tenant without an override.
            cache.entries.clear();
        }
    }

    pub async fn validate_stored_configurations(&self) -> Result<(), AppError> {
        if self.plugins.is_empty() {
            return Ok(());
        }
        let database = &self
            .kv
            .as_ref()
            .ok_or(AppError::internal(
                "plugin_validate_stored_configurations_1",
            ))?
            .database;
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
                    .ok_or(AppError::internal(
                        "plugin_validate_stored_configurations_2",
                    ))?
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
        self.apply_traffic_with_config_and_memory(context, request_json, configurations, None)
    }

    pub(crate) fn has_traffic_hooks(&self) -> bool {
        self.engine.is_some()
            && self.plugins.iter().any(|plugin| {
                plugin.manifest.contributions.traffic_policy
                    || plugin.manifest.contributions.request_rewrite
            })
    }

    pub(crate) fn apply_traffic_with_config_and_memory(
        &self,
        context: types::RequestContext,
        request_json: &Value,
        configurations: &BTreeMap<String, Value>,
        memory: Option<&crate::gateway_body::memory::ProxyMemoryReservation>,
    ) -> Result<TrafficDecision, AppError> {
        let Some(engine) = &self.engine else {
            return Ok(TrafficDecision {
                allow: true,
                ..TrafficDecision::default()
            });
        };
        let http = self.http.as_ref().ok_or(AppError::internal(
            "plugin_apply_traffic_with_config_and_memory_1",
        ))?;
        let runtime = self.runtime.as_ref().ok_or(AppError::internal(
            "plugin_apply_traffic_with_config_and_memory_2",
        ))?;
        let mut decision = TrafficDecision {
            allow: true,
            ..TrafficDecision::default()
        };
        for plugin in self.plugins.iter().filter(|plugin| {
            plugin.manifest.contributions.traffic_policy
                || plugin.manifest.contributions.request_rewrite
        }) {
            let (mut plugin_context, current) = decision.project_context(
                &context,
                decision.request_json.as_ref().unwrap_or(request_json),
            );
            let request = serde_json::to_string(current.as_ref())
                .map_err(|_| AppError::internal("plugin_apply_traffic_with_config_and_memory_1"))?;
            drop(current);
            plugin_context.config_json =
                serde_json::to_string(traffic_configuration(plugin, configurations)).map_err(
                    |_| AppError::internal("plugin_apply_traffic_with_config_and_memory_2"),
                )?;
            let component = plugin.component.as_ref().ok_or_else(|| {
                AppError::storage("plugin_contributes_a_traffic_policy_without_a_component")
            })?;
            let limits = StoreLimitsBuilder::new()
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
                    http_body_limit: None,
                    http_diagnostic: PluginHttpDiagnostic::default(),
                },
            );
            store.limiter(|state| &mut state.limits);
            store.set_hostcall_fuel(usize::MAX);
            store.set_epoch_deadline(epoch_deadline_ticks(self.execution_timeout));
            store
                .set_fuel(self.fuel)
                .map_err(|_| plugin_runtime_failure("fuel_configuration"))?;
            let mut linker = Linker::new(engine);
            Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(|_| plugin_runtime_failure("linker_configuration"))?;
            let bindings = Plugin::instantiate(&mut store, component, &linker)
                .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
            let result = bindings
                .memeloop_token_center_traffic_policy()
                .call_post_auth(&mut store, &plugin_context, &request)
                .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
                .map_err(|error| {
                    plugin_reported_error(&plugin.manifest.id, "traffic-policy", &error)
                })?;
            drop(request);
            drop(store);
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
            if let (Some(memory), Some(rewrite)) = (memory, result.request_json.as_deref())
                && !memory.try_reserve_rewrite(rewrite.as_bytes())
            {
                return Err(AppError::capacity_error("proxy_memory_plugin"));
            }
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
                decision.request_json = Some(request);
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
        let Some((plugin, provider)) = self.plugins.iter().find_map(|plugin| {
            plugin
                .manifest
                .contributions
                .providers
                .iter()
                .find(|provider| provider.id == provider_id)
                .map(|provider| (plugin, provider))
        }) else {
            return Ok(None);
        };
        if provider.component_adapter.is_none() {
            return Ok(None);
        }
        let Some(component) = plugin.component.as_ref() else {
            return Ok(None);
        };
        let engine = self
            .engine
            .as_ref()
            .ok_or(AppError::internal("plugin_list_provider_models_1"))?;
        let http = self
            .http
            .as_ref()
            .ok_or(AppError::internal("plugin_list_provider_models_2"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or(AppError::internal("plugin_list_provider_models_3"))?;
        let limits = StoreLimitsBuilder::new()
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
                http_body_limit: None,
                http_diagnostic: PluginHttpDiagnostic::default(),
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
        let config_json = serde_json::to_string(config)
            .map_err(|_| AppError::internal("plugin_list_provider_models_1"))?;
        let models_json = bindings
            .memeloop_token_center_upstream_provider()
            .call_list_models(&mut store, &config_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| plugin_reported_error(&plugin.manifest.id, "list-models", &error))?;
        let models: Value = serde_json::from_str(&models_json)
            .map_err(|_| AppError::upstream("plugin_returned_invalid_models_json"))?;
        if !models.is_array() {
            return Err(AppError::upstream(
                "plugin_models_response_must_be_a_json_array",
            ));
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
        let component = plugin
            .component
            .as_ref()
            .ok_or_else(|| AppError::storage("plugin_component_provider_is_unavailable"))?;
        let engine = self
            .engine
            .as_ref()
            .ok_or(AppError::internal("plugin_prepare_provider_request_1"))?;
        let (mut store, bindings) = self.instantiate(plugin, component, engine)?;
        let config_json = serde_json::to_string(config)
            .map_err(|_| AppError::internal("plugin_prepare_provider_request_1"))?;
        let request_json = serde_json::to_string(request)
            .map_err(|_| AppError::internal("plugin_prepare_provider_request_2"))?;
        if config_json.len().saturating_add(request_json.len()) > MAX_COMPONENT_PROVIDER_BODY {
            return Err(AppError::upstream_with(
                "plugin_prepared_request_exceeds_the_4_mib_abi_limit",
                crate::error::ErrorFacts::new()
                    .stage("plugin_prepare_provider_request")
                    .bytes(config_json.len().saturating_add(request_json.len()) as u64)
                    .limit(MAX_COMPONENT_PROVIDER_BODY as u64),
            ));
        }
        let result = bindings
            .memeloop_token_center_upstream_provider()
            .call_prepare(&mut store, &context, &config_json, &request_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| plugin_reported_error(&plugin.manifest.id, "prepare", &error))?;
        if result.len() > encoded_body_limit(MAX_COMPONENT_PROVIDER_BODY) {
            return Err(AppError::upstream_with(
                "plugin_prepared_request_exceeds_the_4_mib_abi_limit",
                crate::error::ErrorFacts::new()
                    .bytes(result.len() as u64)
                    .limit(encoded_body_limit(MAX_COMPONENT_PROVIDER_BODY) as u64),
            ));
        }
        let envelope: PreparedProviderEnvelope = serde_json::from_str(&result)
            .map_err(|_| AppError::upstream("plugin_returned_an_invalid_prepared_request"))?;
        if envelope.streaming {
            return Err(AppError::upstream(
                "plugin_requested_unsupported_streaming_transport",
            ));
        }
        let method = validate_provider_method(&envelope.method)?;
        validate_provider_path(&envelope.path)?;
        validate_provider_headers(&envelope.headers, true)?;
        let body = STANDARD
            .decode(envelope.body_base64)
            .map_err(|_| AppError::upstream("plugin_returned_invalid_request_body_encoding"))?;
        if body.len() > MAX_COMPONENT_PROVIDER_BODY {
            return Err(AppError::upstream_with(
                "plugin_prepared_request_body_exceeds_4_mib",
                crate::error::ErrorFacts::new()
                    .bytes(body.len() as u64)
                    .limit(MAX_COMPONENT_PROVIDER_BODY as u64),
            ));
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
            .ok_or(AppError::internal("plugin_normalize_provider_response_1"))?;
        if upstream_body.len() > adapter.max_response_bytes
            || upstream_body.len() > MAX_COMPONENT_PROVIDER_BODY
        {
            return Err(AppError::upstream_with(
                "plugin_upstream_response_exceeds_its_declared_limit",
                crate::error::ErrorFacts::new()
                    .bytes(upstream_body.len() as u64)
                    .limit(adapter.max_response_bytes.min(MAX_COMPONENT_PROVIDER_BODY) as u64),
            ));
        }
        validate_provider_headers(upstream_headers, false)?;
        let response_json = serde_json::to_string(&UpstreamResponseEnvelope {
            status: upstream_status,
            headers: upstream_headers,
            body_base64: STANDARD.encode(upstream_body),
        })
        .map_err(|_| AppError::internal("plugin_normalize_provider_response_1"))?;
        if response_json.len() > encoded_body_limit(adapter.max_response_bytes) {
            return Err(AppError::upstream_with(
                "plugin_upstream_response_exceeds_its_declared_abi_limit",
                crate::error::ErrorFacts::new()
                    .bytes(response_json.len() as u64)
                    .limit(encoded_body_limit(adapter.max_response_bytes) as u64),
            ));
        }
        let component = plugin
            .component
            .as_ref()
            .ok_or_else(|| AppError::storage("plugin_component_provider_is_unavailable"))?;
        let engine = self
            .engine
            .as_ref()
            .ok_or(AppError::internal("plugin_normalize_provider_response_2"))?;
        let (mut store, bindings) = self.instantiate(plugin, component, engine)?;
        let result = bindings
            .memeloop_token_center_upstream_provider()
            .call_normalize(&mut store, &context, &response_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| plugin_reported_error(&plugin.manifest.id, "normalize", &error))?;
        if result.len() > encoded_body_limit(adapter.max_response_bytes) {
            return Err(AppError::upstream_with(
                "plugin_normalized_response_exceeds_its_declared_limit",
                crate::error::ErrorFacts::new()
                    .bytes(result.len() as u64)
                    .limit(encoded_body_limit(adapter.max_response_bytes) as u64),
            ));
        }
        let envelope: NormalizedProviderEnvelope = serde_json::from_str(&result)
            .map_err(|_| AppError::upstream("plugin_returned_an_invalid_normalized_response"))?;
        if !(200..=599).contains(&envelope.status) {
            return Err(AppError::upstream_status(
                "plugin_returned_an_invalid_downstream_status",
                envelope.status,
            ));
        }
        validate_provider_headers(&envelope.headers, false)?;
        let body = STANDARD
            .decode(envelope.body_base64)
            .map_err(|_| AppError::upstream("plugin_returned_invalid_normalized_body_encoding"))?;
        if body.len() > adapter.max_response_bytes || body.len() > MAX_COMPONENT_PROVIDER_BODY {
            return Err(AppError::upstream_with(
                "plugin_normalized_response_body_exceeds_its_declared_limit",
                crate::error::ErrorFacts::new()
                    .bytes(body.len() as u64)
                    .limit(adapter.max_response_bytes.min(MAX_COMPONENT_PROVIDER_BODY) as u64),
            ));
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
            .ok_or(AppError::internal("plugin_component_provider_1"))?;
        if provider.component_adapter.is_none() {
            return Ok(None);
        }
        if plugin.component.is_none() {
            return Err(AppError::storage(
                "plugin_declared_a_component_provider_without_a_component",
            ));
        }
        Ok(Some((plugin, provider)))
    }

    fn instantiate(
        &self,
        plugin: &LoadedPlugin,
        component: &Component,
        engine: &Engine,
    ) -> Result<(Store<HostState>, Plugin), AppError> {
        let http = self
            .http
            .as_ref()
            .ok_or(AppError::internal("plugin_instantiate_1"))?;
        let runtime = self
            .runtime
            .as_ref()
            .ok_or(AppError::internal("plugin_instantiate_2"))?;
        let limits = StoreLimitsBuilder::new()
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
                http_body_limit: None,
                http_diagnostic: PluginHttpDiagnostic::default(),
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

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PluginRuntimeMetrics {
    pub loaded_plugins: usize,
    pub cache_entries: usize,
    pub cache_bytes: usize,
}

fn encoded_body_limit(body_limit: usize) -> usize {
    body_limit
        .saturating_mul(4)
        .div_ceil(3)
        .saturating_add(64 * 1024)
}

fn validate_provider_method(value: &str) -> Result<reqwest::Method, AppError> {
    let method = reqwest::Method::from_bytes(value.as_bytes())
        .map_err(|_| AppError::upstream("component_provider_returned_an_invalid_method"))?;
    if !matches!(
        method,
        reqwest::Method::GET
            | reqwest::Method::POST
            | reqwest::Method::PUT
            | reqwest::Method::PATCH
            | reqwest::Method::DELETE
    ) {
        return Err(AppError::upstream_with(
            "upstream_unavailable",
            crate::error::ErrorFacts::new().stage("plugin_validate_provider_method"),
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
        return Err(AppError::upstream_with(
            "upstream_unavailable",
            crate::error::ErrorFacts::new().stage("plugin_validate_provider_path"),
        ));
    }
    Ok(())
}

fn validate_provider_headers(
    headers: &BTreeMap<String, String>,
    reject_credentials: bool,
) -> Result<(), AppError> {
    if headers.len() > 64 {
        return Err(AppError::upstream_with(
            "upstream_unavailable",
            crate::error::ErrorFacts::new().stage("plugin_validate_provider_headers"),
        ));
    }
    let mut total = 0_usize;
    for (name, value) in headers {
        let parsed_name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_| AppError::upstream("component_provider_header_name_is_invalid"))?;
        reqwest::header::HeaderValue::from_str(value)
            .map_err(|_| AppError::upstream("component_provider_header_value_is_invalid"))?;
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
            return Err(AppError::upstream_with(
                "upstream_unavailable",
                crate::error::ErrorFacts::new().stage("plugin_validate_provider_headers"),
            ));
        }
    }
    if total > 16 * 1024 {
        return Err(AppError::upstream_with(
            "upstream_unavailable",
            crate::error::ErrorFacts::new().stage("plugin_validate_provider_headers"),
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
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "plugin execution deadline exceeded".to_owned())?;
        self.runtime.block_on(async {
            tokio::time::timeout(remaining, kv.database.plugin_kv_get(&self.plugin_id, &key))
                .await
                .map_err(|_| "plugin KV get timed out".to_owned())?
                .map_err(|_| "plugin KV get failed".to_owned())
        })
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
        let remaining = self
            .deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "plugin execution deadline exceeded".to_owned())?;
        self.runtime.block_on(async {
            tokio::time::timeout(
                remaining,
                kv.database.plugin_kv_put(&self.plugin_id, &key, &value),
            )
            .await
            .map_err(|_| "plugin KV put timed out".to_owned())?
            .map_err(|_| "plugin KV put failed".to_owned())
        })
    }

    fn http_request(
        &mut self,
        method: String,
        url: String,
        headers_json: String,
        body: Vec<u8>,
    ) -> Result<Vec<u8>, String> {
        self.http_diagnostic = PluginHttpDiagnostic {
            stage: "request_validation",
            ..PluginHttpDiagnostic::default()
        };
        if self.http_body_limit.is_some_and(|limit| body.len() > limit) {
            return Err("plugin HTTP request exceeds its configured body limit".to_owned());
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
        self.http_diagnostic.origin_slot = self
            .capabilities
            .iter()
            .filter_map(|capability| match capability {
                PluginCapability::Http { allowed_origins } => Some(allowed_origins),
                _ => None,
            })
            .flatten()
            .position(|allowed| allowed == &origin);
        self.http_diagnostic.robots = url.path() == "/robots.txt";
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
        self.http_diagnostic.stage = "destination_validation";
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
        let runtime = self.runtime.clone();
        let http_body_limit = self.http_body_limit;
        self.http_diagnostic.stage = "request_send";
        let diagnostic = &mut self.http_diagnostic;
        let (status, response_headers, response_body) = runtime.block_on(async move {
            let response = request.send().await.map_err(|error| {
                diagnostic.failure_code = Some(if error.is_timeout() {
                    "timeout"
                } else if error.is_connect() {
                    "connect"
                } else {
                    "request"
                });
                "plugin HTTP request failed".to_owned()
            })?;
            let status = response.status().as_u16();
            diagnostic.status = Some(status);
            diagnostic.stage = "response_read";
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
                if http_body_limit
                    .is_some_and(|limit| response_body.len().saturating_add(chunk.len()) > limit)
                {
                    return Err("plugin HTTP response exceeds its configured body limit".to_owned());
                }
                response_body.extend_from_slice(&chunk);
            }
            Ok::<_, String>((status, response_headers, response_body))
        })?;
        self.http_diagnostic.stage = "response_received";
        serde_json::to_vec(&serde_json::json!({
            "status": status,
            "headers": response_headers,
            "body_base64": STANDARD.encode(response_body)
        }))
        .map_err(|_| "plugin HTTP response encoding failed".to_owned())
    }
}

impl memeloop::token_center::types::Host for HostState {}

fn register_wire_shim_host_imports(linker: &mut Linker<HostState>) -> Result<(), wasmtime::Error> {
    let mut host = linker.instance("memeloop:token-center/host@0.3.0")?;
    host.func_wrap(
        "log",
        |_caller: wasmtime::StoreContextMut<'_, HostState>,
         (_level, _message): (String, String)| {
            Err::<(), wasmtime::Error>(wasmtime::Error::msg(
                "wire shim host logging is unavailable",
            ))
        },
    )?;
    host.func_wrap(
        "kv-get",
        |_caller: wasmtime::StoreContextMut<'_, HostState>, (_key,): (String,)| {
            Ok((Err::<Option<Vec<u8>>, String>(
                "wire shim KV is unavailable".into(),
            ),))
        },
    )?;
    host.func_wrap(
        "kv-put",
        |_caller: wasmtime::StoreContextMut<'_, HostState>, (_key, _value): (String, Vec<u8>)| {
            Ok((Err::<(), String>("wire shim KV is unavailable".into()),))
        },
    )?;
    host.func_wrap(
        "http-request",
        |_caller: wasmtime::StoreContextMut<'_, HostState>,
         (_method, _url, _headers_json, _body): (String, String, String, Vec<u8>)| {
            Ok((Err::<Vec<u8>, String>(
                "wire shim HTTP is unavailable".into(),
            ),))
        },
    )?;
    host.func_wrap(
        "random-bytes",
        |_caller: wasmtime::StoreContextMut<'_, HostState>, (_len,): (u32,)| {
            Ok((Err::<Vec<u8>, String>(
                "plugin random capability is unavailable".into(),
            ),))
        },
    )?;
    Ok(())
}

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
            .map_err(|_| AppError::internal("plugin_validate_plugin_package_1"))?;
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
    for entry in manifest
        .contributions
        .operator_ui
        .iter()
        .filter_map(|contribution| contribution.module_entry.as_deref())
    {
        let module_path = safe_child(directory, entry)?;
        require_file_size(
            &module_path,
            PLUGIN_UI_MODULE_BYTES,
            "plugin operator UI module",
        )?;
    }
    Ok(manifest)
}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), AppError> {
    routing::validate_contribution(manifest)?;
    background_actions::validate_contribution(manifest)?;
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
    if CORE_PLUGIN_KV_NAMESPACES.contains(&manifest.id.as_str()) {
        return Err(AppError::BadRequest(format!(
            "plugin id {} is reserved for core storage",
            manifest.id
        )));
    }
    if semver::Version::parse(&manifest.version).is_err()
        || !semver::VersionReq::parse(SUPPORTED_WIT_REQUIREMENT)
            .map_err(|_| AppError::internal("plugin_validate_manifest_1"))?
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
    if let Some(wire_shim) = &manifest.contributions.wire_shim {
        if manifest.wit_version != "0.3.0"
            || manifest.wasm.is_none()
            || !manifest.capabilities.is_empty()
            || wire_shim.drivers.is_empty()
            || wire_shim.drivers.len() > 32
            || wire_shim.protocols.is_empty()
            || wire_shim
                .drivers
                .iter()
                .any(|driver| !safe_plugin_token(driver, MAX_PLUGIN_ID_BYTES))
            || wire_shim
                .protocols
                .iter()
                .any(|protocol| !matches!(protocol.as_str(), "anthropic" | "openai"))
            || wire_shim.drivers.iter().collect::<BTreeSet<_>>().len() != wire_shim.drivers.len()
            || wire_shim.protocols.iter().collect::<BTreeSet<_>>().len()
                != wire_shim.protocols.len()
        {
            return Err(AppError::BadRequest(format!(
                "plugin {} has an invalid privacy wire shim contribution",
                manifest.id
            )));
        }
    } else if manifest.wit_version.starts_with("0.3.") {
        return Err(AppError::BadRequest(format!(
            "plugin {} needs a wire shim contribution for WIT 0.3",
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
    validate_operator_ui_contributions(manifest)?;
    validate_service_data_contributions(manifest)?;
    Ok(())
}

fn validate_operator_ui_contributions(manifest: &PluginManifest) -> Result<(), AppError> {
    let contributions = &manifest.contributions.operator_ui;
    let endpoints = &manifest.contributions.service_data;
    let mut ids = BTreeSet::new();
    let mut routes = BTreeSet::new();
    for contribution in contributions {
        if !safe_plugin_token(&contribution.id, 64) || !ids.insert(&contribution.id) {
            return Err(AppError::BadRequest(format!(
                "plugin {} has an invalid or duplicate operator UI contribution id",
                manifest.id
            )));
        }
        if !safe_plugin_label(&contribution.label) {
            return Err(AppError::BadRequest(format!(
                "plugin {} operator UI contribution has an invalid label",
                manifest.id
            )));
        }
        match contribution.renderer.as_str() {
            "typed_data_v1" => {
                if contribution.component_id.is_some()
                    || contribution.component_props.is_some()
                    || contribution.module_entry.is_some()
                    || contribution.data_endpoint.is_none()
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} typed-data contribution has an invalid renderer contract",
                        manifest.id
                    )));
                }
            }
            "component_v1" => {
                if !contribution
                    .component_id
                    .as_deref()
                    .is_some_and(|value| safe_plugin_token(value, 64))
                    || !contribution
                        .module_entry
                        .as_deref()
                        .is_some_and(safe_operator_ui_module_entry)
                    || contribution.presentation.is_some()
                    || contribution
                        .component_props
                        .as_ref()
                        .is_some_and(|value| !value.is_object())
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} component contribution has an invalid component contract",
                        manifest.id
                    )));
                }
            }
            _ => {
                return Err(AppError::BadRequest(format!(
                    "plugin {} operator UI contribution has an unsupported renderer",
                    manifest.id
                )));
            }
        }
        if !matches!(
            contribution.icon.as_str(),
            "activity" | "chart" | "database" | "heart" | "plug" | "shield"
        ) {
            return Err(AppError::BadRequest(format!(
                "plugin {} operator UI contribution has an unsupported icon token",
                manifest.id
            )));
        }
        if let Some(data_endpoint) = contribution.data_endpoint.as_deref()
            && !endpoints
                .iter()
                .any(|endpoint| endpoint.id == data_endpoint)
        {
            return Err(AppError::BadRequest(format!(
                "plugin {} operator UI contribution references an unknown data endpoint",
                manifest.id
            )));
        }
        match contribution.slot {
            PluginOperatorUiSlot::SidebarTab => {
                if contribution.target_route.is_some() {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} sidebar contribution cannot declare a target route",
                        manifest.id
                    )));
                }
                let route = contribution.route.as_deref().ok_or_else(|| {
                    AppError::BadRequest(format!(
                        "plugin {} sidebar contribution needs a route",
                        manifest.id
                    ))
                })?;
                if !safe_plugin_token(route, 64)
                    || CORE_OPERATOR_ROUTES.contains(&route)
                    || !routes.insert(route)
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} sidebar contribution has a reserved or duplicate route",
                        manifest.id
                    )));
                }
                validate_operator_category(&manifest.id, contribution.category.as_ref())?;
            }
            PluginOperatorUiSlot::OverviewCard => {
                if contribution.route.is_some()
                    || contribution.category.is_some()
                    || contribution.target_route.is_some()
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} overview-card contribution cannot declare a route or category",
                        manifest.id
                    )));
                }
            }
            PluginOperatorUiSlot::ModelRouteAction => {
                if contribution.renderer != "component_v1"
                    || contribution.route.is_some()
                    || contribution.category.is_some()
                    || contribution.target_route.is_some()
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} model-route action requires a component without route or category",
                        manifest.id
                    )));
                }
            }
            PluginOperatorUiSlot::PageBefore | PluginOperatorUiSlot::PageAfter => {
                if contribution.route.is_some()
                    || contribution.category.is_some()
                    || !contribution
                        .target_route
                        .as_deref()
                        .is_some_and(|route| CORE_OPERATOR_ROUTES.contains(&route))
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} page contribution needs a core target route",
                        manifest.id
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_service_data_contributions(manifest: &PluginManifest) -> Result<(), AppError> {
    if manifest.contributions.service_data.len() > MAX_PLUGIN_SERVICE_DATA_ENDPOINTS {
        return Err(AppError::BadRequest(format!(
            "plugin {} declares too many service data endpoints",
            manifest.id
        )));
    }
    let mut ids = BTreeSet::new();
    for endpoint in &manifest.contributions.service_data {
        if !safe_plugin_token(&endpoint.id, 64) || !ids.insert(&endpoint.id) {
            return Err(AppError::BadRequest(format!(
                "plugin {} has an invalid or duplicate service data endpoint id",
                manifest.id
            )));
        }
        if !PLUGIN_DATA_SCOPES.contains(&endpoint.required_scope.as_str()) {
            return Err(AppError::BadRequest(format!(
                "plugin {} service data endpoint requires an unsupported read scope",
                manifest.id
            )));
        }
        match (&endpoint.url, &endpoint.component_adapter) {
            (Some(url), None) => validate_service_data_url(manifest, url)?,
            (None, Some(adapter)) => {
                if manifest.wasm.is_none()
                    || adapter.api_version != PLUGIN_SERVICE_DATA_COMPONENT_API
                    || !safe_plugin_token(&adapter.collector, 64)
                    || !safe_plugin_token(&adapter.normalizer, 64)
                    || !adapter.config.is_object()
                    || serde_json::to_vec(&adapter.config)
                        .map_err(|_| {
                            AppError::internal("plugin_validate_service_data_contributions_1")
                        })?
                        .len()
                        > MAX_PLUGIN_SERVICE_DATA_BODY_BYTES
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} service data component adapter is invalid",
                        manifest.id
                    )));
                }
            }
            _ => {
                return Err(AppError::BadRequest(format!(
                    "plugin {} service data endpoint must declare exactly one collector",
                    manifest.id
                )));
            }
        }
        if endpoint.cache_ttl_seconds == 0
            || endpoint.cache_ttl_seconds > 3_600
            || endpoint.timeout_millis == 0
            || endpoint.timeout_millis > MAX_PLUGIN_SERVICE_DATA_TIMEOUT_MILLIS
            || endpoint.max_body_bytes == 0
            || endpoint.max_body_bytes > MAX_PLUGIN_SERVICE_DATA_BODY_BYTES
        {
            return Err(AppError::BadRequest(format!(
                "plugin {} service data endpoint exceeds core fetch limits",
                manifest.id
            )));
        }
        if endpoint.response_schema.get("type").and_then(Value::as_str) != Some("object")
            || schema_contains_write_only(&endpoint.response_schema)
        {
            return Err(AppError::BadRequest(format!(
                "plugin {} service data endpoint needs a non-secret object response schema",
                manifest.id
            )));
        }
        crate::schema::validate_definition(&endpoint.response_schema)?;
        crate::schema::validate_instance(&endpoint.response_schema, &endpoint.fallback)?;
    }
    Ok(())
}

fn validate_service_data_url(manifest: &PluginManifest, url: &str) -> Result<(), AppError> {
    let parsed = url::Url::parse(url).map_err(|_| {
        AppError::BadRequest(format!(
            "plugin {} service data endpoint has an invalid URL",
            manifest.id
        ))
    })?;
    if url.len() > 2_048
        || parsed.scheme() != "https"
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.fragment().is_some()
        || parsed.query().is_some()
        || parsed.port() == Some(0)
    {
        return Err(AppError::BadRequest(format!(
            "plugin {} service data endpoint must use a bounded HTTPS URL",
            manifest.id
        )));
    }
    let origin = parsed.origin().ascii_serialization();
    let origin_allowed = manifest.capabilities.iter().any(|capability| {
        matches!(capability, PluginCapability::Http { allowed_origins } if allowed_origins.contains(&origin))
    });
    if !origin_allowed {
        return Err(AppError::BadRequest(format!(
            "plugin {} service data endpoint origin is not declared by the HTTP capability",
            manifest.id
        )));
    }
    Ok(())
}

fn validate_operator_category(
    plugin_id: &str,
    category: Option<&PluginOperatorUiCategory>,
) -> Result<(), AppError> {
    let category = category.ok_or_else(|| {
        AppError::BadRequest(format!(
            "plugin {plugin_id} sidebar contribution needs a category"
        ))
    })?;
    if !safe_plugin_token(&category.id, 64) {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} sidebar contribution has an invalid category"
        )));
    }
    if CORE_OPERATOR_CATEGORIES.contains(&category.id.as_str()) {
        if category.label.is_some() {
            return Err(AppError::BadRequest(format!(
                "plugin {plugin_id} cannot override a core operator category label"
            )));
        }
    } else if !category.label.as_deref().is_some_and(safe_plugin_label) {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} new operator category needs a safe label"
        )));
    }
    Ok(())
}

fn validate_loaded_operator_ui_contributions(plugins: &[LoadedPlugin]) -> Result<(), AppError> {
    let mut categories: BTreeMap<&str, &str> = BTreeMap::new();
    for plugin in plugins {
        for contribution in &plugin.manifest.contributions.operator_ui {
            let Some(category) = contribution.category.as_ref() else {
                continue;
            };
            if CORE_OPERATOR_CATEGORIES.contains(&category.id.as_str()) {
                continue;
            }
            let label = category
                .label
                .as_deref()
                .expect("validated new category label");
            if let Some(existing_label) = categories.get(category.id.as_str())
                && *existing_label != label
            {
                return Err(AppError::BadRequest(
                    "conflicting plugin operator category across installed plugins".into(),
                ));
            }
            categories.insert(category.id.as_str(), label);
        }
    }
    Ok(())
}

fn load_operator_ui_modules(
    directory: &Path,
    manifest: &mut PluginManifest,
) -> Result<BTreeMap<String, Arc<[u8]>>, AppError> {
    use sha2::{Digest, Sha256};

    let mut modules: BTreeMap<String, Arc<[u8]>> = BTreeMap::new();
    for contribution in &mut manifest.contributions.operator_ui {
        let Some(entry) = contribution.module_entry.as_deref() else {
            continue;
        };
        let bytes = if let Some(bytes) = modules.get(entry) {
            bytes.clone()
        } else {
            let path = safe_child(directory, entry)?;
            require_file_size(&path, PLUGIN_UI_MODULE_BYTES, "plugin operator UI module")?;
            let bytes: Arc<[u8]> = read_identity_bytes(&path, PLUGIN_UI_MODULE_BYTES)?.into();
            modules.insert(entry.to_owned(), bytes.clone());
            bytes
        };
        contribution.module_sha256 = Some(format!("sha256:{:x}", Sha256::digest(bytes.as_ref())));
    }
    Ok(modules)
}

pub(crate) fn safe_operator_ui_module_entry(value: &str) -> bool {
    let path = Path::new(value);
    if value.is_empty()
        || value.len() > 240
        || (!value.ends_with(".js") && !value.ends_with(".mjs"))
        || path.is_absolute()
    {
        return false;
    }
    value.split('/').all(|segment| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }) && path
        .components()
        .all(|component| matches!(component, PathComponent::Normal(_)))
}

pub(crate) fn safe_plugin_token(value: &str, maximum: usize) -> bool {
    !value.is_empty()
        && value.len() <= maximum
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn safe_plugin_label(value: &str) -> bool {
    !value.trim().is_empty()
        && value.len() <= 120
        && !value.chars().any(char::is_control)
        && !value.contains(['<', '>'])
}

pub fn plugin_configuration_schema_digest(schema: &Value) -> Result<String, AppError> {
    let canonical = canonical_json(schema);
    let bytes = serde_json::to_vec(&canonical)
        .map_err(|_| AppError::internal("plugin_plugin_configuration_schema_digest_1"))?;
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
    let bytes = serde_json::to_vec(&payload)
        .map_err(|_| AppError::internal("plugin_plugin_configuration_request_hash_1"))?;
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
        // Every input byte can expand to at most one six-byte JSON escape.
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
    const PROTOCOLS: &[&str] = &["openai", "anthropic", "openai-audio", "generation"];
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
    if provider.request_compatibility.codex_multi_agent_v2
        && !provider.request_compatibility.third_party
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare third_party for Codex MultiAgentV2 compatibility",
            provider.id
        )));
    }
    if provider.request_compatibility.codex_multi_agent_v2
        && !provider.request_compatibility.responses_via_chat_v1
        && !provider
            .request_compatibility
            .responses_via_anthropic_messages_v1
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare a versioned Responses adapter for Codex MultiAgentV2 compatibility",
            provider.id
        )));
    }
    if provider.codex_model_capabilities.is_some()
        && !provider.request_compatibility.codex_multi_agent_v2
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare codex_multi_agent_v2 for Codex model capabilities",
            provider.id
        )));
    }
    if provider.request_compatibility.responses_via_chat_v1
        && !provider.request_compatibility.third_party
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare third_party for Responses-via-Chat compatibility",
            provider.id
        )));
    }
    if provider.request_compatibility.responses_via_chat_v1
        && provider
            .request_compatibility
            .responses_via_chat_dialect
            .is_none()
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare a Responses-via-Chat dialect",
            provider.id
        )));
    }
    if provider
        .request_compatibility
        .responses_via_chat_dialect
        .is_some()
        && !provider.request_compatibility.responses_via_chat_v1
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} declares a Responses-via-Chat dialect without responses_via_chat_v1",
            provider.id
        )));
    }
    if provider
        .request_compatibility
        .responses_transport_configurable
        && (!provider.request_compatibility.third_party
            || !provider.request_compatibility.responses_via_chat_v1)
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare third_party Responses-via-Chat compatibility before account-selectable transport",
            provider.id
        )));
    }
    if provider
        .request_compatibility
        .responses_via_anthropic_messages_v1
        && !provider.request_compatibility.third_party
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare third_party for Responses-via-Anthropic compatibility",
            provider.id
        )));
    }
    if provider
        .request_compatibility
        .responses_transport_configurable
        && !config_schema_declares_responses_transport(&provider.config_schema)
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare the closed responses_transport configuration",
            provider.id
        )));
    }
    if provider
        .request_compatibility
        .responses_via_anthropic_messages_v1
        && (!provider
            .protocols
            .iter()
            .any(|protocol| protocol == "openai")
            || !provider
                .protocols
                .iter()
                .any(|protocol| protocol == "anthropic"))
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must declare openai and anthropic protocols for Responses-via-Anthropic compatibility",
            provider.id
        )));
    }
    if provider
        .request_compatibility
        .responses_via_anthropic_messages_v1
        && provider.request_compatibility.responses_via_chat_v1
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} must select one Responses translation transport",
            provider.id
        )));
    }
    if provider.request_compatibility.responses_via_chat_dialect
        == Some(crate::provider::ResponsesViaChatDialect::OpenAiChatV1)
        && provider
            .codex_model_capabilities
            .as_ref()
            .is_some_and(|capabilities| {
                !capabilities.supported_reasoning_levels.is_empty()
                    || capabilities.default_reasoning_level.is_some()
            })
    {
        return Err(AppError::BadRequest(format!(
            "plugin {plugin_id} provider {} cannot advertise reasoning levels with the strict OpenAI Chat dialect",
            provider.id
        )));
    }
    crate::schema::validate_definition(&provider.config_schema)?;
    provider
        .request_compatibility
        .anthropic_messages_headers
        .validate()?;
    if !provider
        .request_compatibility
        .anthropic_messages_headers
        .is_empty()
        && !provider
            .protocols
            .iter()
            .any(|protocol| protocol == "anthropic")
        && !provider
            .request_compatibility
            .responses_via_anthropic_messages_v1
    {
        return Err(AppError::BadRequest(
            "Messages headers require a Messages transport".into(),
        ));
    }
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
        crate::oauth::authorization_code::validate_contribution(adapter)?;
        if adapter.api_version != "oauth-adapter-v1"
            || !matches!(
                adapter.flow_kind,
                crate::provider::OAuthFlowKind::CursorPkce
                    | crate::provider::OAuthFlowKind::AuthorizationCodePkce
            )
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
            crate::oauth::validate_oauth_adapter_endpoint(endpoint, field)?;
        }
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

fn config_schema_declares_responses_transport(schema: &Value) -> bool {
    schema
        .pointer("/properties/responses_transport/type")
        .and_then(Value::as_str)
        == Some("string")
        && schema
            .pointer("/properties/responses_transport/enum")
            .and_then(Value::as_array)
            .is_some_and(|values| {
                values.len() == 2
                    && values
                        .iter()
                        .any(|value| value.as_str() == Some("native_responses"))
                    && values
                        .iter()
                        .any(|value| value.as_str() == Some("chat_completions"))
            })
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

fn traffic_configuration<'a>(
    plugin: &'a LoadedPlugin,
    configurations: &'a BTreeMap<String, Value>,
) -> &'a Value {
    static EMPTY: std::sync::LazyLock<Value> = std::sync::LazyLock::new(empty_json_object);
    configurations
        .get(&plugin.manifest.id)
        .or_else(|| {
            plugin
                .manifest
                .contributions
                .configuration
                .as_ref()
                .map(|configuration| &configuration.default)
        })
        .unwrap_or(&EMPTY)
}

fn validate_traffic_request_json(value: &str) -> Result<Value, ()> {
    serde_json::from_str(value).map_err(|_| ())
}

fn invalid_plugin_result(_plugin_id: &str, _field: &str) -> AppError {
    AppError::upstream("plugin_invalid_result")
}

fn plugin_runtime_failure(code: &'static str) -> AppError {
    // The fixed low-cardinality code is safe for logs and metrics. Never retain
    // the underlying fs/Wasmtime/HTTP error: those strings may include mount
    // paths, URLs, configuration fragments, or guest-controlled custom text.
    AppError::storage_with(
        "plugin_runtime_failed",
        crate::error::ErrorFacts::new().stage(code),
    )
}

fn plugin_reported_error(_plugin_id: &str, _operation: &str, error: &str) -> AppError {
    let code = if validate_plugin_text(error, MAX_TRAFFIC_REASON_BYTES, false).is_ok() {
        "plugin_reported_error"
    } else {
        "plugin_reported_invalid_error"
    };
    AppError::upstream(code)
}

fn plugin_failure(_plugin_id: &str, error: wasmtime::Error) -> AppError {
    let category = match error.downcast_ref::<wasmtime::Trap>() {
        Some(wasmtime::Trap::OutOfFuel) => "fuel_exhausted",
        Some(wasmtime::Trap::Interrupt) => "execution_interrupted",
        Some(wasmtime::Trap::MemoryOutOfBounds) => "memory_out_of_bounds",
        Some(_) => "guest_trap",
        None => "component_error",
    };
    tracing::warn!(category, "plugin execution failed");
    AppError::upstream("plugin_execution_failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(all(
        feature = "experimental-plugin-revisions",
        feature = "plugin-distribution"
    ))]
    #[tokio::test]
    async fn inventory_loader_without_kv_still_rejects_incomplete_packages() {
        let root = tempfile::tempdir().unwrap();
        let runtime = PluginRuntime::load_for_inventory(root.path().to_str().unwrap()).unwrap();
        assert!(runtime.kv.is_none());
        assert!(runtime.manifests().is_empty());
        std::fs::create_dir(root.path().join("incomplete")).unwrap();
        assert!(PluginRuntime::load_for_inventory(root.path().to_str().unwrap()).is_err());
    }

    #[cfg(all(
        feature = "experimental-plugin-revisions",
        feature = "plugin-distribution"
    ))]
    #[tokio::test]
    async fn inventory_loader_rejects_root_manifest_hiding_nonempty_child_packages() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("child-package");
        std::fs::create_dir(&child).unwrap();
        let manifest = serde_json::json!({
            "id":"child-package", "version":"1.0.0", "wit_version":"0.2.0",
            "wasm":null, "capabilities":[], "contributions":{}
        });
        std::fs::write(
            child.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        let runtime = PluginRuntime::load_for_inventory(root.path().to_str().unwrap()).unwrap();
        assert_eq!(runtime.manifests()[0].id, "child-package");
        std::fs::write(
            root.path().join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        assert!(PluginRuntime::load_for_inventory(root.path().to_str().unwrap()).is_err());
        let compatible = PluginRuntime::load_with_database(root.path().to_str(), None).unwrap();
        assert_eq!(compatible.manifests()[0].id, "child-package");
    }

    #[test]
    fn privacy_wire_shim_output_rejects_headers_and_ignores_payload_size() {
        assert!(validate_wire_shim_output(&[]).is_ok());
        assert!(validate_wire_shim_output(&[("authorization".into(), "bad".into())]).is_err());
    }

    #[test]
    fn privacy_wire_shim_manifest_is_versioned_and_scoped() {
        let mut manifest: super::PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "claude-code-wire",
            "version": "1.0.1",
            "wit_version": "0.3.0",
            "wasm": "plugin.wasm",
            "capabilities": [],
            "contributions": {
                "wire_shim": {"drivers": ["anthropic-claude"], "protocols": ["anthropic"]},
                "configuration": {"schema": {"type": "object"}, "default": {"enabled": true}}
            }
        }))
        .unwrap();
        assert!(super::validate_manifest(&manifest).is_ok());
        manifest.wit_version = "0.2.0".into();
        assert!(super::validate_manifest(&manifest).is_err());
        manifest.wit_version = "0.3.0".into();
        manifest
            .contributions
            .wire_shim
            .as_mut()
            .unwrap()
            .drivers
            .push("anthropic-claude".into());
        assert!(super::validate_manifest(&manifest).is_err());
        manifest.contributions.wire_shim = None;
        assert!(super::validate_manifest(&manifest).is_err());
        manifest.wit_version = "0.2.0".into();
        assert!(super::validate_manifest(&manifest).is_ok());
    }

    #[test]
    fn privacy_wire_shim_only_matches_declared_driver_and_protocol() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "claude-code-wire",
            "version": "1.0.1",
            "wit_version": "0.3.0",
            "wasm": "plugin.wasm",
            "contributions": {
                "wire_shim": {"drivers": ["anthropic-claude"], "protocols": ["anthropic"]}
            }
        }))
        .unwrap();
        let runtime = PluginRuntime {
            plugins: Arc::new(vec![LoadedPlugin {
                manifest,
                component: None,
                service_data_component: None,
                background_action_component: None,
                wire_shim_component: None,
                ui_modules: BTreeMap::new(),
                configuration_validator: None,
                routing_validator: None,
                routing_fingerprint: String::new(),
                #[cfg(feature = "experimental-plugin-revisions")]
                identity: PluginPackageIdentity {
                    component_sha256: None,
                    provenance: None,
                },
            }]),
            ..PluginRuntime::default()
        };
        assert!(runtime.wire_shim_matches("anthropic-claude", "anthropic"));
        assert!(!runtime.wire_shim_matches("anthropic-claude", "openai"));
        assert!(!runtime.wire_shim_matches("http-json", "anthropic"));
        let context = types::RequestContext {
            tenant_id: Uuid::nil().to_string(),
            principal_id: Uuid::nil().to_string(),
            key_id: Uuid::nil().to_string(),
            protocol: "anthropic".into(),
            model: "claude-test".into(),
            config_json: "{}".into(),
        };
        assert!(
            runtime
                .finalize_wire_shim(
                    "http-json",
                    "anthropic",
                    context.clone(),
                    "{}",
                    "{}",
                    &BTreeMap::new()
                )
                .unwrap()
                .is_none()
        );
        assert!(
            runtime
                .finalize_wire_shim(
                    "anthropic-claude",
                    "anthropic",
                    context,
                    "{}",
                    "{}",
                    &BTreeMap::new()
                )
                .is_err()
        );
    }

    #[test]
    fn subscription_release_messages_declaration_matches_closed_manifest_schema() {
        let schema: Value =
            serde_json::from_str(include_str!("../schemas/plugin-manifest.schema.json")).unwrap();
        let manifest: Value = serde_json::from_str(include_str!(
            "../tests/fixtures/claude-wire-subscription-1.1.0.json"
        ))
        .unwrap();
        crate::schema::validate_instance(&schema, &manifest).unwrap();
        for invalid in [
            Value::Null,
            serde_json::json!("true"),
            serde_json::json!(1),
            serde_json::json!({}),
        ] {
            let mut changed = manifest.clone();
            changed["contributions"]["providers"][0]["request_compatibility"]["responses_via_anthropic_messages_v1"] =
                invalid;
            assert!(crate::schema::validate_instance(&schema, &changed).is_err());
        }
        let mut unknown = manifest.clone();
        unknown["contributions"]["providers"][0]["request_compatibility"]["unreviewed_transport"] =
            serde_json::json!(true);
        assert!(crate::schema::validate_instance(&schema, &unknown).is_err());
        for pointer in [
            "/contributions/providers/0/request_compatibility/third_party",
            "/contributions/providers/0/protocols",
        ] {
            let mut changed = manifest.clone();
            *changed
                .pointer_mut(pointer)
                .unwrap_or_else(|| panic!("missing reviewed field {pointer}")) =
                if pointer.ends_with("protocols") {
                    serde_json::json!(["openai"])
                } else {
                    serde_json::json!(false)
                };
            let parsed: PluginManifest = serde_json::from_value(changed).unwrap();
            assert!(super::validate_manifest(&parsed).is_err());
        }
        let mut mixed = manifest;
        mixed["contributions"]["providers"][0]["request_compatibility"]["responses_via_chat_v1"] =
            serde_json::json!(true);
        mixed["contributions"]["providers"][0]["request_compatibility"]["responses_via_chat_dialect"] =
            serde_json::json!("openai_chat_v1");
        let parsed: PluginManifest = serde_json::from_value(mixed).unwrap();
        assert!(super::validate_manifest(&parsed).is_err());
    }

    #[test]
    #[ignore = "GHA supplies the signed public WASM with the exact 1.1.0 release hash"]
    fn privacy_wire_shim_release_component_subscription_package_validation() {
        use sha2::{Digest, Sha256};

        let manifest_bytes =
            include_bytes!("../tests/fixtures/claude-wire-subscription-1.1.0.json");
        assert_eq!(
            format!("{:x}", Sha256::digest(manifest_bytes)),
            "118cff3218b1d4512477d6fdef17001841a18c3393a81b586801cf39f5d21031"
        );
        let fixture = std::path::PathBuf::from(
            std::env::var("MTC_CLAUDE_WIRE_FIXTURE").expect("GHA public signed fixture"),
        );
        let wasm_bytes = std::fs::read(fixture.join("plugin.wasm")).unwrap();
        assert_eq!(
            format!("{:x}", Sha256::digest(&wasm_bytes)),
            "51043af426632581a218c227a0f90b2de2253d27b5438519098c44d965c01f3c"
        );
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("plugin.json"), manifest_bytes).unwrap();
        std::fs::write(directory.path().join("plugin.wasm"), wasm_bytes).unwrap();
        let manifest = super::validate_plugin_package(directory.path())
            .expect("immutable public 1.1.0 release must pass authoritative package validation");
        assert_eq!(manifest.version, "1.1.0");
        assert_eq!(
            manifest.contributions.providers[0].id,
            "claude-code-subscription"
        );
    }

    #[tokio::test]
    #[ignore = "CI builds the pinned claude-code-wire v1.0.1 component"]
    async fn privacy_wire_shim_release_component() {
        let fixture = std::path::PathBuf::from(
            std::env::var("MTC_CLAUDE_WIRE_FIXTURE").expect("CI fixture directory"),
        );
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("plugins");
        let package = root.join("claude-code-wire");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::copy(fixture.join("plugin.json"), package.join("plugin.json")).unwrap();
        std::fs::copy(fixture.join("plugin.wasm"), package.join("plugin.wasm")).unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("fixture.db").display()
        ))
        .await
        .unwrap();
        let runtime = PluginRuntime::load(root.to_str(), database).unwrap();
        assert_eq!(runtime.manifests()[0].version, "1.0.1");
        assert_eq!(runtime.manifests()[0].wit_version, "0.3.0");
        let context = types::RequestContext {
            tenant_id: Uuid::nil().to_string(),
            principal_id: Uuid::nil().to_string(),
            key_id: Uuid::nil().to_string(),
            protocol: "anthropic".into(),
            model: "claude-test".into(),
            config_json: "{}".into(),
        };
        let request = r#"{ "metadata": {"user_id":"private", "other":"保留"}, "messages":[] }"#;
        let finalize = |configurations: &BTreeMap<String, Value>, body: &str| {
            runtime.finalize_wire_shim(
                "anthropic-claude",
                "anthropic",
                context.clone(),
                body,
                "{}",
                configurations,
            )
        };
        let finalized = finalize(&BTreeMap::new(), request).unwrap().unwrap();
        let value: Value = serde_json::from_str(&finalized.request_json).unwrap();
        assert!(value.pointer("/metadata/user_id").is_none());
        assert_eq!(
            value.pointer("/metadata/other"),
            Some(&serde_json::json!("保留"))
        );
        assert_eq!(value.pointer("/messages"), Some(&serde_json::json!([])));
        for configuration in [
            serde_json::json!({"enabled": false, "remove_user_id": true}),
            serde_json::json!({"enabled": true, "remove_user_id": false}),
        ] {
            let configurations = BTreeMap::from([("claude-code-wire".into(), configuration)]);
            assert_eq!(
                finalize(&configurations, request)
                    .unwrap()
                    .unwrap()
                    .request_json
                    .as_bytes(),
                request.as_bytes()
            );
        }
        let unchanged = r#"{ "metadata": {"other":"保留"}, "messages":[] }"#;
        assert_eq!(
            finalize(&BTreeMap::new(), unchanged)
                .unwrap()
                .unwrap()
                .request_json
                .as_bytes(),
            unchanged.as_bytes()
        );
        assert!(finalize(&BTreeMap::new(), "{invalid").is_err());
        let invalid_configuration = BTreeMap::from([(
            "claude-code-wire".into(),
            serde_json::json!({"enabled": "invalid"}),
        )]);
        assert!(finalize(&invalid_configuration, request).is_err());
        assert!(
            runtime
                .finalize_wire_shim(
                    "http-json",
                    "anthropic",
                    context,
                    request,
                    "{}",
                    &BTreeMap::new()
                )
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn configurable_responses_transport_requires_a_closed_provider_schema() {
        let mut provider = crate::provider::ProviderCatalog::builtins()
            .get("http-json")
            .expect("built-in configurable provider")
            .clone();
        provider.id = "plugin-openai-compatible".into();
        assert!(validate_provider_contribution("provider-plugin", &provider).is_ok());

        provider.config_schema["properties"]
            .as_object_mut()
            .expect("object properties")
            .remove("responses_transport");
        assert!(validate_provider_contribution("provider-plugin", &provider).is_err());

        let mut provider = crate::provider::ProviderCatalog::builtins()
            .get("http-json")
            .expect("built-in configurable provider")
            .clone();
        provider.id = "plugin-openai-compatible".into();
        provider.config_schema["properties"]["responses_transport"]["enum"] =
            serde_json::json!(["native_responses", "chat_completions", "automatic"]);
        assert!(validate_provider_contribution("provider-plugin", &provider).is_err());
    }

    #[test]
    fn operator_ui_module_entries_preserve_exact_safe_segments() {
        assert!(safe_operator_ui_module_entry("assets/operator-ui.mjs"));
        for entry in [
            "assets//operator-ui.mjs",
            "./operator-ui.mjs",
            "assets/../operator-ui.mjs",
            "/assets/operator-ui.mjs",
            "assets/operator ui.mjs",
            "assets/operator-ui.css",
        ] {
            assert!(!safe_operator_ui_module_entry(entry), "accepted {entry}");
        }
    }

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
    fn core_policy_kv_namespaces_cannot_be_claimed_by_plugins() {
        let schema = serde_json::json!({"type": "object"});
        for reserved in CORE_PLUGIN_KV_NAMESPACES {
            let mut manifest = configurable_manifest(schema.clone(), serde_json::json!({}));
            manifest.id = (*reserved).to_owned();
            assert!(
                matches!(validate_manifest(&manifest), Err(AppError::BadRequest(message)) if message.contains("reserved for core storage")),
                "core namespace {reserved} must not be a valid plugin identity"
            );
        }

        let mut ordinary = configurable_manifest(schema, serde_json::json!({}));
        ordinary.id = "typed-filter-extension".to_owned();
        assert!(validate_manifest(&ordinary).is_ok());
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
        let epoch = runtime.configuration_cache.read().await.epoch.clone();
        for index in 0..(PLUGIN_CONFIGURATION_CACHE_ENTRIES + 10) {
            runtime
                .cache_snapshot(
                    &epoch,
                    index as u64 + 1,
                    Instant::now(),
                    ResolvedTrafficSnapshot {
                        tenant_id: Uuid::from_u128(index as u128 + 1),
                        values: BTreeMap::from([(
                            "plugin".to_owned(),
                            Value::String("x".repeat(512 * 1024)),
                        )]),
                        revisions: BTreeMap::new(),
                        freshness_deadline: Instant::now() + PLUGIN_CONFIGURATION_CACHE_TTL,
                    },
                    Instant::now,
                )
                .await;
        }
        let cache = runtime.configuration_cache.read().await;
        assert!(cache.entries.len() <= PLUGIN_CONFIGURATION_CACHE_ENTRIES);
        assert!(
            cache
                .entries
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
            http: crate::http_settings::HttpClient::default(),
            runtime: tokio::runtime::Handle::current(),
            kv: None,
            limits: StoreLimitsBuilder::new().build(),
            deadline: Instant::now() + Duration::from_secs(1),
            http_body_limit: None,
            http_diagnostic: PluginHttpDiagnostic::default(),
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
        assert_eq!(state.http_diagnostic.origin_slot, Some(0));
        assert_eq!(state.http_diagnostic.stage, "request_validation");
        assert_eq!(state.http_diagnostic.status, None);
        state.http_diagnostic.status = Some(599);
        state.http_diagnostic.failure_code = Some("timeout");
        state.http_diagnostic.robots = true;
        let error = memeloop::token_center::host::Host::http_request(
            &mut state,
            "GET".to_owned(),
            "https://private:credential@does-not-resolve.invalid/robots.txt".to_owned(),
            "{}".to_owned(),
            Vec::new(),
        )
        .unwrap_err();
        assert!(error.contains("without credentials"));
        assert_eq!(state.http_diagnostic.origin_slot, None);
        assert_eq!(state.http_diagnostic.status, None);
        assert_eq!(state.http_diagnostic.failure_code, None);
        assert!(!state.http_diagnostic.robots);
        assert_eq!(state.http_diagnostic.stage, "request_validation");
    }

    #[test]
    fn signed_oauth_extensions_reject_credentials_and_header_smuggling() {
        let candidate = serde_json::json!({
            "id":"fixture-oauth","version":"1.0.0","wit_version":"0.2.0","wasm":null,
            "contributions":{"providers":[{
                "id":"fixture-provider","display_name":"Fixture provider","protocols":["anthropic"],"modalities":["text"],
                "config_schema":{"type":"object"},"credential_schema":{"type":"object"},
                "oauth_adapter":{
                    "api_version":"oauth-adapter-v1","flow_kind":"authorization_code_pkce",
                    "login_url":"https://provider.example/authorize","poll_url":"https://provider.example/token","refresh_url":"https://provider.example/token",
                    "authorization_code":{"token_encoding":"json","include_state":true,"client_default":{"client_id":"public-client","redirect_uri":"https://provider.example/callback","scopes":["profile"]}},
                    "identity":{"url":"https://provider.example/profile","id_pointer":"/account/id","headers":{"anthropic_beta":["subscription-v1"]}}
                },
                "request_compatibility":{"anthropic_messages_headers":{"anthropic_version":"2023-06-01","anthropic_beta":["subscription-v1"]}}
            }]}
        });
        let manifest: PluginManifest = serde_json::from_value(candidate.clone()).unwrap();
        validate_manifest(&manifest).unwrap();
        let schema: Value =
            serde_json::from_str(include_str!("../schemas/plugin-manifest.schema.json")).unwrap();
        crate::schema::validate_instance(&schema, &candidate).unwrap();
        for (pointer, value) in [
            (
                "/contributions/providers/0/oauth_adapter/authorization_code/client_default",
                serde_json::json!({"client_id":"public-client","redirect_uri":"https://provider.example/callback","scopes":["profile"],"client_secret":"injected"}),
            ),
            (
                "/contributions/providers/0/oauth_adapter/identity/headers",
                serde_json::json!({"authorization":"Bearer injected"}),
            ),
            (
                "/contributions/providers/0/request_compatibility/anthropic_messages_headers",
                serde_json::json!({"cookie":"injected"}),
            ),
        ] {
            let mut invalid = candidate.clone();
            *invalid.pointer_mut(pointer).unwrap() = value;
            assert!(serde_json::from_value::<PluginManifest>(invalid.clone()).is_err());
            assert!(crate::schema::validate_instance(&schema, &invalid).is_err());
        }
        let mut invalid = manifest.clone();
        invalid.contributions.providers[0]
            .oauth_adapter
            .as_mut()
            .unwrap()
            .flow_kind = crate::provider::OAuthFlowKind::CursorPkce;
        assert!(validate_manifest(&invalid).is_err());
        let mut invalid = manifest.clone();
        invalid.contributions.providers[0]
            .request_compatibility
            .anthropic_messages_headers
            .anthropic_beta = vec!["feature\r\nAuthorization: injected".into()];
        assert!(validate_manifest(&invalid).is_err());
        let mut invalid = manifest;
        invalid.contributions.providers[0].protocols = vec!["openai".into()];
        assert!(validate_manifest(&invalid).is_err());
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
            http: crate::http_settings::HttpClient::default(),
            runtime: tokio::runtime::Handle::current(),
            kv: None,
            limits: StoreLimitsBuilder::new().build(),
            deadline: Instant::now() + Duration::from_secs(1),
            http_body_limit: None,
            http_diagnostic: PluginHttpDiagnostic::default(),
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
