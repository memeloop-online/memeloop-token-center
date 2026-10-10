use super::*;

#[derive(Clone, Debug, Eq, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FailedRequestCostBackfillExactManifest {
    pub tenant_id: String,
    pub currency: String,
    pub requests: Vec<FailedRequestCostBackfillExpectedRequest>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FailedRequestCostBackfillExpectedRequest {
    pub request_id: String,
    pub created_at: i64,
    pub expected_cost_micros: i64,
    pub expected_status_code: i64,
    /// Explicit null and empty string both mean no error code; no trimming or case folding.
    pub expected_error_code: Option<String>,
}

impl<'de> serde::Deserialize<'de> for FailedRequestCostBackfillExpectedRequest {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Preserve the distinction between an omitted error field and explicit JSON null.
        fn present_error<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<Option<String>>, D::Error> {
            <Option<String> as serde::Deserialize>::deserialize(deserializer).map(Some)
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireRequest {
            request_id: String,
            created_at: i64,
            expected_cost_micros: i64,
            expected_status_code: Option<i64>,
            #[serde(default, deserialize_with = "present_error")]
            expected_error_code: Option<Option<String>>,
        }
        let wire = <WireRequest as serde::Deserialize>::deserialize(deserializer)?;
        let (expected_status_code, expected_error_code) =
            match (wire.expected_status_code, wire.expected_error_code) {
                (None, None) => (499, Some("client_cancelled".into())),
                (Some(status), Some(error)) => (status, error),
                _ => {
                    return Err(serde::de::Error::custom(
                        "expected status and error must be supplied together",
                    ));
                }
            };
        Ok(Self {
            request_id: wire.request_id,
            created_at: wire.created_at,
            expected_cost_micros: wire.expected_cost_micros,
            expected_status_code,
            expected_error_code,
        })
    }
}

impl FailedRequestCostBackfillExpectedRequest {
    fn normalized_error_code(&self) -> &str {
        self.expected_error_code.as_deref().unwrap_or("")
    }
}

// Both fresh candidates and replay validate the same terminal evidence.
fn terminal_predicate(status_parameter: &str, error_parameter: &str) -> String {
    format!(
        "AND r.status_code = {status_parameter}
         AND COALESCE(r.error_code, '') = {error_parameter}
         AND COALESCE(r.error_code, '') = COALESCE(f.error_code, '')
         AND f.status_class = 'failure' AND r.usage_basis = 'contract_ceiling'"
    )
}

#[derive(Debug, Serialize)]
pub struct FailedRequestCostBackfillExactReport {
    pub manifest_rows: usize,
    pub already_projected_rows: usize,
    pub backfill: FailedRequestCostBackfillReport,
}

impl FailedRequestCostBackfillExactManifest {
    fn validate(&self) -> Result<(), AppError> {
        let mut ids = HashSet::new();
        let mut total = 0_i64;
        if Uuid::parse_str(&self.tenant_id).is_err()
            || self.currency != "USD"
            || self.requests.is_empty()
            || self.requests.len() > FAILED_REQUEST_COST_BACKFILL_MAX_BATCH_SIZE as usize
        {
            return Err(AppError::BadRequest(
                "invalid exact projection manifest scope".into(),
            ));
        }
        for request in &self.requests {
            if Uuid::parse_str(&request.request_id).is_err()
                || !ids.insert(&request.request_id)
                || request.created_at < 0
                || request.created_at == i64::MAX
                || request.expected_cost_micros <= 0
                || !matches!(request.expected_status_code, 499 | 502 | 504)
            {
                return Err(AppError::BadRequest(
                    "invalid or duplicate exact projection request".into(),
                ));
            }
            total = total
                .checked_add(request.expected_cost_micros)
                .ok_or_else(|| {
                    AppError::BadRequest("exact projection manifest total overflows".into())
                })?;
        }
        Ok(())
    }
}

