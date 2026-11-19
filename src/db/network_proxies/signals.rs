use super::*;
use crate::db::egress::RequestEgressAttempt;
use crate::plugin::network_proxy_strategy::ProxyCandidate;

pub(crate) async fn stage_attempt(
    pool: &AnyPool,
    attempt: &RequestEgressAttempt,
) -> Result<(), AppError> {
    let (Some(proxy), Some(version)) = (attempt.selected.member_id, attempt.selected.proxy_version)
    else {
        return Ok(());
    };
    sqlx::query("INSERT INTO network_proxy_attempt_signals (request_id, attempt_id, proxy_id, proxy_version, observed_at, request_payload_bytes, response_payload_bytes, next_refresh_at) SELECT $1,$2,p.id,$3,$4,$5,$6,0 FROM network_proxies p JOIN upstream_accounts a ON a.tenant_id = p.tenant_id WHERE p.id = $7 AND a.id = $8 AND p.version >= $3 ON CONFLICT (request_id, attempt_id) DO UPDATE SET request_payload_bytes = CASE WHEN network_proxy_attempt_signals.request_payload_bytes IS NULL OR excluded.request_payload_bytes > network_proxy_attempt_signals.request_payload_bytes THEN excluded.request_payload_bytes ELSE network_proxy_attempt_signals.request_payload_bytes END, response_payload_bytes = CASE WHEN network_proxy_attempt_signals.response_payload_bytes IS NULL OR excluded.response_payload_bytes > network_proxy_attempt_signals.response_payload_bytes THEN excluded.response_payload_bytes ELSE network_proxy_attempt_signals.response_payload_bytes END WHERE network_proxy_attempt_signals.proxy_id = excluded.proxy_id AND network_proxy_attempt_signals.proxy_version = excluded.proxy_version")
        .bind(attempt.request_id.to_string()).bind(attempt.attempt_id.to_string()).bind(version).bind(attempt.observed_at)
        .bind(attempt.request_payload_bytes).bind(attempt.response_payload_bytes).bind(proxy.to_string()).bind(attempt.upstream_account_id.to_string())
        .execute(pool).await?;
    Ok(())
}

fn outcome(row: &AnyRow, now: i64) -> Result<&'static str, AppError> {
    let status: Option<i64> = row.try_get("status_code")?;
    let error: Option<String> = row.try_get("error_code")?;
    let cause: Option<String> = row.try_get("terminal_cause_code")?;
    if error
        .as_deref()
        .is_some_and(|value| matches!(value, "client_cancelled" | "request_cancelled"))
        || cause
            .as_deref()
            .is_some_and(|value| matches!(value, "client_cancelled" | "request_cancelled"))
    {
        return Ok("cancelled");
    }
    let exact_final = row.try_get::<i64, _>("final_response")? != 0
        && row.try_get::<Option<String>, _>("final_account")?
            == Some(row.try_get("upstream_account_id")?)
        && row.try_get::<Option<String>, _>("final_route")? == Some(row.try_get("route_id")?);
    if exact_final && row.try_get::<i64, _>("response_body_complete")? != 0 {
        if status.is_some_and(|status| (200..300).contains(&status))
            && error.as_deref().unwrap_or("").is_empty()
            && cause.as_deref().unwrap_or("").is_empty()
        {
            return Ok("succeeded");
        }
        if status.is_some_and(|status| (400..600).contains(&status))
            && row.try_get::<Option<String>, _>("failure_kind")?.is_none()
            && error.as_deref().is_some_and(|error| {
                matches!(
                    error,
                    "upstream_error"
                        | "upstream_http_error"
                        | "upstream_rate_limited"
                        | "upstream_authentication_failed"
                )
            })
        {
            return Ok("provider_failed");
        }
    }
    if status.is_some()
        && row.try_get::<i64, _>("completed")? != 0
        && row.try_get::<i64, _>("matching_attempts")? == 1
        && row.try_get::<Option<String>, _>("failure_kind")?.as_deref() == Some("connection")
        && row
            .try_get::<Option<String>, _>("failure_stage")?
            .as_deref()
            .is_some_and(|stage| matches!(stage, "connect" | "proxy_connect" | "dns" | "tls"))
    {
        return Ok("transport_failed");
    }
    if status.is_some() || now.saturating_sub(row.try_get("observed_at")?) > 86_400_000 {
        Ok("unknown")
    } else {
        Ok("pending")
    }
}

