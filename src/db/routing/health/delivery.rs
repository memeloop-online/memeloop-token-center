use super::*;

impl Database {
    /// Verified, durably delivered streaming output proves transport recovery,
    /// but not final usage/settlement. Retain the token as a terminal fence:
    /// a newer failure replaces it, so this long stream cannot later clear or
    /// overwrite that failure. No schema or credential generation is changed.
    pub(crate) async fn record_upstream_account_probe_delivery(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        lease_token: Uuid,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let result = sqlx::query(
            "UPDATE upstream_account_health
             SET consecutive_failures = 0, cooldown_until = 0,
                 probe_lease_until = 0, last_failure_kind = '', updated_at = $1
             WHERE upstream_account_id = $2 AND credential_generation = $3
               AND probe_lease_token = $4 AND probe_lease_until > $1
               AND consecutive_failures > 0
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_health.upstream_account_id
                   AND account.status = 'active'
                   AND account.credential_generation = $3
               )",
        )
        .bind(now)
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(lease_token.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exercise(database: Database) {
        database.migrate().await.unwrap();
        let tenant = Uuid::new_v4();
        let account = Uuid::new_v4();
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, 1)")
            .bind(tenant.to_string())
            .bind(format!("delivered-probe-{tenant}"))
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'delivered probe', 'http-json', 'none', '{}', 'active', 1, 1, 1)")
            .bind(account.to_string()).bind(tenant.to_string())
            .execute(&database.pool).await.unwrap();
        database
            .record_upstream_account_failure(account, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap();
        let first = open_probe(&database, account).await;
        assert_eq!(
            database
                .claim_upstream_account_attempt(account, 1)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Unavailable
        );
        assert!(
            database
                .record_upstream_account_probe_delivery(account, 1, first)
                .await
                .unwrap()
        );
        let (a, b, c, renewal) = tokio::join!(
            database.claim_upstream_account_attempt(account, 1),
            database.claim_upstream_account_attempt(account, 1),
            database.claim_upstream_account_attempt(account, 1),
            database.renew_upstream_account_probe(account, 1, first),
        );
        for admitted in [a, b, c] {
            assert_eq!(admitted.unwrap(), UpstreamAttemptAdmission::Healthy);
        }
        assert!(
            !renewal.unwrap(),
            "an in-flight heartbeat cannot re-lock delivered recovery"
        );
        // A newer quota failure invalidates the old stream token. Neither its
        // success nor a delayed failure may overwrite that newer observation.
        assert!(
            database
                .record_upstream_account_failure(account, 1, UpstreamFailureKind::RateLimited)
                .await
                .unwrap()
        );
        let cooldown: i64 = sqlx::query_scalar(
            "SELECT cooldown_until FROM upstream_account_health WHERE upstream_account_id = $1",
        )
        .bind(account.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert!(
            !database
                .record_upstream_account_probe_success(account, 1, first)
                .await
                .unwrap()
        );
        assert!(
            !database
                .record_upstream_account_probe_failure(
                    account,
                    1,
                    first,
                    UpstreamFailureKind::Connection
                )
                .await
                .unwrap()
        );
        assert_eq!(
            cooldown,
            sqlx::query_scalar::<_, i64>(
                "SELECT cooldown_until FROM upstream_account_health WHERE upstream_account_id = $1"
            )
            .bind(account.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap()
        );
        let second = open_probe(&database, account).await;
        assert!(
            !database
                .record_upstream_account_probe_delivery(account, 1, first)
                .await
                .unwrap()
        );
        assert!(
            database
                .record_upstream_account_probe_delivery(account, 1, second)
                .await
                .unwrap()
        );
        // With no intervening newer observation, the delivered stream still
        // owns terminal responsibility and can report its later genuine error.
        assert!(
            database
                .record_upstream_account_probe_failure(
                    account,
                    1,
                    second,
                    UpstreamFailureKind::InvalidResponse
                )
                .await
                .unwrap()
        );
        assert_eq!(
            database
                .claim_upstream_account_attempt(account, 1)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Unavailable
        );
        let third = open_probe(&database, account).await;
        assert!(
            database
                .record_upstream_account_probe_delivery(account, 1, third)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
            .bind(account.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(
            !database
                .record_upstream_account_probe_failure(
                    account,
                    1,
                    third,
                    UpstreamFailureKind::Connection
                )
                .await
                .unwrap()
        );
        assert!(
            !database
                .record_upstream_account_probe_success(account, 1, third)
                .await
                .unwrap()
        );
        assert_eq!(
            database
                .claim_upstream_account_attempt(account, 2)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Healthy
        );
        sqlx::query("DELETE FROM upstream_accounts WHERE id = $1")
            .bind(account.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM tenants WHERE id = $1")
            .bind(tenant.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        database.close().await;
    }

    async fn open_probe(database: &Database, account: Uuid) -> Uuid {
        sqlx::query("UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0 WHERE upstream_account_id = $1")
            .bind(account.to_string()).execute(&database.pool).await.unwrap();
        let UpstreamAttemptAdmission::Probe { lease_token } = database
            .claim_upstream_account_attempt(account, 1)
            .await
            .unwrap()
        else {
            panic!("exactly one half-open probe expected");
        };
        lease_token
    }

    #[tokio::test]
    async fn sqlite_delivered_probe_admission_and_terminal_generation_fences() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("delivered-probe.db").display()
        );
        exercise(Database::connect(&url).await.unwrap()).await;
    }

    #[tokio::test]
    async fn postgres_delivered_probe_admission_and_terminal_generation_fences() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        exercise(Database::connect(&url).await.unwrap()).await;
    }
}
