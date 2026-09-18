use std::collections::HashSet;

use super::*;

pub const FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE: i64 = 1_000;
pub const FAILED_REQUEST_COST_CORRECTION_VERSION: &str = "failed-request-projection-cost-v2";

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FailedRequestCostBackfillCursor {
    pub created_at: i64,
    pub request_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedRequestCostBackfillInput {
    pub apply: bool,
    pub batch_size: i64,
    pub from_created_at: i64,
    pub to_created_at: i64,
    pub after: Option<FailedRequestCostBackfillCursor>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FailedRequestCostBackfillReport {
    pub correction_version: &'static str,
    pub applied: bool,
    pub candidate_rows: u64,
    pub changed_rows: u64,
    pub candidate_cost_micros: i64,
    pub changed_cost_micros: i64,
    pub candidate_corrected_cost_micros: i64,
    pub candidate_cost_delta_micros: i64,
    pub changed_cost_delta_micros: i64,
    pub candidate_cost_reduction_micros: i64,
    pub affected_request_daily_dimensions: u64,
    pub affected_usage_daily_dimensions: u64,
    pub affected_usage_analysis_hourly_dimensions: u64,
    pub affected_usage_analysis_daily_dimensions: u64,
    pub affected_session_projections: u64,
    pub request_daily_dimensions_rebuilt: u64,
    pub usage_daily_dimensions_rebuilt: u64,
    pub usage_analysis_hourly_dimensions_rebuilt: u64,
    pub usage_analysis_daily_dimensions_rebuilt: u64,
    pub session_projections_rebuilt: u64,
    pub has_more: bool,
    pub next_cursor: Option<FailedRequestCostBackfillCursor>,
    pub candidates: Vec<FailedRequestCostCorrectionPreview>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FailedRequestCostCorrectionPreview {
    pub request_id: String,
    pub created_at: i64,
    pub status_code: i64,
    pub error_code: String,
    pub usage_basis: Option<String>,
    pub evidence_kind: String,
    pub original_cost_micros: i64,
    pub corrected_cost_micros: i64,
}

#[derive(Clone, Debug)]
struct Candidate {
    request_id: String,
    tenant_id: String,
    key_id: String,
    created_at: i64,
    model: String,
    protocol: String,
    status_class: String,
    error_code: String,
    upstream_account_id: String,
    model_route_id: String,
    service_tier: String,
    currency: String,
    session_id: String,
    cost_micros: i64,
    request_cost_micros: i64,
    corrected_cost_micros: i64,
    status_code: i64,
    usage_basis: Option<String>,
    evidence_kind: String,
    reservation_id: String,
    reservation_reserved_micros: i64,
    reservation_actual_micros: Option<i64>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct UsageDailyKey {
    key_id: String,
    day_bucket: i64,
    model: String,
    status_class: String,
    error_code: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct RequestDailyKey {
    tenant_id: String,
    key_id: String,
    day_bucket: i64,
    model: String,
    protocol: String,
    status_class: String,
    error_code: String,
    upstream_account_id: String,
    model_route_id: String,
    service_tier: String,
    currency: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct AnalysisKey {
    tenant_id: String,
    key_id: String,
    bucket: i64,
    source_kind: String,
    model: String,
    protocol: String,
    status_class: String,
    error_code: String,
    upstream_account_id: String,
    model_route_id: String,
    service_tier: String,
    currency: String,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct SessionKey {
    tenant_id: String,
    key_id: String,
    session_id: String,
}

impl Database {
    /// Correct one explicitly requested, cursor-bounded batch of historical read models.
    ///
    /// Immutable request and settlement rows are eligibility sources only. The transaction writes
    /// request facts and replaces affected aggregate dimensions from facts; it never subtracts a
    /// historical delta or changes a ledger, reservation, account balance, or request ledger row.
    pub async fn backfill_failed_request_costs(
        &self,
        input: FailedRequestCostBackfillInput,
    ) -> Result<FailedRequestCostBackfillReport, AppError> {
        if !(1..=FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE).contains(&input.batch_size) {
            return Err(AppError::BadRequest(format!(
                "batch size must be between 1 and {FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE}"
            )));
        }
        if input.from_created_at < 0 || input.to_created_at <= input.from_created_at {
            return Err(AppError::BadRequest(
                "backfill requires a non-negative, non-empty created_at interval".into(),
            ));
        }
        if input
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.created_at < input.from_created_at)
        {
            return Err(AppError::BadRequest(
                "backfill cursor precedes the requested interval".into(),
            ));
        }
        if input
            .after
            .as_ref()
            .is_some_and(|cursor| cursor.request_id.is_empty())
        {
            return Err(AppError::BadRequest(
                "backfill cursor request id must not be empty".into(),
            ));
        }

        if !input.apply {
            let candidates = select_candidates_from_pool(
                &self.pool,
                input.from_created_at,
                input.to_created_at,
                input.after.as_ref(),
                input.batch_size.saturating_add(1),
            )
            .await?;
            let (_, report) = prepare_batch(candidates, input.batch_size, false);
            return Ok(report);
        }

        let mut transaction = self.begin_write_transaction().await?;
        lock_request_stats_projection_rebuild_in_transaction(&mut transaction).await?;
        let candidates = select_candidates(
            &mut transaction,
            self.backend,
            input.from_created_at,
            input.to_created_at,
            input.after.as_ref(),
            input.batch_size.saturating_add(1),
        )
        .await?;
        let (candidates, mut report) = prepare_batch(candidates, input.batch_size, true);
        if candidates.is_empty() {
            transaction.rollback().await?;
            return Ok(report);
        }

        let applied_at = unix_millis();
        let mut changed = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            let inserted = sqlx::query(
                r#"INSERT INTO request_cost_projection_corrections (
                       correction_version, request_id, request_created_at, evidence_kind,
                       observed_status_code, observed_error_code, observed_usage_basis,
                       reservation_id, reservation_reserved_micros, reservation_actual_micros,
                       original_request_cost_micros, original_fact_cost_micros,
                       corrected_fact_cost_micros, applied_at)
                   VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14)
                   ON CONFLICT (correction_version, request_id) DO NOTHING"#,
            )
            .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
            .bind(&candidate.request_id)
            .bind(candidate.created_at)
            .bind(&candidate.evidence_kind)
            .bind(candidate.status_code)
            .bind(&candidate.error_code)
            .bind(&candidate.usage_basis)
            .bind(&candidate.reservation_id)
            .bind(candidate.reservation_reserved_micros)
            .bind(candidate.reservation_actual_micros)
            .bind(candidate.request_cost_micros)
            .bind(candidate.cost_micros)
            .bind(candidate.corrected_cost_micros)
            .bind(applied_at)
            .execute(&mut *transaction)
            .await?;
            if inserted.rows_affected() == 0 {
                continue;
            }
            let updated = sqlx::query(
                "UPDATE request_stats_facts SET cost_micros = $1 WHERE request_id = $2 AND cost_micros = $3",
            )
            .bind(candidate.corrected_cost_micros)
            .bind(&candidate.request_id)
            .bind(candidate.cost_micros)
            .execute(&mut *transaction)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "historical request cost changed while correction was being applied".into(),
                ));
            }
            report.changed_rows += 1;
            report.changed_cost_micros = report.changed_cost_micros.saturating_add(
                candidate
                    .cost_micros
                    .saturating_sub(candidate.corrected_cost_micros),
            );
            report.changed_cost_delta_micros = report.changed_cost_delta_micros.saturating_add(
                candidate
                    .corrected_cost_micros
                    .saturating_sub(candidate.cost_micros),
            );
            changed.push(candidate);
        }

        let usage_daily_keys = changed
            .iter()
            .map(UsageDailyKey::from)
            .collect::<HashSet<_>>();
        for key in &usage_daily_keys {
            rebuild_usage_daily(&mut transaction, key).await?;
        }
        report.usage_daily_dimensions_rebuilt = usage_daily_keys.len() as u64;

        let request_daily_keys = changed
            .iter()
            .map(RequestDailyKey::from)
            .collect::<HashSet<_>>();
        for key in &request_daily_keys {
            rebuild_request_daily(&mut transaction, key).await?;
        }
        report.request_daily_dimensions_rebuilt = request_daily_keys.len() as u64;

        let hourly_keys = changed
            .iter()
            .map(|candidate| AnalysisKey::from_candidate(candidate, 3_600_000))
            .collect::<HashSet<_>>();
        for key in &hourly_keys {
            rebuild_analysis(
                &mut transaction,
                key,
                "usage_analysis_hourly",
                "hour_bucket",
                3_600_000,
            )
            .await?;
        }
        report.usage_analysis_hourly_dimensions_rebuilt = hourly_keys.len() as u64;

        let daily_keys = changed
            .iter()
            .map(|candidate| AnalysisKey::from_candidate(candidate, 86_400_000))
            .collect::<HashSet<_>>();
        for key in &daily_keys {
            rebuild_analysis(
                &mut transaction,
                key,
                "usage_analysis_daily",
                "day_bucket",
                86_400_000,
            )
            .await?;
        }
        report.usage_analysis_daily_dimensions_rebuilt = daily_keys.len() as u64;

        let session_keys = changed.iter().map(SessionKey::from).collect::<HashSet<_>>();
        for key in &session_keys {
            rebuild_request_session_projection_in_transaction(
                &mut transaction,
                &key.tenant_id,
                &key.key_id,
                &key.session_id,
            )
            .await?;
        }
        report.session_projections_rebuilt = session_keys.len() as u64;

        transaction.commit().await?;
        Ok(report)
    }
}

