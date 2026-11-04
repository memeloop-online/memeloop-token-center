//! Background-only history coverage. Export receipts and applied revisions survive restarts.
use super::{
    AppError, Database, DatabaseBackend,
    allocation::{DAY_MS, HOUR_MS},
};
use crate::request_history::{
    ClickHouse, HistorySnapshot,
    allocation::{ALLOCATION_PAGE, AllocationContribution, AllocationCursor},
};
use sqlx::{Any, Row, Transaction};
use std::{collections::BTreeMap, time::Duration};
use uuid::Uuid;

const CAPTURE_MS: i64 = 600_000;
const LEASE_MS: i64 = 10_000;

fn placeholders(rows: usize, width: usize) -> String {
    let types: &[&str] = match width {
        9 => &[
            "TEXT", "TEXT", "TEXT", "BIGINT", "BIGINT", "TEXT", "BIGINT", "BIGINT", "BIGINT",
        ],
        5 => &["TEXT", "TEXT", "BIGINT", "BIGINT", "BIGINT"],
        3 => &["TEXT", "TEXT", "BIGINT"],
        _ => unreachable!("fixed compact history row shape"),
    };
    (0..rows)
        .map(|row| {
            format!(
                "({})",
                (1..=width)
                    .map(|column| format!(
                        "CAST(${} AS {})",
                        row * width + column,
                        types[column - 1]
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

async fn receipts(
    tx: &mut Transaction<'_, Any>,
    values: &[AllocationContribution],
    now: i64,
    bootstrap: bool,
) -> Result<(), AppError> {
    if values.is_empty() {
        return Ok(());
    }
    let clock = values.len() * 9 + 1;
    // The interpolation contains generated parameter positions only.
    let sql = format!(
        "WITH incoming(tenant_id,source,request_id,revision,created_at,account_id,requests,tokens,covered) AS (VALUES {})
         INSERT INTO group_allocation_history_receipts(tenant_id,source,request_id,revision,created_at,account_id,requests,tokens,covered,bootstrap)
         SELECT i.tenant_id,i.source,i.request_id,i.revision,i.created_at,i.account_id,i.requests,i.tokens,i.covered,${marker}
         FROM incoming i JOIN group_allocation_history_state s ON s.tenant_id=i.tenant_id
         WHERE s.capture_until>${clock} AND i.created_at>=(${clock}/3600000-23)*3600000
         ON CONFLICT(tenant_id,source,request_id,revision) DO UPDATE SET bootstrap=CASE WHEN excluded.bootstrap=1 THEN 1 ELSE group_allocation_history_receipts.bootstrap END",
        placeholders(values.len(), 9), marker=clock+1
    );
    let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
    for value in values {
        query = query
            .bind(value.tenant_id.to_string())
            .bind(&value.source)
            .bind(value.request_id.to_string())
            .bind(value.revision)
            .bind(value.created_at)
            .bind(&value.account_id)
            .bind(value.requests)
            .bind(value.tokens)
            .bind(value.covered);
    }
    query
        .bind(now)
        .bind(i64::from(bootstrap))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

fn contribution(row: &sqlx::any::AnyRow, tenant: Uuid) -> Result<AllocationContribution, AppError> {
    Ok(AllocationContribution {
        tenant_id: tenant,
        source: row.try_get("source")?,
        request_id: Uuid::parse_str(row.try_get("request_id")?)
            .map_err(|_| AppError::internal("allocation_history_identity"))?,
        revision: row.try_get("revision")?,
        created_at: row.try_get("created_at")?,
        account_id: row.try_get("account_id")?,
        requests: row.try_get("requests")?,
        tokens: row.try_get("tokens")?,
        covered: row.try_get("covered")?,
    })
}

impl Database {
    /// Called after the exporter has successfully appended these exact snapshots.
    pub(super) async fn allocation_history_acknowledged(
        &self,
        tx: &mut Transaction<'_, Any>,
        snapshots: &[HistorySnapshot],
    ) -> Result<(), AppError> {
        let compact = snapshots
            .iter()
            .filter_map(AllocationContribution::from_snapshot)
            .collect::<Vec<_>>();
        receipts(tx, &compact, super::unix_millis(), false).await
    }

    /// Background source selection uses the same verified reader switch as history APIs.
    pub(crate) async fn refresh_allocation_history(
        &self,
        tenant: Uuid,
        account: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        let reader = tokio::time::timeout(Duration::from_millis(100), self.history_reader())
            .await
            .map_err(|_| AppError::internal("allocation_history_source_timeout"))??;
        if let Some(reader) = reader {
            self.refresh_allocation_clickhouse(&reader, tenant, now)
                .await
        } else {
            tokio::time::timeout(
                Duration::from_millis(100),
                self.refresh_allocation_usage(tenant, account, now),
            )
            .await
            .map_err(|_| AppError::internal("allocation_history_refresh_timeout"))?
        }
    }

    async fn refresh_allocation_clickhouse(
        &self,
        reader: &ClickHouse,
        tenant: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        self.refresh_allocation_clickhouse_batches(reader, tenant, now, 16)
            .await
    }

    async fn refresh_allocation_clickhouse_batches(
        &self,
        reader: &ClickHouse,
        tenant: Uuid,
        now: i64,
        batches: usize,
    ) -> Result<(), AppError> {
        let lease = Uuid::now_v7().to_string();
        for _ in 0..batches {
            let state = sqlx::query(
            "INSERT INTO group_allocation_history_state(tenant_id,scan_from,scan_to,capture_until,lease,lease_until)
             VALUES($1,$2,$3,$4,$5,$6)
             ON CONFLICT(tenant_id) DO UPDATE SET
                scan_from=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN $2 ELSE group_allocation_history_state.scan_from END,
                scan_to=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN $3 ELSE group_allocation_history_state.scan_to END,
                cursor_created=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN 0 ELSE group_allocation_history_state.cursor_created END,
                cursor_key=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN '' ELSE group_allocation_history_state.cursor_key END,
                cursor_request=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN '' ELSE group_allocation_history_state.cursor_request END,
                cursor_source=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN '' ELSE group_allocation_history_state.cursor_source END,
                scanned=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN 0 ELSE group_allocation_history_state.scanned END,
                ready=CASE WHEN group_allocation_history_state.capture_until<=$3 THEN 0 ELSE group_allocation_history_state.ready END,
                capture_until=$4,lease=$5,lease_until=$6
             WHERE group_allocation_history_state.lease_until<=$3 OR group_allocation_history_state.lease=$5
             RETURNING scan_from,scan_to,cursor_created,cursor_key,cursor_request,cursor_source,scanned"
        ).bind(tenant.to_string()).bind((now/HOUR_MS-23)*HOUR_MS).bind(now).bind(now+CAPTURE_MS)
            .bind(&lease).bind(now+LEASE_MS).fetch_optional(&self.pool).await?;
            let Some(state) = state else {
                return Ok(());
            };
            if state.try_get::<i64, _>("scanned")? == 0 {
                let cursor = AllocationCursor {
                    created_at: state.try_get("cursor_created")?,
                    key_id: state.try_get("cursor_key")?,
                    request_id: state.try_get("cursor_request")?,
                    source: state.try_get("cursor_source")?,
                };
                let page = reader
                    .allocation_page(
                        tenant,
                        state.try_get("scan_from")?,
                        state.try_get("scan_to")?,
                        &cursor,
                    )
                    .await?;
                let next = page
                    .last()
                    .map(|row| AllocationCursor {
                        created_at: row.contribution.created_at,
                        key_id: row.key_id.clone(),
                        request_id: row.contribution.request_id.to_string(),
                        source: row.contribution.source.clone(),
                    })
                    .unwrap_or(cursor);
                let scanned = i64::from(page.len() < ALLOCATION_PAGE);
                let values = page
                    .into_iter()
                    .map(|row| row.contribution)
                    .collect::<Vec<_>>();
                let mut tx = self.begin_write_transaction().await?;
                let changed = sqlx::query("UPDATE group_allocation_history_state SET cursor_created=$3,cursor_key=$4,cursor_request=$5,cursor_source=$6,scanned=$7 WHERE tenant_id=$1 AND lease=$2 AND lease_until>$8")
                .bind(tenant.to_string()).bind(&lease).bind(next.created_at).bind(next.key_id)
                .bind(next.request_id).bind(next.source).bind(scanned).bind(super::unix_millis())
                .execute(&mut *tx).await?.rows_affected();
                if changed == 0 {
                    return Ok(());
                }
                receipts(&mut tx, &values, now, true).await?;
                tx.commit().await?;
            }
            let more = tokio::time::timeout(
                Duration::from_millis(100),
                self.apply_allocation_history(tenant, &lease, now),
            )
            .await
            .map_err(|_| AppError::internal("allocation_history_apply_timeout"))??;
            if !more {
                break;
            }
        }
        sqlx::query("UPDATE group_allocation_history_state SET lease_until=0 WHERE tenant_id=$1 AND lease=$2")
            .bind(tenant.to_string()).bind(&lease).execute(&self.pool).await?;
        Ok(())
    }

    async fn apply_allocation_history(
        &self,
        tenant: Uuid,
        lease: &str,
        now: i64,
    ) -> Result<bool, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
                .execute(&mut *tx)
                .await?;
            sqlx::query("SELECT set_config('lock_timeout','5ms',true),set_config('statement_timeout','80ms',true),set_config('jit','off',true)").execute(&mut *tx).await?;
        }
        let mode_sql = if matches!(self.backend, DatabaseBackend::PostgreSql) {
            "SELECT mode FROM request_history_state WHERE id=1 FOR SHARE"
        } else {
            "SELECT mode FROM request_history_state WHERE id=1"
        };
        let mode: String = sqlx::query_scalar(sqlx::AssertSqlSafe(mode_sql))
            .fetch_one(&mut *tx)
            .await?;
        if mode != "clickhouse" {
            return Ok(false);
        }
        // Match the PostgreSQL-source lock order: source mode, selection revision, worker state.
        sqlx::query("INSERT INTO group_allocation_revisions(tenant_id,revision,refreshed_at) VALUES($1,1,$2) ON CONFLICT(tenant_id) DO UPDATE SET revision=group_allocation_revisions.revision+1,refreshed_at=$2")
            .bind(tenant.to_string()).bind(now).execute(&mut *tx).await?;
        let state = sqlx::query("UPDATE group_allocation_history_state SET lease_until=$3 WHERE tenant_id=$1 AND lease=$2 AND lease_until>$4 RETURNING scanned,ready")
            .bind(tenant.to_string()).bind(lease).bind(now+LEASE_MS).bind(now).fetch_optional(&mut *tx).await?;
        let Some(state) = state else {
            return Ok(false);
        };
        let rows = sqlx::query("SELECT source,request_id,revision,created_at,account_id,requests,tokens,covered FROM group_allocation_history_receipts WHERE tenant_id=$1 ORDER BY bootstrap DESC,source,request_id,revision LIMIT 256")
            .bind(tenant.to_string()).fetch_all(&mut *tx).await?;
        let mut latest = BTreeMap::new();
        for row in &rows {
            let value = contribution(row, tenant)?;
            latest.insert((value.source.clone(), value.request_id), value);
        }
        let mut previous = BTreeMap::new();
        if !latest.is_empty() {
            let keys = latest
                .keys()
                .map(|(_, request)| request.to_string())
                .collect::<Vec<_>>();
            let params = (2..keys.len() + 2)
                .map(|n| format!("${n}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT source,request_id,revision,created_at,account_id,requests,tokens,covered FROM group_allocation_history_contributions WHERE tenant_id=$1 AND request_id IN ({params})"
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql)).bind(tenant.to_string());
            for key in &keys {
                query = query.bind(key);
            }
            for row in query.fetch_all(&mut *tx).await? {
                let value = contribution(&row, tenant)?;
                previous.insert((value.source.clone(), value.request_id), value);
            }
        }
        let mut deltas = BTreeMap::<(String, i64), (i64, i64)>::new();
        let mut changed = Vec::new();
        for (key, value) in &latest {
            let old = previous.get(key);
            if old.is_some_and(|old| old.revision >= value.revision) {
                continue;
            }
            for (item, sign) in old
                .into_iter()
                .map(|v| (v, -1))
                .chain(std::iter::once((value, 1)))
            {
                if item.requests == 0 {
                    continue;
                }
                let delta = deltas
                    .entry((item.account_id.clone(), item.created_at / HOUR_MS))
                    .or_default();
                delta.0 += sign * item.requests;
                delta.1 += sign * item.tokens;
            }
            changed.push(value);
        }
        if !changed.is_empty() {
            let sql = format!(
                "INSERT INTO group_allocation_history_contributions(tenant_id,source,request_id,revision,created_at,account_id,requests,tokens,covered) VALUES {} ON CONFLICT(tenant_id,source,request_id) DO UPDATE SET revision=excluded.revision,created_at=excluded.created_at,account_id=excluded.account_id,requests=excluded.requests,tokens=excluded.tokens,covered=excluded.covered WHERE group_allocation_history_contributions.revision<excluded.revision",
                placeholders(changed.len(), 9)
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for value in changed {
                query = query
                    .bind(tenant.to_string())
                    .bind(&value.source)
                    .bind(value.request_id.to_string())
                    .bind(value.revision)
                    .bind(value.created_at)
                    .bind(&value.account_id)
                    .bind(value.requests)
                    .bind(value.tokens)
                    .bind(value.covered);
            }
            query.execute(&mut *tx).await?;
        }
        if !deltas.is_empty() {
            let sql = format!(
                "INSERT INTO group_allocation_history_hours(tenant_id,account_id,hour_bucket,requests,tokens) VALUES {} ON CONFLICT(tenant_id,account_id,hour_bucket) DO UPDATE SET requests=group_allocation_history_hours.requests+excluded.requests,tokens=group_allocation_history_hours.tokens+excluded.tokens",
                placeholders(deltas.len(), 5)
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for ((account, hour), (requests, tokens)) in deltas {
                query = query
                    .bind(tenant.to_string())
                    .bind(account)
                    .bind(hour)
                    .bind(requests)
                    .bind(tokens);
            }
            query.execute(&mut *tx).await?;
        }
        if !rows.is_empty() {
            // Only this bounded batch's exact revisions are acknowledged by the consumer.
            let sql = format!(
                "WITH batch(source,request_id,revision) AS (VALUES {}) DELETE FROM group_allocation_history_receipts WHERE tenant_id=${} AND EXISTS (SELECT 1 FROM batch b WHERE b.source=group_allocation_history_receipts.source AND b.request_id=group_allocation_history_receipts.request_id AND b.revision=group_allocation_history_receipts.revision)",
                placeholders(rows.len(), 3),
                rows.len() * 3 + 1
            );
            let mut query = sqlx::query(sqlx::AssertSqlSafe(sql));
            for row in &rows {
                query = query
                    .bind(row.try_get::<String, _>("source")?)
                    .bind(row.try_get::<String, _>("request_id")?)
                    .bind(row.try_get::<i64, _>("revision")?);
            }
            query.bind(tenant.to_string()).execute(&mut *tx).await?;
        }
        let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT 1 FROM group_allocation_history_receipts WHERE tenant_id=$1 LIMIT 1) q")
            .bind(tenant.to_string()).fetch_one(&mut *tx).await?;
        let bootstrap_pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM (SELECT 1 FROM group_allocation_history_receipts WHERE tenant_id=$1 AND bootstrap=1 LIMIT 1) q").bind(tenant.to_string()).fetch_one(&mut *tx).await?;
        let scanned = state.try_get::<i64, _>("scanned")? != 0;
        let ready = state.try_get::<i64, _>("ready")? != 0 || (scanned && bootstrap_pending == 0);
        if ready {
            sqlx::query("DELETE FROM group_allocation_usage WHERE tenant_id=$1")
                .bind(tenant.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO group_allocation_usage(tenant_id,account_id,hour_bucket,requests,tokens) SELECT tenant_id,account_id,hour_bucket,requests,tokens FROM group_allocation_history_hours WHERE tenant_id=$1 AND hour_bucket>=$2 AND requests>0")
                .bind(tenant.to_string()).bind(now/HOUR_MS-23).execute(&mut *tx).await?;
            // The compact applied revision, including a confirmed tombstone, is the coverage ACK.
            sqlx::query("DELETE FROM group_allocation_reservations WHERE tenant_id=$1 AND (created_at<$2 OR EXISTS (SELECT 1 FROM group_allocation_history_contributions c WHERE c.tenant_id=$1 AND c.source='native' AND c.request_id=group_allocation_reservations.request_id AND c.covered=1))")
                .bind(tenant.to_string()).bind(now-DAY_MS).execute(&mut *tx).await?;
            sqlx::query("DELETE FROM group_allocation_pending WHERE tenant_id=$1")
                .bind(tenant.to_string())
                .execute(&mut *tx)
                .await?;
            sqlx::query("INSERT INTO group_allocation_pending(tenant_id,account_id,hour_bucket,requests) SELECT tenant_id,account_id,created_at/3600000,COUNT(*) FROM group_allocation_reservations WHERE tenant_id=$1 AND created_at>=$2 GROUP BY tenant_id,account_id,created_at/3600000")
                .bind(tenant.to_string()).bind((now/HOUR_MS-23)*HOUR_MS).execute(&mut *tx).await?;
        }
        for table in [
            "group_allocation_history_receipts",
            "group_allocation_history_contributions",
        ] {
            // Table identifiers are a fixed internal allowlist; the time and tenant are bound.
            let key = if table.ends_with("receipts") {
                "source,request_id,revision"
            } else {
                "source,request_id"
            };
            let sql = format!(
                "DELETE FROM {table} WHERE tenant_id=$1 AND ({key}) IN (SELECT {key} FROM {table} WHERE tenant_id=$1 AND created_at<$2 ORDER BY created_at LIMIT 512)"
            );
            sqlx::query(sqlx::AssertSqlSafe(sql))
                .bind(tenant.to_string())
                .bind((now / HOUR_MS - 23) * HOUR_MS)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query(
            "DELETE FROM group_allocation_history_hours WHERE tenant_id=$1 AND hour_bucket<$2",
        )
        .bind(tenant.to_string())
        .bind(now / HOUR_MS - 23)
        .execute(&mut *tx)
        .await?;
        let more = !scanned || pending != 0;
        sqlx::query("UPDATE group_allocation_history_state SET ready=$3,lease_until=$4 WHERE tenant_id=$1 AND lease=$2")
            .bind(tenant.to_string()).bind(lease).bind(i64::from(ready)).bind(if more {now+LEASE_MS} else {0}).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(more)
    }
}

#[cfg(test)]
mod tests;
