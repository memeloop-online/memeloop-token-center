use super::*;

mod bindings {
    wasmtime::component::bindgen!({
        world: "network-proxy-strategy-plugin",
        path: "wit/token-center.wit",
    });
}

pub const VERSION: &str = "network-proxy-strategy-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkProxyStrategyContribution {
    pub version: String,
    pub schema: Value,
    pub default: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyCandidate {
    pub proxy_id: Uuid,
    pub proxy_version: i64,
    pub active_health: String,
    pub checked_at: Option<i64>,
    pub latency_ms: Option<i64>,
    pub consecutive_successes: i64,
    pub sample_count: i64,
    pub transport_success_rate: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ProxyStrategyInput {
    pub tenant_id: String,
    pub policy_revision: i64,
    pub current_proxy_id: Option<Uuid>,
    pub observed_at: i64,
    pub config: Value,
    pub candidates: Vec<ProxyCandidate>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyStrategyPlan {
    pub candidates: Vec<ProxyIdentity>,
    pub reason: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProxyIdentity {
    pub proxy_id: Uuid,
    pub proxy_version: i64,
}

fn invalid() -> AppError {
    AppError::BadRequest("invalid network proxy strategy contract".into())
}

pub(super) fn validate_contribution(manifest: &PluginManifest) -> Result<(), AppError> {
    if let Some(contribution) = &manifest.contributions.network_proxy_strategy {
        if manifest.wasm.is_none()
            || contribution.version != VERSION
            || contribution.schema.get("type").and_then(Value::as_str) != Some("object")
            || !contribution.default.is_object()
            || serde_json::to_string(&contribution.schema)
                .map_err(|_| invalid())?
                .len()
                > 65536
            || contains_write_only(&contribution.schema)
        {
            return Err(invalid());
        }
        crate::schema::validate_instance(&contribution.schema, &contribution.default)
            .map_err(|_| invalid())?;
    }
    Ok(())
}

fn contains_write_only(value: &Value) -> bool {
    match value {
        Value::Object(fields) => {
            fields.get("writeOnly").and_then(Value::as_bool) == Some(true)
                || fields.values().any(contains_write_only)
        }
        Value::Array(items) => items.iter().any(contains_write_only),
        _ => false,
    }
}

pub fn validate_plan(input: &ProxyStrategyInput, plan: &ProxyStrategyPlan) -> Result<(), AppError> {
    let mut seen = BTreeSet::new();
    if plan.candidates.is_empty()
        || plan.candidates.len() > input.candidates.len()
        || plan.reason.len() > 256
        || plan.reason.chars().any(char::is_control)
    {
        return Err(invalid());
    }
    for chosen in &plan.candidates {
        if !seen.insert(chosen.proxy_id)
            || !input.candidates.iter().any(|candidate| {
                candidate.proxy_id == chosen.proxy_id
                    && candidate.proxy_version == chosen.proxy_version
            })
        {
            return Err(invalid());
        }
    }
    Ok(())
}

impl PluginRuntime {
    pub fn network_proxy_strategies(&self) -> Vec<Value> {
        self.plugins.iter().filter_map(|plugin| {
            let contribution = plugin.manifest.contributions.network_proxy_strategy.as_ref()?;
            Some(serde_json::json!({"id": plugin.manifest.id, "plugin_version": plugin.manifest.version,
                "version": contribution.version, "schema": contribution.schema, "default": contribution.default,
                "revision": plugin.routing_fingerprint}))
        }).collect()
    }

    pub fn validate_network_proxy_configuration(
        &self,
        plugin_id: &str,
        plugin_version: &str,
        config: &Value,
    ) -> Result<(), AppError> {
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| {
                plugin.manifest.id == plugin_id && plugin.manifest.version == plugin_version
            })
            .ok_or_else(invalid)?;
        let contribution = plugin
            .manifest
            .contributions
            .network_proxy_strategy
            .as_ref()
            .ok_or_else(invalid)?;
        crate::schema::validate_instance(&contribution.schema, config).map_err(|_| invalid())
    }

    pub(crate) fn execute_network_proxy_strategy(
        &self,
        plugin_id: &str,
        plugin_version: &str,
        input: &ProxyStrategyInput,
    ) -> Result<ProxyStrategyPlan, AppError> {
        self.validate_network_proxy_configuration(plugin_id, plugin_version, &input.config)?;
        if input.candidates.is_empty() || input.candidates.len() > 4 {
            return Err(invalid());
        }
        let encoded = serde_json::to_string(input).map_err(|_| invalid())?;
        if encoded.len() > 65536 {
            return Err(invalid());
        }
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .ok_or_else(invalid)?;
        let component = plugin.component.as_ref().ok_or_else(invalid)?;
        let engine = self.engine.as_ref().ok_or_else(invalid)?;
        let timeout = Duration::from_millis(100).min(self.execution_timeout);
        let deadline = Instant::now() + timeout;
        let mut store = Store::new(
            engine,
            HostState {
                plugin_id: plugin_id.to_owned(),
                capabilities: Vec::new(),
                http: self.http.as_ref().ok_or_else(invalid)?.clone(),
                runtime: self.runtime.as_ref().ok_or_else(invalid)?.clone(),
                kv: None,
                limits: StoreLimitsBuilder::new()
                    .memory_size(16 * 1024 * 1024)
                    .table_elements(PLUGIN_TABLE_ELEMENTS)
                    .instances(8)
                    .tables(2)
                    .memories(2)
                    .build(),
                deadline,
                http_body_limit: None,
                http_diagnostic: PluginHttpDiagnostic::default(),
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(timeout));
        store
            .set_fuel(self.fuel.min(500_000))
            .map_err(|_| invalid())?;
        let mut linker = Linker::new(engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|_| invalid())?;
        let bindings =
            bindings::NetworkProxyStrategyPlugin::instantiate(&mut store, component, &linker)
                .map_err(|error| plugin_failure(plugin_id, error))?;
        let output = bindings
            .memeloop_token_center_network_proxy_strategy_v1()
            .call_plan(&mut store, &encoded)
            .map_err(|error| plugin_failure(plugin_id, error))?
            .map_err(|_| invalid())?;
        if Instant::now() >= deadline || output.len() > 16384 {
            return Err(invalid());
        }
        let plan: ProxyStrategyPlan = serde_json::from_str(&output).map_err(|_| invalid())?;
        validate_plan(input, &plan)?;
        Ok(plan)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wit_component::{ComponentEncoder, StringEncoding, embed_component_metadata};
    use wit_parser::Resolve;

    fn component(output: &str, endless: bool) -> Vec<u8> {
        let escaped: String = output.bytes().map(|byte| format!("\\{byte:02x}")).collect();
        let body = if endless {
            "(loop br 0) unreachable".to_owned()
        } else {
            format!(
                "i32.const 32 i32.const 0 i32.store i32.const 36 i32.const 1024 i32.store i32.const 40 i32.const {} i32.store i32.const 32",
                output.len()
            )
        };
        let mut module = wat::parse_str(format!(
            r#"(module
            (memory (export "memory") 2)
            (global $heap (mut i32) (i32.const 8192))
            (func (export "cabi_realloc") (param i32 i32 i32 i32) (result i32)
                (local $result i32)
                global.get $heap local.set $result
                global.get $heap local.get 3 i32.add global.set $heap
                local.get $result)
            (data (i32.const 1024) "{escaped}")
            (func (export "memeloop:token-center/network-proxy-strategy-v1@0.2.0#plan")
                (param i32 i32) (result i32) {body}))"#
        ))
        .unwrap();
        let mut resolve = Resolve::default();
        let (package, _) = resolve.push_path("wit/token-center.wit").unwrap();
        let world = resolve
            .select_world(&[package], Some("network-proxy-strategy-plugin"))
            .unwrap();
        embed_component_metadata(&mut module, &resolve, world, StringEncoding::UTF8).unwrap();
        ComponentEncoder::default()
            .module(&module)
            .unwrap()
            .validate(true)
            .encode()
            .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_guest_enforces_candidate_version_schema_and_execution_budget() {
        let directory = tempfile::tempdir().unwrap();
        let package = directory.path().join("plugins/exit-picker");
        fs::create_dir_all(&package).unwrap();
        fs::write(package.join("plugin.json"), serde_json::to_vec(&serde_json::json!({
            "id":"exit-picker", "version":"1.0.0", "wit_version":"0.2.0", "wasm":"plugin.wasm", "capabilities":[],
            "contributions":{"network_proxy_strategy":{"version":VERSION,
                "schema":{"type":"object","additionalProperties":false}, "default":{}}}
        })).unwrap()).unwrap();
        let database = crate::db::Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("db.sqlite").display()
        ))
        .await
        .unwrap();
        let candidate = ProxyCandidate {
            proxy_id: Uuid::now_v7(),
            proxy_version: 7,
            active_health: "unknown".into(),
            checked_at: None,
            latency_ms: None,
            consecutive_successes: 0,
            sample_count: 0,
            transport_success_rate: None,
        };
        let input = ProxyStrategyInput {
            tenant_id: "tenant".into(),
            policy_revision: 3,
            current_proxy_id: None,
            observed_at: 100,
            config: serde_json::json!({}),
            candidates: vec![candidate.clone()],
        };
        let plan = |id, version| {
            serde_json::json!({"candidates":[{"proxy_id":id,"proxy_version":version}],"reason":"configured"}).to_string()
        };
        let load = |output: &str, endless| {
            fs::write(package.join("plugin.wasm"), component(output, endless)).unwrap();
            PluginRuntime::load(directory.path().join("plugins").to_str(), database.clone())
                .unwrap()
        };
        let runtime = load(&plan(candidate.proxy_id, 7), false);
        assert_eq!(
            runtime
                .execute_network_proxy_strategy("exit-picker", "1.0.0", &input)
                .unwrap()
                .candidates[0]
                .proxy_id,
            candidate.proxy_id
        );
        assert!(
            runtime
                .validate_network_proxy_configuration(
                    "exit-picker",
                    "1.0.0",
                    &serde_json::json!({"unknown":true})
                )
                .is_err()
        );
        assert!(
            runtime
                .execute_network_proxy_strategy("exit-picker", "2.0.0", &input)
                .is_err()
        );
        for output in [
            plan(candidate.proxy_id, 6),
            plan(Uuid::now_v7(), 7),
            "{\"candidates\":[],\"reason\":\"empty\"}".into(),
        ] {
            assert!(
                load(&output, false)
                    .execute_network_proxy_strategy("exit-picker", "1.0.0", &input)
                    .is_err()
            );
        }
        assert!(
            load("", true)
                .execute_network_proxy_strategy("exit-picker", "1.0.0", &input)
                .is_err()
        );
        // A package upgrade cannot change a captured runtime's component or version.
        assert!(
            runtime
                .execute_network_proxy_strategy("exit-picker", "1.0.0", &input)
                .is_ok()
        );
    }
}
