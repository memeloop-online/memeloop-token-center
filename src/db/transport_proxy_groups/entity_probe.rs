use super::*;
use crate::db::network_proxies::PROXY_AAD;

impl TransportProxyGroups {
    pub(super) async fn spawn_entity_probes(
        self: &Arc<Self>,
        pool: &AnyPool,
    ) -> Result<(), AppError> {
        let requested = {
            let managed = self.managed.read().map_err(|_| unavailable())?;
            managed
                .values()
                .filter(|item| {
                    item.entry
                        .as_ref()
                        .is_some_and(|entry| entry.probe_requested.swap(false, Ordering::AcqRel))
                })
                .flat_map(|item| item.members.iter().map(|member| member.id))
                .collect::<std::collections::BTreeSet<_>>()
        };
        for id in requested {
            sqlx::query("UPDATE network_proxies SET probe_requested = 1 WHERE id = $1")
                .bind(id.to_string())
                .execute(pool)
                .await?;
        }
        let rows = sqlx::query("SELECT id, version, proxy_ciphertext, probe_target, probe_attempts FROM network_proxies WHERE probe_requested = 1 OR (probe_enabled = 1 AND (last_probe_checked_at IS NULL OR last_probe_checked_at + probe_interval_ms <= $1)) ORDER BY COALESCE(last_probe_checked_at, 0), id LIMIT 257")
            .bind(unix_millis()).fetch_all(pool).await?;
        for row in rows {
            let Ok(permit) = self.probe_permits.clone().try_acquire_owned() else {
                break;
            };
            let id = parse_uuid(row.try_get("id")?)?;
            let version: i64 = row.try_get("version")?;
            if self
                .entity_probe_inflight
                .lock()
                .map_err(|_| unavailable())?
                .contains(&(id, version))
            {
                continue;
            }
            let proxy: String = open_private_json(
                &row.try_get::<String, _>("proxy_ciphertext")?,
                &self.key,
                PROXY_AAD,
            )?;
            let target: String = row.try_get("probe_target")?;
            let attempts: i64 = row.try_get("probe_attempts")?;
            sqlx::query(
                "UPDATE network_proxies SET probe_requested = 0 WHERE id = $1 AND version = $2",
            )
            .bind(id.to_string())
            .bind(version)
            .execute(pool)
            .await?;
            self.entity_probe_inflight
                .lock()
                .map_err(|_| unavailable())?
                .insert((id, version));
            let pool = pool.clone();
            let runtime = Arc::downgrade(self);
            tokio::spawn(async move {
                let started = std::time::Instant::now();
                let mut healthy = false;
                for _ in 0..attempts.clamp(1, 8) {
                    if super::probe::proxy_reaches(&proxy, &target).await {
                        healthy = true;
                        break;
                    }
                }
                let checked_at = unix_millis();
                let latency = i64::try_from(started.elapsed().as_millis()).unwrap_or(i64::MAX);
                let result = sqlx::query("UPDATE network_proxies SET last_probe_status = $1, last_probe_checked_at = $2, last_probe_latency_ms = $3, probe_successes = CASE WHEN $4 = 1 THEN probe_successes + 1 ELSE 0 END WHERE id = $5 AND version = $6 AND probe_target = $7 AND (last_probe_checked_at IS NULL OR last_probe_checked_at <= $2)")
                    .bind(if healthy { "healthy" } else { "unhealthy" }).bind(checked_at).bind(latency).bind(i64::from(healthy))
                    .bind(id.to_string()).bind(version).bind(&target).execute(&pool).await;
                if result.is_err() {
                    tracing::debug!(proxy_id = %id, "network proxy probe persistence deferred");
                }
                if let Some(runtime) = runtime.upgrade() {
                    if let Ok(mut inflight) = runtime.entity_probe_inflight.lock() {
                        inflight.remove(&(id, version));
                    }
                }
                drop(permit);
            });
        }
        Ok(())
    }
}
