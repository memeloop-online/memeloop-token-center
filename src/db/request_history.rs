use super::*;
use crate::request_history::{
    HistoryError, HistoryIdentity, HistoryScope, HistorySnapshot, HistoryStatistics,
};

const COLUMNS: &str = "source, request_id, tenant_id, key_id, created_at";

impl Database {
    /// A selected ClickHouse reader fails closed if its endpoint is missing.
    pub(crate) async fn history_reader(
        &self,
    ) -> Result<Option<crate::request_history::ClickHouse>, HistoryError> {
        if std::env::var_os("MTC_REQUEST_HISTORY_URL").is_none() {
            if self.history_mode().await? == "clickhouse" {
                return Err(HistoryError::Unavailable);
            }
            return Ok(None);
        }
        if self.history_mode().await? != "clickhouse" {
            return Ok(None);
        }
        crate::request_history::ClickHouse::reader_from_env(
            self.http_clients
                .handle(crate::http_settings::HttpPurpose::General),
        )
    }

    pub(crate) async fn history_key_scope(
        &self,
        key_id: Uuid,
    ) -> Result<HistoryScope, HistoryError> {
        let tenant: Option<String> = sqlx::query_scalar("SELECT tenant_id FROM key_records WHERE id = $1 UNION SELECT tenant_id FROM request_history_source_ids WHERE key_id = $1 LIMIT 1")
            .bind(key_id.to_string()).fetch_optional(&self.pool).await?;
        Ok(match tenant {
            Some(tenant) => HistoryScope::Key {
                tenant_id: parse_uuid(tenant)?,
                key_id,
            },
            None => HistoryScope::EmptyTenant,
        })
    }

    pub(crate) async fn history_request_list(
        &self,
        client: &crate::request_history::ClickHouse,
        scope: HistoryScope,
        filter: &RequestListFilter,
    ) -> Result<Vec<RequestView>, AppError> {
        let query = crate::request_history_api::query_from_request_filter(scope.clone(), filter)?;
        let mut snapshots = client.snapshots(&query).await?;
        snapshots.truncate(query.limit + usize::from(filter.lookahead));
        snapshots
            .into_iter()
            .map(|snapshot| {
                let mut view: RequestView = serde_json::from_value(
                    snapshot
                        .payload
                        .ok_or_else(|| AppError::internal("history_payload_missing"))?,
                )
                .map_err(|_| AppError::internal("history_payload_invalid"))?;
                if matches!(scope, HistoryScope::Key { .. }) {
                    view.credential_identity = None;
                }
                Ok(view)
            })
            .collect()
    }

