//! Shared request reservations, session affinity and compact hourly history.
use super::{AppError, Database, DatabaseBackend};
use sqlx::Row;
use uuid::Uuid;

pub(crate) const HOUR_MS: i64 = 3_600_000;
pub(crate) const DAY_MS: i64 = 24 * HOUR_MS;

#[derive(Clone, Copy)]
pub(crate) struct AllocationSession<'a> {
    pub key: &'a crate::model::AuthenticatedKey,
    pub session: &'a str,
    pub model: &'a str,
    pub protocol: &'a str,
}

pub(crate) struct AllocationUsage {
    pub account: String,
    pub hour: i64,
    pub requests: u64,
    pub tokens: u64,
}

fn allocation_refresh_error(stage: &'static str, error: sqlx::Error) -> AppError {
    let code = error.as_database_error().and_then(|error| error.code());
    tracing::warn!(
        stage,
        sqlstate = code.as_deref(),
        "allocation history refresh deferred"
    );
    #[cfg(test)]
    eprintln!("allocation history refresh deferred: stage={stage} sqlstate={code:?}");
    error.into()
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
        let row = sqlx::query("SELECT CASE WHEN t.status_code>=200 AND t.status_code<400 AND COALESCE(t.error_code,'')='' THEN COALESCE(t.upstream_account_id,s.account_id) ELSE s.account_id END AS account_id,CASE WHEN t.status_code>=200 AND t.status_code<400 AND COALESCE(t.error_code,'')='' AND t.upstream_account_id IS NOT NULL THEN -1 ELSE s.generation END AS generation FROM group_allocation_sessions s LEFT JOIN session_routing_terminals t ON t.request_id=s.request_id AND t.tenant_id=s.tenant_id WHERE s.tenant_id=$1 AND s.session_key=$2 AND s.expires_at>$3")
            .bind(tenant.to_string()).bind(session.to_string()).bind(now).fetch_optional(&self.pool).await?;
        Ok(row.map(|row| {
            (
                row.get("account_id"),
                row.get::<i64, _>("generation") as u64,
            )
        }))
    }

    pub(crate) async fn allocation_preferred_session(
        &self,
        identity: AllocationSession<'_>,
        session: Option<Uuid>,
        now: i64,
    ) -> Result<Option<(String, u64)>, AppError> {
        let row = sqlx::query(
            "WITH actual AS (
                SELECT upstream_account_id AS account_id,CAST(-1 AS BIGINT) AS generation
                FROM session_routing_terminals
                WHERE tenant_id=$1 AND principal_id=$2 AND key_id=$3 AND explicit_session_id=$4 AND model=$5 AND protocol=$6 AND expires_at>$7 AND status_code>=200 AND status_code<400 AND COALESCE(error_code,'')='' AND upstream_account_id IS NOT NULL
                ORDER BY observed_at DESC,request_id DESC LIMIT 1
            ), binding AS (
                SELECT CASE WHEN t.status_code>=200 AND t.status_code<400 AND COALESCE(t.error_code,'')='' THEN COALESCE(t.upstream_account_id,s.account_id) ELSE s.account_id END AS account_id,
                    CASE WHEN t.status_code>=200 AND t.status_code<400 AND COALESCE(t.error_code,'')='' AND t.upstream_account_id IS NOT NULL THEN -1 ELSE s.generation END AS generation
                FROM group_allocation_sessions s LEFT JOIN session_routing_terminals t ON t.request_id=s.request_id AND t.tenant_id=s.tenant_id
                WHERE s.tenant_id=$1 AND s.session_key=$8 AND s.expires_at>$7
            )
            SELECT account_id,generation FROM (
                SELECT account_id,generation,0 AS priority FROM actual
                UNION ALL SELECT account_id,generation,1 AS priority FROM binding
            ) AS preferences ORDER BY priority LIMIT 1"
        )
            .bind(identity.key.tenant_id.to_string())
            .bind(identity.key.principal_id.to_string())
            .bind(identity.key.key_id.to_string())
            .bind(identity.session)
            .bind(identity.model)
            .bind(identity.protocol)
            .bind(now)
            .bind(session.map(|id| id.to_string()))
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| {
            (
                row.get("account_id"),
                row.get::<i64, _>("generation") as u64,
            )
        }))
    }

    /// New-session selections share a revision fence. Established sessions append their request count.
    pub(crate) async fn allocation_reserve(
        &self,
        tenant: Uuid,
        revision: Option<i64>,
        request: Uuid,
        session: Option<Uuid>,
        selected: (&str, u64),
        now: i64,
    ) -> Result<bool, AppError> {
        let (account, generation) = selected;
        let mut tx = self.begin_write_transaction().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT set_config('lock_timeout','5ms',true),set_config('statement_timeout','20ms',true)")
                .execute(&mut *tx)
                .await?;
        }
        let changed = if let Some(revision) = revision {
            let acquired = if revision == 0 {
                sqlx::query("INSERT INTO group_allocation_revisions(tenant_id,revision) VALUES($1,1) ON CONFLICT(tenant_id) DO NOTHING")
                    .bind(tenant.to_string()).execute(&mut *tx).await
            } else {
                sqlx::query("UPDATE group_allocation_revisions SET revision=revision+1 WHERE tenant_id=$1 AND revision=$2")
                    .bind(tenant.to_string()).bind(revision).execute(&mut *tx).await
            };
            match acquired {
                Ok(result) => result.rows_affected(),
                Err(error)
                    if error
                        .as_database_error()
                        .and_then(|error| error.code())
                        .as_deref()
                        == Some("55P03") =>
                {
                    tx.rollback().await?;
                    return Ok(false);
                }
                Err(error) => return Err(error.into()),
            }
        } else {
            1
        };
        if changed != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            // Both projections consume the exact newly inserted reservation in this transaction.
            let inserted: i64 = sqlx::query_scalar(
                "WITH inserted AS (
                    INSERT INTO group_allocation_reservations(tenant_id,request_id,account_id,session_key,generation,created_at)
                    VALUES($1,$2,$3,$4,$5,$6)
                    ON CONFLICT(tenant_id,request_id) DO NOTHING
                    RETURNING tenant_id,request_id,account_id,session_key,generation,created_at
                ), pending AS (
                    INSERT INTO group_allocation_pending(tenant_id,account_id,hour_bucket,requests)
                    SELECT tenant_id,account_id,created_at/3600000,1 FROM inserted
                    ON CONFLICT(tenant_id,account_id,hour_bucket) DO UPDATE SET requests=group_allocation_pending.requests+1
                    RETURNING 1
                ), binding AS (
                    INSERT INTO group_allocation_sessions(tenant_id,session_key,account_id,generation,observed_at,expires_at,request_id)
                    SELECT tenant_id,session_key,account_id,generation,created_at,created_at+86400000,request_id FROM inserted CROSS JOIN pending WHERE session_key<>''
                    ON CONFLICT(tenant_id,session_key) DO UPDATE SET account_id=excluded.account_id,generation=excluded.generation,observed_at=excluded.observed_at,expires_at=excluded.expires_at,request_id=excluded.request_id WHERE group_allocation_sessions.observed_at<=excluded.observed_at
                    RETURNING 1
                ) SELECT COUNT(*) FROM inserted"
            )
                .bind(tenant.to_string())
                .bind(request.to_string())
                .bind(account)
                .bind(session.map(|id| id.to_string()).unwrap_or_default())
                .bind(generation as i64)
                .bind(now)
                .fetch_one(&mut *tx)
                .await?;
            if inserted != 1 {
                tx.rollback().await?;
                return Ok(false);
            }
            tx.commit().await?;
            return Ok(true);
        }
        let inserted=sqlx::query("INSERT INTO group_allocation_reservations(tenant_id,request_id,account_id,session_key,generation,created_at) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(tenant_id,request_id) DO NOTHING")
            .bind(tenant.to_string()).bind(request.to_string()).bind(account).bind(session.map(|id|id.to_string()).unwrap_or_default()).bind(generation as i64).bind(now).execute(&mut *tx).await?.rows_affected();
        if inserted != 1 {
            tx.rollback().await?;
            return Ok(false);
        }
        sqlx::query("INSERT INTO group_allocation_pending(tenant_id,account_id,hour_bucket,requests) VALUES($1,$2,$3,1) ON CONFLICT(tenant_id,account_id,hour_bucket) DO UPDATE SET requests=group_allocation_pending.requests+1")
            .bind(tenant.to_string()).bind(account).bind(now/HOUR_MS).execute(&mut *tx).await?;
        if let Some(session) = session {
            sqlx::query("INSERT INTO group_allocation_sessions(tenant_id,session_key,account_id,generation,observed_at,expires_at,request_id) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(tenant_id,session_key) DO UPDATE SET account_id=excluded.account_id,generation=excluded.generation,observed_at=excluded.observed_at,expires_at=excluded.expires_at,request_id=excluded.request_id WHERE group_allocation_sessions.observed_at<=excluded.observed_at")
                .bind(tenant.to_string()).bind(session.to_string()).bind(account).bind(generation as i64).bind(now).bind(now+DAY_MS).bind(request.to_string()).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(true)
    }

    /// Bootstrap and refresh from the existing hourly aggregate in the quota worker.
    pub(crate) async fn refresh_allocation_usage(
        &self,
        tenant: Uuid,
        _account: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SET LOCAL lock_timeout='5ms'")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SET LOCAL statement_timeout='80ms'")
                .execute(&mut *tx)
                .await?;
            // Compact reconciliation benefits from direct execution within its short budget.
            sqlx::query("SET LOCAL jit=off").execute(&mut *tx).await?;
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
        }
        let inserted=sqlx::query("INSERT INTO group_allocation_revisions(tenant_id,revision,refreshed_at) VALUES($1,1,$2) ON CONFLICT(tenant_id) DO NOTHING")
            .bind(tenant.to_string()).bind(now).execute(&mut *tx).await?.rows_affected();
        if inserted == 0 {
            let refreshed=sqlx::query("UPDATE group_allocation_revisions SET revision=revision+1,refreshed_at=$2 WHERE tenant_id=$1 AND refreshed_at<=$3")
                .bind(tenant.to_string()).bind(now).bind(now-5000).execute(&mut *tx).await?.rows_affected();
            if refreshed == 0 {
                tx.rollback().await?;
                return Ok(());
            }
        }
        sqlx::query("DELETE FROM group_allocation_usage WHERE tenant_id=$1")
            .bind(tenant.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|error| allocation_refresh_error("usage_clear", error))?;
        sqlx::query("INSERT INTO group_allocation_usage(tenant_id,account_id,hour_bucket,requests,tokens) SELECT tenant_id,upstream_account_id,hour_bucket,CAST(SUM(requests) AS BIGINT),CAST(SUM(input_tokens+output_tokens) AS BIGINT) FROM usage_analysis_hourly WHERE tenant_id=$1 AND hour_bucket>=$2 GROUP BY tenant_id,upstream_account_id,hour_bucket")
            .bind(tenant.to_string()).bind(now/HOUR_MS-23).execute(&mut *tx).await.map_err(|error| allocation_refresh_error("usage_snapshot", error))?;
        sqlx::query("DELETE FROM group_allocation_reservations WHERE tenant_id=$1 AND (created_at<$2 OR EXISTS (SELECT 1 FROM request_stats_facts f JOIN request_records r ON r.id=f.request_id WHERE f.request_id=group_allocation_reservations.request_id AND f.tenant_id=$1 AND NOT EXISTS (SELECT 1 FROM terminal_projection_outbox t WHERE t.request_id=f.request_id AND t.projected_at IS NULL) AND NOT EXISTS (SELECT 1 FROM metered_usage_projection_outbox m WHERE m.reservation_id=r.reservation_id AND m.projected_at IS NULL)))")
            .bind(tenant.to_string()).bind(now-DAY_MS).execute(&mut *tx).await.map_err(|error| allocation_refresh_error("reservation_reconcile", error))?;
        sqlx::query("DELETE FROM group_allocation_pending WHERE tenant_id=$1")
            .bind(tenant.to_string())
            .execute(&mut *tx)
            .await
            .map_err(|error| allocation_refresh_error("pending_clear", error))?;
        sqlx::query("INSERT INTO group_allocation_pending(tenant_id,account_id,hour_bucket,requests) SELECT tenant_id,account_id,created_at/3600000,COUNT(*) FROM group_allocation_reservations WHERE tenant_id=$1 AND created_at>=$2 GROUP BY tenant_id,account_id,created_at/3600000")
            .bind(tenant.to_string()).bind((now/HOUR_MS-23)*HOUR_MS).execute(&mut *tx).await.map_err(|error| allocation_refresh_error("pending_snapshot", error))?;
        sqlx::query("DELETE FROM group_allocation_sessions WHERE tenant_id=$1 AND session_key IN (SELECT session_key FROM group_allocation_sessions WHERE tenant_id=$1 AND expires_at<=$2 ORDER BY expires_at LIMIT 512)")
            .bind(tenant.to_string()).bind(now).execute(&mut *tx).await.map_err(|error| allocation_refresh_error("session_expiry", error))?;
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
        let placeholders = (3..accounts.len() + 3)
            .map(|index| format!("${index}"))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT account_id,hour_bucket,CAST(SUM(requests) AS BIGINT) AS requests,CAST(SUM(tokens) AS BIGINT) AS tokens FROM (SELECT account_id,hour_bucket,requests,tokens FROM group_allocation_usage WHERE tenant_id=$1 AND hour_bucket>=$2 AND account_id IN ({placeholders}) UNION ALL SELECT account_id,hour_bucket,requests,CAST(0 AS BIGINT) AS tokens FROM group_allocation_pending WHERE tenant_id=$1 AND hour_bucket>=$2 AND account_id IN ({placeholders})) AS history GROUP BY account_id,hour_bucket"
        );
        // SQL interpolation contains only generated parameter indices; account values are bound.
        let mut query = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(tenant.to_string())
            .bind(now / HOUR_MS - 23);
        for account in accounts {
            query = query.bind(account);
        }
        Ok(query
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::unix_millis;

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
                    Some(revision),
                    left_request,
                    Some(session),
                    ("a", 1),
                    now + index
                ),
                second.allocation_reserve(
                    tenant,
                    Some(revision),
                    right_request,
                    None,
                    ("a", 1),
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
                    .allocation_reserve(
                        tenant,
                        Some(revision),
                        request,
                        None,
                        ("b", 1),
                        now + index
                    )
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
                .allocation_reserve(
                    tenant,
                    Some(revision),
                    request,
                    Some(session),
                    ("a", 1),
                    now + 100
                )
                .await
                .unwrap()
        );
        sqlx::query("INSERT INTO session_routing_terminals(tenant_id,principal_id,key_id,explicit_session_id,model,protocol,request_id,observed_at,status_code,upstream_account_id,expires_at) VALUES($1,$2,$3,'session','model','openai',$4,$5,200,'b',$6)")
            .bind(tenant.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).bind(request.to_string()).bind(now+101).bind(now+DAY_MS).execute(&first.pool).await.unwrap();
        let key = crate::model::AuthenticatedKey {
            key_id: Uuid::now_v7(),
            tenant_id: tenant,
            principal_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            alias: "allocation".into(),
            currency: "USD".into(),
            credential_generation: 1,
            policy: Default::default(),
        };
        let identity = AllocationSession {
            key: &key,
            session: "conversation",
            model: "model",
            protocol: "openai",
        };
        assert_eq!(
            first
                .allocation_preferred_session(identity, Some(session), now + 102)
                .await
                .unwrap(),
            Some(("b".into(), u64::MAX))
        );
        assert!(
            first
                .allocation_preferred_session(identity, None, now + 102)
                .await
                .unwrap()
                .is_none()
        );
        sqlx::query("INSERT INTO session_routing_terminals(tenant_id,principal_id,key_id,explicit_session_id,model,protocol,request_id,observed_at,status_code,upstream_account_id,expires_at) VALUES($1,$2,$3,'conversation','model','openai',$4,$5,200,'actual',$6)")
            .bind(tenant.to_string()).bind(key.principal_id.to_string()).bind(key.key_id.to_string()).bind(Uuid::now_v7().to_string()).bind(now+102).bind(now+DAY_MS).execute(&first.pool).await.unwrap();
        for binding in [Some(session), None] {
            assert_eq!(
                first
                    .allocation_preferred_session(identity, binding, now + 103)
                    .await
                    .unwrap(),
                Some(("actual".into(), u64::MAX))
            );
        }
        for other in [
            AllocationSession {
                session: "other",
                ..identity
            },
            AllocationSession {
                model: "other",
                ..identity
            },
            AllocationSession {
                protocol: "other",
                ..identity
            },
        ] {
            assert_eq!(
                first
                    .allocation_preferred_session(other, Some(session), now + 103)
                    .await
                    .unwrap(),
                Some(("b".into(), u64::MAX))
            );
        }
        for other_key in [
            crate::model::AuthenticatedKey {
                key_id: Uuid::now_v7(),
                ..key.clone()
            },
            crate::model::AuthenticatedKey {
                principal_id: Uuid::now_v7(),
                ..key.clone()
            },
        ] {
            assert_eq!(
                first
                    .allocation_preferred_session(
                        AllocationSession {
                            key: &other_key,
                            ..identity
                        },
                        Some(session),
                        now + 103
                    )
                    .await
                    .unwrap(),
                Some(("b".into(), u64::MAX))
            );
        }
        let other_tenant = crate::model::AuthenticatedKey {
            tenant_id: Uuid::now_v7(),
            ..key.clone()
        };
        assert!(
            first
                .allocation_preferred_session(
                    AllocationSession {
                        key: &other_tenant,
                        ..identity
                    },
                    Some(session),
                    now + 103
                )
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            first
                .allocation_preferred_session(identity, Some(session), now + DAY_MS + 101)
                .await
                .unwrap()
                .is_none()
        );
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
        if matches!(restarted.backend, DatabaseBackend::PostgreSql) {
            let locked_tenant = Uuid::now_v7();
            assert!(
                restarted
                    .allocation_reserve(locked_tenant, Some(0), Uuid::now_v7(), None, ("a", 1), now)
                    .await
                    .unwrap()
            );
            let mut holder = restarted.begin_write_transaction().await.unwrap();
            sqlx::query(
                "UPDATE group_allocation_revisions SET revision=revision WHERE tenant_id=$1",
            )
            .bind(locked_tenant.to_string())
            .execute(&mut *holder)
            .await
            .unwrap();
            let retry_request = Uuid::now_v7();
            assert!(
                !second
                    .allocation_reserve(locked_tenant, Some(1), retry_request, None, ("b", 1), now)
                    .await
                    .unwrap()
            );
            holder.rollback().await.unwrap();
            assert!(
                second
                    .allocation_reserve(locked_tenant, Some(1), retry_request, None, ("b", 1), now)
                    .await
                    .unwrap()
            );
            let usage = second
                .allocation_usage(locked_tenant, &accounts, now)
                .await
                .unwrap();
            assert_eq!(usage.iter().map(|row| row.requests).sum::<u64>(), 2);
        }
        let duplicate_tenant = Uuid::now_v7();
        let duplicate_request = Uuid::now_v7();
        let duplicate_session = Uuid::now_v7();
        assert!(
            restarted
                .allocation_reserve(
                    duplicate_tenant,
                    Some(0),
                    duplicate_request,
                    Some(duplicate_session),
                    ("a", 1),
                    now
                )
                .await
                .unwrap()
        );
        for revision in [Some(1), None] {
            assert!(
                !second
                    .allocation_reserve(
                        duplicate_tenant,
                        revision,
                        duplicate_request,
                        Some(duplicate_session),
                        ("b", 1),
                        now + 1
                    )
                    .await
                    .unwrap()
            );
            assert_eq!(
                second.allocation_revision(duplicate_tenant).await.unwrap(),
                1
            );
            let usage = second
                .allocation_usage(duplicate_tenant, &accounts, now)
                .await
                .unwrap();
            assert_eq!(usage.len(), 1);
            assert_eq!(usage[0].account, "a");
            assert_eq!(usage[0].requests, 1);
            assert_eq!(
                second
                    .allocation_session(duplicate_tenant, duplicate_session, now)
                    .await
                    .unwrap()
                    .unwrap()
                    .0,
                "a"
            );
        }
        let concurrent_tenant = Uuid::now_v7();
        let mut committed = 0_i64;
        for index in 0..16 {
            let request = Uuid::now_v7();
            let projection_time = now + index * 5001;
            let account = format!("concurrent-account-{}", index % 2);
            let observed = now - (index / 4) * HOUR_MS;
            let (projection, reservation) = tokio::join!(
                second.refresh_allocation_usage(concurrent_tenant, Uuid::nil(), projection_time),
                restarted.allocation_reserve(
                    concurrent_tenant,
                    None,
                    request,
                    None,
                    (&account, 1),
                    observed
                ),
            );
            if matches!(reservation, Ok(true)) {
                committed += 1;
            }
            let ledger: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM group_allocation_reservations WHERE tenant_id=$1",
            )
            .bind(concurrent_tenant.to_string())
            .fetch_one(&restarted.pool)
            .await
            .unwrap();
            let pending:i64=sqlx::query_scalar("SELECT CAST(COALESCE(SUM(requests),0) AS BIGINT) FROM group_allocation_pending WHERE tenant_id=$1")
                .bind(concurrent_tenant.to_string()).fetch_one(&restarted.pool).await.unwrap();
            assert_eq!(ledger, committed);
            assert_eq!(
                pending,
                ledger,
                "snapshot refresh and existing-session append preserve exact pending counts; refresh_ok={}",
                projection.is_ok()
            );
        }
        let mut refreshed = false;
        for tick in 0..3 {
            if restarted
                .refresh_allocation_usage(
                    concurrent_tenant,
                    Uuid::nil(),
                    now + 100000 + tick * 5001,
                )
                .await
                .is_ok()
            {
                refreshed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            refreshed,
            "a later independent background tick reconciles pending counts"
        );
        let pending:i64=sqlx::query_scalar("SELECT CAST(COALESCE(SUM(requests),0) AS BIGINT) FROM group_allocation_pending WHERE tenant_id=$1")
            .bind(concurrent_tenant.to_string()).fetch_one(&restarted.pool).await.unwrap();
        assert_eq!(pending, committed);
        assert!(
            committed > 0,
            "at least one concurrent old-session count commits"
        );
        let backlog_tenant = Uuid::now_v7();
        sqlx::query("WITH RECURSIVE sequence(n) AS (SELECT 1 UNION ALL SELECT n+1 FROM sequence WHERE n<25000) INSERT INTO group_allocation_reservations(tenant_id,request_id,account_id,session_key,generation,created_at) SELECT $1,CAST(n AS TEXT),'a','',1,$2 FROM sequence")
            .bind(backlog_tenant.to_string()).bind(now).execute(&restarted.pool).await.unwrap();
        sqlx::query("INSERT INTO group_allocation_pending(tenant_id,account_id,hour_bucket,requests) VALUES($1,'a',$2,25000)")
            .bind(backlog_tenant.to_string()).bind(now/HOUR_MS).execute(&restarted.pool).await.unwrap();
        let started = std::time::Instant::now();
        for _ in 0..32 {
            let usage = tokio::time::timeout(
                std::time::Duration::from_millis(25),
                restarted.allocation_usage(backlog_tenant, &accounts, now),
            )
            .await
            .expect("compact history stays inside allocation budget with 25000 pending requests")
            .unwrap();
            assert_eq!(usage.len(), 1);
            assert_eq!(usage[0].requests, 25000);
        }
        eprintln!(
            "allocation compact reads: 32/32 in budget, 25000 pending requests, elapsed_ms={}",
            started.elapsed().as_millis()
        );
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
