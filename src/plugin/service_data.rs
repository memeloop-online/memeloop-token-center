use std::time::{Duration, Instant};

use futures_util::StreamExt;
use serde_json::Value;
use wasmtime::{Store, StoreLimitsBuilder};

use super::{
    HostState, PLUGIN_MEMORY_BYTES, PLUGIN_TABLE_ELEMENTS, PluginRuntime,
    PluginServiceDataEndpoint, PluginServiceDataProvenance, PluginServiceDataView,
    epoch_deadline_ticks, plugin_configuration_schema_digest, plugin_failure,
    plugin_reported_error, service_data_component,
};
use crate::{
    db::PluginServiceDataRefreshErrorCode,
    error::AppError,
    network::{self, OutboundScope},
};

#[derive(Clone)]
pub(crate) struct PluginServiceDataTarget {
    pub(crate) plugin_id: String,
    pub(crate) endpoint: PluginServiceDataEndpoint,
    pub(crate) endpoint_revision: String,
}

pub(crate) struct PluginServiceDataCollected {
    pub(crate) data: Value,
    pub(crate) source: &'static str,
    pub(crate) origin: String,
}

#[derive(Debug)]
pub(crate) struct PluginServiceDataCollectionFailure {
    pub(crate) code: PluginServiceDataRefreshErrorCode,
    pub(crate) error: AppError,
}

impl PluginServiceDataCollectionFailure {
    fn new(code: PluginServiceDataRefreshErrorCode, error: AppError) -> Self {
        Self { code, error }
    }
}

