use super::super::*;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EarlyRequestTerminal {
    pub(crate) request_id: Uuid,
    pub(crate) tenant_id: Uuid,
    pub(crate) key_id: Uuid,
    pub(crate) created_at: i64,
    pub(crate) completed_at: i64,
    pub(crate) duration_ms: i64,
    pub(crate) protocol: String,
    pub(crate) model: String,
    pub(crate) status_code: u16,
    pub(crate) error_code: String,
}

impl Database {
    pub(crate) async fn record_early_request_terminal(
        &self,
        input: &EarlyRequestTerminal,
    ) -> Result<(), AppError> {
        if !(400..=599).contains(&input.status_code)
            || !matches!(input.protocol.as_str(), "openai" | "anthropic")
            || input.model.len() > 200
            || input.error_code.len() > 128
            || input.completed_at < input.created_at
            || input.duration_ms < 0
        {
            return Err(AppError::internal("early_terminal_record_early_request_terminal_1"));
        }
        let request_id = input.request_id.to_string();
        let mut transaction = self.begin_write_transaction().await?;
        let claimed = sqlx::query(
            "INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, $2, $3, $4) ON CONFLICT(id) DO NOTHING",
        )
        .bind(&request_id)
        .bind(input.created_at)
        .bind(input.tenant_id.to_string())
        .bind(input.key_id.to_string())
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() == 0 {
            transaction.rollback().await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, currency, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, '', COALESCE((SELECT currency FROM key_records WHERE id = $3 AND tenant_id = $2), ''), 0, 0, 0)",
        )
        .bind(&request_id)
        .bind(input.tenant_id.to_string())
        .bind(input.key_id.to_string())
        .bind(input.created_at)
        .bind(&input.protocol)
        .bind(&input.model)
        .bind(format!("gap://{request_id}/request"))
        .execute(&mut *transaction)
        .await?;
        let finished = super::lifecycle::record_early_terminal_in_transaction(
            &mut transaction,
            &FinishRequest {
                request_id: input.request_id,
                status_code: i64::from(input.status_code),
                duration_ms: input.duration_ms,
                first_output_ms: None,
                generation_duration_ms: None,
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                service_tier: None,
                cost_micros: 0,
                error_code: Some(input.error_code.clone()),
                response_object: format!("gap://{request_id}/response"),
            },
            input.completed_at,
        )
        .await?;
        if !finished {
            return Err(AppError::internal("early_terminal_record_early_request_terminal_1"));
        }
        transaction.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn postgres_early_terminal_is_once_unbilled_and_projected_without_reservation() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let database = Database::connect_with_max(&url, 4).await.unwrap();
        database.migrate().await.unwrap();
        let tenant = format!("early-terminal-{}", Uuid::now_v7());
        let pepper = b"early terminal synthetic test pepper";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: tenant.clone(),
                    principal_external_id: "synthetic".into(),
                    alias: tenant,
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
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
        let now = unix_millis();
        let terminal = EarlyRequestTerminal {
            request_id: Uuid::now_v7(),
            tenant_id: key.tenant_id,
            key_id: key.key_id,
            created_at: now,
            completed_at: now,
            duration_ms: 0,
            protocol: "openai".into(),
            model: "known-model".into(),
            status_code: 503,
            error_code: "no_authorized_upstream_candidate".into(),
        };
        let (first, second) = tokio::join!(
            database.record_early_request_terminal(&terminal),
            database.record_early_request_terminal(&terminal)
        );
        first.unwrap();
        second.unwrap();
        let rows = database.list_requests(key.key_id, 10).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(!rows[0].billing.billable);
        assert_eq!(rows[0].status_code, Some(503));
        assert_eq!(
            rows[0].terminal_cause_code.as_deref(),
            Some("gateway_pre_admission")
        );
        assert!(rows[0].upstream_account_id.is_none());
        for (statement, expected) in [
            (
                "SELECT COUNT(*) FROM request_events WHERE key_id = $1 AND event_kind = 'finished'",
                1_i64,
            ),
            (
                "SELECT COUNT(*) FROM request_events WHERE key_id = $1 AND event_kind = 'started'",
                0,
            ),
            (
                "SELECT COUNT(*) FROM request_stats_facts WHERE key_id = $1 AND status_class = 'failure'",
                1,
            ),
            (
                "SELECT CAST(SUM(requests) AS BIGINT) FROM request_daily_aggregates WHERE key_id = $1",
                1,
            ),
            (
                "SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1",
                0,
            ),
        ] {
            let actual: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(statement.to_owned()))
                .bind(key.key_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
            assert_eq!(actual, expected, "{statement}");
        }
        database.close().await;
    }
}