impl Database {
    pub async fn backfill_failed_request_costs_exact(
        &self,
        manifest: FailedRequestCostBackfillExactManifest,
        apply: bool,
    ) -> Result<FailedRequestCostBackfillExactReport, AppError> {
        manifest.validate()?;
        let mut transaction = if apply {
            self.begin_write_transaction().await?
        } else {
            self.pool
                .begin_with(match self.backend {
                    DatabaseBackend::PostgreSql => {
                        "BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY"
                    }
                    DatabaseBackend::Sqlite => "BEGIN",
                })
                .await?
        };
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SET LOCAL statement_timeout = '5s'")
                .execute(&mut *transaction)
                .await?;
            sqlx::query("SET LOCAL lock_timeout = '1s'")
                .execute(&mut *transaction)
                .await?;
        }
        if apply {
            lock_request_stats_projection_rebuild_in_transaction(&mut transaction).await?;
        }
        let lock = if apply && matches!(self.backend, DatabaseBackend::PostgreSql) {
            " FOR UPDATE OF f"
        } else {
            ""
        };
        let terminal = terminal_predicate("$8", "$9");
        let statement = scoped_candidate_statement(
            lock,
            false,
            &format!(
                "AND f.request_id = $7 AND r.created_at = $1
             AND r.tenant_id = f.tenant_id AND r.key_id = f.key_id
             AND r.currency = f.currency {terminal}
             AND u.key_id = r.key_id
             AND NOT EXISTS (SELECT 1 FROM request_cost_projection_corrections other
                 WHERE other.request_created_at = $1 AND other.request_id = $7)"
            ),
        );
        let mut candidates = Vec::with_capacity(manifest.requests.len());
        let mut already_projected_rows = 0;
        for expected in &manifest.requests {
            let rows = sqlx::query(sqlx::AssertSqlSafe(statement.clone()))
                .bind(expected.created_at)
                .bind(expected.created_at + 1)
                .bind(expected.created_at - 1)
                .bind("")
                .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
                .bind(1_i64)
                .bind(&expected.request_id)
                .bind(expected.expected_status_code)
                .bind(expected.normalized_error_code())
                .fetch_all(&mut *transaction)
                .await?;
            let mut matches = candidates_from_rows(rows)?;
            if let Some(candidate) = matches.pop() {
                if candidate.request_id != expected.request_id
                    || candidate.created_at != expected.created_at
                    || candidate.tenant_id != manifest.tenant_id
                    || candidate.currency != manifest.currency
                    || candidate.cost_micros != expected.expected_cost_micros
                    || candidate.request_cost_micros != expected.expected_cost_micros
                    || candidate.corrected_cost_micros != 0
                    || candidate.status_code != expected.expected_status_code
                    || candidate.status_class != "failure"
                    || candidate.error_code != expected.normalized_error_code()
                    || candidate.usage_basis.as_deref() != Some("contract_ceiling")
                {
                    transaction.rollback().await?;
                    return Err(scope_drift());
                }
                candidates.push(candidate);
            } else if already_projected(&mut transaction, &manifest, expected, lock).await? {
                already_projected_rows += 1;
            } else {
                transaction.rollback().await?;
                return Err(scope_drift());
            }
        }
        candidates.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.request_id.cmp(&right.request_id))
        });
        let backfill = if apply {
            apply_candidates(
                transaction,
                candidates,
                manifest.requests.len() as i64,
                true,
            )
            .await?
        } else {
            transaction.rollback().await?;
            prepare_batch(candidates, manifest.requests.len() as i64, false).1
        };
        Ok(FailedRequestCostBackfillExactReport {
            manifest_rows: manifest.requests.len(),
            already_projected_rows,
            backfill,
        })
    }
}

fn scope_drift() -> AppError {
    AppError::Conflict("exact projection manifest identity, amount or state drift".into())
}

