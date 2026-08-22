use std::collections::BTreeMap;

use sqlx::Row;
use uuid::Uuid;

use super::*;
use crate::model::{
    ConversationRequestView, LogicalSessionDetail, LogicalSessionSummary, RequestView,
    UsageAnalysisCost,
};

#[derive(Clone, Debug, Default)]
pub struct LogicalSessionListFilter {
    pub limit: i64,
    pub cursor: Option<(i64, String)>,
    pub key_id: Option<Uuid>,
    pub state: String,
    pub model: Option<String>,
    pub query: Option<String>,
}

#[derive(Default)]
struct SessionAccumulator {
    session_id: String,
    key_id: String,
    key_alias: String,
    model: String,
    protocol: String,
    last_status: String,
    last_activity_at: i64,
    active_requests: i64,
    requests: i64,
    errors: i64,
    input_tokens: i64,
    output_tokens: i64,
    duration_count: i64,
    duration_sum_ms: i64,
    costs: BTreeMap<String, i64>,
    archived_only_requests: i64,
    archived_only_errors: i64,
    archived_only_input_tokens: i64,
    archived_only_output_tokens: i64,
    archived_only_duration_count: i64,
    archived_only_duration_sum_ms: i64,
}

impl Database {
    pub async fn operator_recent_sessions(
        &self,
        tenant_external_id: &str,
        filter: LogicalSessionListFilter,
    ) -> Result<Vec<LogicalSessionSummary>, AppError> {
        let tenant_id = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(tenant_external_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| row.try_get::<String, _>("id"))
            .transpose()?
            .unwrap_or_else(|| Uuid::nil().to_string());
        self.recent_sessions(&tenant_id, filter).await
    }

    pub async fn self_recent_sessions(
        &self,
        tenant_id: Uuid,
        filter: LogicalSessionListFilter,
    ) -> Result<Vec<LogicalSessionSummary>, AppError> {
        self.recent_sessions(&tenant_id.to_string(), filter).await
    }

