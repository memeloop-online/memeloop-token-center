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
                    .memory_size(PLUGIN_MEMORY_BYTES)
                    .table_elements(PLUGIN_TABLE_ELEMENTS)
                    .instances(8)
                    .tables(2)
                    .memories(2)
                    .build(),
                deadline,
                http_body_limit: PLUGIN_HTTP_BODY_BYTES,
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