async fn already_projected(
    transaction: &mut Transaction<'_, Any>,
    manifest: &FailedRequestCostBackfillExactManifest,
    expected: &FailedRequestCostBackfillExpectedRequest,
    lock: &str,
) -> Result<bool, AppError> {
    let terminal = terminal_predicate("$7", "$8");
    let statement = format!(
        "SELECT f.request_id FROM request_stats_facts f
         JOIN request_records r ON r.id = f.request_id AND r.created_at = f.created_at
         JOIN usage_reservations u ON u.id = r.reservation_id
         JOIN request_cost_projection_corrections correction
           ON correction.request_id = f.request_id AND correction.correction_version = $6
         WHERE f.request_id = $1 AND f.created_at = $2 AND r.created_at = $2
           AND f.tenant_id = $3 AND r.tenant_id = $3
           AND f.currency = $4 AND r.currency = $4
           AND f.key_id = r.key_id AND u.key_id = r.key_id
           AND r.completed_at IS NOT NULL {terminal}
           AND r.cost_micros = $5 AND f.cost_micros = 0
           AND u.status = 'settled' AND u.actual_micros = $5 AND u.reserved_micros = $5
           AND u.reserved_tokens = r.input_tokens + r.output_tokens
           AND correction.request_created_at = $2
           AND correction.evidence_kind = 'reservation_ceiling_without_usage'
           AND correction.observed_status_code = $7
           AND correction.observed_error_code = $8
           AND correction.observed_usage_basis = 'contract_ceiling'
           AND correction.reservation_id = r.reservation_id
           AND correction.reservation_reserved_micros = $5
           AND correction.reservation_actual_micros = $5
           AND correction.original_request_cost_micros = $5
           AND correction.original_fact_cost_micros = $5
           AND correction.corrected_fact_cost_micros = 0
           AND NOT EXISTS (SELECT 1 FROM request_cost_projection_corrections other
               WHERE other.request_created_at = $2 AND other.request_id = $1
                 AND other.correction_version <> $6){lock}"
    );
    Ok(sqlx::query(sqlx::AssertSqlSafe(statement))
        .bind(&expected.request_id)
        .bind(expected.created_at)
        .bind(&manifest.tenant_id)
        .bind(&manifest.currency)
        .bind(expected.expected_cost_micros)
        .bind(FAILED_REQUEST_COST_CORRECTION_VERSION)
        .bind(expected.expected_status_code)
        .bind(expected.normalized_error_code())
        .fetch_optional(&mut **transaction)
        .await?
        .is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_validation_rejects_unbounded_ambiguous_or_overflowing_scopes() {
        let request = FailedRequestCostBackfillExpectedRequest {
            request_id: Uuid::now_v7().to_string(),
            created_at: 1,
            expected_cost_micros: 1,
            expected_status_code: 499,
            expected_error_code: Some("client_cancelled".into()),
        };
        let valid = FailedRequestCostBackfillExactManifest {
            tenant_id: Uuid::now_v7().to_string(),
            currency: "USD".into(),
            requests: vec![request.clone()],
        };
        assert!(valid.validate().is_ok());
        let mut empty = valid.clone();
        empty.requests.clear();
        let mut oversized = valid.clone();
        oversized.requests = vec![request.clone(); 1001];
        let mut duplicate = valid.clone();
        duplicate.requests.push(request);
        let mut overflow = valid.clone();
        overflow.requests[0].expected_cost_micros = i64::MAX;
        overflow
            .requests
            .push(FailedRequestCostBackfillExpectedRequest {
                request_id: Uuid::now_v7().to_string(),
                created_at: 1,
                expected_cost_micros: 1,
                expected_status_code: 499,
                expected_error_code: Some("client_cancelled".into()),
            });
        let mut invalid_id = valid.clone();
        invalid_id.requests[0].request_id = "not-a-uuid".into();
        let mut invalid_tenant = valid.clone();
        invalid_tenant.tenant_id.clear();
        let mut invalid_currency = valid.clone();
        invalid_currency.currency = "EUR".into();
        let mut invalid_time = valid.clone();
        invalid_time.requests[0].created_at = i64::MAX;
        let mut invalid_cost = valid;
        invalid_cost.requests[0].expected_cost_micros = 0;
        for invalid in [
            empty,
            oversized,
            duplicate,
            overflow,
            invalid_id,
            invalid_tenant,
            invalid_currency,
            invalid_time,
            invalid_cost,
        ] {
            assert!(invalid.validate().is_err());
        }
        assert!(serde_json::from_str::<FailedRequestCostBackfillExactManifest>(
            r#"{"tenant_id":"00000000-0000-4000-8000-000000000001","currency":"USD","requests":[],"allow_all":true}"#,
        ).is_err());
    }

    #[test]
    fn manifest_rows_bind_terminal_evidence_with_legacy_defaults() {
        let legacy: FailedRequestCostBackfillExpectedRequest = serde_json::from_str(
            r#"{"request_id":"00000000-0000-4000-8000-000000000001","created_at":1,"expected_cost_micros":2}"#,
        )
        .unwrap();
        assert_eq!(legacy.expected_status_code, 499);
        assert_eq!(
            legacy.expected_error_code.as_deref(),
            Some("client_cancelled")
        );

        let null_error: FailedRequestCostBackfillExpectedRequest = serde_json::from_str(
            r#"{"request_id":"00000000-0000-4000-8000-000000000001","created_at":1,"expected_cost_micros":2,"expected_status_code":502,"expected_error_code":null}"#,
        )
        .unwrap();
        assert_eq!(null_error.normalized_error_code(), "");
        let empty_error: FailedRequestCostBackfillExpectedRequest = serde_json::from_str(
            r#"{"request_id":"00000000-0000-4000-8000-000000000001","created_at":1,"expected_cost_micros":2,"expected_status_code":504,"expected_error_code":""}"#,
        )
        .unwrap();
        assert_eq!(empty_error.normalized_error_code(), "");

        for partial in [
            r#"{"request_id":"00000000-0000-4000-8000-000000000001","created_at":1,"expected_cost_micros":2,"expected_status_code":502}"#,
            r#"{"request_id":"00000000-0000-4000-8000-000000000001","created_at":1,"expected_cost_micros":2,"expected_error_code":"upstream_error"}"#,
            r#"{"request_id":"00000000-0000-4000-8000-000000000001","created_at":1,"expected_cost_micros":2,"expected_status_code":502,"expected_error_code":"x","extra":true}"#,
        ] {
            assert!(
                serde_json::from_str::<FailedRequestCostBackfillExpectedRequest>(partial).is_err()
            );
        }

        let mut unsupported = FailedRequestCostBackfillExactManifest {
            tenant_id: Uuid::now_v7().to_string(),
            currency: "USD".into(),
            requests: vec![FailedRequestCostBackfillExpectedRequest {
                request_id: Uuid::now_v7().to_string(),
                created_at: 1,
                expected_cost_micros: 1,
                expected_status_code: 503,
                expected_error_code: None,
            }],
        };
        assert!(unsupported.validate().is_err());
        unsupported.requests[0].expected_status_code = 502;
        assert!(unsupported.validate().is_ok());
    }
}
