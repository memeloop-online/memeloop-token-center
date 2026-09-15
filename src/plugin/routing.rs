//! Credential-free group scheduling ABI. The caller pins a runtime snapshot;
//! this module only reorders already-authorized candidates, never authorizes a
//! retry or changes tenant/account/generation identity.
use super::*;

mod bindings {
    wasmtime::component::bindgen!({
        world: "group-routing-plugin",
        path: "wit/token-center.wit",
    });
}

pub const GROUP_ROUTING_VERSION: &str = "group-routing-v1";
const MAX_CANDIDATES: usize = 1024;
pub(crate) const MAX_GROUP_ROUTING_JSON_BYTES: usize = 1024 * 1024;
const MAX_DELAY_MS: u64 = 300_000;
const EXECUTION_LIMIT: Duration = Duration::from_millis(100);
pub const GROUP_ROUTING_QUOTA_VERSION: &str = "account-windows-v1";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingContribution {
    pub version: String,
    #[serde(default, skip_serializing_if = "GroupRoutingHealthPolicy::is_plugin")]
    pub health_policy: GroupRoutingHealthPolicy,
    pub schema: Value,
    pub default: Value,
}

/// Manifest-owned behavior, never an untrusted group configuration switch.
/// Omitting the field preserves both old behavior and serialized fingerprints.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRoutingHealthPolicy {
    #[default]
    Plugin,
    Native,
}

