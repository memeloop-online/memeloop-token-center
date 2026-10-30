//! Read-only operational observations. Never used for request admission.
use super::*;

#[derive(Clone, Serialize)]
pub(crate) struct ConcurrencySample {
    pub tenant_external_id: String,
    pub sample_id: String,
    pub source: String,
    pub upstream_account_id: Option<String>,
    pub state: String,
    pub lease_expires_at: Option<i64>,
    pub lease_state: String,
    pub selected_egress: Option<egress::SelectedEgress>,
    pub egress_attempt_id: Option<String>,
    pub egress_upstream_account_id: Option<String>,
    pub egress_state: String,
}

impl Database {
    pub(crate) async fn upstream_concurrency_samples(
        &self,
        limit: i64,
    ) -> Result<Vec<ConcurrencySample>, AppError> {
        // Database time matches lease renewal. One statement shares a snapshot
        // across both sources; unfinished attempt evidence is never inferred
        // from an account's configured proxy or its independent health probe.
        let now = match self.backend {
            DatabaseBackend::PostgreSql => {
                "CAST(EXTRACT(EPOCH FROM statement_timestamp()) * 1000 AS BIGINT)"
            }
            DatabaseBackend::Sqlite => "CAST((julianday('now') - 2440587.5) * 86400000 AS BIGINT)",
        };
        let query = format!(
            "SELECT observations.*, tenant.external_id AS tenant_external_id,
                    {now} AS sampled_at
             FROM (
               SELECT r.tenant_id, r.id AS sample_id, 'request_owner' AS source,
                      r.upstream_account_id, 'unknown' AS state,
                      r.proxy_owner_expires_at AS lease_expires_at,
                      e.snapshot_json, e.attempt_id AS egress_attempt_id,
                      e.completed AS egress_completed, e.upstream_account_id AS egress_upstream_account_id, r.created_at
               FROM (SELECT * FROM request_records
                     WHERE completed_at IS NULL AND proxy_owner_expires_at > {now}
                       AND NOT EXISTS (SELECT 1 FROM proxy_operational_terminals terminal
                                       WHERE terminal.request_id = request_records.id)
                     ORDER BY proxy_owner_expires_at DESC, id DESC LIMIT $1) r
               LEFT JOIN request_egress_attempts e ON e.request_id = r.id
                 AND e.attempt_id = (
                   SELECT latest.attempt_id FROM request_egress_attempts latest
                   WHERE latest.request_id = r.id
                   ORDER BY latest.observed_at DESC, latest.outbound_attempt DESC,
                            latest.connect_attempt DESC, latest.attempt_id DESC LIMIT 1
                 )
               UNION ALL
               SELECT g.tenant_id, g.id, 'generation_job', g.upstream_account_id,
                      g.status, g.lease_expires_at, NULL, NULL, NULL, NULL, g.created_at
               FROM generation_jobs g
               WHERE g.status IN ('preparing', 'queued', 'submitting', 'running', 'cancelling')
               UNION ALL
               SELECT a.tenant_id, a.id, 'account_inventory', a.id, a.status,
                      NULL, NULL, NULL, NULL, NULL, a.updated_at
               FROM upstream_accounts a
             ) observations JOIN tenants tenant ON tenant.id = observations.tenant_id
             ORDER BY observations.created_at DESC, observations.sample_id DESC LIMIT $1"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(query))
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| {
                let expires: Option<i64> = row.try_get("lease_expires_at")?;
                let sampled_at: i64 = row.try_get("sampled_at")?;
                let snapshot: Option<String> = row.try_get("snapshot_json")?;
                let selected_egress = snapshot
                    .as_deref()
                    .and_then(|value| serde_json::from_str(value).ok());
                let completed: Option<i64> = row.try_get("egress_completed")?;
                Ok(ConcurrencySample {
                    tenant_external_id: row.try_get("tenant_external_id")?,
                    sample_id: row.try_get("sample_id")?,
                    source: row.try_get("source")?,
                    upstream_account_id: row.try_get("upstream_account_id")?,
                    state: row.try_get("state")?,
                    lease_expires_at: expires,
                    lease_state: match expires {
                        Some(expiry) if expiry > sampled_at => "live",
                        Some(_) => "expired",
                        None => "missing",
                    }
                    .into(),
                    egress_state:
                        match (snapshot.is_some(), selected_egress.is_some(), completed) {
                            (_, true, Some(0)) => "unfinished_attempt",
                            (_, true, Some(_)) => "completed_attempt",
                            (true, false, _) => "invalid_metadata",
                            _ => "missing_metadata",
                        }
                        .into(),
                    selected_egress,
                    egress_attempt_id: row.try_get("egress_attempt_id")?,
                    egress_upstream_account_id: row.try_get("egress_upstream_account_id")?,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::requests::owner::ProxyRequestOwner;

    async fn assert_observations(database: &Database) {
        let marker = Uuid::now_v7().to_string();
        let pepper = b"concurrency-observation-regression";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: marker.clone(),
                    principal_external_id: marker.clone(),
                    alias: "concurrency".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = database
            .upsert_model_price(&marker, "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        let request_id = Uuid::now_v7();
        let reservation = database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                protocol: "openai",
                model: &marker,
                request_object: "inline-json:{}",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let owner = ProxyRequestOwner {
            request_id,
            tenant_id: key.tenant_id,
            key_id: key.key_id,
            reservation_id: reservation.id,
            token: Uuid::now_v7(),
        };
        assert!(database.acquire_proxy_request_owner(&owner).await.unwrap());
        let samples = database.upstream_concurrency_samples(4097).await.unwrap();
        let sample = samples
            .iter()
            .find(|sample| sample.sample_id == request_id.to_string())
            .unwrap();
        assert_eq!(sample.tenant_external_id, marker);
        assert_eq!(sample.lease_state, "live");
        assert_eq!(sample.egress_state, "missing_metadata");
        assert!(sample.upstream_account_id.is_none());
        assert!(sample.selected_egress.is_none());
        sqlx::query("UPDATE request_records SET proxy_owner_expires_at = 0 WHERE id = $1")
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(
            !database
                .upstream_concurrency_samples(4097)
                .await
                .unwrap()
                .iter()
                .any(|sample| sample.sample_id == request_id.to_string())
        );
        sqlx::query("UPDATE request_records SET proxy_owner_expires_at = $1 WHERE id = $2")
            .bind(unix_millis() + 60_000)
            .bind(request_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        // Delivery is over; settlement may still leave the original row and
        // lease unfinished. Operational completion takes precedence.
        sqlx::query(
            "INSERT INTO proxy_operational_terminals
            (request_id, request_created_at, tenant_id, key_id, account_id, reservation_id,
             owner_token, fence, contract_digest, receipt_id, operational_digest, status_code,
             completed_at, duration_ms, response_object, recorded_at)
            SELECT id, created_at, tenant_id, key_id, $2, reservation_id, proxy_owner_token,
                   'test-fence', 'test-contract', 'test-receipt', 'test-operation', 200,
                   created_at, 0, 'inline-json:{}', created_at
            FROM request_records WHERE id = $1",
        )
        .bind(request_id.to_string())
        .bind(key.account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        assert!(
            !database
                .upstream_concurrency_samples(4097)
                .await
                .unwrap()
                .iter()
                .any(|sample| sample.sample_id == request_id.to_string())
        );
    }

    #[tokio::test]
    async fn sqlite_counts_only_live_owners_and_preserves_missing_attribution() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("concurrency.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        assert_observations(&database).await;
    }

    #[tokio::test]
    async fn postgres_counts_only_live_owners_and_decodes_nullable_union_columns() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect(&url).await.unwrap();
        database.migrate().await.unwrap();
        assert_observations(&database).await;
    }
}
