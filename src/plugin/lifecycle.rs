//! Explicitly owned, process-local plugin snapshots. Callers pin one snapshot
//! for an entire request, including configuration resolution and hook execution.
//! This is not an installation API: candidates must come from the verified,
//! read-only package directory. It never loads native libraries or source code.

use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, RwLock},
    time::{Duration, Instant},
};

use serde::Serialize;

use super::{PluginCapability, PluginRuntime, TrafficDecision, types};
use crate::error::AppError;

const MAX_REVISIONS: usize = 2;
const FAILURE_THRESHOLD: u32 = 3;
const COOLDOWN: Duration = Duration::from_secs(30);

/// A core/operator-owned allowlist, never read from guest configuration.
#[derive(Clone, Debug)]
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

#[derive(Default)]
struct Circuit {
    failures: u32,
    open_until: Option<Instant>,
    probe_active: bool,
    epoch: Arc<()>,
}

impl Circuit {
    fn admit(&mut self, now: Instant) -> Option<Arc<()>> {
        if let Some(until) = self.open_until {
            if now < until || self.probe_active {
                return None;
            }
            self.probe_active = true;
            self.epoch = Arc::new(());
        }
        Some(self.epoch.clone())
    }

    fn complete(&mut self, epoch: &Arc<()>, failed: bool, now: Instant) {
        if failed {
            // Closed-state concurrent failures count, but completions admitted
            // before opening cannot extend or disturb an open/half-open epoch.
            if self.open_until.is_some() && !Arc::ptr_eq(epoch, &self.epoch) {
                return;
            }
            self.failures = self.failures.saturating_add(1);
            self.probe_active = false;
            self.epoch = Arc::new(());
            if self.failures >= FAILURE_THRESHOLD {
                self.open_until = Some(now + COOLDOWN);
            }
        } else if Arc::ptr_eq(epoch, &self.epoch) {
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
            let result =
                runtime.apply_traffic_with_config(context.clone(), &current, configurations);
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
        validate_grants(&runtime, &grants)?;
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
        validate_grants(&candidate, &self.grants)?;
        let mut state = self.state.write().map_err(|_| AppError::Internal)?;
        let revision = next_revision(&state, expected_revision)?;
        validate_policy_transition(&state.current.runtime, &candidate)?;
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
        let revision = next_revision(&state, expected_revision)?;
        let runtime = state
            .previous
            .last()
            .ok_or_else(|| AppError::BadRequest("no previous plugin revision".into()))?;
        validate_grants(runtime, &self.grants)?;
        validate_policy_transition(&state.current.runtime, runtime)?;
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

fn snapshot(runtime: PluginRuntime, revision: u64, reason: RevisionReason) -> Arc<RuntimeSnapshot> {
    tracing::info!(revision, reason = ?reason, "plugin runtime revision published");
    Arc::new(RuntimeSnapshot {
        receipt: RevisionReceipt { revision, reason },
        runtime,
        circuits: Mutex::default(),
    })
}

fn validate_grants(
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
                        && receipt.signature_policy == "cosign-public-key"
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
                configuration_validator: None,
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
                configuration_validator: None,
                identity: identity(),
            }]),
            ..PluginRuntime::default()
        };
        let grants = BTreeMap::from([(
            "policy".into(),
            vec![approved(&manifest), approved(&upgrade), approved(&disabled)],
        )]);
        let manager = RuntimeRevisions::new(candidate(manifest), grants).unwrap();
        assert!(manager.replace(1, PluginRuntime::default()).is_err());
        assert!(
            manager.replace(1, candidate(disabled)).is_err(),
            "approval of bytes is not disable authority"
        );
        let mut forged = candidate(upgrade.clone());
        Arc::make_mut(&mut forged.plugins)[0].identity.provenance = None;
        assert!(manager.replace(1, forged).is_err());
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