impl GroupRoutingHealthPolicy {
    fn is_plugin(&self) -> bool {
        *self == Self::Plugin
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingStrategy {
    pub id: String,
    pub version: String,
    pub schema: Value,
    pub default: Value,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRoutingHealth {
    Healthy,
    Transient,
    HardQuota,
    Authentication,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingCandidate {
    pub tenant_id: String,
    pub route_id: String,
    pub account_id: String,
    pub generation: u64,
    pub health: GroupRoutingHealth,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingInput {
    pub tenant_id: String,
    pub seed: u64,
    pub remaining_deadline_ms: u64,
    pub config: Value,
    pub candidates: Vec<GroupRoutingCandidate>,
    /// Omitted for existing guests, including strict v1 JSON decoders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quota_context: Option<GroupRoutingQuotaContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingQuotaContext {
    pub version: String,
    pub now_ms: i64,
    pub accounts: Vec<GroupRoutingQuotaAccount>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingQuotaAccount {
    pub account_id: String,
    pub generation: u64,
    pub provider: String,
    pub observed_at: i64,
    pub valid_until: i64,
    pub windows: Vec<GroupRoutingQuotaWindow>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingQuotaWindow {
    pub id: String,
    pub period_seconds: Option<i64>,
    pub reset_at: Option<i64>,
    pub reset_is_estimated: bool,
    pub remaining_fraction: Option<f64>,
    pub exhausted: Option<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingDirective {
    pub tenant_id: String,
    pub route_id: String,
    pub account_id: String,
    pub generation: u64,
    pub allow_transient_probe: bool,
    pub cooldown_ms: u64,
    pub recovery_wait_ms: u64,
    pub recheck_ms: u64,
    pub stickiness: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingPlan {
    pub candidates: Vec<GroupRoutingDirective>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRoutingOutcome {
    Success,
    TransientFailure,
    HardQuota,
    Authentication,
    Cancelled,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingObserveInput {
    pub tenant_id: String,
    pub seed: u64,
    pub remaining_deadline_ms: u64,
    pub config: Value,
    pub candidate: GroupRoutingCandidate,
    pub outcome: GroupRoutingOutcome,
}

fn invalid() -> AppError {
    AppError::BadRequest("invalid group-routing-v1 contract".into())
}

pub(super) fn validate_contribution(manifest: &PluginManifest) -> Result<(), AppError> {
    if manifest
        .capabilities
        .contains(&PluginCapability::GroupRoutingQuota)
        && !manifest
            .contributions
            .group_routing
            .as_ref()
            .is_some_and(|routing| routing.health_policy == GroupRoutingHealthPolicy::Native)
    {
        return Err(invalid());
    }
    let Some(contribution) = &manifest.contributions.group_routing else {
        return Ok(());
    };
    if manifest.wasm.is_none()
        || contribution.version != GROUP_ROUTING_VERSION
        || contribution.schema.get("type").and_then(Value::as_str) != Some("object")
        || schema_contains_write_only(&contribution.schema)
    {
        return Err(invalid());
    }
    crate::schema::validate_instance(&contribution.schema, &contribution.default)
}

fn valid_identifier(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && !value.chars().any(char::is_control)
}

fn validate_candidate(candidate: &GroupRoutingCandidate, tenant: &str) -> Result<(), AppError> {
    if candidate.tenant_id != tenant
        || !valid_identifier(tenant)
        || !valid_identifier(&candidate.route_id)
        || !valid_identifier(&candidate.account_id)
    {
        return Err(invalid());
    }
    Ok(())
}

fn identity(candidate: &GroupRoutingCandidate) -> (&str, &str, &str, u64) {
    (
        &candidate.tenant_id,
        &candidate.route_id,
        &candidate.account_id,
        candidate.generation,
    )
}

fn validate_directive(
    directive: &GroupRoutingDirective,
    candidate: &GroupRoutingCandidate,
    remaining_deadline_ms: u64,
) -> Result<(), AppError> {
    if (
        &*directive.tenant_id,
        &*directive.route_id,
        &*directive.account_id,
        directive.generation,
    ) != identity(candidate)
        || directive.cooldown_ms > MAX_DELAY_MS
        || directive.recheck_ms > MAX_DELAY_MS
        || directive.recovery_wait_ms > MAX_DELAY_MS.min(remaining_deadline_ms)
        || (directive.allow_transient_probe && candidate.health != GroupRoutingHealth::Transient)
    {
        return Err(invalid());
    }
    Ok(())
}

pub fn validate_group_routing_plan(
    input: &GroupRoutingInput,
    output: &GroupRoutingPlan,
) -> Result<(), AppError> {
    validate_input(input)?;
    if output.candidates.len() != input.candidates.len() {
        return Err(invalid());
    }
    let candidates: BTreeMap<_, _> = input
        .candidates
        .iter()
        .map(|candidate| (identity(candidate), candidate))
        .collect();
    let mut seen = BTreeSet::new();
    for directive in &output.candidates {
        let key = (
            &*directive.tenant_id,
            &*directive.route_id,
            &*directive.account_id,
            directive.generation,
        );
        let candidate = candidates.get(&key).ok_or_else(invalid)?;
        if !seen.insert(key) {
            return Err(invalid());
        }
        validate_directive(directive, candidate, input.remaining_deadline_ms)?;
    }
    Ok(())
}

fn validate_input(input: &GroupRoutingInput) -> Result<(), AppError> {
    if input.candidates.len() > MAX_CANDIDATES
        || input.remaining_deadline_ms == 0
        || !valid_identifier(&input.tenant_id)
    {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    for candidate in &input.candidates {
        validate_candidate(candidate, &input.tenant_id)?;
        if !seen.insert(identity(candidate)) {
            return Err(invalid());
        }
    }
    if let Some(context) = &input.quota_context {
        validate_quota_context(input, context)?;
    }
    Ok(())
}

fn validate_quota_context(
    input: &GroupRoutingInput,
    context: &GroupRoutingQuotaContext,
) -> Result<(), AppError> {
    let expected: BTreeSet<_> = input
        .candidates
        .iter()
        .map(|candidate| (candidate.account_id.as_str(), candidate.generation))
        .collect();
    if context.version != GROUP_ROUTING_QUOTA_VERSION
        || context.now_ms < 0
        || context.accounts.len() != expected.len()
    {
        return Err(invalid());
    }
    let mut seen = BTreeSet::new();
    for account in &context.accounts {
        let identity = (account.account_id.as_str(), account.generation);
        if !expected.contains(&identity)
            || !seen.insert(identity)
            || !valid_identifier(&account.provider)
            || account.observed_at < 0
            || account.observed_at > context.now_ms
            || account.valid_until <= context.now_ms
            || account.windows.is_empty()
            || account.windows.len() > 64
        {
            return Err(invalid());
        }
        let mut windows = BTreeSet::new();
        for window in &account.windows {
            if window.id.is_empty()
                || window.id.len() > 512
                || window.id.chars().any(char::is_control)
                || !windows.insert(&window.id)
                || window.period_seconds.is_some_and(|period| period <= 0)
                || window.remaining_fraction.is_some_and(|fraction| {
                    !fraction.is_finite() || !(0.0..=1.0).contains(&fraction)
                })
            {
                return Err(invalid());
            }
        }
    }
    Ok(())
}

impl PluginRuntime {
    pub(crate) fn quota_observation_plugin_ids(&self) -> Vec<String> {
        self.plugins
            .iter()
            .filter(|plugin| {
                plugin
                    .manifest
                    .capabilities
                    .contains(&PluginCapability::GroupRoutingQuota)
                    && plugin
                        .manifest
                        .contributions
                        .group_routing
                        .as_ref()
                        .is_some_and(|routing| {
                            routing.health_policy == GroupRoutingHealthPolicy::Native
                        })
            })
            .map(|plugin| plugin.manifest.id.clone())
            .collect()
    }

    pub(crate) fn group_routing_uses_quota_context(&self, plugin_id: &str) -> bool {
        self.plugins.iter().any(|plugin| {
            plugin.manifest.id == plugin_id
                && plugin
                    .manifest
                    .capabilities
                    .contains(&PluginCapability::GroupRoutingQuota)
        })
    }

    pub(crate) fn group_routing_uses_native_health(&self, plugin_id: &str) -> bool {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .and_then(|plugin| plugin.manifest.contributions.group_routing.as_ref())
            .is_some_and(|contribution| {
                contribution.health_policy == GroupRoutingHealthPolicy::Native
            })
    }

    pub(crate) fn has_group_routing_hooks(&self) -> bool {
        self.plugins
            .iter()
            .any(|plugin| plugin.manifest.contributions.group_routing.is_some())
    }

    pub fn group_routing_strategies(&self) -> Vec<GroupRoutingStrategy> {
        self.plugins
            .iter()
            .filter_map(|plugin| {
                let contribution = plugin.manifest.contributions.group_routing.as_ref()?;
                Some(GroupRoutingStrategy {
                    id: plugin.manifest.id.clone(),
                    version: contribution.version.clone(),
                    schema: contribution.schema.clone(),
                    default: contribution.default.clone(),
                })
            })
            .collect()
    }

    pub fn validate_group_routing_configuration(
        &self,
        plugin_id: &str,
        config: &Value,
    ) -> Result<(), AppError> {
        let validator = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .and_then(|plugin| plugin.routing_validator.as_ref())
            .ok_or_else(invalid)?;
        validator.validate(config)
    }

    pub fn execute_group_routing_plan(
        &self,
        plugin_id: &str,
        input: &GroupRoutingInput,
    ) -> Result<GroupRoutingPlan, AppError> {
        validate_input(input)?;
        if self.group_routing_uses_quota_context(plugin_id) != input.quota_context.is_some() {
            return Err(invalid());
        }
        self.validate_group_routing_configuration(plugin_id, &input.config)?;
        let output =
            self.call_group_routing(plugin_id, input, input.remaining_deadline_ms, false)?;
        let plan = serde_json::from_str(&output).map_err(|_| invalid())?;
        validate_group_routing_plan(input, &plan)?;
        Ok(plan)
    }

    pub fn execute_group_routing_observe(
        &self,
        plugin_id: &str,
        input: &GroupRoutingObserveInput,
    ) -> Result<GroupRoutingDirective, AppError> {
        validate_candidate(&input.candidate, &input.tenant_id)?;
        self.validate_group_routing_configuration(plugin_id, &input.config)?;
        // Observation often occurs after a long stream exhausts the
        // scheduling wait budget. It still gets bounded execution, but
        // validate_directive requires recovery_wait_ms == 0 in that case:
        // observation never extends the original request's wait deadline.
        let output =
            self.call_group_routing(plugin_id, input, EXECUTION_LIMIT.as_millis() as u64, true)?;
        let directive = serde_json::from_str(&output).map_err(|_| invalid())?;
        let mut candidate = input.candidate.clone();
        // A failed authentication or hard-quota outcome cannot be converted
        // back to a transient probe by a guest's observation hook.
        match input.outcome {
            GroupRoutingOutcome::HardQuota => candidate.health = GroupRoutingHealth::HardQuota,
            GroupRoutingOutcome::Authentication => {
                candidate.health = GroupRoutingHealth::Authentication
            }
            _ => {}
        }
        validate_directive(&directive, &candidate, input.remaining_deadline_ms)?;
        Ok(directive)
    }

    fn call_group_routing(
        &self,
        plugin_id: &str,
        input: &impl Serialize,
        remaining_deadline_ms: u64,
        observe: bool,
    ) -> Result<String, AppError> {
        let encoded = serde_json::to_string(input).map_err(|_| invalid())?;
        if encoded.len() > MAX_GROUP_ROUTING_JSON_BYTES {
            return Err(invalid());
        }
        let plugin = self
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .ok_or_else(invalid)?;
        let component = plugin.component.as_ref().ok_or_else(invalid)?;
        let engine = self.engine.as_ref().ok_or(AppError::Internal)?;
        let timeout = EXECUTION_LIMIT
            .min(self.execution_timeout)
            .min(Duration::from_millis(remaining_deadline_ms));
        let deadline = Instant::now() + timeout;
        let mut store = Store::new(
            engine,
            HostState {
                plugin_id: plugin_id.to_owned(),
                // Routing never receives network or KV, even if another
                // contribution in the same package declares these capabilities.
                capabilities: Vec::new(),
                http: self.http.as_ref().ok_or(AppError::Internal)?.clone(),
                runtime: self.runtime.as_ref().ok_or(AppError::Internal)?.clone(),
                kv: None,
                limits: StoreLimitsBuilder::new()
                    .memory_size(PLUGIN_MEMORY_BYTES)
                    .table_elements(PLUGIN_TABLE_ELEMENTS)
                    .instances(8)
                    .tables(2)
                    .memories(2)
                    .build(),
                deadline,
            },
        );
        store.limiter(|state| &mut state.limits);
        store.set_epoch_deadline(epoch_deadline_ticks(timeout));
        store
            .set_fuel(self.fuel.min(500_000))
            .map_err(|_| plugin_runtime_failure("fuel_configuration"))?;
        let mut linker = Linker::new(engine);
        Plugin::add_to_linker::<_, HasSelf<_>>(&mut linker, |state| state)
            .map_err(|_| plugin_runtime_failure("linker_configuration"))?;
        let bindings = bindings::GroupRoutingPlugin::instantiate(&mut store, component, &linker)
            .map_err(|error| plugin_failure(plugin_id, error))?;
        let guest = bindings.memeloop_token_center_group_routing_v1();
        let output = if observe {
            guest.call_observe(&mut store, &encoded)
        } else {
            guest.call_plan(&mut store, &encoded)
        }
        .map_err(|error| plugin_failure(plugin_id, error))?
        .map_err(|_| invalid())?;
        if Instant::now() >= deadline || output.len() > MAX_GROUP_ROUTING_JSON_BYTES {
            return Err(invalid());
        }
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absent_group_capability_preserves_existing_serialized_contract() {
        let contributions = serde_json::to_value(PluginContributions::default()).unwrap();
        assert!(
            contributions.get("group_routing").is_none(),
            "adding a null field would change every existing application's pinned contract digest"
        );
        assert_eq!(
            contributions,
            serde_json::json!({
                "traffic_policy":false,"request_rewrite":false,"configuration":null,
                "providers":[],"operator_ui":[],"service_data":[]
            })
        );
        let legacy = serde_json::json!({"version":"group-routing-v1", "schema":{"type":"object"}, "default":{}});
        let parsed: GroupRoutingContribution = serde_json::from_value(legacy.clone()).unwrap();
        assert_eq!(parsed.health_policy, GroupRoutingHealthPolicy::Plugin);
        assert_eq!(serde_json::to_value(parsed).unwrap(), legacy);
        let mut native = legacy;
        native["health_policy"] = serde_json::json!("native");
        let parsed: GroupRoutingContribution = serde_json::from_value(native.clone()).unwrap();
        assert_eq!(parsed.health_policy, GroupRoutingHealthPolicy::Native);
        assert_eq!(serde_json::to_value(parsed).unwrap(), native);
        let (input, _) = fixture();
        let encoded = serde_json::to_value(input).unwrap();
        assert!(
            encoded.get("quota_context").is_none(),
            "legacy strict guests receive no new field"
        );
    }

    #[test]
    fn quota_capability_requires_native_routing_and_exact_fresh_candidate_scope() {
        let mut manifest: PluginManifest = serde_json::from_value(serde_json::json!({
            "id":"quota-order", "version":"1.0.0", "wit_version":"0.2.0", "wasm":"plugin.wasm",
            "capabilities":[{"kind":"group_routing_quota"}],
            "contributions":{"group_routing":{"version":"group-routing-v1", "health_policy":"native", "schema":{"type":"object"}, "default":{}}}
        })).unwrap();
        assert!(validate_contribution(&manifest).is_ok());
        manifest
            .contributions
            .group_routing
            .as_mut()
            .unwrap()
            .health_policy = GroupRoutingHealthPolicy::Plugin;
        assert!(validate_contribution(&manifest).is_err());
        manifest.contributions.group_routing = None;
        assert!(validate_contribution(&manifest).is_err());

        let (mut input, plan) = fixture();
        let context = GroupRoutingQuotaContext {
            version: GROUP_ROUTING_QUOTA_VERSION.into(),
            now_ms: 1000,
            accounts: vec![GroupRoutingQuotaAccount {
                account_id: "account".into(),
                generation: 7,
                provider: "kimi-oauth".into(),
                observed_at: 900,
                valid_until: 1100,
                windows: vec![GroupRoutingQuotaWindow {
                    id: "summary".into(),
                    period_seconds: Some(604800),
                    reset_at: Some(2000),
                    reset_is_estimated: false,
                    remaining_fraction: None,
                    exhausted: None,
                }],
            }],
        };
        input.quota_context = Some(context.clone());
        assert!(validate_group_routing_plan(&input, &plan).is_ok());
        let encoded = serde_json::to_value(&input).unwrap();
        assert!(encoded["quota_context"]["accounts"][0]["windows"][0]["exhausted"].is_null());
        for variant in 0..8 {
            let mut broken = context.clone();
            match variant {
                0 => broken.accounts[0].account_id = "outside-group".into(),
                1 => broken.accounts[0].generation += 1,
                2 => broken.accounts[0].valid_until = broken.now_ms,
                3 => broken.accounts[0].observed_at = broken.now_ms + 1,
                4 => broken.accounts.push(broken.accounts[0].clone()),
                5 => broken.accounts[0].windows[0].remaining_fraction = Some(f64::NAN),
                6 => broken.accounts[0].windows[0].remaining_fraction = Some(1.01),
                _ => {
                    let window = broken.accounts[0].windows[0].clone();
                    broken.accounts[0].windows.push(window);
                }
            }
            input.quota_context = Some(broken);
            assert!(
                validate_group_routing_plan(&input, &plan).is_err(),
                "variant {variant}"
            );
        }
    }
    fn fixture() -> (GroupRoutingInput, GroupRoutingPlan) {
        let candidate = GroupRoutingCandidate {
            tenant_id: "tenant".into(),
            route_id: "route".into(),
            account_id: "account".into(),
            generation: 7,
            health: GroupRoutingHealth::Transient,
        };
        let directive = GroupRoutingDirective {
            tenant_id: candidate.tenant_id.clone(),
            route_id: candidate.route_id.clone(),
            account_id: candidate.account_id.clone(),
            generation: 7,
            allow_transient_probe: true,
            cooldown_ms: 100,
            recovery_wait_ms: 10,
            recheck_ms: 100,
            stickiness: false,
        };
        (
            GroupRoutingInput {
                tenant_id: "tenant".into(),
                seed: 9,
                remaining_deadline_ms: 1000,
                config: serde_json::json!({}),
                candidates: vec![candidate],
                quota_context: None,
            },
            GroupRoutingPlan {
                candidates: vec![directive],
            },
        )
    }
    #[test]
    fn preserves_authorization_and_generation() {
        let (input, mut plan) = fixture();
        assert!(validate_group_routing_plan(&input, &plan).is_ok());
        plan.candidates[0].generation += 1;
        assert!(validate_group_routing_plan(&input, &plan).is_err());
        plan.candidates[0].generation = 7;
        plan.candidates[0].tenant_id = "other".into();
        assert!(validate_group_routing_plan(&input, &plan).is_err());
    }
    #[test]
    fn rejects_duplicates_hard_resurrection_and_unbounded_wait() {
        let (mut input, mut plan) = fixture();
        plan.candidates.push(plan.candidates[0].clone());
        assert!(validate_group_routing_plan(&input, &plan).is_err());
        plan.candidates.pop();
        input.candidates[0].health = GroupRoutingHealth::HardQuota;
        assert!(validate_group_routing_plan(&input, &plan).is_err());
        input.candidates[0].health = GroupRoutingHealth::Transient;
        plan.candidates[0].recovery_wait_ms = 1001;
        assert!(validate_group_routing_plan(&input, &plan).is_err());
    }
    #[test]
    fn rejects_unknown_replay_controls() {
        let (_, plan) = fixture();
        let mut value = serde_json::to_value(&plan.candidates[0]).unwrap();
        value["replay"] = Value::Bool(true);
        assert!(serde_json::from_value::<GroupRoutingDirective>(value).is_err());
    }

    #[test]
    fn hard_state_can_be_ordered_but_not_probed_and_expired_wait_cannot_extend() {
        let (mut input, mut plan) = fixture();
        input.candidates[0].health = GroupRoutingHealth::HardQuota;
        let directive = &mut plan.candidates[0];
        directive.allow_transient_probe = false;
        directive.cooldown_ms = 0;
        directive.recheck_ms = 0;
        directive.stickiness = true;
        directive.recovery_wait_ms = 0;
        assert!(validate_directive(directive, &input.candidates[0], 0).is_ok());
        directive.recovery_wait_ms = 1;
        assert!(validate_directive(directive, &input.candidates[0], 0).is_err());
        directive.recovery_wait_ms = 0;
        directive.allow_transient_probe = true;
        assert!(validate_directive(directive, &input.candidates[0], 100).is_err());
    }
}
