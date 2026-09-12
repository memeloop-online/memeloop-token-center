use std::{collections::BTreeMap, future::Future, sync::Arc, time::Instant};

use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use super::{
    CachedPluginConfigurations, PLUGIN_CONFIGURATION_CACHE_BYTES,
    PLUGIN_CONFIGURATION_CACHE_ENTRIES, PLUGIN_CONFIGURATION_CACHE_TTL, PluginRuntime,
    StoredPluginConfiguration, estimated_json_bytes, plugin_configuration_schema_digest,
};
use crate::error::AppError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigurationSource {
    Default,
    Global,
    Tenant,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedConfigurationRevision {
    pub source: ConfigurationSource,
    /// Zero denotes the installed manifest default, not a database revision.
    pub version: i64,
    pub schema_digest: String,
}

/// A bounded-stale resolved view, not a strict revocation token. Values and
/// effective source/version metadata come from the same database statement.
#[derive(Clone)]
pub struct ResolvedTrafficSnapshot {
    pub tenant_id: Uuid,
    pub values: BTreeMap<String, Value>,
    pub revisions: BTreeMap<String, ResolvedConfigurationRevision>,
    /// Process-local deadline anchored before the database read. Retaining a
    /// snapshot does not extend this deadline or grant strict revocation.
    pub freshness_deadline: Instant,
}

impl PluginRuntime {
    pub async fn resolved_traffic_snapshot(
        &self,
        tenant_id: Uuid,
    ) -> Result<ResolvedTrafficSnapshot, AppError> {
        self.resolve_snapshot_with(
            tenant_id,
            || async {
                let database = &self.kv.as_ref().ok_or(AppError::Internal)?.database;
                database.plugin_configuration_layers(tenant_id).await
            },
            Instant::now,
        )
        .await
    }

    // Inject reads and a monotonic clock for deterministic interleavings. No
    // global test hooks, sleeping or production-only behavior is necessary.
    async fn resolve_snapshot_with<F, Fut, C>(
        &self,
        tenant_id: Uuid,
        mut read: F,
        now: C,
    ) -> Result<ResolvedTrafficSnapshot, AppError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<Vec<StoredPluginConfiguration>, AppError>>,
        C: Fn() -> Instant,
    {
        let configurable: Vec<_> = self
            .plugins
            .iter()
            .filter(|plugin| {
                (plugin.manifest.contributions.traffic_policy
                    || plugin.manifest.contributions.request_rewrite)
                    && plugin.manifest.contributions.configuration.is_some()
            })
            .collect();
        if configurable.is_empty() {
            return Ok(ResolvedTrafficSnapshot {
                tenant_id,
                values: BTreeMap::new(),
                revisions: BTreeMap::new(),
                freshness_deadline: now() + PLUGIN_CONFIGURATION_CACHE_TTL,
            });
        }
        // At most one retry: continuously changing policy fails closed rather
        // than spinning or returning a known-invalidated read.
        for _ in 0..2 {
            let (epoch, read_started) = {
                let cache = self.configuration_cache.read().await;
                let started = now();
                if let Some(cached) = cache.entries.get(&tenant_id)
                    && started.saturating_duration_since(cached.loaded_at)
                        < PLUGIN_CONFIGURATION_CACHE_TTL
                {
                    return Ok(cached.snapshot.clone());
                }
                (cache.epoch.clone(), started)
            };
            let layers = tokio::time::timeout(PLUGIN_CONFIGURATION_CACHE_TTL, read())
                .await
                .map_err(|_| AppError::Storage("plugin configuration read timed out".into()))??;
            let mut snapshot = ResolvedTrafficSnapshot {
                tenant_id,
                values: BTreeMap::new(),
                revisions: BTreeMap::new(),
                freshness_deadline: read_started + PLUGIN_CONFIGURATION_CACHE_TTL,
            };
            for plugin in &configurable {
                let contribution = plugin
                    .manifest
                    .contributions
                    .configuration
                    .as_ref()
                    .expect("filtered configurable plugin");
                let stored = layers
                    .iter()
                    .find(|layer| {
                        layer.plugin_id == plugin.manifest.id && layer.tenant_id == Some(tenant_id)
                    })
                    .or_else(|| {
                        layers.iter().find(|layer| {
                            layer.plugin_id == plugin.manifest.id && layer.tenant_id.is_none()
                        })
                    });
                let value = stored
                    .map(|row| row.value.clone())
                    .unwrap_or_else(|| contribution.default.clone());
                plugin
                    .configuration_validator
                    .as_ref()
                    .ok_or(AppError::Internal)?
                    .validate(&value)?;
                let revision = match stored {
                    Some(row) => ResolvedConfigurationRevision {
                        source: if row.tenant_id.is_some() {
                            ConfigurationSource::Tenant
                        } else {
                            ConfigurationSource::Global
                        },
                        version: row.version,
                        schema_digest: row.schema_digest.clone(),
                    },
                    None => ResolvedConfigurationRevision {
                        source: ConfigurationSource::Default,
                        version: 0,
                        schema_digest: plugin_configuration_schema_digest(&contribution.schema)?,
                    },
                };
                snapshot.values.insert(plugin.manifest.id.clone(), value);
                snapshot
                    .revisions
                    .insert(plugin.manifest.id.clone(), revision);
            }
            if self
                .cache_snapshot(&epoch, read_started, snapshot.clone(), &now)
                .await
            {
                return Ok(snapshot);
            }
        }
        Err(AppError::Conflict(
            "plugin configuration changed while resolving".into(),
        ))
    }

    pub(super) async fn cache_snapshot<C: Fn() -> Instant>(
        &self,
        epoch: &Arc<()>,
        read_started: Instant,
        snapshot: ResolvedTrafficSnapshot,
        now: C,
    ) -> bool {
        let estimated_bytes = snapshot
            .values
            .iter()
            .fold(0usize, |total, (id, value)| {
                total
                    .saturating_add(id.len())
                    .saturating_add(estimated_json_bytes(value))
            })
            .saturating_add(
                snapshot
                    .revisions
                    .iter()
                    .fold(0usize, |total, (id, revision)| {
                        total
                            .saturating_add(id.len())
                            .saturating_add(revision.schema_digest.len())
                            .saturating_add(32)
                    }),
            );
        let mut state = self.configuration_cache.write().await;
        let current_time = now();
        // TTL is anchored before the read, never restarted by delayed refill.
        if !Arc::ptr_eq(epoch, &state.epoch)
            || current_time.saturating_duration_since(read_started)
                >= PLUGIN_CONFIGURATION_CACHE_TTL
        {
            return false;
        }
        if estimated_bytes > PLUGIN_CONFIGURATION_CACHE_BYTES {
            return true;
        }
        let cache = &mut state.entries;
        cache.retain(|_, entry| {
            current_time.saturating_duration_since(entry.loaded_at) < PLUGIN_CONFIGURATION_CACHE_TTL
        });
        // A late older read must not replace a newer already-published read.
        if cache
            .get(&snapshot.tenant_id)
            .is_some_and(|entry| entry.loaded_at > read_started)
        {
            return false;
        }
        cache.remove(&snapshot.tenant_id);
        loop {
            let bytes = cache.values().fold(0usize, |total, entry| {
                total.saturating_add(entry.estimated_bytes)
            });
            if cache.len() < PLUGIN_CONFIGURATION_CACHE_ENTRIES
                && bytes.saturating_add(estimated_bytes) <= PLUGIN_CONFIGURATION_CACHE_BYTES
            {
                break;
            }
            let Some(oldest) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.loaded_at)
                .map(|(id, _)| *id)
            else {
                break;
            };
            cache.remove(&oldest);
        }
        cache.insert(
            snapshot.tenant_id,
            CachedPluginConfigurations {
                loaded_at: read_started,
                snapshot,
                estimated_bytes,
            },
        );
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::sync::oneshot;

    fn runtime() -> PluginRuntime {
        let manifest: super::super::PluginManifest = serde_json::from_value(serde_json::json!({
            "id": "policy", "version": "1.0.0", "wit_version": "0.2.0", "wasm": null,
            "contributions": {"traffic_policy": true, "configuration": {
                "schema": {"type": "object"}, "default": {"mode": "default"}
            }}
        }))
        .unwrap();
        let validator = crate::schema::compile(
            &manifest
                .contributions
                .configuration
                .as_ref()
                .unwrap()
                .schema,
        )
        .unwrap();
        PluginRuntime {
            plugins: Arc::new(vec![super::super::LoadedPlugin {
                manifest,
                component: None,
                configuration_validator: Some(validator),
            }]),
            ..PluginRuntime::default()
        }
    }

    fn row(version: i64, tenant_id: Option<Uuid>) -> StoredPluginConfiguration {
        StoredPluginConfiguration {
            plugin_id: "policy".into(),
            tenant_id,
            value: serde_json::json!({"version": version}),
            schema_digest: "a".repeat(64),
            version,
            updated_at: version,
        }
    }

    #[tokio::test]
    async fn two_runtimes_fence_local_old_reads_and_bound_remote_staleness() {
        let first = runtime();
        let second = runtime();
        let tenant = Uuid::from_u128(1);
        // The injected read source returns a complete statement snapshot. Gates
        // model an old DB read completing after a committed API update.
        let database_rows = Mutex::new(vec![row(1, None)]);
        let clock = Mutex::new(Instant::now());
        let now = || *clock.lock().unwrap();
        let initial = second
            .resolve_snapshot_with(
                tenant,
                || async { Ok(database_rows.lock().unwrap().clone()) },
                now,
            )
            .await
            .unwrap();
        assert_eq!(initial.revisions["policy"].version, 1);
        let reads = AtomicUsize::new(0);
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let mut started = Some(started_tx);
        let mut release = Some(release_rx);
        let reader = first.resolve_snapshot_with(
            tenant,
            || {
                reads.fetch_add(1, Ordering::SeqCst);
                let rows = database_rows.lock().unwrap().clone();
                let gate = release.take();
                let signal = started.take();
                async move {
                    if let Some(signal) = signal {
                        signal.send(()).unwrap();
                    }
                    if let Some(gate) = gate {
                        gate.await.unwrap();
                    }
                    Ok(rows)
                }
            },
            now,
        );
        let writer = async {
            started_rx.await.unwrap();
            *database_rows.lock().unwrap() = vec![row(2, None)];
            first.invalidate_configuration_cache(None).await;
            // Another process has no broadcast invalidation. Staleness is
            // explicitly permitted until its original five-second deadline.
            let cached = second
                .resolve_snapshot_with(
                    tenant,
                    || async { panic!("remote cache should still be valid") },
                    now,
                )
                .await
                .unwrap();
            assert_eq!(cached.revisions["policy"].version, 1);
            release_tx.send(()).unwrap();
        };
        let (resolved, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(reader, writer)
        })
        .await
        .expect("controlled read/write interleaving must finish");
        let resolved = resolved.unwrap();
        assert_eq!(
            reads.load(Ordering::SeqCst),
            2,
            "invalidated read must retry"
        );
        assert_eq!(resolved.revisions["policy"].version, 2);
        assert_eq!(resolved.values["policy"]["version"], 2);
        assert_eq!(
            first.configuration_cache.read().await.entries[&tenant]
                .snapshot
                .revisions["policy"]
                .version,
            2
        );
        *clock.lock().unwrap() += PLUGIN_CONFIGURATION_CACHE_TTL;
        let refreshed = second
            .resolve_snapshot_with(
                tenant,
                || async { Ok(database_rows.lock().unwrap().clone()) },
                now,
            )
            .await
            .unwrap();
        assert_eq!(refreshed.revisions["policy"].version, 2);
    }

    #[tokio::test]
    async fn delayed_refill_does_not_restart_ttl() {
        let runtime = runtime();
        let tenant = Uuid::from_u128(1);
        let started = Instant::now();
        let clock = Mutex::new(started);
        let now = || *clock.lock().unwrap();
        let old = runtime
            .resolve_snapshot_with(
                tenant,
                || async {
                    *clock.lock().unwrap() = started + std::time::Duration::from_secs(4);
                    Ok(vec![row(1, None)])
                },
                now,
            )
            .await
            .unwrap();
        assert_eq!(old.revisions["policy"].version, 1);
        *clock.lock().unwrap() = started + PLUGIN_CONFIGURATION_CACHE_TTL;
        let fresh = runtime
            .resolve_snapshot_with(tenant, || async { Ok(vec![row(2, None)]) }, now)
            .await
            .unwrap();
        assert_eq!(fresh.revisions["policy"].version, 2);
    }

    #[tokio::test]
    async fn expired_inflight_read_is_neither_returned_nor_cached() {
        let runtime = runtime();
        let tenant = Uuid::from_u128(1);
        let started = Instant::now();
        let clock = Mutex::new(started);
        let mut reads = 0;
        let resolved = runtime
            .resolve_snapshot_with(
                tenant,
                || {
                    reads += 1;
                    let version = reads;
                    if reads == 1 {
                        *clock.lock().unwrap() = started + PLUGIN_CONFIGURATION_CACHE_TTL;
                    }
                    async move { Ok(vec![row(version, None)]) }
                },
                || *clock.lock().unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(reads, 2);
        assert_eq!(resolved.revisions["policy"].version, 2);
    }

    #[tokio::test]
    async fn repeated_invalidations_fail_closed_without_unbounded_retry() {
        let runtime = runtime();
        let result = runtime
            .resolve_snapshot_with(
                Uuid::from_u128(1),
                || async {
                    runtime.invalidate_configuration_cache(None).await;
                    Ok(vec![row(1, None)])
                },
                Instant::now,
            )
            .await;
        assert!(matches!(result, Err(AppError::Conflict(_))));
        assert!(runtime.configuration_cache.read().await.entries.is_empty());
    }

    #[tokio::test]
    async fn revisions_follow_effective_tenant_global_and_default_layers() {
        let runtime = runtime();
        let tenant = Uuid::from_u128(1);
        let resolved = runtime
            .resolve_snapshot_with(
                tenant,
                || async { Ok(vec![row(9, None), row(2, Some(tenant))]) },
                Instant::now,
            )
            .await
            .unwrap();
        assert_eq!(
            resolved.revisions["policy"].source,
            ConfigurationSource::Tenant
        );
        assert_eq!(resolved.revisions["policy"].version, 2);
        runtime.invalidate_configuration_cache(Some(tenant)).await;
        let global = runtime
            .resolve_snapshot_with(tenant, || async { Ok(vec![row(9, None)]) }, Instant::now)
            .await
            .unwrap();
        assert_eq!(
            global.revisions["policy"].source,
            ConfigurationSource::Global
        );
        assert_eq!(global.revisions["policy"].version, 9);
        runtime.invalidate_configuration_cache(None).await;
        let default = runtime
            .resolve_snapshot_with(tenant, || async { Ok(vec![]) }, Instant::now)
            .await
            .unwrap();
        assert_eq!(
            default.revisions["policy"].source,
            ConfigurationSource::Default
        );
        assert_eq!(default.revisions["policy"].version, 0);
        assert_eq!(
            default.values["policy"],
            serde_json::json!({"mode": "default"})
        );
    }
}