pub(crate) async fn merge_pending(pool: &AnyPool) -> Result<(), AppError> {
    let now = unix_millis();
    let rows = sqlx::query("SELECT s.request_id, s.attempt_id, s.observed_at, e.completed, e.final_response, e.response_body_complete, e.upstream_account_id, e.route_id, t.status_code, t.error_code, t.terminal_cause_code, t.upstream_account_id AS final_account, t.model_route_id AS final_route, d.failure_kind, d.failure_stage, (SELECT COUNT(*) FROM request_egress_attempts same_attempt WHERE same_attempt.request_id = e.request_id AND same_attempt.route_id = e.route_id AND same_attempt.upstream_account_id = e.upstream_account_id AND same_attempt.credential_generation = e.credential_generation AND same_attempt.transport_revision = e.transport_revision) AS matching_attempts FROM network_proxy_attempt_signals s JOIN request_egress_attempts e ON e.request_id = s.request_id AND e.attempt_id = s.attempt_id LEFT JOIN proxy_operational_terminals t ON t.request_id = e.request_id LEFT JOIN request_upstream_transport_diagnostics d ON d.request_id = e.request_id AND d.route_id = e.route_id AND d.upstream_account_id = e.upstream_account_id AND d.credential_generation = e.credential_generation AND d.transport_revision = e.transport_revision WHERE s.next_refresh_at <= $1 ORDER BY s.next_refresh_at, s.request_id, s.attempt_id LIMIT 128")
        .bind(now).fetch_all(pool).await?;
    for row in rows {
        let outcome = outcome(&row, now)?;
        let next = if outcome == "pending"
            || outcome == "unknown" && now.saturating_sub(row.try_get("observed_at")?) < 3_600_000
        {
            now + 10_000
        } else {
            i64::MAX
        };
        sqlx::query("UPDATE network_proxy_attempt_signals SET outcome = $1, next_refresh_at = $2 WHERE request_id = $3 AND attempt_id = $4 AND outcome IN ('pending', 'unknown')")
            .bind(outcome).bind(next).bind(row.try_get::<String, _>("request_id")?).bind(row.try_get::<String, _>("attempt_id")?).execute(pool).await?;
    }
    Ok(())
}

pub(crate) async fn candidate_signal(
    pool: &AnyPool,
    id: Uuid,
    version: i64,
    start: i64,
    end: i64,
) -> Result<ProxyCandidate, AppError> {
    let row = sqlx::query("SELECT p.last_probe_status, p.last_probe_checked_at, p.last_probe_latency_ms, p.probe_successes, (SELECT COUNT(*) FROM network_proxy_attempt_signals s WHERE s.proxy_id = p.id AND s.proxy_version = p.version AND s.observed_at >= $3 AND s.observed_at < $4 AND s.outcome IN ('succeeded','provider_failed','transport_failed')) AS sample_count, (SELECT COUNT(*) FROM network_proxy_attempt_signals s WHERE s.proxy_id = p.id AND s.proxy_version = p.version AND s.observed_at >= $3 AND s.observed_at < $4 AND s.outcome IN ('succeeded','provider_failed')) AS transported FROM network_proxies p WHERE p.id = $1 AND p.version = $2")
        .bind(id.to_string()).bind(version).bind(start).bind(end).fetch_optional(pool).await?.ok_or(AppError::NotFound)?;
    let count: i64 = row.try_get("sample_count")?;
    Ok(ProxyCandidate {
        proxy_id: id,
        proxy_version: version,
        active_health: row
            .try_get::<Option<String>, _>("last_probe_status")?
            .unwrap_or_else(|| "unknown".into()),
        checked_at: row.try_get("last_probe_checked_at")?,
        latency_ms: row.try_get("last_probe_latency_ms")?,
        consecutive_successes: row.try_get("probe_successes")?,
        sample_count: count,
        transport_success_rate: (count > 0)
            .then(|| {
                row.try_get::<i64, _>("transported")
                    .map(|value| value as f64 / count as f64)
            })
            .transpose()?,
    })
}