fn prepare_batch(
    mut candidates: Vec<Candidate>,
    batch_size: i64,
    applied: bool,
) -> (Vec<Candidate>, FailedRequestCostBackfillReport) {
    let has_more = candidates.len() > batch_size as usize;
    if has_more {
        candidates.pop();
    }
    let next_cursor = candidates
        .last()
        .map(|candidate| FailedRequestCostBackfillCursor {
            created_at: candidate.created_at,
            request_id: candidate.request_id.clone(),
        });
    let candidate_cost_micros = candidates
        .iter()
        .fold(0_i64, |total, row| total.saturating_add(row.cost_micros));
    let candidate_corrected_cost_micros = candidates.iter().fold(0_i64, |total, row| {
        total.saturating_add(row.corrected_cost_micros)
    });
    let usage_daily_keys = candidates
        .iter()
        .map(UsageDailyKey::from)
        .collect::<HashSet<_>>();
    let request_daily_keys = candidates
        .iter()
        .map(RequestDailyKey::from)
        .collect::<HashSet<_>>();
    let hourly_keys = candidates
        .iter()
        .map(|candidate| AnalysisKey::from_candidate(candidate, 3_600_000))
        .collect::<HashSet<_>>();
    let daily_keys = candidates
        .iter()
        .map(|candidate| AnalysisKey::from_candidate(candidate, 86_400_000))
        .collect::<HashSet<_>>();
    let session_keys = candidates
        .iter()
        .map(SessionKey::from)
        .collect::<HashSet<_>>();
    let previews = candidates
        .iter()
        .map(|candidate| FailedRequestCostCorrectionPreview {
            request_id: candidate.request_id.clone(),
            created_at: candidate.created_at,
            status_code: candidate.status_code,
            error_code: candidate.error_code.clone(),
            usage_basis: candidate.usage_basis.clone(),
            evidence_kind: candidate.evidence_kind.clone(),
            original_cost_micros: candidate.cost_micros,
            corrected_cost_micros: candidate.corrected_cost_micros,
        })
        .collect();
    let report = FailedRequestCostBackfillReport {
        correction_version: FAILED_REQUEST_COST_CORRECTION_VERSION,
        applied,
        candidate_rows: candidates.len() as u64,
        changed_rows: 0,
        candidate_cost_micros,
        changed_cost_micros: 0,
        candidate_corrected_cost_micros,
        candidate_cost_delta_micros: candidate_corrected_cost_micros
            .saturating_sub(candidate_cost_micros),
        changed_cost_delta_micros: 0,
        candidate_cost_reduction_micros: candidate_cost_micros
            .saturating_sub(candidate_corrected_cost_micros),
        affected_request_daily_dimensions: request_daily_keys.len() as u64,
        affected_usage_daily_dimensions: usage_daily_keys.len() as u64,
        affected_usage_analysis_hourly_dimensions: hourly_keys.len() as u64,
        affected_usage_analysis_daily_dimensions: daily_keys.len() as u64,
        affected_session_projections: session_keys.len() as u64,
        request_daily_dimensions_rebuilt: 0,
        usage_daily_dimensions_rebuilt: 0,
        usage_analysis_hourly_dimensions_rebuilt: 0,
        usage_analysis_daily_dimensions_rebuilt: 0,
        session_projections_rebuilt: 0,
        has_more,
        next_cursor,
        candidates: previews,
    };
    (candidates, report)
}

