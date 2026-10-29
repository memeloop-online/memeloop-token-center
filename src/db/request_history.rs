use super::*;
use crate::request_history::{HistoryError, HistoryIdentity, HistoryScope, HistorySnapshot};

const COLUMNS: &str = "source, request_id, tenant_id, key_id, created_at";

impl Database {
    pub async fn history_start(&self) -> Result<(), HistoryError> {
        let mut transaction = self.pool.begin().await?;
        for (table, identity) in [
            ("request_records", Some(("native", "id", "created_at"))),
            ("generation_jobs", Some(("generation", "id", "created_at"))),
            (
                "session_archive_unlinked_requests",
                Some(("archive", "archive_request_id", "source_started_at")),
            ),
            ("proxy_operational_terminals", None),
            ("pending_proxy_settlements", None),
            ("conversation_observations", None),
            ("request_archive_spools", None),
            ("response_archive_spools", None),
            ("request_egress_attempts", None),
            ("generation_stats_facts", None),
            ("session_archive_correlations", None),
            ("key_records", None),
            ("principals", None),
            ("tenants", None),
        ] {
            for event in ["INSERT", "UPDATE", "DELETE"] {
                let record = if event == "DELETE" { "OLD" } else { "NEW" };
                let select = if let Some((source, id, created_at)) = identity {
                    format!(
                        "SELECT '{source}', {record}.{id}, {record}.tenant_id, {record}.key_id, {record}.{created_at}"
                    )
                } else {
                    let predicate = match table {
                        "session_archive_correlations" => format!(
                            "source = 'archive' AND request_id IN (SELECT archive_request_id FROM session_archive_unlinked_requests WHERE tenant_id = {record}.tenant_id AND source = {record}.source AND external_request_id = {record}.external_request_id)"
                        ),
                        "generation_stats_facts" => {
                            format!("source = 'generation' AND request_id = {record}.job_id")
                        }
                        "key_records" => format!("key_id = {record}.id"),
                        "principals" => format!(
                            "key_id IN (SELECT id FROM key_records WHERE principal_id = {record}.id)"
                        ),
                        "tenants" => format!("tenant_id = {record}.id"),
                        _ => format!("request_id = {record}.request_id"),
                    };
                    format!("SELECT {COLUMNS} FROM request_history_source_ids WHERE {predicate}")
                };
                let select = format!(
                    "{select} {} (SELECT mode FROM request_history_state WHERE id = 1) != 'off'",
                    if identity.is_some() { "WHERE" } else { "AND" }
                );
                let revision = match self.backend {
                    DatabaseBackend::PostgreSql => "nextval('request_history_export_revision_seq')",
                    DatabaseBackend::Sqlite => "excluded.revision",
                };
                let enqueue = format!(
                    "INSERT INTO request_history_export ({COLUMNS}) {select} ON CONFLICT (source, request_id) DO UPDATE SET revision = {revision}, snapshot = NULL;"
                );
                let name = format!("history_{table}_{}", event.to_lowercase());
                let sql = match self.backend {
                    DatabaseBackend::PostgreSql => format!(
                        "CREATE OR REPLACE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $history$ BEGIN {enqueue} RETURN NULL; END $history$;
                         DROP TRIGGER IF EXISTS {name} ON {table};
                         CREATE TRIGGER {name} AFTER {event} ON {table} FOR EACH ROW EXECUTE FUNCTION {name}();"
                    ),
                    DatabaseBackend::Sqlite => format!(
                        "CREATE TRIGGER IF NOT EXISTS {name} AFTER {event} ON {table} BEGIN {enqueue} END;"
                    ),
                };
                sqlx::raw_sql(sqlx::AssertSqlSafe(sql))
                    .execute(&mut *transaction)
                    .await?;
            }
        }
        sqlx::query(
            "UPDATE request_history_state SET mode = 'shadow' WHERE id = 1 AND mode = 'off'",
        )
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn history_mode(&self) -> Result<String, HistoryError> {
        Ok(
            sqlx::query_scalar("SELECT mode FROM request_history_state WHERE id = 1")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub(crate) async fn history_scope(
        &self,
        tenant: Option<&str>,
    ) -> Result<HistoryScope, HistoryError> {
        let Some(tenant) = tenant else {
            return Ok(HistoryScope::GlobalOperator);
        };
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM tenants WHERE external_id = $1")
                .bind(tenant)
                .fetch_optional(&self.pool)
                .await?;
        Ok(match id {
            Some(id) => HistoryScope::Tenant(parse_uuid(id)?),
            None => HistoryScope::EmptyTenant,
        })
    }

    pub async fn history_backfill_page(&self, page_size: i64) -> Result<usize, HistoryError> {
        if page_size <= 0 || self.history_mode().await? == "off" {
            return Err(HistoryError::Invalid(
                "start history capture before backfill; page_size must be positive",
            ));
        }
        let mut transaction = self.pool.begin().await?;
        let lock = if matches!(self.backend, DatabaseBackend::PostgreSql) {
            " FOR UPDATE"
        } else {
            ""
        };
        let sql = format!(
            "SELECT source, after_id FROM request_history_backfill WHERE done = 0 ORDER BY source LIMIT 1{lock}"
        );
        let Some(checkpoint) = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_optional(&mut *transaction)
            .await?
        else {
            return Ok(0);
        };
        let source: String = checkpoint.try_get("source")?;
        let after: String = checkpoint.try_get("after_id")?;
        let sql = format!(
            "SELECT {COLUMNS} FROM request_history_source_ids WHERE source = $1 AND request_id > $2 ORDER BY request_id LIMIT $3"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(&source)
            .bind(&after)
            .bind(page_size)
            .fetch_all(&mut *transaction)
            .await?;
        for row in &rows {
            sqlx::query("INSERT INTO request_history_export (source, request_id, tenant_id, key_id, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (source, request_id) DO NOTHING")
                .bind(&source).bind(row.try_get::<String, _>("request_id")?)
                .bind(row.try_get::<String, _>("tenant_id")?).bind(row.try_get::<String, _>("key_id")?)
                .bind(row.try_get::<i64, _>("created_at")?).execute(&mut *transaction).await?;
        }
        let after = rows
            .last()
            .map(|row| row.try_get::<String, _>("request_id"))
            .transpose()?
            .unwrap_or(after);
        sqlx::query(
            "UPDATE request_history_backfill SET after_id = $1, done = $2 WHERE source = $3",
        )
        .bind(after)
        .bind(i64::from(rows.len() < page_size as usize))
        .bind(source)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(rows.len())
    }

    pub async fn history_backfill_done(&self) -> Result<bool, HistoryError> {
        let remaining: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM request_history_backfill WHERE done = 0")
                .fetch_one(&self.pool)
                .await?;
        Ok(remaining == 0)
    }

    pub async fn history_pending(
        &self,
        page_size: i64,
    ) -> Result<Vec<HistorySnapshot>, HistoryError> {
        let rows = sqlx::query("SELECT revision, source, request_id, tenant_id, key_id, created_at, snapshot FROM request_history_export ORDER BY revision LIMIT $1")
            .bind(page_size).fetch_all(&self.pool).await?;
        let mut snapshots = Vec::new();
        for row in rows {
            let revision: i64 = row.try_get("revision")?;
            let frozen: Option<String> = row.try_get("snapshot")?;
            if let Some(frozen) = frozen {
                snapshots.push(serde_json::from_str(&frozen)?);
                continue;
            }
            let identity = history_identity(&row)?;
            let snapshot = self.history_snapshot(identity, revision).await?;
            let frozen = serde_json::to_string(&snapshot)?;
            let stored: Option<String> = sqlx::query_scalar("UPDATE request_history_export SET snapshot = COALESCE(snapshot, $1) WHERE revision = $2 RETURNING snapshot")
                .bind(frozen).bind(revision).fetch_optional(&self.pool).await?;
            if let Some(stored) = stored {
                snapshots.push(serde_json::from_str(&stored)?);
            }
        }
        Ok(snapshots)
    }

    pub async fn history_acknowledge(
        &self,
        snapshots: &[HistorySnapshot],
    ) -> Result<(), HistoryError> {
        let mut transaction = self.pool.begin().await?;
        for snapshot in snapshots {
            sqlx::query("DELETE FROM request_history_export WHERE revision = $1 AND source = $2 AND request_id = $3")
                .bind(snapshot.revision).bind(&snapshot.identity.source).bind(snapshot.identity.request_id.to_string())
                .execute(&mut *transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn history_pending_count(&self) -> Result<i64, HistoryError> {
        Ok(
            sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export")
                .fetch_one(&self.pool)
                .await?,
        )
    }

    pub async fn history_revision(&self) -> Result<i64, HistoryError> {
        let sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM request_history_export_revision_seq"
            }
            DatabaseBackend::Sqlite => {
                "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'request_history_export'), 0)"
            }
        };
        Ok(sqlx::query_scalar(sql).fetch_one(&self.pool).await?)
    }

    pub async fn history_save_verification(
        &self,
        revision: i64,
        report: &Value,
    ) -> Result<(), HistoryError> {
        sqlx::query("UPDATE request_history_state SET verified_revision = $1, verification_json = $2 WHERE id = 1")
            .bind(revision).bind(serde_json::to_string(report)?).execute(&self.pool).await?;
        Ok(())
    }

    pub async fn history_switch(&self, clickhouse: bool) -> Result<(), HistoryError> {
        let mut transaction = self.pool.begin().await?;
        if clickhouse {
            let revision: Option<i64> = sqlx::query_scalar(
                "SELECT verified_revision FROM request_history_state WHERE id = 1",
            )
            .fetch_one(&mut *transaction)
            .await?;
            if revision != Some(self.history_revision().await?)
                || self.history_pending_count().await? != 0
                || !self.history_backfill_done().await?
            {
                return Err(HistoryError::Changed);
            }
        }
        sqlx::query("UPDATE request_history_state SET mode = $1 WHERE id = 1")
            .bind(if clickhouse { "clickhouse" } else { "shadow" })
            .execute(&mut *transaction)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub(crate) async fn history_identity_for_view(
        &self,
        request_id: Uuid,
    ) -> Result<HistoryIdentity, HistoryError> {
        let sql = format!(
            "SELECT {COLUMNS} FROM request_history_source_ids WHERE request_id = $1 ORDER BY source DESC LIMIT 1"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(request_id.to_string())
            .fetch_one(&self.pool)
            .await?;
        history_identity(&row)
    }

    pub(crate) async fn history_snapshot(
        &self,
        identity: HistoryIdentity,
        revision: i64,
    ) -> Result<HistorySnapshot, HistoryError> {
        let payload = self
            .request_history_view(&identity)
            .await?
            .map(|view| serde_json::to_value(&view))
            .transpose()?;
        let filter_cost_micros = match identity.source.as_str() {
            "native" => sqlx::query_scalar::<_, Option<i64>>("SELECT CASE WHEN completed_at IS NOT NULL THEN cost_micros ELSE NULL END FROM request_records WHERE id = $1 AND created_at = $2")
                .bind(identity.request_id.to_string()).bind(identity.created_at).fetch_optional(&self.pool).await?.flatten(),
            "generation" => sqlx::query_scalar::<_, Option<i64>>("SELECT cost_micros FROM generation_stats_facts WHERE job_id = $1")
                .bind(identity.request_id.to_string()).fetch_optional(&self.pool).await?.flatten(),
            _ => None,
        };
        Ok(HistorySnapshot {
            identity,
            revision,
            filter_cost_micros,
            payload,
        })
    }
}

fn parse_uuid(value: String) -> Result<Uuid, HistoryError> {
    value
        .parse()
        .map_err(|_| HistoryError::Invalid("stored history identity"))
}

fn history_identity(row: &AnyRow) -> Result<HistoryIdentity, HistoryError> {
    Ok(HistoryIdentity {
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        key_id: parse_uuid(row.try_get("key_id")?)?,
        created_at: row.try_get("created_at")?,
        request_id: parse_uuid(row.try_get("request_id")?)?,
        source: row.try_get("source")?,
    })
}
