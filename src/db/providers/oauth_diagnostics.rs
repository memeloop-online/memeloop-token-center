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
        let encoded = serde_json::to_string(&diagnostic).map_err(|_| AppError::Internal)?;
        if encoded.len() > 2048 {
            return Err(AppError::Internal);
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