fn candidate_statement(lock: &str) -> String {
    format!(
        r#"SELECT f.request_id, f.tenant_id, f.key_id, f.created_at, f.model, f.protocol,
                  f.status_class, f.error_code, f.upstream_account_id, f.model_route_id,
                  f.service_tier, f.currency, f.session_id, f.cost_micros,
                  r.cost_micros AS request_cost_micros, r.status_code, r.usage_basis,
                  r.reservation_id, u.reserved_micros AS reservation_reserved_micros,
                  u.actual_micros AS reservation_actual_micros,
                  CASE WHEN r.usage_basis = 'provider_reported'
                       THEN 'provider_reported'
                       ELSE 'reservation_ceiling_without_usage' END AS evidence_kind,
                  CASE WHEN r.usage_basis = 'provider_reported'
                       THEN r.cost_micros ELSE 0 END AS corrected_cost_micros
             FROM request_stats_facts f
             JOIN request_records r ON r.id = f.request_id AND r.created_at = f.created_at
             JOIN usage_reservations u ON u.id = r.reservation_id
             LEFT JOIN request_cost_projection_corrections correction
               ON correction.correction_version = $5
              AND correction.request_id = f.request_id
            WHERE f.created_at >= $1 AND f.created_at < $2
              AND r.completed_at IS NOT NULL AND r.status_code IS NOT NULL
              AND ((r.status_code < 200 OR r.status_code >= 400)
                   OR COALESCE(r.error_code, '') <> '')
              AND correction.request_id IS NULL
              AND (
                    (r.usage_basis = 'provider_reported'
                     AND f.cost_micros <> r.cost_micros
                     AND u.status = 'settled'
                     AND u.actual_micros = r.cost_micros)
                    OR
                    (COALESCE(r.usage_basis, '') IN ('', 'not_observed', 'contract_ceiling')
                     AND f.cost_micros <> 0
                     AND f.cost_micros = r.cost_micros
                     AND u.status = 'settled'
                     AND u.actual_micros = r.cost_micros
                     AND u.reserved_micros = r.cost_micros
                     AND u.reserved_tokens = r.input_tokens + r.output_tokens)
                  )
              AND (f.created_at > $3 OR (f.created_at = $3 AND f.request_id > $4))
            ORDER BY f.created_at ASC, f.request_id ASC
            LIMIT $6{lock}"#
    )
}

async fn select_candidates_from_pool(
    pool: &AnyPool,
    from_created_at: i64,
    to_created_at: i64,
    after: Option<&FailedRequestCostBackfillCursor>,
    limit: i64,
) -> Result<Vec<Candidate>, AppError> {
    let rows = sqlx::query(sqlx::AssertSqlSafe(candidate_statement("")))
        .bind(from_created_at)
        .bind(to_created_at)
        .bind(
            after
                .map(|cursor| cursor.created_at)
                .unwrap_or(from_created_at.saturating_sub(1)),
        )
        .bind(
            after
                .map(|cursor| cursor.request_id.as_str())
                .unwrap_or_default(),
        )
        .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    candidates_from_rows(rows)
}

async fn select_candidates(
    tx: &mut Transaction<'_, Any>,
    backend: DatabaseBackend,
    from_created_at: i64,
    to_created_at: i64,
    after: Option<&FailedRequestCostBackfillCursor>,
    limit: i64,
) -> Result<Vec<Candidate>, AppError> {
    let lock = match backend {
        DatabaseBackend::PostgreSql => " FOR UPDATE OF f",
        DatabaseBackend::Sqlite => "",
    };
    let rows = sqlx::query(sqlx::AssertSqlSafe(candidate_statement(lock)))
        .bind(from_created_at)
        .bind(to_created_at)
        .bind(
            after
                .map(|cursor| cursor.created_at)
                .unwrap_or(from_created_at.saturating_sub(1)),
        )
        .bind(
            after
                .map(|cursor| cursor.request_id.as_str())
                .unwrap_or_default(),
        )
        .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
        .bind(limit)
        .fetch_all(&mut **tx)
        .await?;
    candidates_from_rows(rows)
}

fn candidates_from_rows(rows: Vec<sqlx::any::AnyRow>) -> Result<Vec<Candidate>, AppError> {
    rows.into_iter()
        .map(|row| {
            Ok(Candidate {
                request_id: row.try_get("request_id")?,
                tenant_id: row.try_get("tenant_id")?,
                key_id: row.try_get("key_id")?,
                created_at: row.try_get("created_at")?,
                model: row.try_get("model")?,
                protocol: row.try_get("protocol")?,
                status_class: row.try_get("status_class")?,
                error_code: row.try_get("error_code")?,
                upstream_account_id: row.try_get("upstream_account_id")?,
                model_route_id: row.try_get("model_route_id")?,
                service_tier: row.try_get("service_tier")?,
                currency: row.try_get("currency")?,
                session_id: row.try_get("session_id")?,
                cost_micros: row.try_get("cost_micros")?,
                request_cost_micros: row.try_get("request_cost_micros")?,
                corrected_cost_micros: row.try_get("corrected_cost_micros")?,
                status_code: row.try_get("status_code")?,
                usage_basis: row.try_get("usage_basis")?,
                evidence_kind: row.try_get("evidence_kind")?,
                reservation_id: row.try_get("reservation_id")?,
                reservation_reserved_micros: row.try_get("reservation_reserved_micros")?,
                reservation_actual_micros: row.try_get("reservation_actual_micros")?,
            })
        })
        .collect()
}

impl From<&Candidate> for UsageDailyKey {
    fn from(candidate: &Candidate) -> Self {
        Self {
            key_id: candidate.key_id.clone(),
            day_bucket: candidate.created_at / 86_400_000,
            model: candidate.model.clone(),
            status_class: candidate.status_class.clone(),
            error_code: candidate.error_code.clone(),
        }
    }
}

