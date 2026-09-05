use sqlx::Row;
use uuid::Uuid;

use super::super::{AppError, Database, unix_millis};

const PROBE_LEASE_MILLIS: i64 = 30_000;
pub(crate) const UPSTREAM_PROBE_HEARTBEAT_MILLIS: u64 = 10_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpstreamFailureKind {
    RateLimited,
    Unavailable,
    InvalidResponse,
    Connection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UpstreamAttemptAdmission {
    Healthy,
    Probe { lease_token: Uuid },
    Unavailable,
}

impl UpstreamFailureKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limited",
            Self::Unavailable => "unavailable",
            Self::InvalidResponse => "invalid_response",
            Self::Connection => "connection",
        }
    }

    fn base_cooldown_millis(self) -> i64 {
        match self {
            Self::RateLimited => 30_000,
            Self::Unavailable => 15_000,
            Self::InvalidResponse => 15_000,
            Self::Connection => 5_000,
        }
    }
}

impl Database {
    /// Returns true for healthy accounts. An account recovering from cooldown
    /// is admitted only when this caller atomically owns its short half-open
    /// probe lease, preventing a concurrent request wave from stampeding it.
    pub(crate) async fn claim_upstream_account_attempt(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
    ) -> Result<UpstreamAttemptAdmission, AppError> {
        let now = unix_millis();
        let row = sqlx::query(
            "SELECT health.consecutive_failures, health.cooldown_until, health.probe_lease_until
             FROM upstream_accounts account
             LEFT JOIN upstream_account_health health
               ON health.upstream_account_id = account.id
              AND health.credential_generation = $2
             WHERE account.id = $1 AND account.status = 'active'
               AND account.credential_generation = $2",
        )
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(UpstreamAttemptAdmission::Unavailable);
        };
        let Some(consecutive_failures) = row.try_get::<Option<i64>, _>("consecutive_failures")?
        else {
            return Ok(UpstreamAttemptAdmission::Healthy);
        };
        if consecutive_failures == 0 {
            return Ok(UpstreamAttemptAdmission::Healthy);
        }
        let cooldown_until: i64 = row.try_get("cooldown_until")?;
        let probe_lease_until: i64 = row.try_get("probe_lease_until")?;
        if cooldown_until > now || probe_lease_until > now {
            return Ok(UpstreamAttemptAdmission::Unavailable);
        }
        let lease_token = Uuid::now_v7();
        let result = sqlx::query(
            "UPDATE upstream_account_health
             SET probe_lease_until = $1, probe_lease_token = $2, updated_at = $3
             WHERE upstream_account_id = $4
               AND consecutive_failures > 0
               AND credential_generation = $5
               AND cooldown_until <= $3
               AND probe_lease_until <= $3
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_health.upstream_account_id
                   AND account.status = 'active'
                   AND account.credential_generation = $5
               )",
        )
        .bind(now.saturating_add(PROBE_LEASE_MILLIS))
        .bind(lease_token.to_string())
        .bind(now)
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .execute(&self.pool)
        .await?;
        Ok(if result.rows_affected() == 1 {
            UpstreamAttemptAdmission::Probe { lease_token }
        } else {
            UpstreamAttemptAdmission::Unavailable
        })
    }

    pub(crate) async fn record_upstream_account_failure(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        kind: UpstreamFailureKind,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let base = kind.base_cooldown_millis();
        let result = sqlx::query(
            "INSERT INTO upstream_account_health (
                 upstream_account_id, consecutive_failures, cooldown_until,
                 probe_lease_until, probe_lease_token, credential_generation,
                 last_failure_kind, updated_at
             ) SELECT $1, 1, $3, 0, '', $2, $4, $5
               FROM upstream_accounts account
              WHERE account.id = $1 AND account.status = 'active'
                AND account.credential_generation = $2
             ON CONFLICT (upstream_account_id) DO UPDATE SET
                 consecutive_failures = CASE
                     WHEN upstream_account_health.credential_generation = excluded.credential_generation
                         THEN upstream_account_health.consecutive_failures + 1
                     ELSE 1
                 END,
                 cooldown_until = CASE
                     WHEN upstream_account_health.credential_generation = excluded.credential_generation
                          AND upstream_account_health.cooldown_until > excluded.updated_at
                         THEN upstream_account_health.cooldown_until
                     ELSE excluded.updated_at + CASE
                         WHEN upstream_account_health.credential_generation <> excluded.credential_generation THEN $6
                         WHEN upstream_account_health.consecutive_failures >= 6 THEN $6 * 64
                         WHEN upstream_account_health.consecutive_failures = 5 THEN $6 * 32
                         WHEN upstream_account_health.consecutive_failures = 4 THEN $6 * 16
                         WHEN upstream_account_health.consecutive_failures = 3 THEN $6 * 8
                         WHEN upstream_account_health.consecutive_failures = 2 THEN $6 * 4
                         WHEN upstream_account_health.consecutive_failures = 1 THEN $6 * 2
                         ELSE $6
                     END
                 END,
                 probe_lease_until = 0,
                 probe_lease_token = '',
                 credential_generation = excluded.credential_generation,
                 last_failure_kind = excluded.last_failure_kind,
                 updated_at = excluded.updated_at",
        )
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(now.saturating_add(base))
        .bind(kind.as_str())
        .bind(now)
        .bind(base)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(crate) async fn record_upstream_account_probe_failure(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        lease_token: Uuid,
        kind: UpstreamFailureKind,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let base = kind.base_cooldown_millis();
        let result = sqlx::query(
            "UPDATE upstream_account_health SET
                 consecutive_failures = consecutive_failures + 1,
                 cooldown_until = $1 + CASE
                     WHEN consecutive_failures >= 6 THEN $2 * 64
                     WHEN consecutive_failures = 5 THEN $2 * 32
                     WHEN consecutive_failures = 4 THEN $2 * 16
                     WHEN consecutive_failures = 3 THEN $2 * 8
                     WHEN consecutive_failures = 2 THEN $2 * 4
                     WHEN consecutive_failures = 1 THEN $2 * 2
                     ELSE $2
                 END,
                 probe_lease_until = 0,
                 probe_lease_token = '',
                 last_failure_kind = $3,
                 updated_at = $1
             WHERE upstream_account_id = $4 AND credential_generation = $5
               AND probe_lease_token = $6
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_health.upstream_account_id
                   AND account.status = 'active'
                   AND account.credential_generation = $5
               )",
        )
        .bind(now)
        .bind(base)
        .bind(kind.as_str())
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(lease_token.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(crate) async fn record_upstream_account_probe_success(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        lease_token: Uuid,
    ) -> Result<bool, AppError> {
        let result = sqlx::query(
            "DELETE FROM upstream_account_health
             WHERE upstream_account_id = $1 AND credential_generation = $2
               AND probe_lease_token = $3
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_health.upstream_account_id
                   AND account.status = 'active'
                   AND account.credential_generation = $2
               )",
        )
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(lease_token.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(crate) async fn release_upstream_account_probe(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        lease_token: Uuid,
    ) -> Result<bool, AppError> {
        let result = sqlx::query(
            "UPDATE upstream_account_health
             SET probe_lease_until = 0, probe_lease_token = '', updated_at = $1
             WHERE upstream_account_id = $2 AND credential_generation = $3
               AND probe_lease_token = $4
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_health.upstream_account_id
                   AND account.status = 'active'
                   AND account.credential_generation = $3
               )",
        )
        .bind(unix_millis())
        .bind(upstream_account_id.to_string())
        .bind(credential_generation)
        .bind(lease_token.to_string())
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    pub(crate) async fn renew_upstream_account_probe(
        &self,
        upstream_account_id: Uuid,
        credential_generation: i64,
        lease_token: Uuid,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let result = sqlx::query(
            "UPDATE upstream_account_health SET probe_lease_until = $1, updated_at = $2
             WHERE upstream_account_id = $3 AND credential_generation = $4
               AND probe_lease_token = $5
               AND EXISTS (
                 SELECT 1 FROM upstream_accounts account
                 WHERE account.id = upstream_account_health.upstream_account_id
                   AND account.status = 'active'
                   AND account.credential_generation = $4
               )",
        )
        .bind(now.saturating_add(PROBE_LEASE_MILLIS))
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

    async fn fixture() -> (tempfile::TempDir, Database, Uuid) {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("upstream-health.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let account_id = Uuid::now_v7();
        let now = unix_millis();
        sqlx::query(
            "INSERT INTO upstream_accounts (
                 id, tenant_id, name, driver, auth_kind, config_json, status,
                 credential_generation, created_at, updated_at
             ) VALUES ($1, $2, 'health-test', 'http-json', 'none', '{}', 'active', 1, $3, $3)",
        )
        .bind(account_id.to_string())
        .bind(Uuid::now_v7().to_string())
        .bind(now)
        .execute(&database.pool)
        .await
        .unwrap();
        (directory, database, account_id)
    }

    #[tokio::test]
    async fn cooldown_allows_only_one_half_open_probe_and_success_recovers() {
        let (_directory, database, account_id) = fixture().await;
        assert_eq!(
            database
                .claim_upstream_account_attempt(account_id, 1)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Healthy
        );
        database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::RateLimited)
            .await
            .unwrap();
        assert_eq!(
            database
                .claim_upstream_account_attempt(account_id, 1)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Unavailable
        );
        sqlx::query(
            "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
             WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();

        let (left, right) = tokio::join!(
            database.claim_upstream_account_attempt(account_id, 1),
            database.claim_upstream_account_attempt(account_id, 1),
        );
        let admissions = [left.unwrap(), right.unwrap()];
        let lease_token = admissions
            .iter()
            .find_map(|admission| match admission {
                UpstreamAttemptAdmission::Probe { lease_token } => Some(*lease_token),
                UpstreamAttemptAdmission::Healthy | UpstreamAttemptAdmission::Unavailable => None,
            })
            .expect("one caller owns the probe lease");
        assert_eq!(
            admissions
                .iter()
                .filter(|admission| **admission == UpstreamAttemptAdmission::Unavailable)
                .count(),
            1
        );
        assert!(
            database
                .record_upstream_account_probe_success(account_id, 1, lease_token)
                .await
                .unwrap()
        );
        assert_eq!(
            database
                .claim_upstream_account_attempt(account_id, 1)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Healthy
        );
    }

    #[tokio::test]
    async fn stale_probe_terminal_cannot_overwrite_a_renewed_or_reassigned_lease() {
        let (_directory, database, account_id) = fixture().await;
        database
            .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::Connection)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
             WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        let UpstreamAttemptAdmission::Probe {
            lease_token: stale_token,
        } = database
            .claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap()
        else {
            panic!("first probe lease");
        };
        assert!(
            database
                .renew_upstream_account_probe(account_id, 1, stale_token)
                .await
                .unwrap()
        );
        assert_eq!(
            database
                .claim_upstream_account_attempt(account_id, 1)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Unavailable,
            "a renewed long-running lease remains exclusive"
        );

        sqlx::query(
            "UPDATE upstream_account_health SET cooldown_until = 0, probe_lease_until = 0
             WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        let UpstreamAttemptAdmission::Probe {
            lease_token: current_token,
        } = database
            .claim_upstream_account_attempt(account_id, 1)
            .await
            .unwrap()
        else {
            panic!("replacement probe lease");
        };
        assert_ne!(stale_token, current_token);
        assert!(
            !database
                .record_upstream_account_probe_success(account_id, 1, stale_token)
                .await
                .unwrap()
        );
        assert!(
            !database
                .record_upstream_account_probe_failure(
                    account_id,
                    1,
                    stale_token,
                    UpstreamFailureKind::InvalidResponse,
                )
                .await
                .unwrap()
        );
        assert!(
            database
                .record_upstream_account_probe_success(account_id, 1, current_token)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn stale_credential_generation_cannot_poison_rotated_account_health() {
        let (_directory, database, account_id) = fixture().await;
        assert!(
            database
                .record_upstream_account_failure(account_id, 1, UpstreamFailureKind::Connection)
                .await
                .unwrap()
        );
        sqlx::query("UPDATE upstream_accounts SET credential_generation = 2 WHERE id = $1")
            .bind(account_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert!(
            !database
                .record_upstream_account_failure(
                    account_id,
                    1,
                    UpstreamFailureKind::InvalidResponse,
                )
                .await
                .unwrap()
        );
        assert_eq!(
            database
                .claim_upstream_account_attempt(account_id, 2)
                .await
                .unwrap(),
            UpstreamAttemptAdmission::Healthy,
            "the old generation cooldown is not inherited"
        );
        assert!(
            database
                .record_upstream_account_failure(account_id, 2, UpstreamFailureKind::Unavailable)
                .await
                .unwrap()
        );
        let row = sqlx::query(
            "SELECT credential_generation, consecutive_failures, last_failure_kind
             FROM upstream_account_health WHERE upstream_account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(row.try_get::<i64, _>("credential_generation").unwrap(), 2);
        assert_eq!(row.try_get::<i64, _>("consecutive_failures").unwrap(), 1);
        assert_eq!(
            row.try_get::<String, _>("last_failure_kind").unwrap(),
            "unavailable"
        );
    }
}