impl PluginRuntime {
    pub fn service_data_endpoint(
        &self,
        plugin_id: &str,
        endpoint_id: &str,
    ) -> Option<PluginServiceDataEndpoint> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .and_then(|plugin| {
                plugin
                    .manifest
                    .contributions
                    .service_data
                    .iter()
                    .find(|endpoint| endpoint.id == endpoint_id)
            })
            .cloned()
    }

    pub(crate) fn service_data_targets(&self) -> Result<Vec<PluginServiceDataTarget>, AppError> {
        self.plugins
            .iter()
            .flat_map(|plugin| {
                plugin
                    .manifest
                    .contributions
                    .service_data
                    .iter()
                    .map(move |endpoint| (plugin, endpoint))
            })
            .map(|(plugin, endpoint)| {
                let endpoint_revision = plugin_configuration_schema_digest(&serde_json::json!({
                    "plugin_id": plugin.manifest.id,
                    "plugin_version": plugin.manifest.version,
                    "component_identity": plugin.routing_fingerprint,
                    "endpoint": endpoint,
                }))?;
                Ok(PluginServiceDataTarget {
                    plugin_id: plugin.manifest.id.clone(),
                    endpoint: endpoint.clone(),
                    endpoint_revision,
                })
            })
            .collect()
    }

    fn service_data_target(
        &self,
        plugin_id: &str,
        endpoint_id: &str,
    ) -> Result<PluginServiceDataTarget, AppError> {
        self.service_data_targets()?
            .into_iter()
            .find(|target| target.plugin_id == plugin_id && target.endpoint.id == endpoint_id)
            .ok_or(AppError::NotFound)
    }

    /// Read only the durable last-good result for the exact active plugin and
    /// endpoint contract. This path performs no network or guest execution.
    pub async fn service_data(
        &self,
        runtime_revision: i64,
        plugin_id: &str,
        endpoint_id: &str,
    ) -> Result<PluginServiceDataView, AppError> {
        let target = self.service_data_target(plugin_id, endpoint_id)?;
        let database = &self.kv.as_ref().ok_or(AppError::Internal)?.database;
        let snapshot = database
            .plugin_service_data_snapshot(
                runtime_revision,
                plugin_id,
                endpoint_id,
                &target.endpoint_revision,
            )
            .await?;
        let now = crate::db::unix_millis();
        let declared_origin = service_data_origin(plugin_id, &target.endpoint)?;

        let Some(snapshot) = snapshot else {
            return Ok(fallback_view(
                plugin_id,
                endpoint_id,
                declared_origin,
                target.endpoint.fallback,
                None,
                None,
                0,
                None,
            ));
        };

        let attempts = u32::try_from(snapshot.consecutive_failures).unwrap_or(u32::MAX);
        let Some(data_json) = snapshot.data_json.as_deref() else {
            return Ok(fallback_view(
                plugin_id,
                endpoint_id,
                declared_origin,
                target.endpoint.fallback,
                nonzero(snapshot.last_attempt_at),
                nonzero(snapshot.next_attempt_at),
                attempts,
                snapshot.last_error_code,
            ));
        };
        let data: Value = match serde_json::from_str(data_json) {
            Ok(data)
                if crate::schema::validate_instance(&target.endpoint.response_schema, &data)
                    .is_ok() =>
            {
                data
            }
            _ => {
                return Ok(fallback_view(
                    plugin_id,
                    endpoint_id,
                    declared_origin,
                    target.endpoint.fallback,
                    nonzero(snapshot.last_attempt_at),
                    nonzero(snapshot.next_attempt_at),
                    attempts,
                    Some(
                        PluginServiceDataRefreshErrorCode::Database
                            .as_str()
                            .to_owned(),
                    ),
                ));
            }
        };
        let fetched_at = snapshot.fetched_at.ok_or(AppError::Internal)?;
        let fresh_until = fetched_at.saturating_add(
            i64::try_from(target.endpoint.cache_ttl_seconds)
                .unwrap_or(i64::MAX)
                .saturating_mul(1_000),
        );
        let fresh = now <= fresh_until && snapshot.consecutive_failures == 0;
        Ok(PluginServiceDataView {
            data,
            partial: !fresh,
            provenance: PluginServiceDataProvenance {
                plugin_id: plugin_id.to_owned(),
                endpoint_id: endpoint_id.to_owned(),
                origin: snapshot.origin.unwrap_or(declared_origin),
                fetched_at,
                source: if fresh {
                    snapshot.source.unwrap_or_else(|| "cache".to_owned())
                } else {
                    "stale_cache".to_owned()
                },
                freshness: if fresh { "fresh" } else { "stale" }.to_owned(),
                last_attempt_at: nonzero(snapshot.last_attempt_at),
                next_attempt_at: nonzero(snapshot.next_attempt_at),
                consecutive_failures: attempts,
                error_code: snapshot.last_error_code,
            },
        })
    }

    pub(crate) async fn collect_http_service_data(
        &self,
        target: &PluginServiceDataTarget,
    ) -> Result<PluginServiceDataCollected, PluginServiceDataCollectionFailure> {
        let endpoint = &target.endpoint;
        let url = endpoint.url.as_deref().ok_or_else(|| {
            PluginServiceDataCollectionFailure::new(
                PluginServiceDataRefreshErrorCode::Network,
                AppError::Internal,
            )
        })?;
        let http = self.http.as_ref().ok_or_else(|| {
            PluginServiceDataCollectionFailure::new(
                PluginServiceDataRefreshErrorCode::Network,
                AppError::Internal,
            )
        })?;
        let timeout = Duration::from_millis(endpoint.timeout_millis);
        let request = async {
            let client = network::client_for_url(http, url, OutboundScope::Public, false)
                .await
                .map_err(|error| {
                    PluginServiceDataCollectionFailure::new(
                        PluginServiceDataRefreshErrorCode::Network,
                        error,
                    )
                })?;
            let response = client
                .get(url)
                .timeout(timeout)
                .send()
                .await
                .map_err(|error| {
                    collection_failure(if error.is_timeout() {
                        PluginServiceDataRefreshErrorCode::Timeout
                    } else {
                        PluginServiceDataRefreshErrorCode::Network
                    })
                })?;
            if !response.status().is_success() {
                return Err(collection_failure(
                    PluginServiceDataRefreshErrorCode::HttpStatus,
                ));
            }
            let json_content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| {
                    let media_type = value.split(';').next().unwrap_or_default().trim();
                    media_type == "application/json" || media_type.ends_with("+json")
                });
            if !json_content_type {
                return Err(collection_failure(
                    PluginServiceDataRefreshErrorCode::ContentType,
                ));
            }
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk
                    .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::Network))?;
                if body.len().saturating_add(chunk.len()) > endpoint.max_body_bytes {
                    return Err(collection_failure(
                        PluginServiceDataRefreshErrorCode::BodyLimit,
                    ));
                }
                body.extend_from_slice(&chunk);
            }
            let data: Value = serde_json::from_slice(&body)
                .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::InvalidJson))?;
            crate::schema::validate_instance(&endpoint.response_schema, &data).map_err(|_| {
                collection_failure(PluginServiceDataRefreshErrorCode::SchemaValidation)
            })?;
            Ok(PluginServiceDataCollected {
                data,
                source: "network",
                origin: url::Url::parse(url)
                    .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::Network))?
                    .origin()
                    .ascii_serialization(),
            })
        };
        tokio::time::timeout(timeout, request)
            .await
            .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::Timeout))?
    }

    /// Synchronous component execution. Callers must use the worker's tracked
    /// blocking lane so shutdown joins the non-cancellable Wasmtime call.
    pub(crate) fn collect_component_service_data(
        &self,
        target: &PluginServiceDataTarget,
    ) -> Result<PluginServiceDataCollected, PluginServiceDataCollectionFailure> {
        let endpoint = &target.endpoint;
        let adapter = endpoint.component_adapter.as_ref().ok_or_else(|| {
            collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
        })?;
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == target.plugin_id)
            .ok_or_else(|| {
                collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
            })?;
        let pre = plugin.service_data_component.as_ref().ok_or_else(|| {
            collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
        })?;
        let engine = self.engine.as_ref().ok_or_else(|| {
            collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
        })?;
        let http = self.http.as_ref().ok_or_else(|| {
            collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
        })?;
        let runtime = self.runtime.as_ref().ok_or_else(|| {
            collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
        })?;
        let timeout = self
            .execution_timeout
            .min(Duration::from_millis(endpoint.timeout_millis));
        let deadline = Instant::now() + timeout;
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
                deadline,
                http_body_limit: endpoint.max_body_bytes,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(timeout));
        store.set_fuel(self.fuel).map_err(|_| {
            collection_failure(PluginServiceDataRefreshErrorCode::ComponentExecution)
        })?;
        let bindings = pre.instantiate(&mut store).map_err(|error| {
            PluginServiceDataCollectionFailure::new(
                component_execution_code(deadline),
                plugin_failure(&plugin.manifest.id, error),
            )
        })?;
        let config_json = serde_json::to_string(&adapter.config)
            .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::ComponentOutput))?;
        let collected = bindings
            .memeloop_token_center_service_data_v1()
            .call_collect(&mut store, &adapter.collector, &config_json)
            .map_err(|error| {
                PluginServiceDataCollectionFailure::new(
                    component_execution_code(deadline),
                    plugin_failure(&plugin.manifest.id, error),
                )
            })?
            .map_err(|error| {
                PluginServiceDataCollectionFailure::new(
                    PluginServiceDataRefreshErrorCode::ComponentExecution,
                    plugin_reported_error(&plugin.manifest.id, "service-data-collect", &error),
                )
            })?;
        if collected.len() > endpoint.max_body_bytes {
            return Err(collection_failure(
                PluginServiceDataRefreshErrorCode::BodyLimit,
            ));
        }
        serde_json::from_str::<Value>(&collected)
            .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::ComponentOutput))?;
        let normalized = bindings
            .memeloop_token_center_service_data_v1()
            .call_normalize(&mut store, &adapter.normalizer, &config_json, &collected)
            .map_err(|error| {
                PluginServiceDataCollectionFailure::new(
                    component_execution_code(deadline),
                    plugin_failure(&plugin.manifest.id, error),
                )
            })?
            .map_err(|error| {
                PluginServiceDataCollectionFailure::new(
                    PluginServiceDataRefreshErrorCode::ComponentExecution,
                    plugin_reported_error(&plugin.manifest.id, "service-data-normalize", &error),
                )
            })?;
        if normalized.len() > endpoint.max_body_bytes {
            return Err(collection_failure(
                PluginServiceDataRefreshErrorCode::BodyLimit,
            ));
        }
        let data: Value = serde_json::from_str(&normalized)
            .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::ComponentOutput))?;
        crate::schema::validate_instance(&endpoint.response_schema, &data)
            .map_err(|_| collection_failure(PluginServiceDataRefreshErrorCode::SchemaValidation))?;
        Ok(PluginServiceDataCollected {
            data,
            source: "component",
            origin: format!(
                "plugin:{}@{}#{}",
                plugin.manifest.id, plugin.manifest.version, adapter.collector
            ),
        })
    }
}