    pub(crate) async fn history_archive_refs(
        &self,
        client: &crate::request_history::ClickHouse,
        scope: HistoryScope,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        use crate::request_history::{
            Comparison, Dimension, HistoryPredicate, HistoryQuery, PredicateValue,
        };
        let query = HistoryQuery {
            scope: scope.clone(),
            from_created_at: None,
            to_created_at: None,
            cursor: None,
            limit: 1,
            predicates: vec![HistoryPredicate {
                dimension: Dimension::RequestId,
                comparison: Comparison::Equals,
                value: PredicateValue::Uuid(request_id),
            }],
        };
        let snapshot = client
            .snapshots(&query)
            .await?
            .into_iter()
            .next()
            .ok_or(AppError::NotFound)?;
        let mut refs: RequestArchiveRefs =
            serde_json::from_value(snapshot.read_model["archive"].clone())
                .map_err(|_| AppError::internal("history_archive_projection_missing"))?;
        // All read surfaces use the same corrected public entity, even when the
        // locator was captured from a source's more specialized detail reader.
        refs.view = serde_json::from_value(snapshot.payload.ok_or(AppError::NotFound)?)
            .map_err(|_| AppError::internal("history_payload_invalid"))?;
        if matches!(scope, HistoryScope::Key { .. }) {
            refs.view.credential_identity = None;
        }
        Ok(refs)
    }

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
            ("request_stats_facts", None),
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
                let identity_guard = if event == "UPDATE" {
                    if let Some((_, id, created_at)) = identity {
                        let changed = format!(
                            "OLD.{id} <> NEW.{id} OR OLD.tenant_id <> NEW.tenant_id OR OLD.key_id <> NEW.key_id OR OLD.{created_at} <> NEW.{created_at}"
                        );
                        match self.backend {
                            DatabaseBackend::PostgreSql => format!(
                                "IF {changed} THEN RAISE EXCEPTION 'request_history_identity_changed'; END IF;"
                            ),
                            DatabaseBackend::Sqlite => format!(
                                "SELECT RAISE(ABORT, 'request_history_identity_changed') WHERE {changed};"
                            ),
                        }
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let name = format!("history_{table}_{}", event.to_lowercase());
                let sql = match self.backend {
                    DatabaseBackend::PostgreSql => format!(
                        "CREATE OR REPLACE FUNCTION {name}() RETURNS trigger LANGUAGE plpgsql AS $history$ BEGIN PERFORM id FROM request_history_state WHERE id = 1 FOR SHARE; {identity_guard} {enqueue} RETURN NULL; END $history$;
                         DROP TRIGGER IF EXISTS {name} ON {table};
                         CREATE TRIGGER {name} AFTER {event} ON {table} FOR EACH ROW EXECUTE FUNCTION {name}();"
                    ),
                    DatabaseBackend::Sqlite => format!(
                        "CREATE TRIGGER IF NOT EXISTS {name} AFTER {event} ON {table} BEGIN {identity_guard} {enqueue} END;"
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
            sqlx::query("DELETE FROM request_history_export WHERE revision = $1 AND source = $2 AND request_id = $3 AND snapshot = $4")
                .bind(snapshot.revision).bind(&snapshot.identity.source).bind(snapshot.identity.request_id.to_string())
                .bind(serde_json::to_string(snapshot)?)
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
        let mut transaction = self.begin_write_transaction().await?;
        if clickhouse {
            let lock = if matches!(self.backend, DatabaseBackend::PostgreSql) {
                " FOR UPDATE"
            } else {
                ""
            };
            let revision: Option<i64> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT verified_revision FROM request_history_state WHERE id = 1{lock}"
            )))
            .fetch_one(&mut *transaction)
            .await?;
            let revision_sql = match self.backend {
                DatabaseBackend::PostgreSql => {
                    "SELECT CASE WHEN is_called THEN last_value ELSE 0 END FROM request_history_export_revision_seq"
                }
                DatabaseBackend::Sqlite => {
                    "SELECT COALESCE((SELECT seq FROM sqlite_sequence WHERE name = 'request_history_export'), 0)"
                }
            };
            let current: i64 = sqlx::query_scalar(revision_sql)
                .fetch_one(&mut *transaction)
                .await?;
            let pending: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM request_history_export")
                .fetch_one(&mut *transaction)
                .await?;
            let backfill: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM request_history_backfill WHERE done = 0")
                    .fetch_one(&mut *transaction)
                    .await?;
            if revision != Some(current) || pending != 0 || backfill != 0 {
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
        let statistics = if let Some(payload) = &payload {
            self.history_statistics(&identity, payload).await?
        } else {
            None
        };
        let read_model = if let Some(payload) = &payload {
            let mut archive = self
                .source_request_archive_refs(identity.key_id, identity.request_id)
                .await?;
            archive.view = serde_json::from_value(payload.clone())?;
            let fact = self.history_analysis_fact(&identity).await?;
            serde_json::json!({"archive": archive, "fact": fact})
        } else {
            Value::Null
        };
        Ok(HistorySnapshot {
            identity,
            revision,
            filter_cost_micros,
            payload,
            statistics,
            read_model,
        })
    }

    async fn history_analysis_fact(
        &self,
        identity: &HistoryIdentity,
    ) -> Result<Value, HistoryError> {
        let sql = match identity.source.as_str() {
            "native" => {
                "SELECT status_class, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, generation_units, duration_ms, cost_micros, currency FROM request_stats_facts WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3 AND created_at = $4"
            }
            "generation" => {
                "SELECT status_class, CAST(0 AS BIGINT) AS input_tokens, CAST(0 AS BIGINT) AS output_tokens, CAST(0 AS BIGINT) AS cached_input_tokens, CAST(0 AS BIGINT) AS cache_write_tokens, billed_units AS generation_units, duration_ms, cost_micros, currency FROM generation_stats_facts WHERE job_id = $1 AND tenant_id = $2 AND key_id = $3 AND created_at = $4"
            }
            _ => return Ok(Value::Null),
        };
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string())
            .bind(identity.key_id.to_string())
            .bind(identity.created_at)
            .fetch_optional(&self.pool)
            .await?;
        let Some(row) = row else {
            return Ok(Value::Null);
        };
        let mut fact = serde_json::Map::new();
        for name in [
            "input_tokens",
            "output_tokens",
            "cached_input_tokens",
            "cache_write_tokens",
            "generation_units",
            "duration_ms",
            "cost_micros",
        ] {
            fact.insert(
                name.into(),
                serde_json::to_value(row.try_get::<Option<i64>, _>(name)?)?,
            );
        }
        for name in ["status_class", "currency"] {
            fact.insert(
                name.into(),
                serde_json::to_value(row.try_get::<Option<String>, _>(name)?)?,
            );
        }
        Ok(Value::Object(fact))
    }

    async fn history_statistics(
        &self,
        identity: &HistoryIdentity,
        payload: &Value,
    ) -> Result<Option<HistoryStatistics>, HistoryError> {
        if identity.source == "native" && !payload["completed_at"].is_null() {
            let fact = sqlx::query("SELECT f.status_class, CASE WHEN f.protocol = 'audio-transcription' THEN 0 ELSE f.input_tokens END AS input_tokens, CASE WHEN f.protocol = 'audio-transcription' THEN 0 ELSE f.output_tokens END AS output_tokens, f.cache_known_read_tokens, f.cache_known_input_tokens, f.cost_micros, f.currency FROM request_stats_facts f WHERE f.request_id = $1 AND f.tenant_id = $2 AND f.key_id = $3 AND f.created_at = $4 AND NOT EXISTS (SELECT 1 FROM pending_proxy_settlements pending WHERE pending.request_id = f.request_id AND pending.state = 'pending') AND NOT EXISTS (SELECT 1 FROM terminal_projection_outbox outbox WHERE outbox.request_id = f.request_id AND outbox.projected_at IS NULL)")
                .bind(identity.request_id.to_string()).bind(identity.tenant_id.to_string()).bind(identity.key_id.to_string()).bind(identity.created_at)
                .fetch_optional(&self.pool).await?;
            if let Some(fact) = fact {
                let status: String = fact.try_get("status_class")?;
                return Ok(Some(HistoryStatistics {
                    status: if status == "success" {
                        "success".into()
                    } else {
                        "error".into()
                    },
                    input_tokens: fact.try_get("input_tokens")?,
                    output_tokens: fact.try_get("output_tokens")?,
                    cache_known_read_tokens: fact.try_get("cache_known_read_tokens")?,
                    cache_known_input_tokens: fact.try_get("cache_known_input_tokens")?,
                    cost_micros: fact.try_get("cost_micros")?,
                    currency: fact.try_get("currency")?,
                }));
            }
        }
        let terminal =
            payload["operational_terminal"].is_object() || !payload["completed_at"].is_null();
        let status = if !terminal {
            "pending"
        } else if payload["status_code"]
            .as_i64()
            .is_some_and(|status| (200..400).contains(&status))
            && payload["error_code"].as_str().is_none_or(str::is_empty)
        {
            "success"
        } else {
            "error"
        };
        let sql = match identity.source.as_str() {
            "native" => {
                "SELECT CASE WHEN completed_at IS NOT NULL AND protocol != 'audio-transcription' THEN input_tokens ELSE 0 END AS input_tokens, CASE WHEN completed_at IS NOT NULL AND protocol != 'audio-transcription' THEN output_tokens ELSE 0 END AS output_tokens, CASE WHEN completed_at IS NOT NULL THEN cache_known_read_tokens ELSE NULL END AS cache_known_read_tokens, CASE WHEN completed_at IS NOT NULL THEN cache_known_input_tokens ELSE NULL END AS cache_known_input_tokens, CASE WHEN completed_at IS NOT NULL THEN cost_micros ELSE NULL END AS cost_micros, currency FROM request_records WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND created_at = $4"
            }
            "generation" => {
                "SELECT CAST(0 AS BIGINT) AS input_tokens, CAST(0 AS BIGINT) AS output_tokens, CAST(NULL AS BIGINT) AS cache_known_read_tokens, CAST(NULL AS BIGINT) AS cache_known_input_tokens, cost_micros, currency FROM generation_stats_facts WHERE job_id = $1 AND tenant_id = $2 AND key_id = $3 AND created_at = $4"
            }
            _ => return Ok(None),
        };
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string())
            .bind(identity.key_id.to_string())
            .bind(identity.created_at)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(row) = row {
            Ok(Some(HistoryStatistics {
                status: status.into(),
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                cache_known_read_tokens: row.try_get("cache_known_read_tokens")?,
                cache_known_input_tokens: row.try_get("cache_known_input_tokens")?,
                cost_micros: row.try_get("cost_micros")?,
                currency: row.try_get("currency")?,
            }))
        } else {
            Ok(Some(HistoryStatistics {
                status: "pending".into(),
                input_tokens: 0,
                output_tokens: 0,
                cache_known_read_tokens: None,
                cache_known_input_tokens: None,
                cost_micros: None,
                currency: payload["billing"]["currency"].as_str().map(str::to_owned),
            }))
        }
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

#[cfg(test)]
#[path = "../request_history/persistence_tests.rs"]
mod tests;
