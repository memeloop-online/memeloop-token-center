use std::collections::HashMap;

use super::super::*;
use crate::oauth::{
    OAuthRefreshDiagnostic, OAuthRefreshFailure, OAuthRefreshMetadata, OAuthRefreshOutcome,
    OAuthRefreshStatus,
};

impl Database {
    pub(crate) async fn record_upstream_oauth_refresh_failure(
        &self,
        account_id: Uuid,
        generation: i64,
        attempt_created_at: i64,
        idempotency_key: &str,
        failure: OAuthRefreshFailure,
    ) -> Result<bool, AppError> {
        let row = sqlx::query(
            "SELECT l.request_started_at, CASE WHEN l.pending_credential_ciphertext IS NOT NULL THEN 1 ELSE 0 END AS has_pending FROM upstream_oauth_refresh_leases l JOIN upstream_accounts a ON a.id = l.account_id AND a.credential_generation = l.credential_generation WHERE l.account_id = $1 AND l.credential_generation = $2 AND l.created_at = $3 AND l.idempotency_key = $4",
        )
        .bind(account_id.to_string())
        .bind(generation)
        .bind(attempt_created_at)
        .bind(idempotency_key.trim())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(false);
        };
        let started = row
            .try_get::<Option<i64>, _>("request_started_at")?
            .is_some();
        let outcome = if row.try_get::<i64, _>("has_pending")? != 0 {
            OAuthRefreshOutcome::PendingLocal
        } else if !started {
            OAuthRefreshOutcome::NotDispatched
        } else if failure.requires_reauthorization() {
            OAuthRefreshOutcome::ReauthorizationRequired
        } else {
            OAuthRefreshOutcome::OutcomeUnknown
        };
        let diagnostic = OAuthRefreshDiagnostic {
            version: 1,
            credential_generation: generation,
            attempt_created_at,
            attempt_finished_at: unix_millis().max(attempt_created_at),
            outcome,
            failure,
        };
        let encoded = serde_json::to_string(&diagnostic).map_err(|_| {
            AppError::internal("oauth_diagnostics_record_upstream_oauth_refresh_failure_1")
        })?;
        if encoded.len() > 2048 {
            return Err(AppError::internal(
                "oauth_diagnostics_record_upstream_oauth_refresh_failure_1",
            ));
        }
        let changed = sqlx::query(
            "UPDATE upstream_credentials SET oauth_refresh_diagnostic_json = $1 WHERE upstream_account_id = $2 AND generation = $3 AND EXISTS (SELECT 1 FROM upstream_oauth_refresh_leases l JOIN upstream_accounts a ON a.id = l.account_id AND a.credential_generation = l.credential_generation WHERE l.account_id = $2 AND l.credential_generation = $3 AND l.created_at = $4 AND l.idempotency_key = $5)",
        )
        .bind(encoded)
        .bind(account_id.to_string())
        .bind(generation)
        .bind(attempt_created_at)
        .bind(idempotency_key.trim())
        .execute(&self.pool)
        .await?;
        Ok(changed.rows_affected() == 1)
    }

    pub(crate) async fn upstream_oauth_refresh_statuses(
        &self,
        accounts: &[UpstreamAccountView],
    ) -> Result<HashMap<Uuid, OAuthRefreshStatus>, AppError> {
        if accounts.len() > 100 {
            return Err(AppError::BadRequest(
                "Select at most 100 accounts at a time.".into(),
            ));
        }
        let selected: Vec<_> = accounts
            .iter()
            .filter(|account| account.auth_kind == "oauth")
            .collect();
        if selected.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = (0..selected.len())
            .map(|index| {
                let first = index * 3 + 1;
                format!("(${}, ${}, ${})", first, first + 1, first + 2)
            })
            .collect::<Vec<_>>()
            .join(", ");
        let statement = format!(
            "WITH selected(account_id, generation, tenant_id) AS (VALUES {placeholders}) SELECT a.id, a.credential_generation, c.expires_at, c.revoked_at, c.oauth_refresh_diagnostic_json, l.created_at AS lease_created_at, l.lease_expires_at, l.request_started_at, CASE WHEN l.pending_credential_ciphertext IS NOT NULL THEN 1 ELSE 0 END AS has_pending FROM selected s JOIN upstream_accounts a ON a.id = s.account_id AND a.tenant_id = s.tenant_id AND a.credential_generation = s.generation AND a.auth_kind = 'oauth' LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation LEFT JOIN upstream_oauth_refresh_leases l ON l.account_id = a.id AND l.credential_generation = a.credential_generation"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement));
        for account in selected {
            query = query
                .bind(account.id.to_string())
                .bind(account.credential_generation)
                .bind(account.tenant_id.to_string());
        }
        let now = unix_millis();
        query
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                let account_id = parse_uuid(row.try_get("id")?)?;
                let metadata = OAuthRefreshMetadata {
                    generation: row.try_get("credential_generation")?,
                    expires_at: row.try_get("expires_at")?,
                    revoked_at: row.try_get("revoked_at")?,
                    lease_created_at: row.try_get("lease_created_at")?,
                    lease_expires_at: row.try_get("lease_expires_at")?,
                    request_started_at: row.try_get("request_started_at")?,
                    has_pending: row.try_get::<i64, _>("has_pending")? != 0,
                    diagnostic_json: row.try_get("oauth_refresh_diagnostic_json")?,
                };
                Ok((account_id, metadata.status(now)))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{db::CreateUpstreamAccountInput, oauth::OAuthRefreshFailureKind};
    use serde_json::json;

    const PEPPER: &[u8] = b"oauth-diagnostics-fixture-pepper";

    async fn setup() -> (Database, tempfile::TempDir, UpstreamAccountView) {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("diagnostics.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "diagnostic-tenant".into(),
                    name: "Diagnostic fixture".into(),
                    driver: "http-json".into(),
                    config: json!({"base_url":"https://example.test"}),
                    credential: credential(10),
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some("cursor".into()),
                    oauth_refresh_url: Some("https://example.test/refresh".into()),
                },
                PEPPER,
            )
            .await
            .unwrap();
        (database, directory, account)
    }

    fn credential(expiry: i64) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: "diagnostic-fixture-access".into(),
            refresh_token: Some("diagnostic-fixture-refresh".into()),
            expires_at: Some(expiry),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        }
    }

    async fn claim(database: &Database, account: &UpstreamAccountView, key: &str) -> i64 {
        let ClaimUpstreamOAuthRefreshResult::Claimed(claimed) = database
            .claim_upstream_oauth_refresh(account.id, key, PEPPER)
            .await
            .unwrap()
        else {
            panic!("expected a new claim");
        };
        claimed.attempt_created_at
    }

    #[tokio::test]
    async fn started_unknown_is_not_backfilled_and_projection_is_page_tenant_bound() {
        let (database, _directory, account) = setup().await;
        claim(&database, &account, "legacy-attempt").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "legacy-attempt")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE upstream_oauth_refresh_leases SET lease_expires_at = 0 WHERE account_id = $1",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        let statuses = database
            .upstream_oauth_refresh_statuses(std::slice::from_ref(&account))
            .await
            .unwrap();
        let value = serde_json::to_value(&statuses[&account.id]).unwrap();
        assert_eq!(value["refresh_state"], "outcome_unknown");
        assert_eq!(value["access_state"], "expired");
        assert_eq!(value["reauthorization_required"], false);
        assert_eq!(value["automatic_refresh_paused"], true);
        let persisted: Option<String> = sqlx::query_scalar("SELECT oauth_refresh_diagnostic_json FROM upstream_credentials WHERE upstream_account_id = $1 AND generation = 1")
            .bind(account.id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(persisted.is_none());
        let mut wrong_tenant = account.clone();
        wrong_tenant.tenant_id = Uuid::now_v7();
        assert!(
            database
                .upstream_oauth_refresh_statuses(&[wrong_tenant])
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            database
                .list_managed_oauth_refresh_candidates(unix_millis(), 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn proven_unsent_connector_failure_can_reenter_worker_without_rotating() {
        let (database, _directory, account) = setup().await;
        let created = claim(&database, &account, "connect-failed").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "connect-failed")
            .await
            .unwrap();
        database
            .mark_upstream_oauth_refresh_request_not_sent(account.id, "connect-failed", 1)
            .await
            .unwrap();
        assert!(
            database
                .record_upstream_oauth_refresh_failure(
                    account.id,
                    1,
                    created,
                    "connect-failed",
                    OAuthRefreshFailure::new(OAuthRefreshFailureKind::Network, None),
                )
                .await
                .unwrap()
        );
        database
            .abort_upstream_oauth_refresh(account.id, "connect-failed")
            .await
            .unwrap();
        let statuses = database
            .upstream_oauth_refresh_statuses(std::slice::from_ref(&account))
            .await
            .unwrap();
        let status = serde_json::to_value(&statuses[&account.id]).unwrap();
        assert_eq!(status["refresh_state"], "failed");
        assert_eq!(status["automatic_refresh_paused"], false);
        assert_eq!(status["credential_generation"], 1);
        assert_eq!(
            database
                .list_managed_oauth_refresh_candidates(unix_millis(), 20)
                .await
                .unwrap(),
            vec![(account.id, 1)],
        );
        claim(&database, &account, "connect-recovery").await;
    }

    #[tokio::test]
    async fn unsent_release_cannot_clear_saved_results_or_a_superseding_claim() {
        let (database, _directory, account) = setup().await;
        claim(&database, &account, "original-connect").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "original-connect")
            .await
            .unwrap();
        sqlx::query("UPDATE upstream_oauth_refresh_leases SET pending_credential_ciphertext = 'synthetic-pending' WHERE account_id = $1")
            .bind(account.id.to_string()).execute(&database.pool).await.unwrap();
        assert!(
            database
                .mark_upstream_oauth_refresh_request_not_sent(account.id, "original-connect", 1)
                .await
                .is_err()
        );
        sqlx::query("UPDATE upstream_oauth_refresh_leases SET pending_credential_ciphertext = NULL, credential_generation = 2, idempotency_key = 'new-generation' WHERE account_id = $1")
            .bind(account.id.to_string()).execute(&database.pool).await.unwrap();
        sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
            .bind(account.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(
            database
                .mark_upstream_oauth_refresh_request_not_sent(account.id, "original-connect", 1)
                .await
                .is_err()
        );
        assert!(
            database
                .mark_upstream_oauth_refresh_request_not_sent(account.id, "new-generation", 1)
                .await
                .is_err()
        );
        let started: Option<i64> = sqlx::query_scalar("SELECT request_started_at FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND credential_generation = 2 AND idempotency_key = 'new-generation'")
            .bind(account.id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(started.is_some());
    }

    #[tokio::test]
    async fn received_result_survives_pool_timeout_before_durable_staging() {
        let (database, directory, account) = setup().await;
        let key = "pool-timeout-stage-recovery";
        claim(&database, &account, key).await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, key)
            .await
            .unwrap();
        // A separate one-connection process pool reproduces worker/control
        // pool exhaustion without changing any application-wide timeouts.
        let mut writer = database.clone();
        writer.pool = sqlx::any::AnyPoolOptions::new()
            .max_connections(1)
            .acquire_timeout(std::time::Duration::from_millis(25))
            .connect(&format!(
                "sqlite://{}",
                directory.path().join("diagnostics.db").display()
            ))
            .await
            .unwrap();
        let held = writer.pool.acquire().await.unwrap();
        let account_id = account.id;
        let finish = tokio::spawn(async move {
            writer
                .finish_upstream_oauth_refresh(
                    account_id,
                    credential(unix_millis() + 900_000),
                    key,
                    PEPPER,
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(
            !finish.is_finished(),
            "pool timeouts must retain the received rotated token for local retry"
        );
        drop(held);
        let installed = tokio::time::timeout(std::time::Duration::from_secs(10), finish)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(installed.credential_generation, 2);
        let replay = database
            .claim_upstream_oauth_refresh(account_id, key, PEPPER)
            .await
            .unwrap();
        assert!(matches!(replay, ClaimUpstreamOAuthRefreshResult::Replay(_)));
        let statuses = database
            .upstream_oauth_refresh_statuses(&[installed])
            .await
            .unwrap();
        assert!(!statuses[&account_id].automatic_refresh_paused);
        assert!(!statuses[&account_id].reauthorization_required);
    }

    #[tokio::test]
    async fn kimi_received_result_survives_a_temporary_local_stage_failure() {
        let (database, directory, account) = setup().await;
        claim(&database, &account, "kimi-stage-recovery").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "kimi-stage-recovery")
            .await
            .unwrap();
        let writer = Database::connect_with_max(
            &format!(
                "sqlite://{}",
                directory.path().join("diagnostics.db").display()
            ),
            1,
        )
        .await
        .unwrap();
        sqlx::query("PRAGMA busy_timeout = 0")
            .execute(&writer.pool)
            .await
            .unwrap();
        let blocked = database.begin_write_transaction().await.unwrap();
        let account_id = account.id;
        let finish = tokio::spawn(async move {
            writer
                .finish_upstream_oauth_refresh(
                    account_id,
                    credential(unix_millis() + 900_000),
                    "kimi-stage-recovery",
                    PEPPER,
                )
                .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
        assert!(!finish.is_finished());
        blocked.rollback().await.unwrap();
        let saved = tokio::time::timeout(std::time::Duration::from_secs(10), finish)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(saved.credential_generation, 2);
        let replay = database
            .claim_upstream_oauth_refresh(account.id, "kimi-stage-recovery", PEPPER)
            .await
            .unwrap();
        let ClaimUpstreamOAuthRefreshResult::Replay(replay) = replay else {
            panic!("a received result must recover locally, not dispatch again");
        };
        assert_eq!(replay.credential_generation, 2);
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upstream_credentials WHERE upstream_account_id = $1",
        )
        .bind(account.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn failure_diagnostic_survives_safe_abort_but_does_not_change_claim_rules() {
        let (database, _directory, account) = setup().await;
        let created = claim(&database, &account, "not-dispatched").await;
        assert!(
            database
                .record_upstream_oauth_refresh_failure(
                    account.id,
                    1,
                    created,
                    "not-dispatched",
                    OAuthRefreshFailure::new(OAuthRefreshFailureKind::Configuration, None)
                )
                .await
                .unwrap()
        );
        database
            .abort_upstream_oauth_refresh(account.id, "not-dispatched")
            .await
            .unwrap();
        let statuses = database
            .upstream_oauth_refresh_statuses(std::slice::from_ref(&account))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&statuses[&account.id]).unwrap()["refresh_state"],
            "failed"
        );
        assert!(
            !database
                .record_upstream_oauth_refresh_failure(
                    account.id,
                    1,
                    created,
                    "not-dispatched",
                    OAuthRefreshFailure::new(OAuthRefreshFailureKind::InvalidGrant, Some(400))
                )
                .await
                .unwrap()
        );
        assert_eq!(
            database
                .list_managed_oauth_refresh_candidates(unix_millis(), 20)
                .await
                .unwrap()
                .len(),
            1
        );
        claim(&database, &account, "new-attempt").await;
    }

    #[tokio::test]
    async fn explicit_rejection_is_informational_and_cannot_release_started_claim() {
        let (database, _directory, account) = setup().await;
        let created = claim(&database, &account, "rejected-attempt").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "rejected-attempt")
            .await
            .unwrap();
        assert!(
            database
                .record_upstream_oauth_refresh_failure(
                    account.id,
                    1,
                    created,
                    "rejected-attempt",
                    OAuthRefreshFailure::new(OAuthRefreshFailureKind::InvalidGrant, Some(400))
                )
                .await
                .unwrap()
        );
        database
            .abort_upstream_oauth_refresh(account.id, "rejected-attempt")
            .await
            .unwrap();
        let statuses = database
            .upstream_oauth_refresh_statuses(std::slice::from_ref(&account))
            .await
            .unwrap();
        assert_eq!(
            serde_json::to_value(&statuses[&account.id]).unwrap()["reauthorization_required"],
            true
        );
        assert!(
            database
                .claim_upstream_oauth_refresh(account.id, "another-key", PEPPER)
                .await
                .is_err()
        );
        assert!(
            database
                .list_managed_oauth_refresh_candidates(unix_millis(), 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn lost_diagnostic_storage_cannot_discard_or_block_successful_credentials() {
        let (database, _directory, account) = setup().await;
        let created = claim(&database, &account, "local-success").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "local-success")
            .await
            .unwrap();
        sqlx::query("ALTER TABLE upstream_credentials DROP COLUMN oauth_refresh_diagnostic_json")
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(
            database
                .record_upstream_oauth_refresh_failure(
                    account.id,
                    1,
                    created,
                    "local-success",
                    OAuthRefreshFailure::new(OAuthRefreshFailureKind::LocalPersistence, None)
                )
                .await
                .is_err()
        );
        let refreshed = database
            .finish_upstream_oauth_refresh(
                account.id,
                credential(unix_millis() + 60_000),
                "local-success",
                PEPPER,
            )
            .await
            .unwrap();
        assert_eq!(refreshed.credential_generation, 2);
        let replayed = database
            .claim_upstream_oauth_refresh(account.id, "local-success", PEPPER)
            .await
            .unwrap();
        assert!(matches!(
            replayed,
            ClaimUpstreamOAuthRefreshResult::Replay(_)
        ));
    }

    #[tokio::test]
    async fn migration_120_adds_nullable_metadata_without_classifying_legacy_attempts() {
        let (database, _directory, account) = setup().await;
        claim(&database, &account, "old-unknown").await;
        database
            .mark_upstream_oauth_refresh_request_started(account.id, "old-unknown")
            .await
            .unwrap();
        sqlx::query(
            "UPDATE upstream_oauth_refresh_leases SET lease_expires_at = 0 WHERE account_id = $1",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query("ALTER TABLE upstream_credentials DROP COLUMN oauth_refresh_diagnostic_json")
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM schema_migrations WHERE version = 120")
            .execute(&database.pool)
            .await
            .unwrap();
        database.migrate().await.unwrap();
        let persisted: Option<String> = sqlx::query_scalar("SELECT oauth_refresh_diagnostic_json FROM upstream_credentials WHERE upstream_account_id = $1 AND generation = 1")
            .bind(account.id.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(persisted.is_none());
        let statuses = database
            .upstream_oauth_refresh_statuses(std::slice::from_ref(&account))
            .await
            .unwrap();
        let status = serde_json::to_value(&statuses[&account.id]).unwrap();
        assert_eq!(status["refresh_state"], "outcome_unknown");
        assert!(status["failure_class"].is_null());
        assert_eq!(status["reauthorization_required"], false);
        let oversized = "x".repeat(2049);
        assert!(sqlx::query("UPDATE upstream_credentials SET oauth_refresh_diagnostic_json = $1 WHERE upstream_account_id = $2")
            .bind(oversized).bind(account.id.to_string()).execute(&database.pool).await.is_err());
    }
}