fn service_data_origin(
    plugin_id: &str,
    endpoint: &PluginServiceDataEndpoint,
) -> Result<String, AppError> {
    match (&endpoint.url, &endpoint.component_adapter) {
        (Some(url), None) => Ok(url::Url::parse(url)
            .map_err(|_| AppError::Internal)?
            .origin()
            .ascii_serialization()),
        (None, Some(adapter)) => Ok(format!("plugin:{plugin_id}#{}", adapter.collector)),
        _ => Err(AppError::Internal),
    }
}

#[allow(clippy::too_many_arguments)]
fn fallback_view(
    plugin_id: &str,
    endpoint_id: &str,
    origin: String,
    data: Value,
    last_attempt_at: Option<i64>,
    next_attempt_at: Option<i64>,
    consecutive_failures: u32,
    error_code: Option<String>,
) -> PluginServiceDataView {
    PluginServiceDataView {
        data,
        partial: true,
        provenance: PluginServiceDataProvenance {
            plugin_id: plugin_id.to_owned(),
            endpoint_id: endpoint_id.to_owned(),
            origin,
            fetched_at: 0,
            source: "fallback".to_owned(),
            freshness: "unavailable".to_owned(),
            last_attempt_at,
            next_attempt_at,
            consecutive_failures,
            error_code,
        },
    }
}