impl Database {
    pub(crate) async fn network_proxy_observations(
        &self,
        id: Uuid,
        external: &str,
        start: i64,
        end: i64,
        key: &[u8],
    ) -> Result<Value, AppError> {
        if start >= end || end.saturating_sub(start) > 31 * 86_400_000 {
            return Err(invalid_proxy());
        }
        let proxy = self.get_network_proxy(id, external, key).await?;
        let row = sqlx::query("SELECT COUNT(*) AS recorded_attempts, COUNT(request_payload_bytes) AS request_known, COUNT(response_payload_bytes) AS response_known, CAST(SUM(request_payload_bytes) AS BIGINT) AS request_bytes, CAST(SUM(response_payload_bytes) AS BIGINT) AS response_bytes, CAST(COALESCE(SUM(CASE WHEN outcome = 'succeeded' THEN 1 ELSE 0 END), 0) AS BIGINT) AS succeeded, CAST(COALESCE(SUM(CASE WHEN outcome = 'provider_failed' THEN 1 ELSE 0 END), 0) AS BIGINT) AS provider_failed, CAST(COALESCE(SUM(CASE WHEN outcome = 'transport_failed' THEN 1 ELSE 0 END), 0) AS BIGINT) AS transport_failed, CAST(COALESCE(SUM(CASE WHEN outcome = 'cancelled' THEN 1 ELSE 0 END), 0) AS BIGINT) AS cancelled, CAST(COALESCE(SUM(CASE WHEN outcome = 'unknown' THEN 1 ELSE 0 END), 0) AS BIGINT) AS unknown, CAST(COALESCE(SUM(CASE WHEN outcome = 'pending' THEN 1 ELSE 0 END), 0) AS BIGINT) AS pending FROM network_proxy_attempt_signals WHERE proxy_id = $1 AND observed_at >= $2 AND observed_at < $3")
            .bind(id.to_string()).bind(start).bind(end).fetch_one(&self.pool).await?;
        let succeeded: i64 = row.try_get("succeeded")?;
        let provider: i64 = row.try_get("provider_failed")?;
        let failed: i64 = row.try_get("transport_failed")?;
        let denominator = succeeded + provider + failed;
        let checked = proxy["active_health"]["checked_at"].as_i64();
        Ok(
            serde_json::json!({"proxy_id": id, "proxy_version": proxy["version"], "observed_at": unix_millis(), "window_start": start, "window_end": end,
            "coverage": "recorded_codex_attempts_with_proxy_version", "scope": "persisted_observations", "stale": checked.is_none_or(|at| unix_millis().saturating_sub(at) > 180000),
            "active_health": proxy["active_health"],
            "request_outcomes": {"succeeded": succeeded, "provider_failed": provider, "transport_failed": failed, "sample_count": denominator,
                "cancelled": row.try_get::<i64, _>("cancelled")?, "unknown": row.try_get::<i64, _>("unknown")?, "pending": row.try_get::<i64, _>("pending")?,
                "request_success_rate": (denominator > 0).then(|| succeeded as f64 / denominator as f64),
                "transport_success_rate": (denominator > 0).then(|| (succeeded + provider) as f64 / denominator as f64)},
            "traffic": {"measurement_basis": "application_payload_estimate", "request_payload_bytes": row.try_get::<Option<i64>, _>("request_bytes")?,
                "response_payload_bytes": row.try_get::<Option<i64>, _>("response_bytes")?, "recorded_attempts": row.try_get::<i64, _>("recorded_attempts")?,
                "request_known_attempts": row.try_get::<i64, _>("request_known")?, "response_known_attempts": row.try_get::<i64, _>("response_known")?, "wire_bytes": null, "supplier_billed_bytes": null}}),
        )
    }

    pub(crate) async fn request_network_proxy_probe(
        &self,
        id: Uuid,
        external: &str,
        expected_version: i64,
    ) -> Result<Value, AppError> {
        let result = sqlx::query("UPDATE network_proxies SET probe_requested = 1 WHERE id = $1 AND version = $2 AND tenant_id = (SELECT id FROM tenants WHERE external_id = $3)")
            .bind(id.to_string()).bind(expected_version).bind(external).execute(&self.pool).await?;
        if result.rows_affected() != 1 {
            return Err(AppError::ProxyGroupConflict(
                "network_proxy_version_conflict",
            ));
        }
        Ok(
            serde_json::json!({"proxy_id": id, "proxy_version": expected_version, "queued": true, "scope": "process"}),
        )
    }
}