impl From<&Candidate> for RequestDailyKey {
    fn from(candidate: &Candidate) -> Self {
        Self {
            tenant_id: candidate.tenant_id.clone(),
            key_id: candidate.key_id.clone(),
            day_bucket: candidate.created_at / 86_400_000,
            model: candidate.model.clone(),
            protocol: candidate.protocol.clone(),
            status_class: candidate.status_class.clone(),
            error_code: candidate.error_code.clone(),
            upstream_account_id: candidate.upstream_account_id.clone(),
            model_route_id: candidate.model_route_id.clone(),
            service_tier: candidate.service_tier.clone(),
            currency: candidate.currency.clone(),
        }
    }
}

impl AnalysisKey {
    fn from_candidate(candidate: &Candidate, divisor: i64) -> Self {
        Self {
            tenant_id: candidate.tenant_id.clone(),
            key_id: candidate.key_id.clone(),
            bucket: candidate.created_at / divisor,
            source_kind: if candidate.protocol == "audio-transcription" {
                "generation".into()
            } else {
                "request".into()
            },
            model: candidate.model.clone(),
            protocol: canonical_protocol(&candidate.protocol).into(),
            status_class: candidate.status_class.clone(),
            error_code: candidate.error_code.clone(),
            upstream_account_id: candidate.upstream_account_id.clone(),
            model_route_id: candidate.model_route_id.clone(),
            service_tier: candidate.service_tier.clone(),
            currency: candidate.currency.clone(),
        }
    }
}

impl From<&Candidate> for SessionKey {
    fn from(candidate: &Candidate) -> Self {
        Self {
            tenant_id: candidate.tenant_id.clone(),
            key_id: candidate.key_id.clone(),
            session_id: candidate.session_id.clone(),
        }
    }
}

fn canonical_protocol(protocol: &str) -> &str {
    if protocol == "anthropic" || protocol.starts_with("anthropic-") {
        "anthropic"
    } else if protocol == "openai-image" {
        "openai-image"
    } else if protocol == "audio-transcription" {
        "audio-transcription"
    } else {
        "openai"
    }
}

