//! Shared request reservations, session affinity and compact hourly history.
use super::{AppError, Database, unix_millis};
use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

pub(crate) const HOUR_MS: i64 = 3_600_000;
pub(crate) const DAY_MS: i64 = 24 * HOUR_MS;

pub(crate) struct AllocationUsage {
    pub account: String,
    pub hour: i64,
    pub requests: u64,
    pub tokens: u64,
}

impl Database {
    pub(crate) async fn allocation_revision(&self, tenant: Uuid) -> Result<i64, AppError> {
        Ok(
            sqlx::query_scalar(
                "SELECT revision FROM group_allocation_revisions WHERE tenant_id=$1",
            )
            .bind(tenant.to_string())
            .fetch_optional(&self.pool)
            .await?
            .unwrap_or(0),
        )
    }

    pub(crate) async fn allocation_session(
        &self,
        tenant: Uuid,
        session: Uuid,
        now: i64,
    ) -> Result<Option<(String, u64)>, AppError> {
        let row = sqlx::query("SELECT account_id,generation FROM group_allocation_sessions WHERE tenant_id=$1 AND session_key=$2 AND expires_at>$3")
            .bind(tenant.to_string()).bind(session.to_string()).bind(now).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| {
            (
                row.get("account_id"),
                row.get::<i64, _>("generation") as u64,
            )
        }))
    }

    /// The revision fences every shared allocation mutation; the guest runs before this transaction.
    pub(crate) async fn allocation_reserve(
        &self,
        tenant: Uuid,
        revision: i64,
        request: Uuid,
        session: Option<Uuid>,
        account: &str,
        generation: u64,
        now: i64,
    ) -> Result<bool, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let changed = if revision == 0 {
            sqlx::query("INSERT INTO group_allocation_revisions(tenant_id,revision) VALUES($1,1) ON CONFLICT(tenant_id) DO NOTHING")
                .bind(tenant.to_string()).execute(&mut *tx).await?.rows_affected()
        } else {
            sqlx::query("UPDATE group_allocation_revisions SET revision=revision+1 WHERE tenant_id=$1 AND revision=$2")
                .bind(tenant.to_string()).bind(revision).execute(&mut *tx).await?.rows_affected()
        };
        if changed != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("INSERT INTO group_allocation_reservations(tenant_id,request_id,account_id,session_key,generation,created_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(tenant_id,request_id) DO NOTHING")
            .bind(tenant.to_string()).bind(request.to_string()).bind(account).bind(session.map(|id|id.to_string()).unwrap_or_default()).bind(generation as i64).bind(now).execute(&mut *tx).await?;
        if let Some(session) = session {
            sqlx::query("INSERT INTO group_allocation_sessions(tenant_id,session_key,account_id,generation,observed_at,expires_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(tenant_id,session_key) DO UPDATE SET account_id=excluded.account_id,generation=excluded.generation,observed_at=excluded.observed_at,expires_at=excluded.expires_at WHERE group_allocation_sessions.observed_at<=excluded.observed_at")
                .bind(tenant.to_string()).bind(session.to_string()).bind(account).bind(generation as i64).bind(now).bind(now+DAY_MS).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Bootstrap and refresh from the existing hourly aggregate in the quota worker.
    pub(crate) async fn refresh_allocation_usage(
        &self,
        tenant: Uuid,
        account: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        bump_revision(&mut tx, &tenant.to_string()).await?;
        sqlx::query("DELETE FROM group_allocation_usage WHERE tenant_id=$1 AND account_id=$2")
            .bind(tenant.to_string())
            .bind(account.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO group_allocation_usage(tenant_id,account_id,hour_bucket,requests,tokens) SELECT tenant_id,upstream_account_id,hour_bucket,CAST(SUM(requests) AS BIGINT),CAST(SUM(input_tokens+output_tokens) AS BIGINT) FROM usage_analysis_hourly WHERE tenant_id=$1 AND upstream_account_id=$2 AND hour_bucket>=$3 GROUP BY tenant_id,upstream_account_id,hour_bucket")
            .bind(tenant.to_string()).bind(account.to_string()).bind(now/HOUR_MS-23).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM group_allocation_reservations WHERE tenant_id=$1 AND account_id=$2 AND created_at<$3")
            .bind(tenant.to_string()).bind(account.to_string()).bind(now-DAY_MS).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM group_allocation_sessions WHERE tenant_id=$1 AND session_key IN (SELECT session_key FROM group_allocation_sessions WHERE tenant_id=$1 AND expires_at<=$2 ORDER BY expires_at LIMIT 512)")
            .bind(tenant.to_string())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    pub(crate) async fn allocation_usage(
        &self,
        tenant: Uuid,
        accounts: &[String],
        now: i64,
    ) -> Result<Vec<AllocationUsage>, AppError> {
        if accounts.is_empty() {
            return Ok(Vec::new());
        }
        let mut query = sqlx::QueryBuilder::<Any>::new(
            "SELECT account_id,hour_bucket,CAST(SUM(requests) AS BIGINT) AS requests,CAST(SUM(tokens) AS BIGINT) AS tokens FROM (SELECT account_id,hour_bucket,requests,tokens FROM group_allocation_usage WHERE tenant_id=",
        );
        query.push_bind(tenant.to_string()).push(" UNION ALL SELECT account_id,created_at / 3600000 AS hour_bucket,COUNT(*) AS requests,CAST(0 AS BIGINT) AS tokens FROM group_allocation_reservations WHERE tenant_id=").push_bind(tenant.to_string()).push(" AND created_at>=").push_bind(now-DAY_MS).push(" GROUP BY account_id,created_at / 3600000) AS history WHERE hour_bucket>=").push_bind(now/HOUR_MS-23).push(" AND account_id IN (");
        let mut list = query.separated(",");
        for account in accounts {
            list.push_bind(account);
        }
        list.push_unseparated(") GROUP BY account_id,hour_bucket");
        Ok(query
            .build()
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| AllocationUsage {
                account: row.get("account_id"),
                hour: row.get("hour_bucket"),
                requests: row.get::<i64, _>("requests").max(0) as u64,
                tokens: row.get::<i64, _>("tokens").max(0) as u64,
            })
            .collect())
    }
}

async fn bump_revision(tx: &mut Transaction<'_, Any>, tenant: &str) -> Result<(), AppError> {
    sqlx::query("INSERT INTO group_allocation_revisions(tenant_id,revision) VALUES($1,1) ON CONFLICT(tenant_id) DO UPDATE SET revision=group_allocation_revisions.revision+1")
        .bind(tenant).execute(&mut **tx).await?;
    Ok(())
}

/// Completion replaces its exact reservation with the actual account's usage in one transaction.
pub(crate) async fn project_request(
    tx: &mut Transaction<'_, Any>,
    request: &str,
    direction: i64,
) -> Result<(), AppError> {
    let fact = sqlx::query("SELECT tenant_id,upstream_account_id,created_at,input_tokens,output_tokens,status_class FROM request_stats_facts WHERE request_id=$1")
        .bind(request).fetch_optional(&mut **tx).await?;
    let Some(fact) = fact else {
        return Ok(());
    };
    let tenant: String = fact.get("tenant_id");
    // Accounts enter the compact projection through the quota worker.
    let active: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM group_allocation_revisions WHERE tenant_id=$1")
            .bind(&tenant)
            .fetch_optional(&mut **tx)
            .await?;
    if active.is_none() {
        return Ok(());
    }
    bump_revision(tx, &tenant).await?;
    sqlx::query("INSERT INTO group_allocation_usage(tenant_id,account_id,hour_bucket,requests,tokens) VALUES($1,$2,$3,$4,$5) ON CONFLICT(tenant_id,account_id,hour_bucket) DO UPDATE SET requests=group_allocation_usage.requests+excluded.requests,tokens=group_allocation_usage.tokens+excluded.tokens")
        .bind(&tenant).bind(fact.get::<String,_>("upstream_account_id")).bind(fact.get::<i64,_>("created_at")/HOUR_MS).bind(direction).bind(direction*fact.get::<i64,_>("input_tokens").saturating_add(fact.get::<i64,_>("output_tokens"))).execute(&mut **tx).await?;
    if direction > 0 {
        if fact.get::<String, _>("status_class") == "success" {
            record_terminal(
                tx,
                request,
                &fact.get::<String, _>("upstream_account_id"),
                unix_millis(),
            )
            .await?;
        }
        sqlx::query(
            "DELETE FROM group_allocation_reservations WHERE tenant_id=$1 AND request_id=$2",
        )
        .bind(&tenant)
        .bind(request)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

/// Successful fallback supplies the next turn's preferred account.
pub(crate) async fn record_terminal(
    tx: &mut Transaction<'_, Any>,
    request: &str,
    account: &str,
    observed: i64,
) -> Result<(), AppError> {
    let reservation = sqlx::query("SELECT tenant_id,session_key,generation,account_id FROM group_allocation_reservations WHERE request_id=$1")
        .bind(request).fetch_optional(&mut **tx).await?;
    let Some(reservation) = reservation else {
        return Ok(());
    };
    let tenant: String = reservation.get("tenant_id");
    let session: String = reservation.get("session_key");
    if session.is_empty() {
        return Ok(());
    }
    bump_revision(tx, &tenant).await?;
    // The authorized candidate snapshot validates the current credential generation on read.
    sqlx::query("UPDATE group_allocation_sessions SET account_id=$1,generation=CASE WHEN account_id=$1 THEN generation ELSE -1 END,observed_at=$2,expires_at=$3 WHERE tenant_id=$4 AND session_key=$5 AND observed_at<=$2")
        .bind(account).bind(observed).bind(observed+DAY_MS).bind(tenant).bind(session).execute(&mut **tx).await?;
    Ok(())
}

pub(crate) async fn correct_terminal(
    tx: &mut Transaction<'_, Any>,
    request: &str,
) -> Result<(), AppError> {
    let row = sqlx::query("SELECT upstream_account_id,observed_at FROM session_routing_terminals WHERE request_id=$1 AND status_code>=200 AND status_code<400 AND COALESCE(error_code,'')='' AND upstream_account_id IS NOT NULL")
        .bind(request).fetch_optional(&mut **tx).await?;
    if let Some(row) = row {
        record_terminal(
            tx,
            request,
            &row.get::<String, _>("upstream_account_id"),
            row.get("observed_at"),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn exercise_shared_allocations(url: &str) {
        let first = Database::connect(url).await.unwrap();
        first.migrate().await.unwrap();
        let second = Database::connect(url).await.unwrap();
        let tenant = Uuid::now_v7();
        let now = unix_millis();
        let accounts = vec!["a".to_owned(), "b".to_owned()];
        let session = Uuid::now_v7();
        let mut retries = 0;
        for index in 0..32 {
            let revision = first.allocation_revision(tenant).await.unwrap();
            let left_request = Uuid::now_v7();
            let right_request = Uuid::now_v7();
            let (left, right) = tokio::join!(
                first.allocation_reserve(
                    tenant,
                    revision,
                    left_request,
                    Some(session),
                    "a",
                    1,
                    now + index
                ),
                second.allocation_reserve(
                    tenant,
                    revision,
                    right_request,
                    None,
                    "a",
                    1,
                    now + index
                ),
            );
            let left = left.unwrap();
            let right = right.unwrap();
            assert_ne!(left, right, "exactly one revision holder reserves first");
            let loser = if left { &second } else { &first };
            let request = if left { right_request } else { left_request };
            let usage = loser
                .allocation_usage(tenant, &accounts, now)
                .await
                .unwrap();
            let a: u64 = usage
                .iter()
                .filter(|row| row.account == "a")
                .map(|row| row.requests)
                .sum();
            let b: u64 = usage
                .iter()
                .filter(|row| row.account == "b")
                .map(|row| row.requests)
                .sum();
            assert_eq!(
                a,
                b + 1,
                "the second gateway sees the first reservation immediately"
            );
            let revision = loser.allocation_revision(tenant).await.unwrap();
            assert!(
                loser
                    .allocation_reserve(tenant, revision, request, None, "b", 1, now + index)
                    .await
                    .unwrap()
            );
            retries += 1;
        }
        assert_eq!(retries, 32);
        let usage = second
            .allocation_usage(tenant, &accounts, now)
            .await
            .unwrap();
        assert_eq!(
            usage
                .iter()
                .filter(|row| row.account == "a")
                .map(|row| row.requests)
                .sum::<u64>(),
            32
        );
        assert_eq!(
            usage
                .iter()
                .filter(|row| row.account == "b")
                .map(|row| row.requests)
                .sum::<u64>(),
            32
        );
        let persisted = second
            .allocation_session(tenant, session, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(persisted.0, "a");
        let request = Uuid::now_v7();
        let revision = first.allocation_revision(tenant).await.unwrap();
        assert!(
            first
                .allocation_reserve(tenant, revision, request, Some(session), "a", 1, now + 100)
                .await
                .unwrap()
        );
        let mut tx = first.begin_write_transaction().await.unwrap();
        record_terminal(&mut tx, &request.to_string(), "b", now + 101)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(first);
        let restarted = Database::connect(url).await.unwrap();
        assert_eq!(
            restarted
                .allocation_session(tenant, session, now + 102)
                .await
                .unwrap()
                .unwrap()
                .0,
            "b"
        );
        // Yesterday's final hour participates after midnight; expired hours drop out.
        let midnight = (now / DAY_MS + 1) * DAY_MS;
        sqlx::query("INSERT INTO group_allocation_usage(tenant_id,account_id,hour_bucket,requests,tokens) VALUES($1,'a',$2,90,90000),($1,'b',$2,10,10000),($1,'a',$3,1000,1000000)")
            .bind(tenant.to_string()).bind(midnight/HOUR_MS-1).bind(midnight/HOUR_MS-24).execute(&restarted.pool).await.unwrap();
        let usage = restarted
            .allocation_usage(tenant, &accounts, midnight + 1)
            .await
            .unwrap();
        assert!(usage.iter().all(|row| row.hour >= midnight / HOUR_MS - 23));
        assert!(usage.iter().any(|row| row.requests >= 90));
        assert!(
            restarted
                .allocation_session(tenant, session, now + DAY_MS + 102)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn sqlite_allocation_two_gateways_retry_share_history_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("allocation.db").display()
        );
        exercise_shared_allocations(&url).await;
    }

    #[tokio::test]
    async fn postgres_allocation_two_gateways_retry_share_history_and_restart() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!("allocation_{}", Uuid::now_v7().simple());
        let admin = sqlx::PgPool::connect(&url).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated = url::Url::parse(&url).unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        exercise_shared_allocations(isolated.as_str()).await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