fn nonzero(value: i64) -> Option<i64> {
    (value > 0).then_some(value)
}

fn collection_failure(
    code: PluginServiceDataRefreshErrorCode,
) -> PluginServiceDataCollectionFailure {
    PluginServiceDataCollectionFailure::new(
        code,
        AppError::Upstream("plugin service data collection failed".into()),
    )
}

fn component_execution_code(deadline: Instant) -> PluginServiceDataRefreshErrorCode {
    if Instant::now() >= deadline {
        PluginServiceDataRefreshErrorCode::Timeout
    } else {
        PluginServiceDataRefreshErrorCode::ComponentExecution
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;
    use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
    use wit_parser::Resolve;

    use super::*;
    use crate::db::Database;

    fn service_data_component() -> Vec<u8> {
        let mut module = wat::parse_str(
            r#"(module
              (type $log (func (param i32 i32 i32 i32)))
              (type $kv-get (func (param i32 i32 i32)))
              (type $kv-put (func (param i32 i32 i32 i32 i32)))
              (type $http-request (func (param i32 i32 i32 i32 i32 i32 i32 i32 i32)))
              (type $two-strings (func (param i32 i32 i32 i32) (result i32)))
              (type $three-strings (func (param i32 i32 i32 i32 i32 i32) (result i32)))
              (type $post-one (func (param i32)))
              (type $realloc (func (param i32 i32 i32 i32) (result i32)))
              (type $initialize (func))
              (import "cm32p2|memeloop:token-center/host@0.2" "log" (func $log (type $log)))
              (import "cm32p2|memeloop:token-center/host@0.2" "kv-get" (func $kv-get (type $kv-get)))
              (import "cm32p2|memeloop:token-center/host@0.2" "kv-put" (func $kv-put (type $kv-put)))
              (import "cm32p2|memeloop:token-center/host@0.2" "http-request" (func $http-request (type $http-request)))
              (memory $memory 1)
              (global $heap (mut i32) (i32.const 8192))
              (data (i32.const 64) "{\22raw\22:\22ok\22}")
              (data (i32.const 128) "{\22status\22:\22healthy\22}")
              (func $collect (type $two-strings) (param i32 i32 i32 i32) (result i32)
                local.get 1 i32.const 16 i32.ne if unreachable end
                local.get 3 i32.const 20 i32.ne if unreachable end
                i32.const 256 i32.const 0 i32.store
                i32.const 260 i32.const 64 i32.store
                i32.const 264 i32.const 12 i32.store
                i32.const 256)
              (func $collect-post (type $post-one) (param i32))
              (func $normalize (type $three-strings) (param i32 i32 i32 i32 i32 i32) (result i32)
                local.get 1 i32.const 17 i32.ne if unreachable end
                local.get 3 i32.const 20 i32.ne if unreachable end
                local.get 5 i32.const 12 i32.ne if unreachable end
                i32.const 272 i32.const 0 i32.store
                i32.const 276 i32.const 128 i32.store
                i32.const 280 i32.const 20 i32.store
                i32.const 272)
              (func $normalize-post (type $post-one) (param i32))
              (func $realloc (type $realloc) (param i32 i32 i32 i32) (result i32)
                (local $result i32)
                global.get $heap
                local.tee $result
                local.get 3
                i32.add
                global.set $heap
                local.get $result)
              (func $initialize (type $initialize))
              (export "cm32p2|memeloop:token-center/service-data-v1@0.2|collect" (func $collect))
              (export "cm32p2|memeloop:token-center/service-data-v1@0.2|collect_post" (func $collect-post))
              (export "cm32p2|memeloop:token-center/service-data-v1@0.2|normalize" (func $normalize))
              (export "cm32p2|memeloop:token-center/service-data-v1@0.2|normalize_post" (func $normalize-post))
              (export "cm32p2_memory" (memory $memory))
              (export "cm32p2_realloc" (func $realloc))
              (export "cm32p2_initialize" (func $initialize)))"#,
        )
        .expect("parse service data fixture");
        let mut resolve = Resolve::default();
        let (package, _) = resolve
            .push_path("wit/token-center.wit")
            .expect("parse plugin WIT");
        let world = resolve
            .select_world(&[package], Some("service-data-plugin"))
            .expect("select service data world");
        embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8)
            .expect("embed component metadata");
        ComponentEncoder::default()
            .module(&module)
            .expect("read service data core module")
            .validate(true)
            .encode()
            .expect("encode service data component")
    }

    #[tokio::test]
    async fn component_collector_and_normalizer_execute_under_the_service_data_world() {
        let directory = tempfile::tempdir().unwrap();
        let plugins = directory.path().join("plugins");
        let package = plugins.join("service-data-fixture");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("collector.wasm"), service_data_component()).unwrap();
        fs::write(
            package.join("plugin.json"),
            serde_json::to_vec(&json!({
                "id": "service-data-fixture",
                "version": "1.0.0",
                "wit_version": "0.2.0",
                "wasm": "collector.wasm",
                "capabilities": [],
                "contributions": {
                    "service_data": [{
                        "id": "health",
                        "component_adapter": {
                            "api_version": "component-v1",
                            "collector": "health-collector",
                            "normalizer": "health-normalizer",
                            "config": {"source": "fixture"}
                        },
                        "required_scope": "metrics:read",
                        "response_schema": {
                            "type": "object",
                            "additionalProperties": false,
                            "required": ["status"],
                            "properties": {"status": {"const": "healthy"}}
                        },
                        "fallback": {"status": "healthy"},
                        "cache_ttl_seconds": 30,
                        "timeout_millis": 2000,
                        "max_body_bytes": 65536
                    }]
                }
            }))
            .unwrap(),
        )
        .unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("plugin.db").display()
        ))
        .await
        .unwrap();
        let runtime = PluginRuntime::load(plugins.to_str(), database).unwrap();
        let target = runtime.service_data_targets().unwrap().remove(0);
        let collected = runtime.collect_component_service_data(&target).unwrap();
        assert_eq!(collected.data, json!({"status": "healthy"}));
        assert_eq!(collected.source, "component");
        assert_eq!(
            collected.origin,
            "plugin:service-data-fixture@1.0.0#health-collector"
        );
    }
}
