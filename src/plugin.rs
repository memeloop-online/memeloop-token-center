use std::{
    collections::BTreeMap,
    fs,
    io::Read,
    path::{Component as PathComponent, Path, PathBuf},
    sync::Arc,
};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use wasmtime::component::{Component, HasSelf, Linker};
use wasmtime::{Config as WasmtimeConfig, Engine, Store, StoreLimits, StoreLimitsBuilder};

use self::memeloop::token_center::types;
use crate::{db::Database, error::AppError, provider::ProviderType};

const PLUGIN_FUEL: u64 = 5_000_000;
const PLUGIN_MEMORY_BYTES: usize = 32 * 1024 * 1024;
const PLUGIN_HTTP_BODY_BYTES: usize = 16 * 1024 * 1024;

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
    pub wasm: String,
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
    component: Component,
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
    http: Option<reqwest::blocking::Client>,
    kv: Option<PluginKv>,
    plugins: Arc<Vec<LoadedPlugin>>,
    providers: Arc<Vec<ProviderType>>,
}

#[derive(Clone)]
struct PluginKv {
    database: Database,
    runtime: tokio::runtime::Handle,
}

struct HostState {
    plugin_id: String,
    capabilities: Vec<PluginCapability>,
    http: reqwest::blocking::Client,
    kv: Option<PluginKv>,
    limits: StoreLimits,
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
        let engine = Engine::new(&engine_config)
            .map_err(|error| AppError::Storage(format!("initialize plugin runtime: {error}")))?;
        let http = reqwest::blocking::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(5))
            .timeout(std::time::Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|error| AppError::Storage(format!("initialize plugin HTTP: {error}")))?;
        let mut directories = fs::read_dir(root)
            .map_err(|error| AppError::Storage(error.to_string()))?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| AppError::Storage(error.to_string()))?;
        directories.sort_by_key(fs::DirEntry::file_name);

        let mut plugins = Vec::new();
        let mut providers = Vec::new();
        for directory in directories {
            if !directory
                .file_type()
                .map_err(|error| AppError::Storage(error.to_string()))?
                .is_dir()
            {
                continue;
            }
            let manifest_path = directory.path().join("plugin.json");
            if !manifest_path.is_file() {
                continue;
            }
            let manifest: PluginManifest = serde_json::from_slice(
                &fs::read(&manifest_path).map_err(|error| AppError::Storage(error.to_string()))?,
            )
            .map_err(|error| {
                AppError::BadRequest(format!("{}: {error}", manifest_path.display()))
            })?;
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
                providers.push(provider);
            }
            let wasm_path = safe_child(&directory.path(), &manifest.wasm)?;
            let component = Component::from_file(&engine, &wasm_path).map_err(|error| {
                AppError::BadRequest(format!("compile {}: {error}", wasm_path.display()))
            })?;
            plugins.push(LoadedPlugin {
                manifest,
                component,
            });
        }

        Ok(Self {
            engine: Some(engine),
            http: Some(http),
            kv: Some(PluginKv {
                database,
                runtime: tokio::runtime::Handle::current(),
            }),
            plugins: Arc::new(plugins),
            providers: Arc::new(providers),
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
            let limits = StoreLimitsBuilder::new()
                .memory_size(PLUGIN_MEMORY_BYTES)
                .instances(1)
                .memories(2)
                .build();
            let mut store = Store::new(
                engine,
                HostState {
                    plugin_id: plugin.manifest.id.clone(),
                    capabilities: plugin.manifest.capabilities.clone(),
                    http: http.clone(),
                    kv: self.kv.clone(),
                    limits,
                },
            );
            store.limiter(|state| &mut state.limits);
            store
                .set_fuel(PLUGIN_FUEL)
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let mut linker = Linker::new(engine);
            Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
                .map_err(|error| AppError::Storage(error.to_string()))?;
            let bindings = Plugin::instantiate(&mut store, &plugin.component, &linker)
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
}

impl memeloop::token_center::host::Host for HostState {
    fn log(&mut self, level: String, message: String) {
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
        kv.runtime
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
        kv.runtime
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
        let mut request = self.http.request(method, url).body(body);
        for (name, value) in headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| "plugin HTTP header name is invalid".to_owned())?;
            let value = reqwest::header::HeaderValue::from_str(&value)
                .map_err(|_| "plugin HTTP header value is invalid".to_owned())?;
            request = request.header(name, value);
        }
        let mut response = request.send().map_err(|error| error.to_string())?;
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
        response
            .by_ref()
            .take((PLUGIN_HTTP_BODY_BYTES + 1) as u64)
            .read_to_end(&mut response_body)
            .map_err(|error| error.to_string())?;
        if response_body.len() > PLUGIN_HTTP_BODY_BYTES {
            return Err("plugin HTTP response exceeds 16 MiB".to_owned());
        }
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
    if manifest.version.trim().is_empty() || manifest.wit_version != "0.1.0" {
        return Err(AppError::BadRequest(format!(
            "plugin {} has an unsupported version or WIT version",
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
                if !matches!(parsed.scheme(), "http" | "https")
                    || parsed.host_str().is_none()
                    || parsed.origin().ascii_serialization() != *origin
                    || parsed.path() != "/"
                    || parsed.query().is_some()
                    || parsed.fragment().is_some()
                {
                    return Err(AppError::BadRequest(format!(
                        "plugin {} HTTP allowlist entries must be exact origins",
                        manifest.id
                    )));
                }
            }
        }
    }
    for provider in &manifest.contributions.providers {
        if provider.id.is_empty()
            || !provider
                .id
                .chars()
                .all(|value| value.is_ascii_lowercase() || value.is_ascii_digit() || value == '-')
            || provider.display_name.trim().is_empty()
            || provider.protocols.is_empty()
            || provider.modalities.is_empty()
        {
            return Err(AppError::BadRequest(format!(
                "plugin {} contributes an invalid provider",
                manifest.id
            )));
        }
        if let Some(adapter) = &provider.oauth_adapter {
            for (field, endpoint) in [
                ("login_url", &adapter.login_url),
                ("poll_url", &adapter.poll_url),
                ("refresh_url", &adapter.refresh_url),
            ] {
                crate::oauth::validate_oauth_endpoint(endpoint, field)?;
            }
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
    Ok(root.join(child))
}

fn default_wasm_file() -> String {
    "plugin.wasm".to_owned()
}

fn plugin_failure(plugin_id: &str, error: wasmtime::Error) -> AppError {
    AppError::Upstream(format!("plugin {plugin_id} failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_plugin_paths_that_escape_the_package() {
        assert!(safe_child(Path::new("/plugins/test"), "../secret.wasm").is_err());
        assert!(safe_child(Path::new("/plugins/test"), "/secret.wasm").is_err());
        assert_eq!(
            safe_child(Path::new("/plugins/test"), "plugin.wasm").expect("safe path"),
            Path::new("/plugins/test/plugin.wasm")
        );
    }

    #[test]
    fn rejects_non_origin_http_allowlist_entries() {
        let manifest = PluginManifest {
            id: "test-plugin".to_owned(),
            version: "1.0.0".to_owned(),
            wit_version: "0.1.0".to_owned(),
            wasm: "plugin.wasm".to_owned(),
            capabilities: vec![PluginCapability::Http {
                allowed_origins: vec!["https://example.com/oauth/token".to_owned()],
            }],
            contributions: PluginContributions::default(),
        };

        assert!(validate_manifest(&manifest).is_err());
    }
}
