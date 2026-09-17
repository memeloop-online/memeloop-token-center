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
pub const GROUP_ROUTING_V2_VERSION: &str = "group-routing-v2";
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingTransientSignal {
    pub sample_count: u64,
    pub ewma_micros: u32,
    pub last_observed_at: i64,
    pub recovery_successes: u64,
    pub revision: u64,
}

impl GroupRoutingTransientSignal {
    pub(crate) fn should_open(self, minimum_samples: u32, open_threshold_micros: u32) -> bool {
        self.sample_count >= u64::from(minimum_samples) && self.ewma_micros >= open_threshold_micros
    }

    pub(crate) fn should_recover(
        self,
        recover_threshold_micros: u32,
        minimum_probe_successes: u32,
    ) -> bool {
        self.ewma_micros <= recover_threshold_micros
            && self.recovery_successes >= u64::from(minimum_probe_successes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GroupRoutingCandidateV2 {
    tenant_id: String,
    route_id: String,
    account_id: String,
    generation: u64,
    health: GroupRoutingHealth,
    transient_signal: GroupRoutingTransientSignal,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum GroupRoutingTransientPolicyMode {
    Shadow,
    Active,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct GroupRoutingTransientPolicy {
    pub mode: GroupRoutingTransientPolicyMode,
    pub min_samples: u32,
    pub open_micros: u32,
    pub recover_micros: u32,
    pub min_probe_successes: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GroupRoutingDirectiveV2 {
    tenant_id: String,
    route_id: String,
    account_id: String,
    generation: u64,
    allow_transient_probe: bool,
    cooldown_ms: u64,
    recovery_wait_ms: u64,
    recheck_ms: u64,
    stickiness: bool,
    transient_policy: GroupRoutingTransientPolicy,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GroupRoutingInputV2 {
    tenant_id: String,
    seed: u64,
    remaining_deadline_ms: u64,
    config: Value,
    candidates: Vec<GroupRoutingCandidateV2>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quota_context: Option<GroupRoutingQuotaContext>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GroupRoutingPlanV2 {
    candidates: Vec<GroupRoutingDirectiveV2>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct GroupRoutingObserveInputV2 {
    tenant_id: String,
    seed: u64,
    remaining_deadline_ms: u64,
    config: Value,
    candidate: GroupRoutingCandidateV2,
    outcome: GroupRoutingOutcome,
}

#[derive(Clone, Debug)]
pub(crate) struct GroupRoutingExecutionDirective {
    pub(crate) directive: GroupRoutingDirective,
    pub(crate) transient_policy: Option<GroupRoutingTransientPolicy>,
}

#[derive(Clone, Debug)]
pub(crate) struct GroupRoutingExecutionPlan {
    pub(crate) candidates: Vec<GroupRoutingExecutionDirective>,
}

impl GroupRoutingTransientPolicy {
    pub(crate) fn is_active(self) -> bool {
        self.mode == GroupRoutingTransientPolicyMode::Active
    }

    pub(crate) fn is_valid(self) -> bool {
        (1..=10_000).contains(&self.min_samples)
            && self.open_micros <= 1_000_000
            && self.recover_micros <= self.open_micros
            && (1..=64).contains(&self.min_probe_successes)
    }
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
        || !matches!(
            contribution.version.as_str(),
            GROUP_ROUTING_VERSION | GROUP_ROUTING_V2_VERSION
        )
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

fn v1_candidate(candidate: &GroupRoutingCandidateV2) -> GroupRoutingCandidate {
    GroupRoutingCandidate {
        tenant_id: candidate.tenant_id.clone(),
        route_id: candidate.route_id.clone(),
        account_id: candidate.account_id.clone(),
        generation: candidate.generation,
        health: candidate.health,
    }
}

fn v1_directive(directive: &GroupRoutingDirectiveV2) -> GroupRoutingDirective {
    GroupRoutingDirective {
        tenant_id: directive.tenant_id.clone(),
        route_id: directive.route_id.clone(),
        account_id: directive.account_id.clone(),
        generation: directive.generation,
        allow_transient_probe: directive.allow_transient_probe,
        cooldown_ms: directive.cooldown_ms,
        recovery_wait_ms: directive.recovery_wait_ms,
        recheck_ms: directive.recheck_ms,
        stickiness: directive.stickiness,
    }
}

fn validate_v2_input(input: &GroupRoutingInputV2) -> Result<GroupRoutingInput, AppError> {
    if input.candidates.iter().any(|candidate| {
        candidate.transient_signal.ewma_micros > 1_000_000
            || candidate.transient_signal.last_observed_at < 0
    }) {
        return Err(invalid());
    }
    let v1 = GroupRoutingInput {
        tenant_id: input.tenant_id.clone(),
        seed: input.seed,
        remaining_deadline_ms: input.remaining_deadline_ms,
        config: input.config.clone(),
        candidates: input.candidates.iter().map(v1_candidate).collect(),
        quota_context: input.quota_context.clone(),
    };
    validate_input(&v1)?;
    Ok(v1)
}

fn validate_v2_plan(
    input: &GroupRoutingInputV2,
    mut output: GroupRoutingPlanV2,
) -> Result<GroupRoutingExecutionPlan, AppError> {
    let v1_input = validate_v2_input(input)?;
    if output
        .candidates
        .iter()
        .any(|directive| !directive.transient_policy.is_valid())
    {
        return Err(invalid());
    }
    let activation_requested = input
        .config
        .get("transient_health_mode")
        .and_then(Value::as_str)
        == Some("active");
    if !activation_requested {
        for directive in &mut output.candidates {
            directive.transient_policy.mode = GroupRoutingTransientPolicyMode::Shadow;
        }
    }
    let v1_plan = GroupRoutingPlan {
        candidates: output.candidates.iter().map(v1_directive).collect(),
    };
    validate_group_routing_plan(&v1_input, &v1_plan)?;
    Ok(GroupRoutingExecutionPlan {
        candidates: output
            .candidates
            .into_iter()
            .zip(v1_plan.candidates)
            .map(|(v2, directive)| GroupRoutingExecutionDirective {
                directive,
                transient_policy: Some(v2.transient_policy),
            })
            .collect(),
    })
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
    pub(crate) fn group_routing_version(&self, plugin_id: &str) -> Option<&str> {
        self.plugins
            .iter()
            .find(|plugin| plugin.manifest.id == plugin_id)
            .and_then(|plugin| plugin.manifest.contributions.group_routing.as_ref())
            .map(|contribution| contribution.version.as_str())
    }

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

    pub(crate) fn execute_group_routing_plan_with_health(
        &self,
        plugin_id: &str,
        input: &GroupRoutingInput,
        transient_signals: Option<&[GroupRoutingTransientSignal]>,
    ) -> Result<GroupRoutingExecutionPlan, AppError> {
        let version = self.group_routing_version(plugin_id).ok_or_else(invalid)?;
        validate_input(input)?;
        if self.group_routing_uses_quota_context(plugin_id) != input.quota_context.is_some() {
            return Err(invalid());
        }
        self.validate_group_routing_configuration(plugin_id, &input.config)?;
        if version == GROUP_ROUTING_V2_VERSION {
            let signals = transient_signals
                .filter(|signals| signals.len() == input.candidates.len())
                .ok_or_else(invalid)?;
            let v2 = GroupRoutingInputV2 {
                tenant_id: input.tenant_id.clone(),
                seed: input.seed,
                remaining_deadline_ms: input.remaining_deadline_ms,
                config: input.config.clone(),
                candidates: input
                    .candidates
                    .iter()
                    .cloned()
                    .zip(signals.iter().copied())
                    .map(|(candidate, transient_signal)| GroupRoutingCandidateV2 {
                        tenant_id: candidate.tenant_id,
                        route_id: candidate.route_id,
                        account_id: candidate.account_id,
                        generation: candidate.generation,
                        health: candidate.health,
                        transient_signal,
                    })
                    .collect(),
                quota_context: input.quota_context.clone(),
            };
            let output =
                self.call_group_routing(plugin_id, &v2, input.remaining_deadline_ms, false)?;
            return validate_v2_plan(&v2, serde_json::from_str(&output).map_err(|_| invalid())?);
        }
        if transient_signals.is_some() {
            return Err(invalid());
        }
        let output =
            self.call_group_routing(plugin_id, input, input.remaining_deadline_ms, false)?;
        let plan: GroupRoutingPlan = serde_json::from_str(&output).map_err(|_| invalid())?;
        validate_group_routing_plan(input, &plan)?;
        Ok(GroupRoutingExecutionPlan {
            candidates: plan
                .candidates
                .into_iter()
                .map(|directive| GroupRoutingExecutionDirective {
                    directive,
                    transient_policy: None,
                })
                .collect(),
        })
    }

    pub fn execute_group_routing_plan(
        &self,
        plugin_id: &str,
        input: &GroupRoutingInput,
    ) -> Result<GroupRoutingPlan, AppError> {
        if self.group_routing_version(plugin_id) != Some(GROUP_ROUTING_VERSION) {
            return Err(invalid());
        }
        Ok(GroupRoutingPlan {
            candidates: self
                .execute_group_routing_plan_with_health(plugin_id, input, None)?
                .candidates
                .into_iter()
                .map(|entry| entry.directive)
                .collect(),
        })
    }

    pub(crate) fn execute_group_routing_observe_with_health(
        &self,
        plugin_id: &str,
        input: &GroupRoutingObserveInput,
        transient_signal: Option<GroupRoutingTransientSignal>,
    ) -> Result<GroupRoutingExecutionDirective, AppError> {
        validate_candidate(&input.candidate, &input.tenant_id)?;
        let version = self.group_routing_version(plugin_id).ok_or_else(invalid)?;
        validate_input(&GroupRoutingInput {
            tenant_id: input.tenant_id.clone(),
            seed: input.seed,
            remaining_deadline_ms: input.remaining_deadline_ms.max(1),
            config: input.config.clone(),
            candidates: vec![input.candidate.clone()],
            quota_context: None,
        })?;
        self.validate_group_routing_configuration(plugin_id, &input.config)?;
        // Observation often occurs after a long stream exhausts the
        // scheduling wait budget. It still gets bounded execution, but
        // validate_directive requires recovery_wait_ms == 0 in that case:
        // observation never extends the original request's wait deadline.
        if version == GROUP_ROUTING_V2_VERSION {
            let signal = transient_signal.ok_or_else(invalid)?;
            let v2 = GroupRoutingObserveInputV2 {
                tenant_id: input.tenant_id.clone(),
                seed: input.seed,
                remaining_deadline_ms: input.remaining_deadline_ms,
                config: input.config.clone(),
                candidate: GroupRoutingCandidateV2 {
                    tenant_id: input.candidate.tenant_id.clone(),
                    route_id: input.candidate.route_id.clone(),
                    account_id: input.candidate.account_id.clone(),
                    generation: input.candidate.generation,
                    health: input.candidate.health,
                    transient_signal: signal,
                },
                outcome: input.outcome,
            };
            let output =
                self.call_group_routing(plugin_id, &v2, EXECUTION_LIMIT.as_millis() as u64, true)?;
            let mut directive: GroupRoutingDirectiveV2 =
                serde_json::from_str(&output).map_err(|_| invalid())?;
            if !directive.transient_policy.is_valid() {
                return Err(invalid());
            }
            if input
                .config
                .get("transient_health_mode")
                .and_then(Value::as_str)
                != Some("active")
            {
                directive.transient_policy.mode = GroupRoutingTransientPolicyMode::Shadow;
            }
            let directive_v1 = v1_directive(&directive);
            let mut candidate = input.candidate.clone();
            match input.outcome {
                GroupRoutingOutcome::HardQuota => candidate.health = GroupRoutingHealth::HardQuota,
                GroupRoutingOutcome::Authentication => {
                    candidate.health = GroupRoutingHealth::Authentication
                }
                _ => {}
            }
            validate_directive(&directive_v1, &candidate, input.remaining_deadline_ms)?;
            return Ok(GroupRoutingExecutionDirective {
                directive: directive_v1,
                transient_policy: Some(directive.transient_policy),
            });
        }
        if transient_signal.is_some() {
            return Err(invalid());
        }
        let output =
            self.call_group_routing(plugin_id, input, EXECUTION_LIMIT.as_millis() as u64, true)?;
        let directive: GroupRoutingDirective =
            serde_json::from_str(&output).map_err(|_| invalid())?;
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
        Ok(GroupRoutingExecutionDirective {
            directive,
            transient_policy: None,
        })
    }

    pub fn execute_group_routing_observe(
        &self,
        plugin_id: &str,
        input: &GroupRoutingObserveInput,
    ) -> Result<GroupRoutingDirective, AppError> {
        if self.group_routing_version(plugin_id) != Some(GROUP_ROUTING_VERSION) {
            return Err(invalid());
        }
        Ok(self
            .execute_group_routing_observe_with_health(plugin_id, input, None)?
            .directive)
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
        assert!(
            encoded["candidates"][0].get("transient_signal").is_none(),
            "v1 candidate JSON must remain byte-compatible"
        );
    }

    #[test]
    fn v2_requires_explicit_active_config_and_valid_integer_thresholds() {
        let (v1, _) = fixture();
        let mut input = GroupRoutingInputV2 {
            tenant_id: v1.tenant_id,
            seed: v1.seed,
            remaining_deadline_ms: v1.remaining_deadline_ms,
            config: serde_json::json!({}),
            candidates: v1
                .candidates
                .into_iter()
                .map(|candidate| GroupRoutingCandidateV2 {
                    tenant_id: candidate.tenant_id,
                    route_id: candidate.route_id,
                    account_id: candidate.account_id,
                    generation: candidate.generation,
                    health: candidate.health,
                    transient_signal: GroupRoutingTransientSignal {
                        sample_count: 7,
                        ewma_micros: 625_000,
                        last_observed_at: 42,
                        recovery_successes: 1,
                        revision: 9,
                    },
                })
                .collect(),
            quota_context: None,
        };
        let directive = |mode| GroupRoutingDirectiveV2 {
            tenant_id: "tenant".into(),
            route_id: "route".into(),
            account_id: "account".into(),
            generation: 7,
            allow_transient_probe: true,
            cooldown_ms: 100,
            recovery_wait_ms: 10,
            recheck_ms: 100,
            stickiness: false,
            transient_policy: GroupRoutingTransientPolicy {
                mode,
                min_samples: 4,
                open_micros: 700_000,
                recover_micros: 300_000,
                min_probe_successes: 2,
            },
        };
        let shadowed = validate_v2_plan(
            &input,
            GroupRoutingPlanV2 {
                candidates: vec![directive(GroupRoutingTransientPolicyMode::Active)],
            },
        )
        .unwrap();
        assert_eq!(
            shadowed.candidates[0].transient_policy.unwrap().mode,
            GroupRoutingTransientPolicyMode::Shadow
        );
        input.config = serde_json::json!({"transient_health_mode":"active"});
        let active = validate_v2_plan(
            &input,
            GroupRoutingPlanV2 {
                candidates: vec![directive(GroupRoutingTransientPolicyMode::Active)],
            },
        )
        .unwrap();
        assert!(active.candidates[0].transient_policy.unwrap().is_active());
        let mut invalid = directive(GroupRoutingTransientPolicyMode::Active);
        invalid.transient_policy.recover_micros = 700_001;
        assert!(
            validate_v2_plan(
                &input,
                GroupRoutingPlanV2 {
                    candidates: vec![invalid]
                }
            )
            .is_err()
        );
    }

    #[test]
    fn checked_in_manifest_schema_accepts_only_the_two_group_routing_versions() {
        let schema: Value =
            serde_json::from_str(include_str!("../../schemas/plugin-manifest.schema.json"))
                .unwrap();
        let manifest = |version: &str| {
            serde_json::json!({
                "id":"health-router", "version":"1.0.0", "wit_version":"0.2.0",
                "wasm":"plugin.wasm", "capabilities":[],
                "contributions":{"group_routing":{
                    "version":version, "schema":{"type":"object"}, "default":{}
                }}
            })
        };
        assert!(
            crate::schema::validate_instance(&schema, &manifest(GROUP_ROUTING_VERSION)).is_ok()
        );
        assert!(
            crate::schema::validate_instance(&schema, &manifest(GROUP_ROUTING_V2_VERSION)).is_ok()
        );
        assert!(crate::schema::validate_instance(&schema, &manifest("group-routing-v3")).is_err());
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