    pub async fn logical_session_detail(
        &self,
        tenant_id: Uuid,
        key_id: Uuid,
        session_id: &str,
        filter: ConversationDetailFilter,
    ) -> Result<LogicalSessionDetail, AppError> {
        let owned = sqlx::query("SELECT id FROM key_records WHERE id = $1 AND tenant_id = $2")
            .bind(key_id.to_string())
            .bind(tenant_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .is_some();
        if !owned {
            return Err(AppError::NotFound);
        }
        let unlinked_id = format!("unlinked:{key_id}");
        if session_id == unlinked_id {
            return self
                .unlinked_session_detail(key_id, &unlinked_id, filter)
                .await;
        }
        if session_id.starts_with("unlinked:") {
            return Err(AppError::NotFound);
        }
        let cluster_id = Uuid::parse_str(session_id).map_err(|_| AppError::NotFound)?;
        let detail = self
            .conversation_cluster_detail(key_id, cluster_id, filter)
            .await?;
        Ok(LogicalSessionDetail {
            session_id: session_id.to_owned(),
            cluster_id: Some(cluster_id),
            unlinked: false,
            requests: detail.requests,
            edges: detail.edges,
            has_more: detail.has_more,
            next_cursor: detail.next_cursor,
            edges_truncated: detail.edges_truncated,
        })
    }

    pub async fn operator_logical_session_detail(
        &self,
        tenant_external_id: &str,
        key_id: Uuid,
        session_id: &str,
        filter: ConversationDetailFilter,
    ) -> Result<LogicalSessionDetail, AppError> {
        let tenant_id = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(tenant_external_id)
            .fetch_optional(&self.pool)
            .await?
            .map(|row| row.try_get::<String, _>("id"))
            .transpose()?
            .ok_or(AppError::NotFound)
            .and_then(parse_uuid)?;
        self.logical_session_detail(tenant_id, key_id, session_id, filter)
            .await
    }

    async fn recent_sessions(
        &self,
        tenant_id: &str,
        filter: LogicalSessionListFilter,
    ) -> Result<Vec<LogicalSessionSummary>, AppError> {
        let limit = filter.limit.clamp(1, 100) + 1;
        let key_id = filter.key_id.map(|id| id.to_string()).unwrap_or_default();
        let (before_last_activity_at, before_session_id) =
            filter.cursor.unwrap_or((-1, String::new()));
        let model = filter.model.unwrap_or_default();
        let query = search_prefix(filter.query.as_deref());
        let rows = sqlx::query(
            r#"WITH completed AS (
                   SELECT totals.tenant_id, totals.key_id, totals.session_id,
                          MAX(totals.last_activity_at) AS last_activity_at,
                          CAST(SUM(totals.requests) AS BIGINT) AS requests,
                          CAST(SUM(totals.errors) AS BIGINT) AS errors,
                          CAST(SUM(totals.input_tokens) AS BIGINT) AS input_tokens,
                          CAST(SUM(totals.output_tokens) AS BIGINT) AS output_tokens,
                          CAST(SUM(totals.duration_count) AS BIGINT) AS duration_count,
                          CAST(SUM(totals.duration_sum_ms) AS BIGINT) AS duration_sum_ms
                     FROM session_usage_totals totals
                    WHERE totals.tenant_id = $1
                      AND ($2 = '' OR totals.key_id = $2)
                    GROUP BY totals.tenant_id, totals.key_id, totals.session_id
               ), active AS (
                   SELECT request.tenant_id, request.key_id,
                          COALESCE(request.conversation_cluster_id,
                              'unlinked:' || request.key_id) AS session_id,
                          MAX(request.created_at) AS last_activity_at,
                          COUNT(*) AS active_requests
                     FROM request_records request
                    WHERE request.tenant_id = $1
                      AND ($2 = '' OR request.key_id = $2)
                      AND request.status_code IS NULL
                    GROUP BY request.tenant_id, request.key_id,
                             COALESCE(request.conversation_cluster_id,
                                 'unlinked:' || request.key_id)
               ), projected AS (
                   SELECT key_record.tenant_id, projection.key_id,
                          projection.cluster_id AS session_id,
                          projection.updated_at AS last_activity_at,
                          projection.request_count
                     FROM conversation_key_clusters projection
                     JOIN key_records key_record ON key_record.id = projection.key_id
                    WHERE key_record.tenant_id = $1
                      AND ($2 = '' OR projection.key_id = $2)
               ), archived AS (
                   SELECT tenant_id, key_id, session_id, last_activity_at,
                          requests, errors, input_tokens, output_tokens,
                          duration_count, duration_sum_ms
                     FROM session_archive_totals
                    WHERE tenant_id = $1 AND ($2 = '' OR key_id = $2)
               ), session_activity AS (
                   SELECT tenant_id, key_id, session_id, last_activity_at FROM completed
                   UNION ALL
                   SELECT tenant_id, key_id, session_id, last_activity_at FROM active
                   UNION ALL
                   SELECT tenant_id, key_id, session_id, last_activity_at FROM projected
                   UNION ALL
                   SELECT tenant_id, key_id, session_id, last_activity_at FROM archived
               ), ranked AS (
                   SELECT tenant_id, key_id, session_id,
                          MAX(last_activity_at) AS last_activity_at
                     FROM session_activity
                    GROUP BY tenant_id, key_id, session_id
               ), filterable AS (
                   SELECT ranked.*
                     FROM ranked
                     JOIN key_records filter_key
                       ON filter_key.id = ranked.key_id
                      AND filter_key.tenant_id = ranked.tenant_id
                     LEFT JOIN completed
                       ON completed.tenant_id = ranked.tenant_id
                      AND completed.key_id = ranked.key_id
                      AND completed.session_id = ranked.session_id
                     LEFT JOIN active
                       ON active.tenant_id = ranked.tenant_id
                      AND active.key_id = ranked.key_id
                      AND active.session_id = ranked.session_id
                     LEFT JOIN archived
                       ON archived.tenant_id = ranked.tenant_id
                      AND archived.key_id = ranked.key_id
                      AND archived.session_id = ranked.session_id
                    WHERE ($6 = 'all' OR ($6 = 'active' AND active.active_requests > 0)
                           OR ($6 = 'has_errors' AND
                               COALESCE(completed.errors, 0) + COALESCE(archived.errors, 0) > 0))
                      AND ($8 = '' OR LOWER(ranked.session_id) LIKE $8 ESCAPE '\'
                           OR LOWER(filter_key.alias) LIKE $8 ESCAPE '\')
                      AND ($7 = '' OR EXISTS (
                              SELECT 1 FROM session_usage_hourly model_usage
                               WHERE model_usage.tenant_id = ranked.tenant_id
                                 AND model_usage.key_id = ranked.key_id
                                 AND model_usage.session_id = ranked.session_id
                                 AND model_usage.model = $7)
                           OR EXISTS (
                              SELECT 1 FROM request_records model_active
                               WHERE model_active.tenant_id = ranked.tenant_id
                                 AND model_active.key_id = ranked.key_id
                                 AND COALESCE(model_active.conversation_cluster_id,
                                     'unlinked:' || model_active.key_id) = ranked.session_id
                                 AND model_active.status_code IS NULL
                                 AND model_active.model = $7)
                           OR EXISTS (
                              SELECT 1 FROM session_archive_unlinked_requests model_archive
                               WHERE model_archive.tenant_id = ranked.tenant_id
                                 AND model_archive.key_id = ranked.key_id
                                 AND COALESCE(model_archive.conversation_cluster_id,
                                     'unlinked:' || model_archive.key_id) = ranked.session_id
                                 AND model_archive.model = $7))
               ), recent AS (
                   SELECT * FROM filterable
                    WHERE $4 < 0 OR last_activity_at < $4
                       OR (last_activity_at = $4 AND session_id < $5)
                    ORDER BY last_activity_at DESC, session_id DESC
                    LIMIT $3
               ), recent_activity AS (
                   SELECT recent.key_id, recent.session_id, request.model,
                          request.protocol, request.status_code, request.created_at,
                          request.id, 1 AS live
                     FROM recent
                     JOIN request_records request
                       ON request.key_id = recent.key_id
                      AND request.conversation_cluster_id = recent.session_id
                   UNION ALL
                   SELECT recent.key_id, recent.session_id, request.model,
                          request.protocol, request.status_code, request.created_at,
                          request.id, 1
                     FROM recent
                     JOIN request_records request
                       ON request.key_id = recent.key_id
                      AND request.conversation_cluster_id IS NULL
                      AND recent.session_id = 'unlinked:' || recent.key_id
                   UNION ALL
                   SELECT recent.key_id, recent.session_id, archive.model,
                          archive.protocol, archive.status_code,
                          archive.source_started_at, archive.archive_request_id, 0
                     FROM recent
                     JOIN session_archive_unlinked_requests archive
                       ON archive.key_id = recent.key_id
                      AND archive.conversation_cluster_id = recent.session_id
                   UNION ALL
                   SELECT recent.key_id, recent.session_id, archive.model,
                          archive.protocol, archive.status_code,
                          archive.source_started_at, archive.archive_request_id, 0
                     FROM recent
                     JOIN session_archive_unlinked_requests archive
                       ON archive.key_id = recent.key_id
                      AND archive.conversation_cluster_id IS NULL
                      AND recent.session_id = 'unlinked:' || recent.key_id
               ), latest_activity AS (
                   SELECT recent_activity.*,
                          ROW_NUMBER() OVER (
                              PARTITION BY key_id, session_id
                              ORDER BY created_at DESC, id DESC
                          ) AS activity_rank
                     FROM recent_activity
               )
               SELECT recent.*, key_record.alias AS key_alias,
                      COALESCE(totals.currency, '') AS currency,
                      COALESCE(totals.cost_micros, 0) AS cost_micros,
                      COALESCE(completed.requests, 0) AS requests,
                      COALESCE(completed.errors, 0) AS errors,
                      COALESCE(completed.input_tokens, 0) AS input_tokens,
                      COALESCE(completed.output_tokens, 0) AS output_tokens,
                      COALESCE(completed.duration_count, 0) AS duration_count,
                      COALESCE(completed.duration_sum_ms, 0) AS duration_sum_ms,
                      COALESCE(archived.requests, 0) AS archived_only_requests,
                      COALESCE(archived.errors, 0) AS archived_only_errors,
                      COALESCE(archived.input_tokens, 0) AS archived_only_input_tokens,
                      COALESCE(archived.output_tokens, 0) AS archived_only_output_tokens,
                      COALESCE(archived.duration_count, 0) AS archived_only_duration_count,
                      COALESCE(archived.duration_sum_ms, 0) AS archived_only_duration_sum_ms,
                      COALESCE(active.active_requests, 0) AS active_requests,
                      COALESCE(latest_activity.model, '') AS model,
                      COALESCE(latest_activity.protocol, '') AS protocol,
                      CASE WHEN latest_activity.status_code IS NULL AND
                                     latest_activity.live = 1 THEN 'active'
                           WHEN latest_activity.status_code IS NULL THEN 'unknown'
                           WHEN latest_activity.status_code BETWEEN 200 AND 399 THEN 'success'
                           ELSE 'error' END AS last_status
                 FROM recent
                 JOIN key_records key_record
                   ON key_record.id = recent.key_id
                  AND key_record.tenant_id = recent.tenant_id
                 LEFT JOIN completed
                   ON completed.tenant_id = recent.tenant_id
                  AND completed.key_id = recent.key_id
                  AND completed.session_id = recent.session_id
                 LEFT JOIN active
                   ON active.tenant_id = recent.tenant_id
                  AND active.key_id = recent.key_id
                  AND active.session_id = recent.session_id
                 LEFT JOIN projected
                   ON projected.tenant_id = recent.tenant_id
                  AND projected.key_id = recent.key_id
                  AND projected.session_id = recent.session_id
                 LEFT JOIN archived
                   ON archived.tenant_id = recent.tenant_id
                  AND archived.key_id = recent.key_id
                  AND archived.session_id = recent.session_id
                 LEFT JOIN session_usage_totals totals
                   ON totals.tenant_id = recent.tenant_id
                  AND totals.key_id = recent.key_id
                  AND totals.session_id = recent.session_id
                 LEFT JOIN latest_activity
                   ON latest_activity.key_id = recent.key_id
                  AND latest_activity.session_id = recent.session_id
                  AND latest_activity.activity_rank = 1
                ORDER BY recent.last_activity_at DESC, recent.session_id DESC,
                         recent.key_id DESC, totals.currency ASC"#,
        )
        .bind(tenant_id)
        .bind(&key_id)
        .bind(limit)
        .bind(before_last_activity_at)
        .bind(&before_session_id)
        .bind(&filter.state)
        .bind(&model)
        .bind(&query)
        .fetch_all(&self.pool)
        .await?;
        let mut sessions = BTreeMap::<(String, String), SessionAccumulator>::new();
        for row in rows {
            let session_id: String = row.try_get("session_id")?;
            let row_key_id: String = row.try_get("key_id")?;
            let accumulator = sessions
                .entry((session_id.clone(), row_key_id.clone()))
                .or_default();
            if accumulator.session_id.is_empty() {
                accumulator.session_id = session_id;
                accumulator.key_id = row_key_id;
                accumulator.key_alias = row.try_get("key_alias")?;
                accumulator.model = row.try_get("model")?;
                accumulator.protocol = row.try_get("protocol")?;
                accumulator.last_status = row.try_get("last_status")?;
                accumulator.last_activity_at = row.try_get("last_activity_at")?;
                accumulator.requests = row.try_get("requests")?;
                accumulator.errors = row.try_get("errors")?;
                accumulator.input_tokens = row.try_get("input_tokens")?;
                accumulator.output_tokens = row.try_get("output_tokens")?;
                accumulator.duration_count = row.try_get("duration_count")?;
                accumulator.duration_sum_ms = row.try_get("duration_sum_ms")?;
                accumulator.active_requests = row.try_get("active_requests")?;
                accumulator.archived_only_requests = row.try_get("archived_only_requests")?;
                accumulator.archived_only_errors = row.try_get("archived_only_errors")?;
                accumulator.archived_only_input_tokens =
                    row.try_get("archived_only_input_tokens")?;
                accumulator.archived_only_output_tokens =
                    row.try_get("archived_only_output_tokens")?;
                accumulator.archived_only_duration_count =
                    row.try_get("archived_only_duration_count")?;
                accumulator.archived_only_duration_sum_ms =
                    row.try_get("archived_only_duration_sum_ms")?;
            }
            let currency: String = row.try_get("currency")?;
            if !currency.is_empty() {
                accumulator
                    .costs
                    .insert(currency, row.try_get("cost_micros")?);
            }
        }
        let mut result = sessions
            .into_values()
            .map(SessionAccumulator::finish)
            .collect::<Result<Vec<_>, AppError>>()?;
        result.sort_by(|left, right| {
            right
                .last_activity_at
                .cmp(&left.last_activity_at)
                .then_with(|| right.session_id.cmp(&left.session_id))
                .then_with(|| right.key_id.cmp(&left.key_id))
        });
        result.truncate(limit as usize);
        Ok(result)
    }

    async fn unlinked_session_detail(
        &self,
        key_id: Uuid,
        session_id: &str,
        filter: ConversationDetailFilter,
    ) -> Result<LogicalSessionDetail, AppError> {
        let key_id = key_id.to_string();
        let limit = filter.limit.clamp(1, 200);
        let rows = if let (Some(before_created_at), Some(before_request_id)) =
            (filter.before_created_at, filter.before_request_id)
        {
            sqlx::query(
                r#"SELECT id, created_at, protocol, model, status_code, duration_ms,
                          input_tokens, output_tokens, cost_micros, currency, error_code,
                          source_kind, provenance_kind, archive_source, external_request_id
                     FROM (
                         SELECT id, created_at, protocol, model, status_code, duration_ms,
                                input_tokens, output_tokens, cost_micros, currency, error_code,
                                'live' AS source_kind, 'native' AS provenance_kind,
                                NULL AS archive_source, NULL AS external_request_id
                           FROM request_records
                          WHERE key_id = $1 AND conversation_cluster_id IS NULL
                         UNION ALL
                         SELECT archive_request_id, source_started_at, protocol, model,
                                status_code, duration_ms, input_tokens, output_tokens,
                                CAST(0 AS BIGINT), NULL AS currency, error_code, 'session_archive',
                                'archive_unlinked', source, external_request_id
                           FROM session_archive_unlinked_requests
                          WHERE key_id = $1 AND conversation_cluster_id IS NULL
                     ) activity
                    WHERE created_at < $2 OR (created_at = $2 AND id < $3)
                    ORDER BY created_at DESC, id DESC LIMIT $4"#,
            )
            .bind(&key_id)
            .bind(before_created_at)
            .bind(before_request_id.to_string())
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                r#"SELECT id, created_at, protocol, model, status_code, duration_ms,
                          input_tokens, output_tokens, cost_micros, currency, error_code,
                          source_kind, provenance_kind, archive_source, external_request_id
                     FROM (
                         SELECT id, created_at, protocol, model, status_code, duration_ms,
                                input_tokens, output_tokens, cost_micros, currency, error_code,
                                'live' AS source_kind, 'native' AS provenance_kind,
                                NULL AS archive_source, NULL AS external_request_id
                           FROM request_records
                          WHERE key_id = $1 AND conversation_cluster_id IS NULL
                         UNION ALL
                         SELECT archive_request_id, source_started_at, protocol, model,
                                status_code, duration_ms, input_tokens, output_tokens,
                                CAST(0 AS BIGINT), NULL AS currency, error_code, 'session_archive',
                                'archive_unlinked', source, external_request_id
                           FROM session_archive_unlinked_requests
                          WHERE key_id = $1 AND conversation_cluster_id IS NULL
                     ) activity
                    ORDER BY created_at DESC, id DESC LIMIT $2"#,
            )
            .bind(&key_id)
            .bind(limit + 1)
            .fetch_all(&self.pool)
            .await?
        };
        let mut requests = rows
            .into_iter()
            .map(|row| {
                Ok(ConversationRequestView {
                    request: RequestView {
                        request_id: parse_uuid(row.try_get("id")?)?,
                        created_at: row.try_get("created_at")?,
                        protocol: row.try_get("protocol")?,
                        model: row.try_get("model")?,
                        status_code: row.try_get("status_code")?,
                        duration_ms: row.try_get("duration_ms")?,
                        input_tokens: row.try_get("input_tokens")?,
                        output_tokens: row.try_get("output_tokens")?,
                        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                        error_code: row.try_get("error_code")?,
                    },
                    source: row.try_get("source_kind")?,
                    provenance: row.try_get("provenance_kind")?,
                    unlinked: true,
                    currency: row.try_get("currency")?,
                    archive_source: row.try_get("archive_source")?,
                    external_request_id: row.try_get("external_request_id")?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let has_more = requests.len() > limit as usize;
        if has_more {
            requests.truncate(limit as usize);
        }
        let next_cursor = has_more.then(|| {
            let oldest = requests
                .last()
                .expect("a page with another row returns at least one request");
            ConversationCursor {
                before_created_at: oldest.request.created_at,
                before_request_id: oldest.request.request_id,
            }
        });
        requests.reverse();
        Ok(LogicalSessionDetail {
            session_id: session_id.to_owned(),
            cluster_id: None,
            unlinked: true,
            requests,
            edges: Vec::new(),
            has_more,
            next_cursor,
            edges_truncated: false,
        })
    }
}

impl SessionAccumulator {
    fn finish(self) -> Result<LogicalSessionSummary, AppError> {
        let unlinked = self.session_id.starts_with("unlinked:");
        let cluster_id = if unlinked {
            None
        } else {
            Some(parse_uuid(self.session_id.clone())?)
        };
        Ok(LogicalSessionSummary {
            session_id: self.session_id,
            cluster_id,
            unlinked,
            key_id: parse_uuid(self.key_id)?,
            key_alias: self.key_alias,
            model: self.model,
            protocol: self.protocol,
            last_status: self.last_status,
            last_activity_at: self.last_activity_at,
            active_requests: self.active_requests,
            requests: self.requests,
            errors: self.errors,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            avg_duration_ms: (self.duration_count > 0)
                .then(|| self.duration_sum_ms as f64 / self.duration_count as f64),
            costs: self
                .costs
                .into_iter()
                .map(|(currency, micros)| UsageAnalysisCost {
                    currency,
                    cost: micros_to_decimal_string(micros),
                })
                .collect(),
            archived_only_requests: self.archived_only_requests,
            archived_only_errors: self.archived_only_errors,
            archived_only_input_tokens: self.archived_only_input_tokens,
            archived_only_output_tokens: self.archived_only_output_tokens,
            archived_only_avg_duration_ms: (self.archived_only_duration_count > 0).then(|| {
                self.archived_only_duration_sum_ms as f64 / self.archived_only_duration_count as f64
            }),
        })
    }
}
