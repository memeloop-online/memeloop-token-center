use std::{
    collections::BTreeMap,
    fs,
    path::{Component as PathComponent, Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::Value;
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
const PLUGIN_MANIFEST_BYTES: u64 = 1024 * 1024;
const PLUGIN_COMPONENT_BYTES: u64 = 64 * 1024 * 1024;
const PLUGIN_EXECUTION_TIMEOUT: Duration = Duration::from_secs(30);
const PLUGIN_EPOCH_TICK: Duration = Duration::from_millis(10);
const SUPPORTED_WIT_REQUIREMENT: &str = ">=0.1.0, <0.2.0";

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
    #[serde(default)]
    pub providers: Vec<ProviderType>,
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
}

#[derive(Clone, Debug, Default)]
pub struct TrafficDecision {
    pub allow: bool,
    pub reason: Option<String>,
    pub model: Option<String>,
    pub upstream_account_id: Option<String>,
    pub request_json: Option<Value>,
}

#[derive(Clone, Default)]
pub struct PluginRuntime {
    engine: Option<Engine>,
    http: Option<reqwest::Client>,
    runtime: Option<tokio::runtime::Handle>,
    kv: Option<PluginKv>,
    plugins: Arc<Vec<LoadedPlugin>>,
    providers: Arc<Vec<ProviderType>>,
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
            return Err(AppError::BadRequest(format!(
                "plugin directory does not exist: {}",
                root.display()
            )));
        }

        let mut engine_config = WasmtimeConfig::new();
        engine_config.wasm_component_model(true);
        engine_config.consume_fuel(true);
        engine_config.epoch_interruption(true);
        let engine = Engine::new(&engine_config)
            .map_err(|error| AppError::Storage(format!("initialize plugin runtime: {error}")))?;
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
            .map_err(|error| AppError::Storage(format!("initialize plugin HTTP: {error}")))?;
        let canonical_root = fs::canonicalize(root)
            .map_err(|error| AppError::Storage(format!("resolve plugin directory: {error}")))?;
        let directories = plugin_directories(&canonical_root)?;

        let mut plugins = Vec::new();
        let mut providers = Vec::new();
        for directory in directories {
            let manifest_path = directory.join("plugin.json");
            if !manifest_path.is_file() {
                return Err(AppError::BadRequest(format!(
                    "plugin package has no plugin.json: {}",
                    directory.display()
                )));
            }
            require_file_size(&manifest_path, PLUGIN_MANIFEST_BYTES, "plugin manifest")?;
            let manifest: PluginManifest = serde_json::from_slice(
                &fs::read(&manifest_path).map_err(|error| AppError::Storage(error.to_string()))?,
            )
            .map_err(|error| {
                AppError::BadRequest(format!("{}: {error}", manifest_path.display()))
            })?;
            let mut manifest_schema: Value =
                serde_json::from_str(include_str!("../schemas/plugin-manifest.schema.json"))
                    .map_err(|_| AppError::Internal)?;
            manifest_schema
                .as_object_mut()
                .ok_or(AppError::Internal)?
                .remove("$id");
            crate::schema::validate_instance(
                &manifest_schema,
                &serde_json::to_value(&manifest).map_err(|_| AppError::Internal)?,
            )?;
            validate_manifest(&manifest)?;
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
                validate_provider_contribution(&manifest.id, &provider)?;
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
                    Component::from_file(&engine, &wasm_path).map_err(|error| {
                        AppError::BadRequest(format!("compile {}: {error}", wasm_path.display()))
                    })
                })
                .transpose()?;
            plugins.push(LoadedPlugin {
                manifest,
                component,
            });
        }

        Ok(Self {
            engine: Some(engine),
            http: Some(http),
            runtime: Some(tokio::runtime::Handle::current()),
            kv: Some(PluginKv { database }),
            plugins: Arc::new(plugins),
            providers: Arc::new(providers),
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

    pub fn apply_traffic(
        &self,
        context: types::RequestContext,
        request_json: &Value,
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
        for plugin in self
            .plugins
            .iter()
            .filter(|plugin| plugin.manifest.contributions.traffic_policy)
        {
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
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let mut linker = Linker::new(engine);
            Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let bindings = Plugin::instantiate(&mut store, component, &linker)
                .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
            let request = serde_json::to_string(&current).map_err(|_| AppError::Internal)?;
            let result = bindings
                .memeloop_token_center_traffic_policy()
                .call_post_auth(&mut store, &context, &request)
                .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
                .map_err(|error| {
                    AppError::Upstream(format!("plugin {}: {error}", plugin.manifest.id))
                })?;
            if !result.allow {
                return Ok(TrafficDecision {
                    allow: false,
                    reason: result.reason,
                    ..decision
                });
            }
            if let Some(request) = result.request_json {
                current = serde_json::from_str(&request).map_err(|_| {
                    AppError::Upstream(format!(
                        "plugin {} returned invalid request JSON",
                        plugin.manifest.id
                    ))
                })?;
                decision.request_json = Some(current.clone());
            }
            if result.model.is_some() {
                decision.model = result.model;
            }
            if result.upstream_account_id.is_some() {
                decision.upstream_account_id = result.upstream_account_id;
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
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let mut linker = Linker::new(engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|error| AppError::Storage(error.to_string()))?;
        let bindings = Plugin::instantiate(&mut store, component, &linker)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?;
        let config_json = serde_json::to_string(config).map_err(|_| AppError::Internal)?;
        let models_json = bindings
            .memeloop_token_center_upstream_provider()
            .call_list_models(&mut store, &config_json)
            .map_err(|error| plugin_failure(&plugin.manifest.id, error))?
            .map_err(|error| {
                AppError::Upstream(format!("plugin {}: {error}", plugin.manifest.id))
            })?;
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

    #[doc(hidden)]
    pub fn set_execution_limits_for_tests(&mut self, timeout: Duration, fuel: u64) {
        self.execution_timeout = timeout;
        self.fuel = fuel;
    }
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
        .map_err(|error| AppError::Storage(error.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| AppError::Storage(error.to_string()))?;
    let mut directories = Vec::new();
    for entry in entries {
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        let file_type = entry
            .file_type()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        if file_type.is_symlink() {
            return Err(AppError::BadRequest(format!(
                "plugin package directory cannot be a symlink: {}",
                entry.path().display()
            )));
        }
        if file_type.is_dir() {
            directories.push(fs::canonicalize(entry.path()).map_err(|error| {
                AppError::Storage(format!("resolve plugin package directory: {error}"))
            })?);
        }
    }
    directories.sort();
    Ok(directories)
}

impl memeloop::token_center::host::Host for HostState {
    fn log(&mut self, level: String, message: String) {
        if !self
            .capabilities
            .iter()
            .any(|capability| matches!(capability, PluginCapability::Log))
        {
            return;
        }
        match level.as_str() {
            "error" => tracing::error!(plugin_id = %self.plugin_id, %message, "plugin"),
            "warn" => tracing::warn!(plugin_id = %self.plugin_id, %message, "plugin"),
            "debug" => tracing::debug!(plugin_id = %self.plugin_id, %message, "plugin"),
            _ => tracing::info!(plugin_id = %self.plugin_id, %message, "plugin"),
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
            .map_err(|error| error.to_string())
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
            .map_err(|error| error.to_string())
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
        {
            return Err("plugin HTTP URL must be an HTTP(S) URL without credentials".to_owned());
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
        let method = reqwest::Method::from_bytes(method.as_bytes())
            .map_err(|_| "plugin HTTP method is invalid".to_owned())?;
        let headers: BTreeMap<String, String> = serde_json::from_str(&headers_json)
            .map_err(|_| "plugin HTTP headers must be a string map".to_owned())?;
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
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "plugin HTTP header name is invalid".to_owned())?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| "plugin HTTP header value is invalid".to_owned())?;
            request = request.header(name, value);
        }
        let (status, response_headers, response_body) = self.runtime.block_on(async move {
            let response = request.send().await.map_err(|error| error.to_string())?;
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
                let chunk = chunk.map_err(|error| error.to_string())?;
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
        .map_err(|error| error.to_string())
    }
}

impl memeloop::token_center::types::Host for HostState {}

fn validate_manifest(manifest: &PluginManifest) -> Result<(), AppError> {
    if manifest.id.is_empty()
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
    if manifest.contributions.traffic_policy && manifest.wasm.is_none() {
        return Err(AppError::BadRequest(format!(
            "plugin {} needs a component for its traffic policy",
            manifest.id
        )));
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

fn validate_provider_contribution(
    plugin_id: &str,
    provider: &ProviderType,
) -> Result<(), AppError> {
    const PROTOCOLS: &[&str] = &["openai", "anthropic", "generation"];
    const MODALITIES: &[&str] = &["text", "embedding", "image", "video", "audio"];
    if provider.id.is_empty()
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
    if let Some(adapter) = &provider.oauth_adapter {
        for (field, endpoint) in [
            ("login_url", &adapter.login_url),
            ("poll_url", &adapter.poll_url),
            ("refresh_url", &adapter.refresh_url),
        ] {
            crate::oauth::validate_oauth_endpoint(endpoint, field)?;
        }
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
    let canonical_root = fs::canonicalize(root)
        .map_err(|error| AppError::Storage(format!("resolve plugin package: {error}")))?;
    let canonical_child = fs::canonicalize(root.join(child))
        .map_err(|error| AppError::Storage(format!("resolve plugin component: {error}")))?;
    if !canonical_child.starts_with(&canonical_root) {
        return Err(AppError::BadRequest(
            "plugin wasm path must stay inside its package".into(),
        ));
    }
    Ok(canonical_child)
}

fn require_file_size(path: &Path, maximum: u64, kind: &str) -> Result<(), AppError> {
    let length = fs::metadata(path)
        .map_err(|error| AppError::Storage(format!("inspect {}: {error}", path.display())))?
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

fn plugin_failure(plugin_id: &str, error: wasmtime::Error) -> AppError {
    AppError::Upstream(format!("plugin {plugin_id} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            wit_version: "0.1.0".to_owned(),
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
            wit_version: "0.1.0".to_owned(),
            wasm: Some("plugin.wasm".to_owned()),
            capabilities: vec![PluginCapability::Http {
                allowed_origins: vec!["http://metadata.internal".to_owned()],
            }],
            contributions: PluginContributions::default(),
        };

        assert!(validate_manifest(&manifest).is_err());
    }

    #[test]
    fn manifest_only_oauth_provider_does_not_require_wasm() {
        let manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "example-oauth",
            "version": "1.0.0",
            "wit_version": "0.1.0",
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
                "wit_version": "0.1.0",
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