async fn rebuild_usage_daily(
    tx: &mut Transaction<'_, Any>,
    key: &UsageDailyKey,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO usage_daily_aggregates (
               key_id, day_bucket, model, status_class, error_code, requests,
               input_tokens, output_tokens, cost_micros)
           SELECT key_id, created_at / 86400000, model, status_class, error_code,
                  COUNT(*),
                  SUM(CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE input_tokens END),
                  SUM(CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE output_tokens END),
                  SUM(cost_micros)
             FROM request_stats_facts
            WHERE key_id = $1 AND created_at / 86400000 = $2 AND model = $3
              AND status_class = $4 AND error_code = $5
            GROUP BY key_id, created_at / 86400000, model, status_class, error_code
           ON CONFLICT (key_id, day_bucket, model, status_class, error_code) DO UPDATE SET
               requests = excluded.requests,
               input_tokens = excluded.input_tokens,
               output_tokens = excluded.output_tokens,
               cost_micros = excluded.cost_micros"#,
    )
    .bind(&key.key_id)
    .bind(key.day_bucket)
    .bind(&key.model)
    .bind(&key.status_class)
    .bind(&key.error_code)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn rebuild_request_daily(
    tx: &mut Transaction<'_, Any>,
    key: &RequestDailyKey,
) -> Result<(), AppError> {
    sqlx::query(
        r#"INSERT INTO request_daily_aggregates (
               tenant_id, key_id, day_bucket, model, protocol, status_class, error_code,
               upstream_account_id, model_route_id, service_tier, currency, requests,
               input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
               duration_count, duration_sum_ms, cost_micros)
           SELECT tenant_id, key_id, created_at / 86400000, model, protocol, status_class,
                  error_code, upstream_account_id, model_route_id, service_tier, currency,
                  COUNT(*),
                  SUM(CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE input_tokens END),
                  SUM(CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE output_tokens END),
                  SUM(cached_input_tokens), SUM(cache_write_tokens), COUNT(*),
                  SUM(duration_ms), SUM(cost_micros)
             FROM request_stats_facts
            WHERE tenant_id = $1 AND key_id = $2 AND created_at / 86400000 = $3
              AND model = $4 AND protocol = $5 AND status_class = $6 AND error_code = $7
              AND upstream_account_id = $8 AND model_route_id = $9 AND service_tier = $10
              AND currency = $11
            GROUP BY tenant_id, key_id, created_at / 86400000, model, protocol,
                     status_class, error_code, upstream_account_id, model_route_id,
                     service_tier, currency
           ON CONFLICT (tenant_id, key_id, day_bucket, model, protocol, status_class,
                        error_code, upstream_account_id, model_route_id, service_tier, currency)
           DO UPDATE SET requests = excluded.requests,
               input_tokens = excluded.input_tokens,
               output_tokens = excluded.output_tokens,
               cached_input_tokens = excluded.cached_input_tokens,
               cache_write_tokens = excluded.cache_write_tokens,
               duration_count = excluded.duration_count,
               duration_sum_ms = excluded.duration_sum_ms,
               cost_micros = excluded.cost_micros"#,
    )
    .bind(&key.tenant_id)
    .bind(&key.key_id)
    .bind(key.day_bucket)
    .bind(&key.model)
    .bind(&key.protocol)
    .bind(&key.status_class)
    .bind(&key.error_code)
    .bind(&key.upstream_account_id)
    .bind(&key.model_route_id)
    .bind(&key.service_tier)
    .bind(&key.currency)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn rebuild_analysis(
    tx: &mut Transaction<'_, Any>,
    key: &AnalysisKey,
    table: &str,
    bucket_column: &str,
    divisor: i64,
) -> Result<(), AppError> {
    let statement = format!(
        r#"INSERT INTO {table} (
               tenant_id, key_id, {bucket_column}, source_kind, model, protocol, status_class,
               error_code, upstream_account_id, model_route_id, service_tier, currency, requests,
               input_tokens, output_tokens, cached_input_tokens, cache_write_tokens,
               generation_units, duration_count, duration_sum_ms, duration_bucket_0,
               duration_bucket_1, duration_bucket_2, duration_bucket_3, duration_bucket_4,
               duration_bucket_5, duration_bucket_6, duration_bucket_7, duration_bucket_8,
               duration_bucket_9, duration_bucket_10, duration_bucket_11, cost_micros)
           SELECT tenant_id, key_id, created_at / {divisor},
                  CASE WHEN protocol = 'audio-transcription' THEN 'generation' ELSE 'request' END,
                  model,
                  CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%' THEN 'anthropic'
                       WHEN protocol = 'openai-image' THEN 'openai-image'
                       WHEN protocol = 'audio-transcription' THEN 'audio-transcription'
                       ELSE 'openai' END,
                  status_class, error_code, upstream_account_id, model_route_id,
                  service_tier, currency, COUNT(*),
                  SUM(CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                           THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END),
                  SUM(output_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens),
                  SUM(generation_units), COUNT(*), SUM(duration_ms),
                  SUM(CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END),
                  SUM(CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END),
                  SUM(cost_micros)
             FROM request_stats_facts
            WHERE tenant_id = $1 AND key_id = $2 AND created_at / {divisor} = $3
              AND (CASE WHEN protocol = 'audio-transcription' THEN 'generation' ELSE 'request' END) = $4
              AND model = $5
              AND (CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%' THEN 'anthropic'
                        WHEN protocol = 'openai-image' THEN 'openai-image'
                        WHEN protocol = 'audio-transcription' THEN 'audio-transcription'
                        ELSE 'openai' END) = $6
              AND status_class = $7 AND error_code = $8 AND upstream_account_id = $9
              AND model_route_id = $10 AND service_tier = $11 AND currency = $12
            GROUP BY tenant_id, key_id, created_at / {divisor},
                     CASE WHEN protocol = 'audio-transcription' THEN 'generation' ELSE 'request' END,
                     model,
                     CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%' THEN 'anthropic'
                          WHEN protocol = 'openai-image' THEN 'openai-image'
                          WHEN protocol = 'audio-transcription' THEN 'audio-transcription'
                          ELSE 'openai' END,
                     status_class, error_code, upstream_account_id, model_route_id,
                     service_tier, currency
           ON CONFLICT (tenant_id, key_id, {bucket_column}, source_kind, model, protocol,
                        status_class, error_code, upstream_account_id, model_route_id,
                        service_tier, currency)
           DO UPDATE SET requests = excluded.requests,
               input_tokens = excluded.input_tokens,
               output_tokens = excluded.output_tokens,
               cached_input_tokens = excluded.cached_input_tokens,
               cache_write_tokens = excluded.cache_write_tokens,
               generation_units = excluded.generation_units,
               duration_count = excluded.duration_count,
               duration_sum_ms = excluded.duration_sum_ms,
               duration_bucket_0 = excluded.duration_bucket_0,
               duration_bucket_1 = excluded.duration_bucket_1,
               duration_bucket_2 = excluded.duration_bucket_2,
               duration_bucket_3 = excluded.duration_bucket_3,
               duration_bucket_4 = excluded.duration_bucket_4,
               duration_bucket_5 = excluded.duration_bucket_5,
               duration_bucket_6 = excluded.duration_bucket_6,
               duration_bucket_7 = excluded.duration_bucket_7,
               duration_bucket_8 = excluded.duration_bucket_8,
               duration_bucket_9 = excluded.duration_bucket_9,
               duration_bucket_10 = excluded.duration_bucket_10,
               duration_bucket_11 = excluded.duration_bucket_11,
               cost_micros = excluded.cost_micros"#
    );
    sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(&key.tenant_id)
        .bind(&key.key_id)
        .bind(key.bucket)
        .bind(&key.source_kind)
        .bind(&key.model)
        .bind(&key.protocol)
        .bind(&key.status_class)
        .bind(&key.error_code)
        .bind(&key.upstream_account_id)
        .bind(&key.model_route_id)
        .bind(&key.service_tier)
        .bind(&key.currency)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use rust_decimal::Decimal;
    use url::Url;

    use super::*;
    use crate::model::{KeyPolicy, RequestUsageBasis};

    struct Fixture {
        _directory: Option<tempfile::TempDir>,
        database: Database,
        account_id: Uuid,
        key: AuthenticatedKey,
        model: String,
        price: ModelPrice,
    }

    async fn fixture(database_url: &str, directory: Option<tempfile::TempDir>) -> Fixture {
        let database = Database::connect(database_url).await.unwrap();
        database.migrate().await.unwrap();
        let unique = Uuid::now_v7();
        let pepper = b"failed cost backfill test pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: format!("failed-cost-backfill-{unique}"),
                    principal_external_id: "member".into(),
                    alias: format!("failed-cost-backfill-{unique}"),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::from(100),
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let model = format!("failed-cost-backfill-{unique}");
        let price = database
            .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
            .await
            .unwrap();
        Fixture {
            _directory: directory,
            database,
            account_id: issued.account_id,
            key,
            model,
            price,
        }
    }

    async fn seed_historical_case(
        fixture: &Fixture,
        status_code: i64,
        error_code: Option<&str>,
        historical_usage_basis: Option<RequestUsageBasis>,
    ) -> (Uuid, i64) {
        let request_id = Uuid::now_v7();
        let reservation = fixture
            .database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &fixture.key,
                price: &fixture.price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai",
                model: &fixture.model,
                request_object: "gap://failed-cost-backfill/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let response_object = format!("gap://failed-cost-backfill/{request_id}/response");
        let result = fixture
            .database
            .finish_proxy_request(FinishProxyRequest {
                usage_basis: Some(RequestUsageBasis::ProviderReported),
                first_output_ms: None,
                generation_duration_ms: None,
                request_id,
                tenant_id: fixture.key.tenant_id,
                reservation: &reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                requested_service_tier: None,
                status_code,
                duration_ms: 25,
                usage: TokenUsage {
                    input_tokens: 10,
                    output_tokens: 10,
                    ..TokenUsage::default()
                },
                error_code,
                response_object: &response_object,
                routing_session_id: None,
                routing_terminal_observed_at: None,
                conversation: None,
            })
            .await
            .unwrap();
        let cost_micros = match result {
            FinishProxyRequestResult::Finished { cost_micros, .. }
            | FinishProxyRequestResult::AlreadyFinished { cost_micros, .. } => cost_micros,
        };
        assert!(cost_micros > 0);
        sqlx::query("UPDATE request_records SET usage_basis = $1 WHERE id = $2")
            .bind(historical_usage_basis.map(RequestUsageBasis::as_str))
            .bind(request_id.to_string())
            .execute(&fixture.database.pool)
            .await
            .unwrap();
        (request_id, cost_micros)
    }

    async fn immutable_snapshot(
        fixture: &Fixture,
    ) -> (Vec<(String, i64, Option<String>)>, i64, i64, i64, i64) {
        let rows = sqlx::query(
            "SELECT id, cost_micros, usage_basis FROM request_records WHERE key_id = $1 ORDER BY id",
        )
        .bind(fixture.key.key_id.to_string())
        .fetch_all(&fixture.database.pool)
        .await
        .unwrap();
        let requests = rows
            .into_iter()
            .map(|row| {
                (
                    row.try_get("id").unwrap(),
                    row.try_get("cost_micros").unwrap(),
                    row.try_get("usage_basis").unwrap(),
                )
            })
            .collect();
        let account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(fixture.account_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        let ledger_count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM ledger_entries WHERE account_id = $1")
                .bind(fixture.account_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
        let reservation_cost: i64 = sqlx::query_scalar(
            "SELECT CAST(COALESCE(SUM(actual_micros), 0) AS BIGINT) FROM usage_reservations WHERE key_id = $1",
        )
        .bind(fixture.key.key_id.to_string())
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        (
            requests,
            account.try_get("available_micros").unwrap(),
            account.try_get("reserved_micros").unwrap(),
            ledger_count,
            reservation_cost,
        )
    }

    async fn aggregate_costs(fixture: &Fixture) -> Vec<i64> {
        let mut costs = Vec::new();
        for table in [
            "usage_daily_aggregates",
            "request_daily_aggregates",
            "usage_analysis_hourly",
            "usage_analysis_daily",
            "session_usage_totals",
            "session_usage_hourly",
            "session_usage_daily",
        ] {
            let statement = format!(
                "SELECT CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) FROM {table} WHERE key_id = $1"
            );
            costs.push(
                sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
                    .bind(fixture.key.key_id.to_string())
                    .fetch_one(&fixture.database.pool)
                    .await
                    .unwrap(),
            );
        }
        costs
    }

    async fn exercise_backfill(fixture: &Fixture) {
        let mut corrected = Vec::new();
        for (status, error, usage_basis) in [
            (499, None, Some(RequestUsageBasis::NotObserved)),
            (503, None, Some(RequestUsageBasis::ContractCeiling)),
            (200, Some("upstream_incomplete_response"), None),
        ] {
            corrected.push(seed_historical_case(fixture, status, error, usage_basis).await);
        }
        let provider_estimated = seed_historical_case(
            fixture,
            502,
            None,
            Some(RequestUsageBasis::ProviderEstimated),
        )
        .await;
        let provider_reported = seed_historical_case(
            fixture,
            503,
            None,
            Some(RequestUsageBasis::ProviderReported),
        )
        .await;
        let success =
            seed_historical_case(fixture, 200, None, Some(RequestUsageBasis::NotObserved)).await;

        let immutable_before = immutable_snapshot(fixture).await;
        let aggregates_before = aggregate_costs(fixture).await;
        let dry_run = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: false,
                batch_size: 100,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(dry_run.candidate_rows, 3);
        assert_eq!(dry_run.changed_rows, 0);
        assert!(!dry_run.applied);
        assert_eq!(dry_run.candidate_corrected_cost_micros, 0);
        assert_eq!(
            dry_run.candidate_cost_delta_micros,
            -dry_run.candidate_cost_micros
        );
        assert_eq!(
            dry_run.candidate_cost_reduction_micros,
            dry_run.candidate_cost_micros
        );
        assert_eq!(dry_run.candidates.len(), 3);
        assert_eq!(immutable_snapshot(fixture).await, immutable_before);
        assert_eq!(aggregate_costs(fixture).await, aggregates_before);

        let first = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: true,
                batch_size: 2,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(first.candidate_rows, 2);
        assert_eq!(first.changed_rows, 2);
        assert!(first.has_more);
        let second = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: true,
                batch_size: 2,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: first.next_cursor.clone(),
            })
            .await
            .unwrap();
        assert_eq!(second.candidate_rows, 1);
        assert_eq!(second.changed_rows, 1);
        assert!(!second.has_more);

        let replay = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: true,
                batch_size: 100,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(replay.candidate_rows, 0);
        assert_eq!(replay.changed_rows, 0);
        assert_eq!(immutable_snapshot(fixture).await, immutable_before);
        let correction_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_cost_projection_corrections WHERE correction_version = $1",
        )
        .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(correction_rows, 3);
        let zeroed_rows: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM request_cost_projection_corrections WHERE correction_version = $1 AND evidence_kind = 'reservation_ceiling_without_usage' AND corrected_fact_cost_micros = 0 AND original_fact_cost_micros = original_request_cost_micros",
        )
        .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
        .fetch_one(&fixture.database.pool)
        .await
        .unwrap();
        assert_eq!(zeroed_rows, 3);

        for (request_id, _) in &corrected {
            let cost: i64 = sqlx::query_scalar(
                "SELECT cost_micros FROM request_stats_facts WHERE request_id = $1",
            )
            .bind(request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
            assert_eq!(cost, 0);
        }
        for (request_id, expected) in [provider_reported, provider_estimated, success] {
            let cost: i64 = sqlx::query_scalar(
                "SELECT cost_micros FROM request_stats_facts WHERE request_id = $1",
            )
            .bind(request_id.to_string())
            .fetch_one(&fixture.database.pool)
            .await
            .unwrap();
            assert_eq!(cost, expected);
        }
        let retained_cost = provider_reported
            .1
            .saturating_add(provider_estimated.1)
            .saturating_add(success.1);
        assert!(
            aggregate_costs(fixture)
                .await
                .into_iter()
                .all(|cost| cost == retained_cost)
        );
    }

    async fn exercise_provider_reported_fact_repair(fixture: &Fixture) {
        let (request_id, actual_cost) = seed_historical_case(
            fixture,
            502,
            None,
            Some(RequestUsageBasis::ProviderReported),
        )
        .await;
        let incorrect_cost = actual_cost.saturating_add(7);
        sqlx::query("UPDATE request_stats_facts SET cost_micros = $1 WHERE request_id = $2")
            .bind(incorrect_cost)
            .bind(request_id.to_string())
            .execute(&fixture.database.pool)
            .await
            .unwrap();

        let preview = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: false,
                batch_size: 1,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(preview.candidate_rows, 1);
        assert_eq!(preview.candidates[0].evidence_kind, "provider_reported");
        assert_eq!(preview.candidates[0].corrected_cost_micros, actual_cost);

        let applied = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: true,
                batch_size: 1,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(applied.changed_rows, 1);
        let corrected: i64 =
            sqlx::query_scalar("SELECT cost_micros FROM request_stats_facts WHERE request_id = $1")
                .bind(request_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
        assert_eq!(corrected, actual_cost);
    }

    async fn wait_for_postgres_blocker(observer: &AnyPool, waiter_pid: i32, blocker_pid: i32) {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let waiting: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE pid = $1 AND state = 'active' AND wait_event_type = 'Lock' AND $2 = ANY(pg_blocking_pids(pid)))",
                )
                .bind(waiter_pid)
                .bind(blocker_pid)
                .fetch_one(observer)
                .await
                .unwrap();
                if waiting {
                    return;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("backfill did not wait for the request statistics writer lock");
    }

    async fn exercise_postgres_projection_serialization(
        fixture: &Fixture,
        database_url: &str,
        observer: &AnyPool,
    ) {
        seed_historical_case(fixture, 503, None, Some(RequestUsageBasis::ContractCeiling)).await;
        let writer_request_id = Uuid::now_v7();
        let writer = Database::connect_with_max(database_url, 1).await.unwrap();
        let reservation = writer
            .start_proxy_request(StartProxyRequest {
                request_id: writer_request_id,
                key: &fixture.key,
                price: &fixture.price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai",
                model: &fixture.model,
                request_object: "gap://failed-cost-backfill/concurrent-writer/request",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
        let writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&writer.pool)
            .await
            .unwrap();

        let mut lock_bytes = [0_u8; 8];
        lock_bytes.copy_from_slice(&writer_request_id.as_bytes()[8..]);
        let pause_lock = i64::from_be_bytes(lock_bytes) & i64::MAX;
        let pause_function = format!(
            "CREATE FUNCTION mtc_pause_failed_cost_writer() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({pause_lock}); RETURN NEW; END $$"
        );
        sqlx::query(sqlx::AssertSqlSafe(pause_function))
            .execute(&fixture.database.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TRIGGER mtc_pause_failed_cost_writer AFTER INSERT ON request_stats_facts FOR EACH ROW EXECUTE FUNCTION mtc_pause_failed_cost_writer()",
        )
        .execute(&fixture.database.pool)
        .await
        .unwrap();

        let gate = Database::connect_with_max(database_url, 1).await.unwrap();
        let mut gate_tx = gate.begin_write_transaction().await.unwrap();
        let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate_tx)
            .await
            .unwrap();
        sqlx::query("SELECT pg_advisory_xact_lock($1)")
            .bind(pause_lock)
            .execute(&mut *gate_tx)
            .await
            .unwrap();

        let writer_tenant_id = fixture.key.tenant_id;
        let writer_task = tokio::spawn(async move {
            let response_object = "gap://failed-cost-backfill/concurrent-writer/response";
            writer
                .finish_proxy_request(FinishProxyRequest {
                    usage_basis: Some(RequestUsageBasis::ProviderReported),
                    first_output_ms: None,
                    generation_duration_ms: None,
                    request_id: writer_request_id,
                    tenant_id: writer_tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 10,
                    output_token_ceiling: 10,
                    requested_service_tier: None,
                    status_code: 503,
                    duration_ms: 25,
                    usage: TokenUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        ..TokenUsage::default()
                    },
                    error_code: None,
                    response_object,
                    routing_session_id: None,
                    routing_terminal_observed_at: None,
                    conversation: None,
                })
                .await
        });
        wait_for_postgres_blocker(observer, writer_pid, gate_pid).await;

        let backfill = Database::connect_with_max(database_url, 1).await.unwrap();
        let waiter_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&backfill.pool)
            .await
            .unwrap();
        let backfill_task = tokio::spawn(async move {
            backfill
                .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                    apply: true,
                    batch_size: 1,
                    from_created_at: 0,
                    to_created_at: i64::MAX,
                    after: None,
                })
                .await
        });
        wait_for_postgres_blocker(observer, waiter_pid, writer_pid).await;
        gate_tx.commit().await.unwrap();
        let writer_result = writer_task.await.unwrap().unwrap();
        let writer_cost = match writer_result {
            FinishProxyRequestResult::Finished { cost_micros, .. }
            | FinishProxyRequestResult::AlreadyFinished { cost_micros, .. } => cost_micros,
        };
        let report = backfill_task.await.unwrap().unwrap();
        assert_eq!(report.changed_rows, 1);
        assert!(
            aggregate_costs(fixture)
                .await
                .into_iter()
                .all(|cost| cost == writer_cost)
        );
        sqlx::query("DROP TRIGGER mtc_pause_failed_cost_writer ON request_stats_facts")
            .execute(&fixture.database.pool)
            .await
            .unwrap();
        sqlx::query("DROP FUNCTION mtc_pause_failed_cost_writer()")
            .execute(&fixture.database.pool)
            .await
            .unwrap();

        seed_historical_case(fixture, 503, None, Some(RequestUsageBasis::ContractCeiling)).await;
        seed_historical_case(fixture, 503, None, Some(RequestUsageBasis::ContractCeiling)).await;
        let gate = Database::connect_with_max(database_url, 1).await.unwrap();
        let mut gate_tx = gate.begin_write_transaction().await.unwrap();
        let gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *gate_tx)
            .await
            .unwrap();
        lock_request_stats_projection_rebuild_in_transaction(&mut gate_tx)
            .await
            .unwrap();
        let first = Database::connect_with_max(database_url, 1).await.unwrap();
        let second = Database::connect_with_max(database_url, 1).await.unwrap();
        let first_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&first.pool)
            .await
            .unwrap();
        let second_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&second.pool)
            .await
            .unwrap();
        let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(3));
        let first_barrier = barrier.clone();
        let first_task = tokio::spawn(async move {
            first_barrier.wait().await;
            first
                .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                    apply: true,
                    batch_size: 1,
                    from_created_at: 0,
                    to_created_at: i64::MAX,
                    after: None,
                })
                .await
        });
        let second_barrier = barrier.clone();
        let second_task = tokio::spawn(async move {
            second_barrier.wait().await;
            second
                .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                    apply: true,
                    batch_size: 1,
                    from_created_at: 0,
                    to_created_at: i64::MAX,
                    after: None,
                })
                .await
        });
        barrier.wait().await;
        wait_for_postgres_blocker(observer, first_pid, gate_pid).await;
        wait_for_postgres_blocker(observer, second_pid, gate_pid).await;
        gate_tx.commit().await.unwrap();
        let first_report = first_task.await.unwrap().unwrap();
        let second_report = second_task.await.unwrap().unwrap();
        assert_eq!(first_report.changed_rows + second_report.changed_rows, 2);
        assert!(
            aggregate_costs(fixture)
                .await
                .into_iter()
                .all(|cost| cost == writer_cost)
        );

        let stats_gate = Database::connect_with_max(database_url, 1).await.unwrap();
        let mut stats_gate_tx = stats_gate.begin_write_transaction().await.unwrap();
        let stats_gate_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *stats_gate_tx)
            .await
            .unwrap();
        lock_request_stats_projection_rebuild_in_transaction(&mut stats_gate_tx)
            .await
            .unwrap();

        let source_writer = Database::connect_with_max(database_url, 1).await.unwrap();
        let source_writer_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&source_writer.pool)
            .await
            .unwrap();
        let source_writer_task = tokio::spawn(async move {
            let mut transaction = source_writer.begin_write_transaction().await?;
            lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
            lock_request_records_projection_source_in_transaction(&mut transaction).await?;
            lock_generation_jobs_projection_source_in_transaction(&mut transaction).await?;
            transaction.commit().await?;
            Ok::<(), AppError>(())
        });
        wait_for_postgres_blocker(observer, source_writer_pid, stats_gate_pid).await;

        let prune = Database::connect_with_max(database_url, 1).await.unwrap();
        let prune_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&prune.pool)
            .await
            .unwrap();
        let prune_task = tokio::spawn(async move {
            let mut transaction = prune.begin_write_transaction().await?;
            lock_request_stats_projection_rebuild_in_transaction(&mut transaction).await?;
            sqlx::query("LOCK TABLE request_records, generation_jobs IN SHARE MODE")
                .execute(&mut *transaction)
                .await?;
            transaction.commit().await?;
            Ok::<(), AppError>(())
        });
        wait_for_postgres_blocker(observer, prune_pid, stats_gate_pid).await;
        stats_gate_tx.commit().await.unwrap();
        let (source_writer_result, prune_result) =
            tokio::time::timeout(std::time::Duration::from_secs(10), async {
                tokio::join!(source_writer_task, prune_task)
            })
            .await
            .expect("source writer and pruning lock order deadlocked");
        source_writer_result.unwrap().unwrap();
        prune_result.unwrap().unwrap();
    }

    #[tokio::test]
    async fn sqlite_backfill_is_bounded_resumable_idempotent_and_rebuilds_from_facts() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("failed-cost-backfill.db").display()
        );
        let fixture = fixture(&database_url, Some(directory)).await;
        exercise_backfill(&fixture).await;
    }

    #[tokio::test]
    async fn sqlite_backfill_uses_only_provider_reported_usage_for_nonzero_repairs() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("failed-cost-provider-evidence.db")
                .display()
        );
        let fixture = fixture(&database_url, Some(directory)).await;
        exercise_provider_reported_fact_repair(&fixture).await;
    }

    #[tokio::test]
    async fn sqlite_backfill_rolls_back_the_fact_and_retries_after_projection_failure() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("failed-cost-backfill-retry.db")
                .display()
        );
        let fixture = fixture(&database_url, Some(directory)).await;
        let (request_id, original_cost) = seed_historical_case(
            &fixture,
            503,
            None,
            Some(RequestUsageBasis::ContractCeiling),
        )
        .await;
        let immutable_before = immutable_snapshot(&fixture).await;
        let aggregates_before = aggregate_costs(&fixture).await;
        sqlx::query(
            "CREATE TRIGGER fail_cost_backfill_projection BEFORE UPDATE ON request_daily_aggregates BEGIN SELECT RAISE(ABORT, 'injected projection failure'); END",
        )
        .execute(&fixture.database.pool)
        .await
        .unwrap();

        assert!(
            fixture
                .database
                .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                    apply: true,
                    batch_size: 1,
                    from_created_at: 0,
                    to_created_at: i64::MAX,
                    after: None,
                })
                .await
                .is_err()
        );
        let rolled_back_cost: i64 =
            sqlx::query_scalar("SELECT cost_micros FROM request_stats_facts WHERE request_id = $1")
                .bind(request_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
        assert_eq!(rolled_back_cost, original_cost);
        assert_eq!(immutable_snapshot(&fixture).await, immutable_before);
        assert_eq!(aggregate_costs(&fixture).await, aggregates_before);

        sqlx::query("DROP TRIGGER fail_cost_backfill_projection")
            .execute(&fixture.database.pool)
            .await
            .unwrap();
        let retry = fixture
            .database
            .backfill_failed_request_costs(FailedRequestCostBackfillInput {
                apply: true,
                batch_size: 1,
                from_created_at: 0,
                to_created_at: i64::MAX,
                after: None,
            })
            .await
            .unwrap();
        assert_eq!(retry.changed_rows, 1);
        let corrected_cost: i64 =
            sqlx::query_scalar("SELECT cost_micros FROM request_stats_facts WHERE request_id = $1")
                .bind(request_id.to_string())
                .fetch_one(&fixture.database.pool)
                .await
                .unwrap();
        assert_eq!(corrected_cost, 0);
    }

    #[tokio::test]
    async fn postgres_backfill_is_bounded_resumable_idempotent_and_rebuilds_from_facts() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            eprintln!("MTC_TEST_POSTGRES_URL is unset; skipping failed cost backfill contract");
            return;
        };
        sqlx::any::install_default_drivers();
        let admin = AnyPool::connect(&database_url).await.unwrap();
        let schema = format!("failed_cost_backfill_{}", Uuid::now_v7().simple());
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated = Url::parse(&database_url).unwrap();
        isolated
            .query_pairs_mut()
            .append_pair("options", &format!("-c search_path={schema}"));
        let contract_fixture = fixture(isolated.as_str(), None).await;
        exercise_backfill(&contract_fixture).await;
        let concurrency_fixture = fixture(isolated.as_str(), None).await;
        exercise_postgres_projection_serialization(&concurrency_fixture, isolated.as_str(), &admin)
            .await;
        contract_fixture.database.close().await;
        concurrency_fixture.database.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
