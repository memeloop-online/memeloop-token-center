//! Explicitly owned, process-local plugin snapshots. Callers pin one snapshot
//! for an entire request, including configuration resolution and hook execution.
//! This is not an installation API: candidates must come from the verified,
//! read-only package directory. It never loads native libraries or source code.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use super::{PluginCapability, PluginRuntime, TrafficDecision, types};
use crate::error::AppError;

const MAX_REVISIONS: usize = 2;
const FAILURE_THRESHOLD: u32 = 3;
const COOLDOWN: Duration = Duration::from_secs(30);

/// A core/operator-owned allowlist, never read from guest configuration.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PluginGrant {
    pub version: String,
    pub capabilities: Vec<PluginCapability>,
    /// Pins all declared providers, endpoints, schemas and UI contributions;
    /// a matching version alone is not permission to expand host surface area.
    pub manifest_digest: String,
    /// Independently provisioned host approval, not copied from a candidate.
    /// Pins executable bytes and the trusted installer's source/artifact receipt.
    pub identity: super::PluginPackageIdentity,
}

pub fn manifest_digest(manifest: &super::PluginManifest) -> Result<String, AppError> {
    super::plugin_configuration_schema_digest(
        &serde_json::to_value(manifest).map_err(|_| AppError::Internal)?,
    )
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RevisionReason {
    Initial,
    Reload,
    Rollback,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct RevisionReceipt {
    pub revision: u64,
    pub reason: RevisionReason,
}

/// Closed host-owned stages: never include a guest error, identity or grant.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum RejectionStage {
    Grants,
    Revision,
    RequiredPolicy,
    History,
}

#[derive(Serialize)]
struct RevisionRejection {
    schema_version: u8,
    operation: RevisionReason,
    expected_revision: u64,
    stage: RejectionStage,
    error_category: &'static str,
}

impl RevisionRejection {
    fn new(
        operation: RevisionReason,
        expected_revision: u64,
        stage: RejectionStage,
        error: &AppError,
    ) -> Self {
        Self {
            schema_version: 1,
            operation,
            expected_revision,
            stage,
            error_category: error.diagnostic_category(),
        }
    }
}

fn rejected(
    operation: RevisionReason,
    expected_revision: u64,
    stage: RejectionStage,
    error: AppError,
) -> AppError {
    let diagnostic = RevisionRejection::new(operation, expected_revision, stage, &error);
    tracing::warn!(
        event = "plugin_runtime_revision_rejected",
        schema_version = diagnostic.schema_version,
        operation = ?diagnostic.operation,
        expected_revision = diagnostic.expected_revision,
        stage = ?diagnostic.stage,
        error_category = diagnostic.error_category,
        "plugin runtime revision rejected"
    );
    error
}

#[derive(Default)]
struct Circuit {
    failures: u32,
    open_until: Option<Instant>,
    probe_active: bool,
    epoch: Arc<()>,
}

struct Admission {
    epoch: Arc<()>,
    failures: u32,
}

impl Circuit {
    fn admit(&mut self, now: Instant) -> Option<Admission> {
        if let Some(until) = self.open_until {
            if now < until || self.probe_active {
                return None;
            }
            self.probe_active = true;
            self.epoch = Arc::new(());
        }
        Some(Admission {
            epoch: self.epoch.clone(),
            failures: self.failures,
        })
    }

    fn complete(&mut self, admission: &Admission, failed: bool, now: Instant) {
        // Successful recovery/reset retires the entire admission generation,
        // including slow failures admitted before it, even while closed.
        if !Arc::ptr_eq(&admission.epoch, &self.epoch) {
            return;
        }
        if failed {
            // Concurrent closed-state failures share a generation and count.
            self.failures = self.failures.saturating_add(1);
            self.probe_active = false;
            if self.failures >= FAILURE_THRESHOLD {
                self.open_until = Some(now + COOLDOWN);
                self.epoch = Arc::new(());
            }
        } else if admission.failures == self.failures {
            // A success admitted before a newer failure cannot erase it.
            *self = Self::default();
        }
    }
}

pub struct RuntimeSnapshot {
    pub receipt: RevisionReceipt,
    runtime: PluginRuntime,
    circuits: Mutex<BTreeMap<String, Circuit>>,
}

impl RuntimeSnapshot {
    /// Read-only access to the same pinned runtime for directory, schemas and
    /// configuration resolution. Do not resolve configuration on another revision.
    pub fn runtime(&self) -> &PluginRuntime {
        &self.runtime
    }

    /// Fail closed on a broken security policy; never silently skip a policy.
    /// A failure is isolated to that plugin/revision and emits no guest text.
    pub fn apply_traffic_with_config(
        &self,
        context: types::RequestContext,
        request: &serde_json::Value,
        configurations: &BTreeMap<String, serde_json::Value>,
    ) -> Result<TrafficDecision, AppError> {
        self.apply_traffic_with_config_and_memory(context, request, configurations, None)
    }

    pub(crate) fn apply_traffic_with_config_and_memory(
        &self,
        context: types::RequestContext,
        request: &serde_json::Value,
        configurations: &BTreeMap<String, serde_json::Value>,
        memory: Option<&crate::gateway_body::memory::ProxyMemoryReservation>,
    ) -> Result<TrafficDecision, AppError> {
        let mut current = request.clone();
        let mut decision = TrafficDecision {
            allow: true,
            ..Default::default()
        };
        for plugin in self.runtime.plugins.iter().filter(|plugin| {
            plugin.manifest.contributions.traffic_policy
                || plugin.manifest.contributions.request_rewrite
        }) {
            let id = &plugin.manifest.id;
            let epoch = {
                let mut circuits = self.circuits.lock().map_err(|_| AppError::Internal)?;
                let circuit = circuits.entry(id.clone()).or_default();
                let Some(epoch) = circuit.admit(Instant::now()) else {
                    return Ok(unavailable(id, "policy_circuit_open"));
                };
                epoch
            };
            // The legacy executor retains its typed WIT validation and resource
            // budgets. Restrict this invocation to exactly one installed plugin.
            let mut runtime = self.runtime.clone();
            runtime.plugins = Arc::new(vec![plugin.clone()]);
            let result = runtime.apply_traffic_with_config_and_memory(
                context.clone(),
                &current,
                configurations,
                memory,
            );
            let mut circuits = self.circuits.lock().map_err(|_| AppError::Internal)?;
            let circuit = circuits.entry(id.clone()).or_default();
            circuit.complete(&epoch, result.is_err(), Instant::now());
            match result {
                Err(_) => {
                    return Ok(unavailable(id, "policy_execution_failed"));
                }
                Ok(next) => {
                    if !next.allow {
                        return Ok(next);
                    }
                    if let Some(request) = next.request_json {
                        current = request;
                        decision.request_json = Some(current.clone());
                    }
                    if next.model.is_some() {
                        decision.model = next.model;
                    }
                    if next.upstream_account_id.is_some() {
                        decision.upstream_account_id = next.upstream_account_id;
                    }
                }
            }
        }
        Ok(decision)
    }
}

fn unavailable(plugin_id: &str, reason: &'static str) -> TrafficDecision {
    tracing::warn!(
        plugin_id,
        decision_code = reason,
        "plugin policy unavailable"
    );
    TrafficDecision {
        allow: false,
        denied_by_plugin_id: Some(plugin_id.to_owned()),
        decision_code: Some(reason),
        ..Default::default()
    }
}

struct State {
    current: Arc<RuntimeSnapshot>,
    previous: Vec<PluginRuntime>,
}

/// Atomic process-local revision publication. Database configuration already has
/// independent durable CAS/idempotency. This does not claim cross-node reload.
pub struct RuntimeRevisions {
    grants: BTreeMap<String, Vec<PluginGrant>>,
    state: RwLock<State>,
}

impl RuntimeRevisions {
    pub fn new(
        runtime: PluginRuntime,
        grants: BTreeMap<String, Vec<PluginGrant>>,
    ) -> Result<Self, AppError> {
        validate_grants(&runtime, &grants)
            .map_err(|error| rejected(RevisionReason::Initial, 0, RejectionStage::Grants, error))?;
        Ok(Self {
            grants,
            state: RwLock::new(State {
                current: snapshot(runtime, 1, RevisionReason::Initial),
                previous: Vec::new(),
            }),
        })
    }

    pub fn pin(&self) -> Result<Arc<RuntimeSnapshot>, AppError> {
        Ok(self
            .state
            .read()
            .map_err(|_| AppError::Internal)?
            .current
            .clone())
    }

    /// Build/compile the candidate before calling this; failures cannot mutate
    /// the live revision. A stale operator cannot replace a newer snapshot.
    pub fn replace(
        &self,
        expected_revision: u64,
        candidate: PluginRuntime,
    ) -> Result<RevisionReceipt, AppError> {
        validate_grants(&candidate, &self.grants).map_err(|error| {
            rejected(
                RevisionReason::Reload,
                expected_revision,
                RejectionStage::Grants,
                error,
            )
        })?;
        let mut state = self.state.write().map_err(|_| AppError::Internal)?;
        let revision = next_revision(&state, expected_revision).map_err(|error| {
            rejected(
                RevisionReason::Reload,
                expected_revision,
                RejectionStage::Revision,
                error,
            )
        })?;
        validate_policy_transition(&state.current.runtime, &candidate).map_err(|error| {
            rejected(
                RevisionReason::Reload,
                expected_revision,
                RejectionStage::RequiredPolicy,
                error,
            )
        })?;
        let old = state.current.runtime.clone();
        state.previous.push(old);
        if state.previous.len() > MAX_REVISIONS {
            state.previous.remove(0);
        }
        state.current = snapshot(candidate, revision, RevisionReason::Reload);
        Ok(state.current.receipt)
    }

    pub fn rollback(&self, expected_revision: u64) -> Result<RevisionReceipt, AppError> {
        let mut state = self.state.write().map_err(|_| AppError::Internal)?;
        let revision = next_revision(&state, expected_revision).map_err(|error| {
            rejected(
                RevisionReason::Rollback,
                expected_revision,
                RejectionStage::Revision,
                error,
            )
        })?;
        let runtime = state.previous.last().ok_or_else(|| {
            rejected(
                RevisionReason::Rollback,
                expected_revision,
                RejectionStage::History,
                AppError::BadRequest("no previous plugin revision".into()),
            )
        })?;
        validate_grants(runtime, &self.grants).map_err(|error| {
            rejected(
                RevisionReason::Rollback,
                expected_revision,
                RejectionStage::Grants,
                error,
            )
        })?;
        validate_policy_transition(&state.current.runtime, runtime).map_err(|error| {
            rejected(
                RevisionReason::Rollback,
                expected_revision,
                RejectionStage::RequiredPolicy,
                error,
            )
        })?;
        let runtime = state.previous.pop().ok_or(AppError::Internal)?;
        state.current = snapshot(runtime, revision, RevisionReason::Rollback);
        Ok(state.current.receipt)
    }
}

fn validate_policy_transition(
    current: &PluginRuntime,
    candidate: &PluginRuntime,
) -> Result<(), AppError> {
    for old in current.plugins.iter() {
        let next = candidate
            .plugins
            .iter()
            .find(|plugin| plugin.manifest.id == old.manifest.id)
            .ok_or(AppError::Forbidden)?;
        if (old.manifest.contributions.traffic_policy
            && !next.manifest.contributions.traffic_policy)
            || (old.manifest.contributions.request_rewrite
                && !next.manifest.contributions.request_rewrite)
        {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

fn next_revision(state: &State, expected: u64) -> Result<u64, AppError> {
    if state.current.receipt.revision != expected {
        return Err(AppError::Conflict("plugin runtime revision changed".into()));
    }
    expected.checked_add(1).ok_or(AppError::Internal)
}

pub(super) fn snapshot(
    runtime: PluginRuntime,
    revision: u64,
    reason: RevisionReason,
) -> Arc<RuntimeSnapshot> {
    tracing::info!(
        event = "plugin_runtime_revision_published",
        schema_version = 1_u8,
        revision,
        reason = ?reason,
        "plugin runtime revision published"
    );
    Arc::new(RuntimeSnapshot {
        receipt: RevisionReceipt { revision, reason },
        runtime,
        circuits: Mutex::default(),
    })
}

pub(super) fn validate_grants(
    runtime: &PluginRuntime,
    grants: &BTreeMap<String, Vec<PluginGrant>>,
) -> Result<(), AppError> {
    // Grants describe the complete required inventory; omission is not disable.
    if runtime.plugins.len() != grants.len() {
        return Err(AppError::Forbidden);
    }
    for plugin in runtime.plugins.iter() {
        let manifest = &plugin.manifest;
        let Some(versions) = grants.get(&manifest.id) else {
            return Err(AppError::Forbidden);
        };
        let digest = manifest_digest(manifest)?;
        if !versions.iter().any(|grant| {
            digest == grant.manifest_digest
                && manifest.version == grant.version
                && plugin.identity == grant.identity
                && grant.identity.provenance.as_ref().is_some_and(|receipt| {
                    receipt.format_version == 1
                        && matches!(
                            receipt.signature_policy.as_str(),
                            "cosign-public-key" | "cosign-keyless"
                        )
                        && !receipt.source.is_empty()
                        && valid_digest(&receipt.digest)
                })
                && grant
                    .identity
                    .component_sha256
                    .as_ref()
                    .is_none_or(|digest| valid_digest(digest))
                && manifest
                    .capabilities
                    .iter()
                    .all(|capability| capability_allowed(capability, &grant.capabilities))
        }) {
            return Err(AppError::Forbidden);
        }
    }
    Ok(())
}

fn valid_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

fn capability_allowed(capability: &PluginCapability, grants: &[PluginCapability]) -> bool {
    grants.iter().any(|grant| match (capability, grant) {
        (
            PluginCapability::Http { allowed_origins },
            PluginCapability::Http {
                allowed_origins: approved,
            },
        ) => allowed_origins
            .iter()
            .all(|origin| approved.contains(origin)),
        _ => capability == grant,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity() -> super::super::PluginPackageIdentity {
        super::super::PluginPackageIdentity {
            component_sha256: None,
            provenance: Some(super::super::PluginInstallProvenance {
                format_version: 1,
                source: "registry.example/plugins/policy".into(),
                digest: format!("sha256:{}", "a".repeat(64)),
                signature_policy: "cosign-public-key".into(),
            }),
        }
    }

    #[test]
    fn revisions_are_atomic_pinned_and_monotonic() {
        let manager = RuntimeRevisions::new(PluginRuntime::default(), BTreeMap::new()).unwrap();
        let pinned = manager.pin().unwrap();
        assert_eq!(
            manager
                .replace(1, PluginRuntime::default())
                .unwrap()
                .revision,
            2
        );
        assert!(manager.replace(1, PluginRuntime::default()).is_err());
        assert_eq!(manager.pin().unwrap().receipt.revision, 2);
        assert_eq!(pinned.receipt.revision, 1);
        let receipt = manager.rollback(2).unwrap();
        assert_eq!(receipt.revision, 3);
        assert_eq!(receipt.reason, RevisionReason::Rollback);
        assert!(manager.rollback(3).is_err());
        assert_eq!(manager.pin().unwrap().receipt.revision, 3);
    }

    #[test]
    fn concurrent_publishers_have_exactly_one_cas_winner() {
        let manager = RuntimeRevisions::new(PluginRuntime::default(), BTreeMap::new()).unwrap();
        let pinned = manager.pin().unwrap();
        let barrier = std::sync::Barrier::new(2);
        let results = std::thread::scope(|scope| {
            let publish = || {
                barrier.wait();
                manager.replace(1, PluginRuntime::default())
            };
            let first = scope.spawn(publish);
            let second = scope.spawn(publish);
            [first.join().unwrap(), second.join().unwrap()]
        });
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter(|result| matches!(result, Err(AppError::Conflict(_))))
                .count(),
            1
        );
        assert_eq!(manager.pin().unwrap().receipt.revision, 2);
        assert_eq!(pinned.receipt.revision, 1);
        assert_eq!(manager.state.read().unwrap().previous.len(), 1);
        assert_eq!(manager.rollback(2).unwrap().revision, 3);
        assert!(manager.rollback(3).is_err());
    }

    #[test]
    fn rollback_history_is_bounded_and_rejection_preserves_snapshot_identity() {
        let manager = RuntimeRevisions::new(PluginRuntime::default(), BTreeMap::new()).unwrap();
        let initial = manager.pin().unwrap();
        for revision in 1..=5 {
            manager.replace(revision, PluginRuntime::default()).unwrap();
            assert_eq!(
                manager.state.read().unwrap().previous.len(),
                (revision as usize).min(MAX_REVISIONS)
            );
        }
        let current = manager.pin().unwrap();
        assert!(manager.rollback(5).is_err());
        assert!(Arc::ptr_eq(&current, &manager.pin().unwrap()));
        assert_eq!(manager.state.read().unwrap().previous.len(), MAX_REVISIONS);
        assert_eq!(manager.rollback(6).unwrap().revision, 7);
        assert_eq!(manager.rollback(7).unwrap().revision, 8);
        let last = manager.pin().unwrap();
        assert!(manager.rollback(8).is_err());
        assert!(Arc::ptr_eq(&last, &manager.pin().unwrap()));
        assert_eq!(initial.receipt.revision, 1);
        assert_eq!(current.receipt.revision, 6);
    }

    #[test]
    fn rejection_diagnostic_is_versioned_and_drops_untrusted_error_text() {
        let marker = "secret-config-token-provider-response";
        for error in [
            AppError::BadRequest(marker.into()),
            AppError::Upstream(marker.into()),
            AppError::Conflict(marker.into()),
            AppError::Storage(marker.into()),
        ] {
            let diagnostic =
                RevisionRejection::new(RevisionReason::Reload, 7, RejectionStage::Grants, &error);
            let value = serde_json::to_value(diagnostic).unwrap();
            assert_eq!(
                value,
                serde_json::json!({
                    "schema_version": 1,
                    "operation": "reload",
                    "expected_revision": 7,
                    "stage": "grants",
                    "error_category": error.diagnostic_category(),
                })
            );
            assert!(!value.to_string().contains(marker));
        }
    }

    #[test]
    fn grants_pin_versions_capabilities_and_contributions() {
        let manifest: super::super::PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "policy", "version": "1.0.0", "wit_version": "0.2.0", "wasm": null
        }))
        .unwrap();
        let grant = PluginGrant {
            version: manifest.version.clone(),
            capabilities: vec![],
            manifest_digest: manifest_digest(&manifest).unwrap(),
            identity: identity(),
        };
        let candidate = |manifest| PluginRuntime {
            plugins: Arc::new(vec![super::super::LoadedPlugin {
                manifest,
                component: None,
                service_data_component: None,
                ui_modules: BTreeMap::new(),
                configuration_validator: None,
                routing_validator: None,
                routing_fingerprint: String::new(),
                identity: identity(),
            }]),
            ..PluginRuntime::default()
        };
        let grants = BTreeMap::from([("policy".into(), vec![grant])]);
        assert!(validate_grants(&PluginRuntime::default(), &grants).is_err());
        assert!(validate_grants(&candidate(manifest.clone()), &grants).is_ok());
        let mut changed = manifest.clone();
        changed.version = "1.0.1".into();
        assert!(validate_grants(&candidate(changed), &grants).is_err());
        let mut changed = manifest.clone();
        changed.capabilities.push(PluginCapability::Kv);
        assert!(validate_grants(&candidate(changed), &grants).is_err());
        let mut changed = manifest;
        changed.contributions.traffic_policy = true;
        assert!(validate_grants(&candidate(changed), &grants).is_err());
    }

    #[test]
    fn approved_upgrade_roundtrip_preserves_required_policy_inventory() {
        let manifest: super::super::PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "policy", "version": "1.0.0", "wit_version": "0.2.0", "wasm": null,
            "contributions": {"traffic_policy": true}
        }))
        .unwrap();
        let mut upgrade = manifest.clone();
        upgrade.version = "2.0.0".into();
        let mut disabled = upgrade.clone();
        disabled.contributions.traffic_policy = false;
        let approved = |manifest: &super::super::PluginManifest| PluginGrant {
            version: manifest.version.clone(),
            capabilities: vec![],
            manifest_digest: manifest_digest(manifest).unwrap(),
            identity: identity(),
        };
        let candidate = |manifest| PluginRuntime {
            plugins: Arc::new(vec![super::super::LoadedPlugin {
                manifest,
                component: None,
                service_data_component: None,
                ui_modules: BTreeMap::new(),
                configuration_validator: None,
                routing_validator: None,
                routing_fingerprint: String::new(),
                identity: identity(),
            }]),
            ..PluginRuntime::default()
        };
        let grants = BTreeMap::from([(
            "policy".into(),
            vec![approved(&manifest), approved(&upgrade), approved(&disabled)],
        )]);
        let manager = RuntimeRevisions::new(candidate(manifest), grants).unwrap();
        let original = manager.pin().unwrap();
        assert!(manager.replace(1, PluginRuntime::default()).is_err());
        assert!(
            manager.replace(1, candidate(disabled)).is_err(),
            "approval of bytes is not disable authority"
        );
        let mut forged = candidate(upgrade.clone());
        Arc::make_mut(&mut forged.plugins)[0].identity.provenance = None;
        assert!(manager.replace(1, forged).is_err());
        assert!(Arc::ptr_eq(&original, &manager.pin().unwrap()));
        assert!(manager.state.read().unwrap().previous.is_empty());
        assert_eq!(manager.replace(1, candidate(upgrade)).unwrap().revision, 2);
        assert_eq!(manager.rollback(2).unwrap().revision, 3);
    }

    #[test]
    fn stale_success_cannot_clear_new_failures_or_steal_half_open_probe() {
        let now = Instant::now();
        let mut circuit = Circuit::default();
        let stale = circuit.admit(now).unwrap();
        for _ in 0..3 {
            let token = circuit.admit(now).unwrap();
            circuit.complete(&token, true, now);
        }
        circuit.complete(&stale, false, now);
        assert!(circuit.admit(now).is_none());
        let probe = circuit.admit(now + COOLDOWN).unwrap();
        circuit.complete(&stale, false, now + COOLDOWN);
        assert!(circuit.admit(now + COOLDOWN).is_none());
        circuit.complete(&probe, false, now + COOLDOWN);
        assert!(circuit.admit(now + COOLDOWN).is_some());
    }

    #[test]
    fn recovered_circuit_ignores_old_failures_but_counts_new_concurrent_failures() {
        let now = Instant::now();
        let mut circuit = Circuit::default();
        let old = (0..FAILURE_THRESHOLD)
            .map(|_| circuit.admit(now).unwrap())
            .collect::<Vec<_>>();
        let success = circuit.admit(now).unwrap();
        circuit.complete(&success, false, now);
        for admission in &old {
            circuit.complete(admission, true, now);
        }
        assert_eq!(circuit.failures, 0);
        let current = (0..FAILURE_THRESHOLD)
            .map(|_| circuit.admit(now).unwrap())
            .collect::<Vec<_>>();
        for admission in &current {
            circuit.complete(admission, true, now);
        }
        assert!(circuit.admit(now).is_none());
        let probe = circuit.admit(now + COOLDOWN).unwrap();
        circuit.complete(&probe, false, now + COOLDOWN);
        for admission in old.iter().chain(&current) {
            circuit.complete(admission, true, now + COOLDOWN);
        }
        assert_eq!(circuit.failures, 0);
        assert!(circuit.admit(now + COOLDOWN).is_some());
    }

    #[test]
    fn http_capability_origins_are_a_subset_not_vector_equality() {
        let allowed = vec![PluginCapability::Http {
            allowed_origins: vec!["https://a.example".into(), "https://b.example".into()],
        }];
        assert!(capability_allowed(
            &PluginCapability::Http {
                allowed_origins: vec!["https://b.example".into()]
            },
            &allowed
        ));
        assert!(!capability_allowed(
            &PluginCapability::Http {
                allowed_origins: vec!["https://c.example".into()]
            },
            &allowed
        ));
    }
}
