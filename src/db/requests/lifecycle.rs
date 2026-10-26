use super::super::archive_spool::BudgetHold;
use super::super::archive_staging::bind_archive_staging_attempt_in_transaction;
use super::super::*;
use super::conversations::{
    ConversationProjectionEnqueueInput, enqueue_conversation_projection_in_transaction,
};
use super::settlement::{
    reserve_metered_usage_with_id_in_transaction, resize_usage_reservation_in_transaction,
    settle_metered_usage_in_transaction,
};
use crate::archive_staging::{
    ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease,
};
use crate::model::MeteredUsageReservation;
use tracing::Instrument;

const ADMISSION_UPSTREAM_MODEL_UPDATE: &str = "UPDATE request_records SET upstream_model = $1 WHERE id = $2 AND reservation_id = $3 AND created_at = $4";

#[cfg(test)]
mod admission_lock_tests;
#[cfg(test)]
mod unlimited_admission_tests;

fn output_validity_price_snapshot(
    price: &ModelPrice,
    output_token_limit: Option<i64>,
) -> Result<String, AppError> {
    if output_token_limit.is_some_and(|limit| !(0..=1_000_000_000).contains(&limit)) {
        return Err(AppError::internal(
            "lifecycle_output_validity_price_snapshot_1",
        ));
    }
    let mut snapshot = serde_json::to_value(price)
        .map_err(|_| AppError::internal("lifecycle_output_validity_price_snapshot_1"))?;
    snapshot["output_token_limit"] = serde_json::json!(output_token_limit);
    serde_json::to_string(&snapshot)
        .map_err(|_| AppError::internal("lifecycle_output_validity_price_snapshot_2"))
}

async fn persist_output_token_limit(
    transaction: &mut Transaction<'_, Any>,
    reservation_id: Uuid,
    price: &ModelPrice,
    output_token_limit: Option<i64>,
) -> Result<(), AppError> {
    let snapshot = output_validity_price_snapshot(price, output_token_limit)?;
    let updated = sqlx::query("UPDATE usage_reservations SET price_snapshot_json = $1 WHERE id = $2 AND status = 'reserved'")
        .bind(snapshot)
        .bind(reservation_id.to_string())
        .execute(&mut **transaction)
        .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Conflict(
            "request output validity snapshot changed".into(),
        ));
    }
    Ok(())
}

fn output_token_limit_from_snapshot(
    snapshot: Option<&str>,
    legacy_ceiling: i64,
) -> Result<Option<i64>, AppError> {
    let Some(snapshot) = snapshot else {
        return Ok(Some(legacy_ceiling));
    };
    let snapshot: serde_json::Value = serde_json::from_str(snapshot)
        .map_err(|_| AppError::internal("lifecycle_output_token_limit_from_snapshot_1"))?;
    match snapshot.get("output_token_limit") {
        None => Ok(Some(legacy_ceiling)),
        Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_i64()
            .filter(|limit| (0..=1_000_000_000).contains(limit))
            .map(Some)
            .ok_or(AppError::internal(
                "lifecycle_output_token_limit_from_snapshot_1",
            )),
    }
}

#[cfg(test)]
mod output_validity_tests {
    use super::*;

    #[test]
    fn snapshot_output_validity_distinguishes_legacy_unknown_and_explicit() {
        assert_eq!(
            output_token_limit_from_snapshot(None, 4096).unwrap(),
            Some(4096)
        );
        assert_eq!(
            output_token_limit_from_snapshot(Some("{}"), 4096).unwrap(),
            Some(4096)
        );
        assert_eq!(
            output_token_limit_from_snapshot(Some(r#"{"output_token_limit":null}"#), 4096).unwrap(),
            None
        );
        assert_eq!(
            output_token_limit_from_snapshot(Some(r#"{"output_token_limit":17}"#), 4096).unwrap(),
            Some(17)
        );
        for invalid in [
            r#"{"output_token_limit":-1}"#,
            r#"{"output_token_limit":"17"}"#,
            r#"{"output_token_limit":1000000001}"#,
        ] {
            assert!(output_token_limit_from_snapshot(Some(invalid), 4096).is_err());
        }
        let usage = TokenUsage {
            output_tokens: 5000,
            ..TokenUsage::default()
        };
        assert_eq!(
            normalize_proxy_usage_with_output_limit(&usage, 10, None, None).unwrap(),
            usage
        );
        assert!(normalize_proxy_usage_with_output_limit(&usage, 10, Some(4096), None).is_err());
        assert!(normalize_proxy_usage_with_output_limit(&usage, 10, Some(17), None).is_err());
        assert!(
            normalize_proxy_usage_with_output_limit(
                &TokenUsage {
                    output_tokens: -1,
                    ..TokenUsage::default()
                },
                10,
                None,
                None
            )
            .is_err()
        );
        assert!(
            normalize_proxy_usage_with_output_limit(
                &TokenUsage {
                    input_tokens: 11,
                    ..usage.clone()
                },
                10,
                None,
                None
            )
            .is_err()
        );
        assert!(
            normalize_proxy_usage_with_output_limit(
                &TokenUsage {
                    output_tokens: 1_000_000_001,
                    ..TokenUsage::default()
                },
                10,
                None,
                None
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod admission_partition_tests {
    use super::ADMISSION_UPSTREAM_MODEL_UPDATE;

    #[tokio::test]
    async fn postgres_admission_snapshot_update_prunes_to_known_partition() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let pool = sqlx::PgPool::connect(&database_url).await.unwrap();
        let mut transaction = pool.begin().await.unwrap();
        for statement in [
            "CREATE TEMP TABLE request_records (id TEXT, reservation_id TEXT, created_at BIGINT, upstream_model TEXT) PARTITION BY RANGE (created_at)",
            "CREATE TEMP TABLE admission_older PARTITION OF request_records FOR VALUES FROM (0) TO (1000)",
            "CREATE TEMP TABLE admission_current PARTITION OF request_records FOR VALUES FROM (1000) TO (2000)",
            "CREATE TEMP TABLE admission_newer PARTITION OF request_records FOR VALUES FROM (2000) TO (3000)",
            "CREATE TEMP TABLE admission_default PARTITION OF request_records DEFAULT",
            "INSERT INTO request_records VALUES ('request', 'reservation', 500, 'older'), ('request', 'reservation', 1500, NULL), ('request', 'reservation', 2500, 'newer'), ('request', 'reservation', 3500, 'default')",
        ] {
            sqlx::query(statement)
                .execute(&mut *transaction)
                .await
                .unwrap();
        }
        let statement =
            format!("EXPLAIN (FORMAT TEXT, COSTS OFF) {ADMISSION_UPSTREAM_MODEL_UPDATE}");
        let plan: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
            .bind("selected-model")
            .bind("request")
            .bind("reservation")
            .bind(1500_i64)
            .fetch_all(&mut *transaction)
            .await
            .unwrap();
        let rendered = plan.join("\n");
        assert!(rendered.contains("admission_current"), "{rendered}");
        for unrelated in ["admission_older", "admission_newer", "admission_default"] {
            assert!(!rendered.contains(unrelated), "{rendered}");
        }
        let updated = sqlx::query(ADMISSION_UPSTREAM_MODEL_UPDATE)
            .bind("selected-model")
            .bind("request")
            .bind("reservation")
            .bind(1500_i64)
            .execute(&mut *transaction)
            .await
            .unwrap();
        assert_eq!(updated.rows_affected(), 1);
        let models: Vec<Option<String>> =
            sqlx::query_scalar("SELECT upstream_model FROM request_records ORDER BY created_at")
                .fetch_all(&mut *transaction)
                .await
                .unwrap();
        assert_eq!(
            models,
            ["older", "selected-model", "newer", "default"].map(|model| Some(model.to_owned()))
        );
        transaction.rollback().await.unwrap();
        pool.close().await;
    }
}

pub struct NewRequest {
    pub request_id: Uuid,
    pub key_id: Uuid,
    pub tenant_id: Uuid,
    pub protocol: String,
    pub model: String,
    pub request_object: String,
    pub reservation_id: Uuid,
    pub upstream_account_id: Option<Uuid>,
    pub model_route_id: Option<Uuid>,
}

pub struct FinishRequest {
    pub first_output_ms: Option<i64>,
    pub generation_duration_ms: Option<i64>,
    pub request_id: Uuid,
    pub status_code: i64,
    pub duration_ms: i64,
    pub input_tokens: i64,
    /// Cached input tokens are included in `input_tokens` for compatibility with existing
    /// request views and `/stats`; analysis rollups subtract the separately cached portions.
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub service_tier: Option<String>,
    pub cost_micros: i64,
    pub error_code: Option<String>,
    pub response_object: String,
}

struct MeteredRequestUsage<'a> {
    billed_units: i64,
    billing_unit: &'a str,
}

#[derive(Default)]
struct TerminalMetadata {
    cause: Option<crate::model::RequestTerminalCause>,
    cache_coverage: Option<crate::model::CacheUsageCoverage>,
}

pub struct StartProxyRequest<'a> {
    pub request_id: Uuid,
    pub key: &'a AuthenticatedKey,
    pub price: &'a ModelPrice,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub protocol: &'a str,
    pub model: &'a str,
    pub request_object: &'a str,
    pub upstream_account_id: Option<Uuid>,
    pub model_route_id: Option<Uuid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RequestArchiveAdmission {
    Captured,
    Queued,
    GapCapacity,
    GapRetentionLimit,
}

/// `session_dialogue_unavailable` means this settlement could not build 会话正文.
/// The worker reads 原始请求 JSON and 原始返回 JSON from the object locators
/// already stored on the request. 会话正文 is the cleaned training dialogue in CNPG.
const SESSION_DIALOGUE_UNAVAILABLE: &str = "session_dialogue_unavailable";

fn session_dialogue_unavailable(error: &AppError) -> bool {
    match error {
        AppError::Conflict(message) => message == "conversation semantic payload exceeds its bound",
        AppError::Internal { code, .. } => matches!(
            *code,
            "semantic_payload_encode_1"
                | "semantic_payload_encode_2"
                | "memory_flush_1"
                | "conversations_enqueue_conversation_projection_in_transaction_1"
                | "conversations_enqueue_conversation_projection_in_transaction_2"
        ),
        _ => false,
    }
}

async fn enqueue_conversation_task(
    transaction: &mut Transaction<'_, Any>,
    request_id: Uuid,
    conversation: &ProxyConversationInput<'_>,
    observed_at: i64,
) -> Result<(), AppError> {
    match enqueue_conversation_projection_in_transaction(
        transaction,
        ConversationProjectionEnqueueInput {
            request_id,
            key: conversation.key,
            request_json: conversation.request_json,
            hints: conversation.hints,
            client_name: conversation.client_name,
            upstream_response_id: conversation.upstream_response_id,
            observed_at,
            content_materialized: false,
        },
    )
    .await
    {
        Ok(()) => Ok(()),
        Err(error) if session_dialogue_unavailable(&error) => {
            tracing::warn!(
                phase = "conversation_enqueue",
                reason = SESSION_DIALOGUE_UNAVAILABLE,
                error_code = error.diagnostic_category(),
                "会话正文 could not be built; settlement continues"
            );
            Ok(())
        }
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub(crate) struct StartedProxyRequest {
    pub(crate) reservation: UsageReservation,
    pub(crate) archive_admission: RequestArchiveAdmission,
}

impl std::ops::Deref for StartedProxyRequest {
    type Target = UsageReservation;

    fn deref(&self) -> &Self::Target {
        &self.reservation
    }
}

/// Admission contract for a synchronous request billed in provider-defined
/// generation units rather than tokens.
///
/// Unit quantity, billing unit, and price snapshot are persisted independently
/// from token ceilings so metered media can share RPM, concurrency, balance,
/// and budget enforcement without consuming TPM.
pub struct StartMeteredSynchronousRequest<'a> {
    pub request_id: Uuid,
    pub key: &'a AuthenticatedKey,
    pub price: &'a GenerationPrice,
    pub unit_ceiling: i64,
    pub protocol: &'a str,
    pub model: &'a str,
    pub request_object: &'a str,
    pub upstream_account_id: Option<Uuid>,
    pub model_route_id: Option<Uuid>,
}

pub(crate) struct SwitchProxyCandidateInput<'a> {
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub key: &'a AuthenticatedKey,
    pub price: &'a ModelPrice,
    pub reservation: &'a UsageReservation,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub output_token_limit: Option<i64>,
    pub upstream_model: &'a str,
    pub expected_assignment: (Uuid, Uuid),
    pub next_assignment: (Uuid, Uuid),
}

#[derive(Clone)]
pub struct ProxyConversationInput<'a> {
    pub key: &'a AuthenticatedKey,
    pub request_json: &'a serde_json::Value,
    pub hints: &'a ConversationHints,
    pub client_name: Option<&'a str>,
    pub upstream_response_id: Option<&'a str>,
}

#[derive(Clone)]
pub struct FinishProxyRequest<'a> {
    pub terminal_cause: Option<crate::model::RequestTerminalCause>,
    pub usage_basis: Option<crate::model::RequestUsageBasis>,
    pub first_output_ms: Option<i64>,
    pub generation_duration_ms: Option<i64>,
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub reservation: &'a UsageReservation,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub requested_service_tier: Option<&'a str>,
    pub status_code: i64,
    pub duration_ms: i64,
    pub usage: TokenUsage,
    pub error_code: Option<&'a str>,
    pub response_object: &'a str,
    /// Explicit downstream session identity captured independently from
    /// semantic conversation projection. Terminal routing evidence must be
    /// durable even when projection is deferred or skipped.
    pub routing_session_id: Option<&'a str>,
    /// Stable time at which the terminal became observable. Streaming callers
    /// capture this before closing the response body so delayed settlement
    /// cannot reorder concurrent requests in the same session.
    pub routing_terminal_observed_at: Option<i64>,
    pub conversation: Option<ProxyConversationInput<'a>>,
}

/// Terminal contract paired with [`StartMeteredSynchronousRequest`].
pub struct FinishMeteredSynchronousRequest<'a> {
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub reservation: &'a MeteredUsageReservation,
    pub unit_ceiling: i64,
    pub billed_units: i64,
    pub generation_duration_ms: Option<i64>,
    pub status_code: i64,
    pub duration_ms: i64,
    pub error_code: Option<&'a str>,
    pub response_object: &'a str,
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ProxyRequestUpstreamAttribution {
    KeepSelected,
    LastDispatched(Option<(Uuid, Uuid)>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FinishProxyRequestResult {
    Finished {
        cost_micros: i64,
        usage_invalid: bool,
    },
    AlreadyFinished {
        status_code: i64,
        cost_micros: i64,
        error_code: Option<String>,
        response_object: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AttachProxyArchiveResult {
    Attached,
    AlreadyAttached,
}

impl Database {
    pub async fn allowed_models(&self, key: &AuthenticatedKey) -> Result<Vec<String>, AppError> {
        self.granted_available_models(key.key_id, key.tenant_id)
            .await
    }

    pub async fn start_proxy_request(
        &self,
        input: StartProxyRequest<'_>,
    ) -> Result<UsageReservation, AppError> {
        let output_token_limit = Some(input.output_token_ceiling);
        self.start_proxy_request_inner(input, None, true, None, output_token_limit)
            .await
            .map(|started| started.reservation)
    }

    #[cfg(test)]
    pub(crate) async fn start_proxy_forwarding_request(
        &self,
        input: StartProxyRequest<'_>,
        upstream_model: Option<&str>,
    ) -> Result<UsageReservation, AppError> {
        let output_token_limit = Some(input.output_token_ceiling);
        self.start_proxy_forwarding_request_with_output_limit(
            input,
            upstream_model,
            output_token_limit,
        )
        .await
    }

    pub(crate) async fn start_proxy_forwarding_request_with_output_limit(
        &self,
        input: StartProxyRequest<'_>,
        upstream_model: Option<&str>,
        output_token_limit: Option<i64>,
    ) -> Result<UsageReservation, AppError> {
        self.start_proxy_request_inner(input, None, false, upstream_model, output_token_limit)
            .await
            .map(|started| started.reservation)
            .map_err(|error| match error {
                error @ (AppError::Storage { .. } | AppError::Internal { .. }) => error
                    .retarget_capacity(
                        "request_record_persist_failed",
                        crate::error::ErrorFacts::new().phase("request_record"),
                    ),
                other => other,
            })
    }

    pub(crate) async fn publish_proxy_started_event(
        &self,
        identity: ArchiveSpoolIdentity,
    ) -> Result<(), AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let row = sqlx::query("SELECT * FROM request_records WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3")
            .bind(identity.request_id.to_string())
            .bind(identity.tenant_id.to_string())
            .bind(identity.reservation_id.to_string())
            .fetch_one(&mut *transaction).await?;
        let started = NewRequest {
            request_id: identity.request_id,
            tenant_id: identity.tenant_id,
            key_id: parse_uuid(row.try_get("key_id")?)?,
            reservation_id: identity.reservation_id,
            protocol: row.try_get("protocol")?,
            model: row.try_get("model")?,
            request_object: row.try_get("request_object")?,
            upstream_account_id: None,
            model_route_id: None,
        };
        insert_request_started_event_in_transaction(
            &mut transaction,
            &started,
            row.try_get("created_at")?,
        )
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn start_metered_synchronous_request(
        &self,
        input: StartMeteredSynchronousRequest<'_>,
    ) -> Result<MeteredUsageReservation, AppError> {
        if input.unit_ceiling <= 0 || input.price.billing_unit.trim().is_empty() {
            return Err(AppError::BadRequest(
                "metered request requires a positive unit ceiling and billing unit".into(),
            ));
        }
        let reservation_id = Uuid::now_v7();
        let mut transaction = self.begin_write_transaction().await?;
        let now = unix_millis();
        let reservation = reserve_metered_usage_with_id_in_transaction(
            &mut transaction,
            input.key,
            input.price,
            input.unit_ceiling,
            now,
            reservation_id,
        )
        .await?;
        let started = NewRequest {
            request_id: input.request_id,
            key_id: input.key.key_id,
            tenant_id: input.key.tenant_id,
            protocol: input.protocol.to_owned(),
            model: input.model.to_owned(),
            request_object: input.request_object.to_owned(),
            reservation_id,
            upstream_account_id: input.upstream_account_id,
            model_route_id: input.model_route_id,
        };
        insert_request_started_record_in_transaction(&mut transaction, &started, now).await?;
        insert_request_started_event_in_transaction(&mut transaction, &started, now).await?;
        transaction.commit().await?;
        Ok(reservation)
    }

    #[cfg(test)]
    pub(crate) async fn start_proxy_request_with_archive(
        &self,
        input: StartProxyRequest<'_>,
        body: &bytes::Bytes,
        pepper: &[u8],
    ) -> Result<StartedProxyRequest, AppError> {
        self.start_proxy_request_with_archive_compression(input, body, pepper, false)
            .await
    }

    #[cfg(test)]
    pub(crate) async fn start_proxy_request_with_archive_compression(
        &self,
        input: StartProxyRequest<'_>,
        body: &bytes::Bytes,
        pepper: &[u8],
        compression_enabled: bool,
    ) -> Result<StartedProxyRequest, AppError> {
        let started = std::time::Instant::now();
        let output_token_limit = Some(input.output_token_ceiling);
        let result = self
            .start_proxy_request_inner(
                input,
                Some((body, pepper, compression_enabled)),
                true,
                None,
                output_token_limit,
            )
            .await
            .map_err(|error| match error {
                error @ (AppError::Storage { .. } | AppError::Internal { .. }) => error
                    .retarget_capacity(
                        "request_record_persist_failed",
                        crate::error::ErrorFacts::new().phase("request_admission"),
                    ),
                other => other,
            });
        if let Err(error) = &result {
            tracing::warn!(
                phase = "request_admission",
                error_code = error.diagnostic_category(),
                elapsed_ms = started.elapsed().as_millis() as u64,
                "durable request admission failed; upstream dispatch prohibited"
            );
        }
        result
    }

    async fn start_proxy_request_inner(
        &self,
        input: StartProxyRequest<'_>,
        archive: Option<(&bytes::Bytes, &[u8], bool)>,
        publish_started: bool,
        upstream_model: Option<&str>,
        output_token_limit: Option<i64>,
    ) -> Result<StartedProxyRequest, AppError> {
        let price_snapshot_json = output_validity_price_snapshot(input.price, output_token_limit)?;
        // A stable reservation UUID supplies the authenticated encryption owner
        // before any transaction or global budget lock is acquired.
        let reservation_id = Uuid::now_v7();
        let identity = ArchiveSpoolIdentity {
            request_id: input.request_id,
            tenant_id: input.key.tenant_id,
            reservation_id,
        };
        let purpose = crate::response_archive_spool::BufferedArchivePurpose::Request;
        // Admission passes the full request bytes to the spool. Prepare or
        // capture capacity is recorded as spool_capacity and the client continues.
        let buffered_archive = match archive {
            Some((body, pepper, compression_enabled)) => {
                match crate::response_archive_spool::BufferedArchive::new(
                    identity,
                    purpose,
                    body,
                    pepper,
                    compression_enabled,
                ) {
                    Ok(prepared) => Some(prepared),
                    Err(error @ AppError::Capacity { .. }) => {
                        tracing::warn!(
                            phase = "request_admission_capture",
                            reason = "spool_capacity",
                            error_code = error.diagnostic_category(),
                            "request archive prepare reported spool capacity; admission continues"
                        );
                        None
                    }
                    Err(other) => {
                        return Err(other.retarget_capacity(
                            "request_archive_prepare_failed",
                            crate::error::ErrorFacts::new().phase("request_admission"),
                        ));
                    }
                }
            }
            None => None,
        };
        // Prepare only the existing bounded first insert batch before taking
        // the shared budget lock. Cancellation here exposes no admission
        // facts, and capacity rejection drops this transient ciphertext.
        let prepared_request_batch = match buffered_archive.as_ref() {
            Some(archive) => Some(archive.prepare_first_batch().await?),
            None => None,
        };
        // Reserve capacity in a short, independently committed transaction.
        // Request/account/session locks hold only the private reservation row,
        // never the shared counter used by every stream.
        let budget_reservation = match buffered_archive.as_ref() {
            Some(archive) => self.reserve_buffered_archive_capacity(archive).await?,
            None => None,
        };
        let archive_admission = RequestArchiveAdmission::Captured;
        let (mut transaction, now, mut hold) =
            if let Some(reservation) = budget_reservation.as_ref() {
                let (tx, now, hold) = self
                    .reserved_spool_transaction(reservation, "request_admission")
                    .await
                    .map_err(|error| match error {
                        error @ AppError::Capacity { .. } => error,
                        other => other.retarget_capacity(
                            "request_archive_reservation_failed",
                            crate::error::ErrorFacts::new().phase("request_admission"),
                        ),
                    })?;
                (tx, now, Some(hold))
            } else {
                (self.begin_write_transaction().await?, unix_millis(), None)
            };
        // Claim this request's private identity before locking the shared key
        // budget. A slow/contended locator or request insert must not hold up
        // unrelated reservations on the same key. These rows remain invisible
        // until the financial reservation and request commit together below.
        BudgetHold::set_phase(&mut hold, "request_record");
        let started_request = NewRequest {
            request_id: input.request_id,
            key_id: input.key.key_id,
            tenant_id: input.key.tenant_id,
            protocol: input.protocol.to_owned(),
            model: input.model.to_owned(),
            request_object: input.request_object.to_owned(),
            reservation_id,
            upstream_account_id: input.upstream_account_id,
            model_route_id: input.model_route_id,
        };
        if let Err(error) =
            insert_request_started_record_in_transaction(&mut transaction, &started_request, now)
                .await
        {
            BudgetHold::rollback_optional(transaction, hold).await?;
            return Err(error);
        }
        if let Some(upstream_model) = upstream_model {
            let updated = sqlx::query(ADMISSION_UPSTREAM_MODEL_UPDATE)
                .bind(upstream_model)
                .bind(input.request_id.to_string())
                .bind(reservation_id.to_string())
                .bind(now)
                .execute(&mut *transaction)
                .await?;
            if updated.rows_affected() != 1 {
                BudgetHold::rollback_optional(transaction, hold).await?;
                return Err(AppError::internal("lifecycle_start_proxy_request_inner_1"));
            }
        }
        if let Some(archive_body) = buffered_archive.as_ref() {
            BudgetHold::set_phase(&mut hold, "archive_capture");
            let prepared_request_batch = prepared_request_batch
                .ok_or(AppError::internal("lifecycle_start_proxy_request_inner_2"))?;
            let capture_started = std::time::Instant::now();
            let captured = self
                .capture_reserved_buffered_archive_body_in_transaction(
                    &mut transaction,
                    now,
                    archive_body,
                    Some(prepared_request_batch),
                    budget_reservation.as_ref(),
                )
                .await;
            match captured {
                Ok(true) => {}
                Ok(false) => {
                    tracing::warn!(
                        phase = "request_admission_capture",
                        reason = "spool_capacity",
                        error_code = "capacity",
                        elapsed_ms = capture_started.elapsed().as_millis() as u64,
                        "request archive capture reported spool capacity; admission continues"
                    );
                }
                Err(error) => {
                    tracing::warn!(
                        phase = "request_admission_capture",
                        error_code = "capture_failed",
                        elapsed_ms = capture_started.elapsed().as_millis() as u64,
                        "durable request archive capture failed"
                    );
                    BudgetHold::rollback_optional(transaction, hold).await?;
                    if let Some(reservation) = budget_reservation.as_ref() {
                        reservation.release().await;
                    }
                    let mut facts = crate::error::ErrorFacts::new()
                        .phase("archive_capture")
                        .stage("capture_failed");
                    let sql_kind = match &error {
                        AppError::Internal { facts, .. }
                        | AppError::Capacity { facts, .. }
                        | AppError::Storage { facts, .. }
                        | AppError::Upstream { facts, .. } => facts.kind,
                        _ => None,
                    };
                    if let Some(kind) = sql_kind {
                        facts = facts.kind(kind);
                    }
                    return Err(AppError::capacity("request_archive_capture_failed", facts));
                }
            }
        } else if prepared_request_batch.is_some() {
            return Err(AppError::internal("lifecycle_site_3"));
        }
        // Finish private archive writes before taking any financial hold.
        // Admission remains one atomic commit, but a slow archive batch cannot
        // hold the shared key/account rows against another request.
        BudgetHold::set_phase(&mut hold, "key_account_reservation");
        let reservation_span = tracing::info_span!("archive_budget_reservation", request_id = %input.request_id,
            backend_pid = ?hold.as_ref().and_then(BudgetHold::backend_pid));
        let reservation = super::settlement::reserve_usage_with_snapshot_in_transaction(
            &mut transaction,
            input.key,
            input.price,
            input.input_token_ceiling,
            input.output_token_ceiling,
            now,
            reservation_id,
            price_snapshot_json,
        )
        .instrument(reservation_span)
        .await?;
        // Acquire the globally serialized event cursor only after all archive
        // batches are inserted. The event and admission remain one commit,
        // without holding the cross-tenant cursor during compression/inserts.
        BudgetHold::set_phase(&mut hold, "event_cursor");
        if publish_started {
            insert_request_started_event_in_transaction(&mut transaction, &started_request, now)
                .await?;
        }
        // No cancellation deadline: only a positively observed COMMIT permits
        // dispatch. Unknown COMMIT returns unavailable, leaving orphan recovery
        // to settle any admission which actually committed without dispatch.
        BudgetHold::commit_optional(transaction, hold)
            .await
            .map_err(|error| {
                let kind = crate::error::sqlx_error_kind(&error);
                let app = AppError::from(error);
                if archive.is_some() {
                    app.retarget_capacity(
                        "request_archive_commit_failed",
                        crate::error::ErrorFacts::new()
                            .phase("request_admission")
                            .kind(kind),
                    )
                } else {
                    app
                }
            })?;
        if let Some(reservation) = budget_reservation.as_ref() {
            reservation.release().await;
        }
        Ok(StartedProxyRequest {
            reservation,
            archive_admission,
        })
    }

    /// Moves an admitted but unfinished request to the next authorized route
    /// after a pre-delivery upstream failure. The expected assignment makes
    /// this a tenant-scoped CAS; completed requests and concurrent changes are
    /// never overwritten.
    pub async fn reassign_pending_proxy_upstream(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation_id: Uuid,
        expected_assignment: (Uuid, Uuid),
        next_assignment: (Uuid, Uuid),
    ) -> Result<(), AppError> {
        let (expected_upstream_account_id, expected_model_route_id) = expected_assignment;
        let (next_upstream_account_id, next_model_route_id) = next_assignment;
        let updated = sqlx::query(
            "UPDATE request_records
             SET upstream_account_id = $1, model_route_id = $2
             WHERE id = $3 AND tenant_id = $4 AND reservation_id = $5
               AND upstream_account_id = $6 AND model_route_id = $7
               AND completed_at IS NULL",
        )
        .bind(next_upstream_account_id.to_string())
        .bind(next_model_route_id.to_string())
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(reservation_id.to_string())
        .bind(expected_upstream_account_id.to_string())
        .bind(expected_model_route_id.to_string())
        .execute(&self.pool)
        .await?;
        if updated.rows_affected() == 1 {
            return Ok(());
        }
        let current = sqlx::query(
            "SELECT upstream_account_id, model_route_id, completed_at
             FROM request_records
             WHERE id = $1 AND tenant_id = $2 AND reservation_id = $3",
        )
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(reservation_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        if let Some(current) = current
            && current.try_get::<Option<i64>, _>("completed_at")?.is_none()
            && current.try_get::<Option<String>, _>("upstream_account_id")?
                == Some(next_upstream_account_id.to_string())
            && current.try_get::<Option<String>, _>("model_route_id")?
                == Some(next_model_route_id.to_string())
        {
            return Ok(());
        }
        Err(AppError::Conflict(
            "proxy upstream assignment changed before failover".into(),
        ))
    }

    /// Atomically moves one pending downstream request to a newly selected
    /// candidate and resizes its existing reservation to that candidate's
    /// contract. The reservation identity and RPM/concurrency admission are
    /// retained; only token/balance capacity changes.
    pub(crate) async fn switch_pending_proxy_candidate(
        &self,
        input: SwitchProxyCandidateInput<'_>,
    ) -> Result<UsageReservation, AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        let (expected_upstream_account_id, expected_model_route_id) = input.expected_assignment;
        let (next_upstream_account_id, next_model_route_id) = input.next_assignment;
        let updated = sqlx::query(
            "UPDATE request_records
             SET upstream_account_id = $1, model_route_id = $2, upstream_model = $9
             WHERE id = $3 AND tenant_id = $4 AND key_id = $5 AND reservation_id = $6
               AND upstream_account_id = $7 AND model_route_id = $8
               AND (upstream_account_id <> $1 OR model_route_id <> $2 OR upstream_model IS NULL OR upstream_model = $9)
               AND completed_at IS NULL AND error_code IS NULL",
        )
        .bind(next_upstream_account_id.to_string())
        .bind(next_model_route_id.to_string())
        .bind(input.request_id.to_string())
        .bind(input.tenant_id.to_string())
        .bind(input.key.key_id.to_string())
        .bind(input.reservation.id.to_string())
        .bind(expected_upstream_account_id.to_string())
        .bind(expected_model_route_id.to_string())
        .bind(input.upstream_model)
        .execute(&mut *transaction)
        .await?;
        if updated.rows_affected() != 1 {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "proxy candidate changed after downstream delivery preparation".into(),
            ));
        }
        let retained_price: ModelPrice;
        let price = if input.expected_assignment == input.next_assignment {
            let snapshot: String = sqlx::query_scalar("SELECT price_snapshot_json FROM usage_reservations WHERE id = $1 AND status = 'reserved'")
                .bind(input.reservation.id.to_string())
                .fetch_one(&mut *transaction)
                .await?;
            retained_price = serde_json::from_str(&snapshot)
                .map_err(|_| AppError::internal("lifecycle_switch_pending_proxy_candidate_1"))?;
            &retained_price
        } else {
            input.price
        };
        let resized = resize_usage_reservation_in_transaction(
            &mut transaction,
            input.key,
            price,
            input.reservation,
            input.input_token_ceiling,
            input.output_token_ceiling,
            unix_millis(),
        )
        .await?;
        persist_output_token_limit(
            &mut transaction,
            resized.id,
            price,
            input.output_token_limit,
        )
        .await?;
        transaction.commit().await?;
        Ok(resized)
    }

    /// Legacy split attachment retained only for pre-v35 unit fixtures. The
    /// production proxy path has no untracked/CAS attachment writer; historical
    /// locators remain accepted by the request-detail compatibility path and
    /// surface as incomplete when their object is unavailable.
    #[cfg(test)]
    pub async fn attach_proxy_request_archive(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation_id: Uuid,
        staging_object: &str,
        archived_object: &str,
    ) -> Result<AttachProxyArchiveResult, AppError> {
        let _digest = staging_object
            .strip_prefix("staging://blake3/")
            .filter(|digest| {
                digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or_else(|| AppError::BadRequest("invalid proxy archive staging object".into()))?;
        let expected_archived = format!("staging/proxy/{request_id}/request.bin");
        if archived_object != expected_archived {
            return Err(AppError::BadRequest(
                "proxy archive object does not match its admitted request owner".into(),
            ));
        }
        let request_id = request_id.to_string();
        let tenant_id = tenant_id.to_string();
        let reservation_id = reservation_id.to_string();
        let mut transaction = self.begin_write_transaction().await?;
        let attached = sqlx::query(
            "UPDATE request_records SET request_object = $1 WHERE id = $2 AND tenant_id = $3 AND reservation_id = $4 AND request_object = $5 AND completed_at IS NULL",
        )
        .bind(archived_object)
        .bind(&request_id)
        .bind(&tenant_id)
        .bind(&reservation_id)
        .bind(staging_object)
        .execute(&mut *transaction)
        .await?;
        if attached.rows_affected() == 1 {
            transaction.commit().await?;
            return Ok(AttachProxyArchiveResult::Attached);
        }
        let current = sqlx::query(
            "SELECT reservation_id, request_object, completed_at FROM request_records WHERE id = $1 AND tenant_id = $2",
        )
        .bind(&request_id)
        .bind(&tenant_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AppError::NotFound)?;
        if current.try_get::<String, _>("reservation_id")? != reservation_id {
            return Err(AppError::Conflict(
                "request archive reservation ownership mismatch".into(),
            ));
        }
        if current.try_get::<Option<i64>, _>("completed_at")?.is_none()
            && current.try_get::<String, _>("request_object")? == archived_object
        {
            transaction.commit().await?;
            return Ok(AttachProxyArchiveResult::AlreadyAttached);
        }
        Err(AppError::Conflict(
            "request archive ownership changed before attachment".into(),
        ))
    }

    /// Attaches a proxy request object and binds its durable staging attempt in
    /// the same transaction. A fenced writer cannot publish a locator, and an
    /// exact replay after an unknown commit acknowledgement observes both the
    /// locator and the already-bound attempt.
    pub async fn attach_proxy_request_archive_staged(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation_id: Uuid,
        expected_request_object: &str,
        archive_lease: &ArchiveStagingWriteLease,
        archived_object: &str,
    ) -> Result<AttachProxyArchiveResult, AppError> {
        if archive_lease.key.owner != ArchiveStagingOwner::ProxyRequest(request_id)
            || archive_lease.key.purpose != ArchiveStagingPurpose::Request
        {
            return Err(AppError::BadRequest(
                "proxy request archive lease does not match its request owner".into(),
            ));
        }
        let request_id = request_id.to_string();
        let tenant_id = tenant_id.to_string();
        let reservation_id = reservation_id.to_string();
        let mut transaction = self.begin_write_transaction().await?;
        let attached = sqlx::query(
            "UPDATE request_records SET request_object = $1 WHERE id = $2 AND tenant_id = $3 AND reservation_id = $4 AND request_object = $5 AND completed_at IS NULL",
        )
        .bind(archived_object)
        .bind(&request_id)
        .bind(&tenant_id)
        .bind(&reservation_id)
        .bind(expected_request_object)
        .execute(&mut *transaction)
        .await?;
        let result = if attached.rows_affected() == 1 {
            AttachProxyArchiveResult::Attached
        } else {
            let current = sqlx::query(
                "SELECT reservation_id, request_object, completed_at FROM request_records WHERE id = $1 AND tenant_id = $2",
            )
            .bind(&request_id)
            .bind(&tenant_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
            if current.try_get::<String, _>("reservation_id")? != reservation_id {
                return Err(AppError::Conflict(
                    "request archive reservation ownership mismatch".into(),
                ));
            }
            if current.try_get::<Option<i64>, _>("completed_at")?.is_some()
                || current.try_get::<String, _>("request_object")? != archived_object
            {
                return Err(AppError::Conflict(
                    "request archive ownership changed before attachment".into(),
                ));
            }
            AttachProxyArchiveResult::AlreadyAttached
        };
        if !bind_archive_staging_attempt_in_transaction(
            &mut transaction,
            self.backend,
            archive_lease,
            archived_object,
        )
        .await?
        {
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "proxy request archive writer was fenced".into(),
            ));
        }
        transaction.commit().await?;
        Ok(result)
    }

    pub async fn prepare_proxy_delivery(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation: &UsageReservation,
        input_token_ceiling: i64,
        output_token_ceiling: i64,
        requested_service_tier: Option<&str>,
    ) -> Result<(), AppError> {
        if input_token_ceiling < 0
            || output_token_ceiling < 0
            || input_token_ceiling.checked_add(output_token_ceiling)
                != Some(reservation.reserved_tokens)
        {
            return Err(AppError::Conflict(
                "proxy delivery ceiling does not match its reservation".into(),
            ));
        }
        if let Some(tier) = requested_service_tier {
            validate_service_tier(tier)?;
        }
        let service_tier = requested_service_tier.unwrap_or("default");
        let marked = sqlx::query(
            "UPDATE request_records SET error_code = 'delivery_prepared', input_tokens = $1, output_tokens = $2, service_tier = $3 WHERE id = $4 AND tenant_id = $5 AND key_id = $6 AND reservation_id = $7 AND completed_at IS NULL AND ((error_code IS NULL AND input_tokens = 0 AND output_tokens = 0) OR (error_code = 'delivery_prepared' AND input_tokens = $8 AND output_tokens = $9 AND service_tier = $10))",
        )
        .bind(input_token_ceiling)
        .bind(output_token_ceiling)
        .bind(service_tier)
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(reservation.id.to_string())
        .bind(input_token_ceiling)
        .bind(output_token_ceiling)
        .bind(service_tier)
        .execute(&self.pool)
        .await?;
        if marked.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "proxy delivery owner cannot be prepared".into(),
            ));
        }
        Ok(())
    }

    pub async fn mark_proxy_delivery_started(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation: &UsageReservation,
    ) -> Result<(), AppError> {
        let marked = sqlx::query(
            "UPDATE request_records SET error_code = 'delivery_started' WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NULL AND error_code IN ('delivery_prepared', 'delivery_started')",
        )
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(reservation.id.to_string())
        .execute(&self.pool)
        .await?;
        if marked.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "proxy delivery owner is no longer prepared".into(),
            ));
        }
        Ok(())
    }

    /// Stores the stage a still-open proxy request has actually reached.
    /// This is operator evidence for the request list. It is not delivery
    /// preparation, not a settlement claim, and not a terminal cause.
    /// A missing row or an already finished request is left unchanged.
    pub(crate) async fn record_proxy_in_flight_stage(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        key_id: Uuid,
        reservation_id: Uuid,
        stage: &'static str,
    ) -> Result<(), AppError> {
        if crate::model::recorded_in_flight_stage(Some(stage.to_owned())).is_none() {
            return Err(AppError::internal(
                "lifecycle_record_proxy_in_flight_stage_1",
            ));
        }
        sqlx::query(
            "UPDATE request_records SET in_flight_stage = $1 WHERE id = $2 AND tenant_id = $3 AND key_id = $4 AND reservation_id = $5 AND completed_at IS NULL AND (in_flight_stage IS NULL OR in_flight_stage <> $1)",
        )
        .bind(stage)
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .bind(reservation_id.to_string())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Converges an admitted stream after its in-process lifecycle deadline.
    ///
    /// The durable delivery marker, rather than the cancelled task's local
    /// state, selects the persisted ceiling split used to validate the
    /// reservation. With no trustworthy terminal usage, the reservation is
    /// released rather than promoted to actual cost; its immutable estimate
    /// remains in `usage_reservations`. The terminal write below is still the
    /// normal request-owner CAS, so a late normal finalizer is returned as
    /// `AlreadyFinished` and an unavailable database leaves the record pending
    /// for the orphan reaper.
    pub async fn expire_proxy_lifecycle_deadline(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation: &UsageReservation,
        duration_ms: i64,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_unobserved_proxy_request(
            request_id,
            tenant_id,
            reservation,
            duration_ms,
            (504, "request_lifecycle_timeout"),
        )
        .await
    }

    pub(crate) async fn finish_cancelled_proxy_request(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation: &UsageReservation,
        duration_ms: i64,
    ) -> Result<FinishProxyRequestResult, AppError> {
        let mut reservation = reservation.clone();
        reservation.reserved_tokens = sqlx::query_scalar(
            "SELECT reserved_tokens FROM usage_reservations WHERE id = $1 AND key_id = $2 AND account_id = $3",
        )
        .bind(reservation.id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(reservation.account_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        self.finish_unobserved_proxy_request(
            request_id,
            tenant_id,
            &reservation,
            duration_ms,
            (499, "request_cancelled"),
        )
        .await
    }

    async fn finish_unobserved_proxy_request(
        &self,
        request_id: Uuid,
        tenant_id: Uuid,
        reservation: &UsageReservation,
        duration_ms: i64,
        terminal: (i64, &'static str),
    ) -> Result<FinishProxyRequestResult, AppError> {
        let pending = sqlx::query(
            "SELECT error_code, input_tokens, output_tokens, service_tier FROM request_records WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NULL",
        )
        .bind(request_id.to_string())
        .bind(tenant_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(reservation.id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        let delivery_started = pending
            .as_ref()
            .and_then(|row| {
                row.try_get::<Option<String>, _>("error_code")
                    .ok()
                    .flatten()
            })
            .as_deref()
            == Some("delivery_started");
        let input_token_ceiling = if delivery_started {
            pending
                .as_ref()
                .ok_or(AppError::internal(
                    "lifecycle_finish_unobserved_proxy_request_1",
                ))?
                .try_get::<i64, _>("input_tokens")?
        } else {
            reservation.reserved_tokens
        };
        let output_token_ceiling = if delivery_started {
            pending
                .as_ref()
                .ok_or(AppError::internal(
                    "lifecycle_finish_unobserved_proxy_request_2",
                ))?
                .try_get::<i64, _>("output_tokens")?
        } else {
            0
        };
        if input_token_ceiling.checked_add(output_token_ceiling)
            != Some(reservation.reserved_tokens)
        {
            return Err(AppError::Conflict(
                "persisted proxy delivery ceiling does not match its reservation".into(),
            ));
        }
        let requested_service_tier = if delivery_started {
            pending
                .as_ref()
                .ok_or(AppError::internal(
                    "lifecycle_finish_unobserved_proxy_request_3",
                ))?
                .try_get::<Option<String>, _>("service_tier")?
        } else {
            None
        };
        let response_object = format!("gap://{request_id}/response");
        self.finish_proxy_request(FinishProxyRequest {
            terminal_cause: None,
            usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id,
            reservation,
            input_token_ceiling,
            output_token_ceiling,
            requested_service_tier: requested_service_tier.as_deref(),
            status_code: terminal.0,
            duration_ms: duration_ms.max(0),
            usage: TokenUsage::default(),
            error_code: Some(terminal.1),
            response_object: &response_object,
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        })
        .await
    }

    pub async fn finish_proxy_request(
        &self,
        input: FinishProxyRequest<'_>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_with_archive_staging(input, None)
            .await
    }

    pub async fn finish_metered_synchronous_request(
        &self,
        input: FinishMeteredSynchronousRequest<'_>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        if input.unit_ceiling <= 0
            || input.billed_units < 0
            || input.billed_units > input.unit_ceiling
        {
            return Err(AppError::BadRequest(
                "metered request usage exceeds its admitted unit ceiling".into(),
            ));
        }
        let proxy_input = FinishProxyRequest {
            terminal_cause: None,
            usage_basis: None,
            first_output_ms: None,
            generation_duration_ms: input.generation_duration_ms,
            request_id: input.request_id,
            tenant_id: input.tenant_id,
            reservation: &input.reservation.reservation,
            input_token_ceiling: 0,
            output_token_ceiling: 0,
            requested_service_tier: None,
            status_code: input.status_code,
            duration_ms: input.duration_ms,
            usage: TokenUsage::default(),
            error_code: input.error_code,
            response_object: input.response_object,
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        };
        self.finish_proxy_request_inner(
            proxy_input,
            None,
            None,
            ProxyRequestUpstreamAttribution::KeepSelected,
            Some(&input),
            self.terminal_projection_enabled,
            false,
            None,
        )
        .await
    }

    /// Commits a terminal proxy response and its durable staging binding as a
    /// single database transaction. `None` is retained for gap locators and
    /// historical content-addressed response locators.
    pub async fn finish_proxy_request_with_archive_staging(
        &self,
        input: FinishProxyRequest<'_>,
        response_archive_lease: Option<&ArchiveStagingWriteLease>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_inner(
            input,
            response_archive_lease,
            None,
            ProxyRequestUpstreamAttribution::KeepSelected,
            None,
            self.terminal_projection_enabled,
            false,
            None,
        )
        .await
    }

    pub(crate) async fn finish_proxy_request_with_archive_staging_and_upstream_attribution(
        &self,
        input: FinishProxyRequest<'_>,
        response_archive_lease: Option<&ArchiveStagingWriteLease>,
        upstream_attribution: ProxyRequestUpstreamAttribution,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_inner(
            input,
            response_archive_lease,
            None,
            upstream_attribution,
            None,
            self.terminal_projection_enabled,
            false,
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn finish_proxy_request_with_buffered_archive_and_upstream_attribution(
        &self,
        input: FinishProxyRequest<'_>,
        archive: &crate::response_archive_spool::BufferedArchive<'_>,
        upstream_attribution: ProxyRequestUpstreamAttribution,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_inner(
            input,
            None,
            Some(archive),
            upstream_attribution,
            None,
            self.terminal_projection_enabled,
            false,
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn finish_proxy_request_deferred(
        &self,
        input: FinishProxyRequest<'_>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.clone()
            .with_terminal_projection_enabled(true)
            .finish_proxy_request(input)
            .await
    }

    pub(crate) async fn finish_orphaned_proxy_request(
        &self,
        input: FinishProxyRequest<'_>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_inner(
            input,
            None,
            None,
            ProxyRequestUpstreamAttribution::KeepSelected,
            None,
            self.terminal_projection_enabled,
            true,
            None,
        )
        .await
    }

    pub(super) async fn finish_proxy_request_with_pending_receipt(
        &self,
        input: FinishProxyRequest<'_>,
        upstream_attribution: ProxyRequestUpstreamAttribution,
        receipt: &super::pending_settlement::Receipt<'_>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_inner(
            input,
            None,
            None,
            upstream_attribution,
            None,
            true,
            false,
            Some(receipt),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn finish_proxy_request_inner(
        &self,
        input: FinishProxyRequest<'_>,
        response_archive_lease: Option<&ArchiveStagingWriteLease>,
        buffered_archive: Option<&crate::response_archive_spool::BufferedArchive<'_>>,
        upstream_attribution: ProxyRequestUpstreamAttribution,
        metered: Option<&FinishMeteredSynchronousRequest<'_>>,
        defer_projection: bool,
        reap_orphan: bool,
        pending_receipt: Option<&super::pending_settlement::Receipt<'_>>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        if let Some(archive) = buffered_archive
            && (archive.identity().request_id != input.request_id
                || archive.identity().tenant_id != input.tenant_id
                || archive.identity().reservation_id != input.reservation.id
                || archive.purpose()
                    != crate::response_archive_spool::BufferedArchivePurpose::Response)
        {
            return Err(AppError::BadRequest(
                "buffered response archive does not match its request owner".into(),
            ));
        }
        if let Some(lease) = response_archive_lease
            && (lease.key.owner != ArchiveStagingOwner::ProxyRequest(input.request_id)
                || lease.key.purpose != ArchiveStagingPurpose::Response)
        {
            return Err(AppError::BadRequest(
                "proxy response archive lease does not match its request owner".into(),
            ));
        }
        let request_id = input.request_id.to_string();
        let tenant_id = input.tenant_id.to_string();
        let key_id = input.reservation.key_id.to_string();
        let reservation_id = input.reservation.id.to_string();
        let mut preparation_complete = input.conversation.is_none();
        // Match request admission: retain at most one existing insert batch,
        // and perform its compression/encryption before shared database locks.
        let prepared_response_batch = match buffered_archive {
            Some(archive) => Some(archive.prepare_first_batch().await?),
            None => None,
        };
        let budget_reservation = match buffered_archive {
            Some(archive) => self.reserve_buffered_archive_capacity(archive).await?,
            None => None,
        };
        if buffered_archive.is_some() && budget_reservation.is_none() {
            tracing::warn!(
                phase = "response_terminal_capture",
                error_code = "capacity",
                "buffered response archive gap"
            );
        }
        let (mut transaction, now, created_at, reservation_row, trusted_reservation, mut hold) = loop {
            // A buffered response archive takes its private reservation before
            // the request owner row. Settlement does not build 会话正文 here.
            let (mut transaction, mut hold) = if let Some(reservation) =
                budget_reservation.as_ref().filter(|_| preparation_complete)
            {
                let (tx, _, hold) = self
                    .reserved_spool_transaction(reservation, "buffered_terminal")
                    .await?;
                (tx, Some(hold))
            } else {
                (self.begin_write_transaction().await?, None)
            };
            if !defer_projection {
                lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
            }
            BudgetHold::set_phase(&mut hold, "terminal_owner");
            // SQLite's BEGIN IMMEDIATE can wait for an earlier terminal writer.
            // Capture the observation boundary only after that wait so live writes
            // cannot acquire timestamps in the opposite order from their commits.
            let now = unix_millis();

            // This no-op update is the portable owner CAS. PostgreSQL takes a row
            // lock and rechecks the pending predicate after a concurrent owner
            // commits; as the first SQLite statement it acquires the write lock
            // without a deferred read-to-write upgrade race. Only the winner may
            // settle usage or create terminal lineage/statistics.
            let claimed = sqlx::query(
            "UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND completed_at IS NULL",
        )
        .bind(&request_id)
        .bind(&tenant_id)
        .bind(&key_id)
        .bind(&reservation_id)
        .execute(&mut *transaction)
        .await?;
            if let Some(result) =
                super::pending_settlement::guard(&mut transaction, &input, pending_receipt).await?
            {
                BudgetHold::commit_optional(transaction, hold).await?;
                if let Some(reservation) = budget_reservation.as_ref() {
                    reservation.release().await;
                }
                return Ok(result);
            }
            if pending_receipt.is_some() && claimed.rows_affected() == 0 {
                return Err(AppError::Conflict(
                    "request completed without matching settlement receipt".into(),
                ));
            }
            if reap_orphan && claimed.rows_affected() == 1 {
                let expires_at: Option<i64> = sqlx::query_scalar("SELECT proxy_owner_expires_at FROM request_records WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4")
                    .bind(&request_id).bind(&tenant_id).bind(&key_id).bind(&reservation_id)
                    .fetch_one(&mut *transaction).await?;
                let owner_now = self.request_owner_now(&mut transaction).await?;
                if expires_at.is_some_and(|expires_at| expires_at > owner_now) {
                    return Err(AppError::Conflict(
                        "proxy request owner is still live".into(),
                    ));
                }
            }
            if claimed.rows_affected() == 1 && sqlx::query("SELECT 1 FROM request_records WHERE id = $1 AND key_id = $2 AND submission_started_at IS NOT NULL")
                .bind(&request_id).bind(&key_id).fetch_optional(&mut *transaction).await?.is_some() {
                return Err(AppError::Conflict("submitted image requires authoritative image settlement or quarantine".into()));
            }
            if claimed.rows_affected() == 0 && defer_projection
                && let Some(existing) = sqlx::query("SELECT status_code, cost_micros, NULLIF(error_code, '') AS error_code, response_object FROM terminal_projection_outbox WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4")
                    .bind(&request_id).bind(&tenant_id).bind(&key_id).bind(&reservation_id)
                    .fetch_optional(&mut *transaction).await?
            {
                let result = FinishProxyRequestResult::AlreadyFinished {
                    status_code: existing.try_get("status_code")?,
                    cost_micros: existing.try_get("cost_micros")?,
                    error_code: existing.try_get("error_code")?,
                    response_object: existing.try_get("response_object")?,
                };
                BudgetHold::commit_optional(transaction, hold).await?;
                if let Some(reservation) = budget_reservation.as_ref() {
                    reservation.release().await;
                }
                return Ok(result);
            }
            let locator = sqlx::query(
                "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
            )
            .bind(&request_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
            let created_at: i64 = locator.try_get("created_at")?;
            if locator.try_get::<String, _>("tenant_id")? != tenant_id
                || locator.try_get::<String, _>("key_id")? != key_id
            {
                return Err(AppError::NotFound);
            }
            if claimed.rows_affected() == 0 {
                let existing = sqlx::query(
                "SELECT reservation_id, completed_at, status_code, cost_micros, error_code, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND tenant_id = $3 AND key_id = $4",
            )
            .bind(&request_id)
            .bind(created_at)
            .bind(&tenant_id)
            .bind(&key_id)
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
                if existing.try_get::<String, _>("reservation_id")? != reservation_id {
                    return Err(AppError::Conflict(
                        "request reservation ownership mismatch".into(),
                    ));
                }
                if existing
                    .try_get::<Option<i64>, _>("completed_at")?
                    .is_none()
                {
                    return Err(AppError::Conflict(
                        "request terminal ownership changed".into(),
                    ));
                }
                let result = FinishProxyRequestResult::AlreadyFinished {
                    status_code: existing
                        .try_get::<Option<i64>, _>("status_code")?
                        .ok_or(AppError::internal("lifecycle_site_4"))?,
                    cost_micros: existing.try_get("cost_micros")?,
                    error_code: existing.try_get("error_code")?,
                    response_object: existing
                        .try_get::<Option<String>, _>("response_object")?
                        .ok_or(AppError::internal("lifecycle_site_5"))?,
                };
                // Repair feeds for terminal requests written before settlement
                // publication existed. The publisher is idempotent for replays.
                super::super::billing::publish_text_settlement_in_transaction(
                    &mut transaction,
                    input.request_id,
                )
                .await?;
                // A finished request can still be missing its conversation task.
                // Enqueue that task. The worker reads 原始请求 JSON and 原始返回
                // JSON from the object locators already on the request.
                if let Some(conversation) = input.conversation.as_ref() {
                    if conversation.key.tenant_id != input.tenant_id
                        || conversation.key.key_id != input.reservation.key_id
                    {
                        return Err(AppError::NotFound);
                    }
                    let observation_count: i64 = sqlx::query_scalar(
                        "SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1",
                    )
                    .bind(input.request_id.to_string())
                    .fetch_one(&mut *transaction)
                    .await?;
                    if observation_count == 0 {
                        enqueue_conversation_task(
                            &mut transaction,
                            input.request_id,
                            conversation,
                            now,
                        )
                        .await?;
                    }
                }
                BudgetHold::commit_optional(transaction, hold).await?;
                if let Some(reservation) = budget_reservation.as_ref() {
                    reservation.release().await;
                }
                return Ok(result);
            }

            let reservation_row = sqlx::query(
            "SELECT account_id, key_id, enforcement_mode, reserved_micros, reserved_tokens, reserved_units, billing_unit, micros_per_unit, rate_window_start, status, actual_micros, price_snapshot_json FROM usage_reservations WHERE id = $1",
        )
        .bind(&reservation_id)
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AppError::NotFound)?;
            let trusted_account_id = parse_uuid(reservation_row.try_get("account_id")?)?;
            let trusted_key_id = parse_uuid(reservation_row.try_get("key_id")?)?;
            if trusted_account_id != input.reservation.account_id
                || trusted_key_id != input.reservation.key_id
            {
                return Err(AppError::Conflict(
                    "request reservation ownership mismatch".into(),
                ));
            }
            let reserved_tokens: i64 = reservation_row.try_get("reserved_tokens")?;
            if input
                .input_token_ceiling
                .checked_add(input.output_token_ceiling)
                != Some(reserved_tokens)
            {
                return Err(AppError::Conflict(
                    "request reservation ceiling mismatch".into(),
                ));
            }
            let price_snapshot_json: Option<String> =
                reservation_row.try_get("price_snapshot_json")?;
            let (input_micros_per_million, output_micros_per_million, price_tiers) =
                if metered.is_some() {
                    (0, 0, Vec::new())
                } else if let Some(snapshot) = price_snapshot_json {
                    let price: ModelPrice = serde_json::from_str(&snapshot)
                        .map_err(|_| AppError::internal("lifecycle_site_1"))?;
                    (
                        price.input_micros_per_million,
                        price.output_micros_per_million,
                        price.tiers,
                    )
                } else {
                    (
                        input.reservation.input_micros_per_million,
                        input.reservation.output_micros_per_million,
                        input.reservation.price_tiers.clone(),
                    )
                };
            let trusted_reservation = UsageReservation {
                id: input.reservation.id,
                account_id: trusted_account_id,
                key_id: trusted_key_id,
                enforcement_mode: EnforcementMode::from_storage(
                    reservation_row
                        .try_get::<String, _>("enforcement_mode")?
                        .as_str(),
                )
                .ok_or(AppError::internal("lifecycle_site_6"))?,
                reserved_micros: reservation_row.try_get("reserved_micros")?,
                input_micros_per_million,
                output_micros_per_million,
                price_tiers,
                rate_window_start: reservation_row.try_get("rate_window_start")?,
                reserved_tokens,
            };

            if let Some(conversation) = input.conversation.as_ref() {
                if conversation.key.tenant_id != input.tenant_id
                    || conversation.key.key_id != input.reservation.key_id
                {
                    return Err(AppError::NotFound);
                }
                let owner = sqlx::query(
                    "SELECT q.error_code, q.input_tokens, q.output_tokens, q.service_tier, account.principal_id, account.id AS account_id FROM request_records q JOIN usage_reservations reservation ON reservation.id = q.reservation_id AND reservation.key_id = q.key_id JOIN credit_accounts account ON account.id = reservation.account_id WHERE q.id = $1 AND q.created_at = $2 AND q.tenant_id = $3 AND q.key_id = $4",
                )
                .bind(&request_id)
                .bind(created_at)
                .bind(&tenant_id)
                .bind(&key_id)
                .fetch_optional(&mut *transaction)
                .await?
                .ok_or(AppError::NotFound)?;
                if owner.try_get::<String, _>("principal_id")?
                    != conversation.key.principal_id.to_string()
                    || owner.try_get::<String, _>("account_id")?
                        != conversation.key.account_id.to_string()
                    || conversation.key.account_id != trusted_reservation.account_id
                {
                    return Err(AppError::NotFound);
                }
                // Prepared deliveries have a persisted per-direction contract.
                // Keep the historical total-only fallback for requests which
                // never passed delivery preparation (including early errors).
                let delivery_state: Option<String> = owner.try_get("error_code")?;
                if matches!(
                    delivery_state.as_deref(),
                    Some("delivery_prepared" | "delivery_started")
                ) && (owner.try_get::<i64, _>("input_tokens")? != input.input_token_ceiling
                    || owner.try_get::<i64, _>("output_tokens")? != input.output_token_ceiling
                    || owner
                        .try_get::<Option<String>, _>("service_tier")?
                        .as_deref()
                        != Some(input.requested_service_tier.unwrap_or("default")))
                {
                    return Err(AppError::Conflict(
                        "request prepared delivery contract mismatch".into(),
                    ));
                }
                let reservation_status: String = reservation_row.try_get("status")?;
                if !matches!(reservation_status.as_str(), "reserved" | "settled") {
                    return Err(AppError::Conflict(
                        "request reservation is not finishable".into(),
                    ));
                }
                if !preparation_complete {
                    preparation_complete = true;
                    // Restart once so a buffered response archive holds its
                    // private reservation before the request owner row.
                    if buffered_archive.is_some() {
                        BudgetHold::commit_optional(transaction, hold).await?;
                        continue;
                    }
                }
            }
            break (
                transaction,
                now,
                created_at,
                reservation_row,
                trusted_reservation,
                hold,
            );
        };

        let trusted_metered = if let Some(metered_input) = metered {
            let reserved_units: i64 = reservation_row.try_get("reserved_units")?;
            let billing_unit: String = reservation_row.try_get("billing_unit")?;
            let micros_per_unit: i64 = reservation_row.try_get("micros_per_unit")?;
            if trusted_reservation.reserved_tokens != 0
                || reserved_units != metered_input.unit_ceiling
                || reserved_units != metered_input.reservation.unit_ceiling
                || billing_unit != metered_input.reservation.billing_unit
                || micros_per_unit != metered_input.reservation.micros_per_unit
            {
                return Err(AppError::Conflict(
                    "metered request reservation contract mismatch".into(),
                ));
            }
            Some(MeteredUsageReservation {
                reservation: trusted_reservation.clone(),
                unit_ceiling: reserved_units,
                billing_unit,
                micros_per_unit,
            })
        } else {
            if reservation_row.try_get::<i64, _>("reserved_units")? != 0
                || !reservation_row
                    .try_get::<String, _>("billing_unit")?
                    .is_empty()
            {
                return Err(AppError::Conflict(
                    "token request cannot settle a metered reservation".into(),
                ));
            }
            None
        };

        if let ProxyRequestUpstreamAttribution::LastDispatched(assignment) = upstream_attribution {
            let upstream_account_id = assignment.map(|(account_id, _)| account_id.to_string());
            let model_route_id = assignment.map(|(_, route_id)| route_id.to_string());
            let attributed = sqlx::query(
                "UPDATE request_records SET upstream_account_id = $1, model_route_id = $2 WHERE id = $3 AND created_at = $4 AND tenant_id = $5 AND key_id = $6 AND reservation_id = $7 AND completed_at IS NULL",
            )
            .bind(upstream_account_id)
            .bind(model_route_id)
            .bind(&request_id)
            .bind(created_at)
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(&reservation_id)
            .execute(&mut *transaction)
            .await?;
            if attributed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "request terminal upstream attribution changed".into(),
                ));
            }
        }

        if let Some(archive) = buffered_archive.filter(|_| budget_reservation.is_some()) {
            BudgetHold::set_phase(&mut hold, "archive_capture");
            let capture_started = std::time::Instant::now();
            if !self
                .capture_reserved_buffered_archive_body_in_transaction(
                    &mut transaction,
                    now,
                    archive,
                    prepared_response_batch,
                    budget_reservation.as_ref(),
                )
                .await?
            {
                tracing::warn!(
                    phase = "response_terminal_capture",
                    error_code = "capacity",
                    elapsed_ms = capture_started.elapsed().as_millis() as u64,
                    "buffered response archive gap"
                );
            }
        }

        if let Some(lease) = response_archive_lease
            && !bind_archive_staging_attempt_in_transaction(
                &mut transaction,
                self.backend,
                lease,
                input.response_object,
            )
            .await?
        {
            BudgetHold::rollback_optional(transaction, hold).await?;
            return Err(AppError::Conflict(
                "proxy response archive writer was fenced".into(),
            ));
        }

        let (usage, status_code, error_code, response_object, usage_invalid) = if metered.is_some()
        {
            (
                TokenUsage::default(),
                input.status_code,
                input.error_code.map(str::to_owned),
                input.response_object.to_owned(),
                false,
            )
        } else {
            let snapshot: Option<String> = reservation_row.try_get("price_snapshot_json")?;
            let output_token_limit =
                output_token_limit_from_snapshot(snapshot.as_deref(), input.output_token_ceiling)?;
            match normalize_proxy_usage_with_output_limit(
                &input.usage,
                input.input_token_ceiling,
                output_token_limit,
                input.requested_service_tier,
            ) {
                Ok(usage) => (
                    usage,
                    input.status_code,
                    input.error_code.map(str::to_owned),
                    input.response_object.to_owned(),
                    false,
                ),
                Err(AppError::Upstream { .. }) => (
                    TokenUsage::default(),
                    502,
                    Some("upstream_invalid_usage".to_owned()),
                    "{\"error\":{\"message\":\"upstream returned invalid usage\",\"type\":\"upstream_error\"}}".to_owned(),
                    true,
                ),
                Err(error) => return Err(error),
            }
        };

        if usage_invalid && pending_receipt.is_some() {
            return Err(AppError::Conflict(
                "pending settlement receipt contains invalid usage".into(),
            ));
        }

        if let Some(explicit_session_id) = input.routing_session_id {
            super::session_routing::upsert_session_routing_terminal_from_request_in_transaction(
                &mut transaction,
                &request_id,
                created_at,
                explicit_session_id,
                status_code,
                error_code.as_deref(),
                input.routing_terminal_observed_at.unwrap_or(now),
            )
            .await?;
        }

        // Settlement only enqueues the conversation task. The worker reads
        // 原始请求 JSON and 原始返回 JSON from the object locators already on
        // the request and builds 会话正文 in CNPG. A failure to build 会话正文
        // records session_dialogue_unavailable and leaves settlement running.
        BudgetHold::set_phase(&mut hold, "conversation_enqueue");
        if let Some(conversation) = input.conversation.as_ref() {
            if conversation.key.tenant_id != input.tenant_id
                || conversation.key.key_id != input.reservation.key_id
            {
                return Err(AppError::NotFound);
            }
            enqueue_conversation_task(&mut transaction, input.request_id, conversation, now)
                .await?;
        }
        BudgetHold::set_phase(&mut hold, "key_account_settlement");
        let reservation_status: String = reservation_row.try_get("status")?;
        let cost_micros = match reservation_status.as_str() {
            "reserved" => {
                if defer_projection {
                    super::settlement::settle_terminal_usage_in_transaction(
                        &mut transaction,
                        &trusted_reservation,
                        &usage,
                        trusted_metered.as_ref().map(|reservation| {
                            (
                                reservation,
                                metered.map(|input| input.billed_units).unwrap_or_default(),
                            )
                        }),
                        now,
                    )
                    .await?
                } else if let Some(trusted_metered) = trusted_metered.as_ref() {
                    settle_metered_usage_in_transaction(
                        &mut transaction,
                        trusted_metered,
                        metered.map(|input| input.billed_units).unwrap_or_default(),
                        now,
                    )
                    .await?
                } else {
                    settle_token_usage_in_transaction(
                        &mut transaction,
                        &trusted_reservation,
                        &usage,
                        now,
                    )
                    .await?
                }
            }
            // A split settlement acquires its feed sequence only when this
            // transaction publishes the complete terminal snapshot.
            "settled" => reservation_row
                .try_get::<Option<i64>, _>("actual_micros")?
                .ok_or(AppError::internal("lifecycle_site_7"))?,
            _ => {
                return Err(AppError::Conflict(
                    "request reservation is not finishable".into(),
                ));
            }
        };
        let usage_basis = if usage_invalid {
            Some(crate::model::RequestUsageBasis::NotObserved)
        } else {
            input.usage_basis
        };
        BudgetHold::set_phase(&mut hold, "terminal_facts_and_event");
        let finished = record_request_finished_with_basis_and_metering_in_transaction(
            &mut transaction,
            &FinishRequest {
                first_output_ms: input.first_output_ms,
                generation_duration_ms: input.generation_duration_ms,
                request_id: input.request_id,
                status_code,
                duration_ms: input.duration_ms.max(0),
                input_tokens: usage.total_input_tokens(),
                cached_input_tokens: usage.cached_input_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                output_tokens: usage.output_tokens,
                service_tier: usage.service_tier.clone(),
                cost_micros,
                error_code,
                response_object,
            },
            now,
            (!defer_projection).then_some(
                trusted_reservation
                    .enforcement_mode
                    .enforces_prepaid_limits(),
            ),
            usage_basis,
            trusted_metered
                .as_ref()
                .map(|reservation| MeteredRequestUsage {
                    billed_units: metered.map(|input| input.billed_units).unwrap_or_default(),
                    billing_unit: &reservation.billing_unit,
                }),
            TerminalMetadata {
                cause: input.terminal_cause,
                cache_coverage: usage.cache_coverage.filter(|coverage| {
                    usage_basis == Some(crate::model::RequestUsageBasis::ProviderReported)
                        && coverage.read_tokens == usage.cached_input_tokens
                        && coverage
                            .inclusive_input_tokens
                            .is_none_or(|input| input == usage.total_input_tokens())
                        && trusted_metered.is_none()
                }),
            },
        )
        .await?;
        if !finished {
            return Err(AppError::Conflict(
                "request terminal ownership changed".into(),
            ));
        }
        if let Some(receipt) = pending_receipt {
            super::pending_settlement::acknowledge(
                &mut transaction,
                receipt,
                &input,
                cost_micros,
                now,
            )
            .await?;
        }
        BudgetHold::commit_optional(transaction, hold).await?;
        if let Some(reservation) = budget_reservation.as_ref() {
            reservation.release().await;
        }
        Ok(FinishProxyRequestResult::Finished {
            cost_micros,
            usage_invalid,
        })
    }

    pub async fn record_request_started(&self, request: NewRequest) -> Result<(), AppError> {
        let now = unix_millis();
        let mut transaction = self.begin_write_transaction().await?;
        let request_id = request.request_id.to_string();
        let tenant_id = request.tenant_id.to_string();
        let key_id = request.key_id.to_string();
        let reservation_id = request.reservation_id.to_string();
        let upstream_account_id = request.upstream_account_id.map(|id| id.to_string());
        let model_route_id = request.model_route_id.map(|id| id.to_string());
        let claimed =
            claim_request_record_locator(&mut transaction, &request_id, now, &tenant_id, &key_id)
                .await?;
        if !claimed {
            let existing = sqlx::query(
                "SELECT tenant_id, key_id, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id FROM request_records WHERE id = $1 AND created_at = $2",
            )
            .bind(&request_id)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
            let Some(existing) = existing else {
                return Err(AppError::BadRequest(
                    "request locator exists without its request record".into(),
                ));
            };
            let replay_matches = existing.try_get::<String, _>("tenant_id")? == tenant_id
                && existing.try_get::<String, _>("key_id")? == key_id
                && existing.try_get::<String, _>("protocol")? == request.protocol
                && existing.try_get::<String, _>("model")? == request.model
                && existing.try_get::<String, _>("request_object")? == request.request_object
                && existing.try_get::<String, _>("reservation_id")? == reservation_id
                && existing.try_get::<Option<String>, _>("upstream_account_id")?
                    == upstream_account_id
                && existing.try_get::<Option<String>, _>("model_route_id")? == model_route_id;
            if !replay_matches {
                return Err(AppError::BadRequest(
                    "request id replay does not match the existing request".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, currency, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE((SELECT account.currency FROM usage_reservations reservation JOIN credit_accounts account ON account.id = reservation.account_id WHERE reservation.id = $8 AND reservation.key_id = $3), (SELECT currency FROM key_records WHERE id = $3), ''), 0, 0, 0)",
        )
        .bind(&request_id)
        .bind(&tenant_id)
        .bind(&key_id)
        .bind(now)
        .bind(&request.protocol)
        .bind(&request.model)
        .bind(&request.request_object)
        .bind(&reservation_id)
        .bind(&upstream_account_id)
        .bind(&model_route_id)
        .execute(&mut *transaction)
        .await?;
        let event =
            allocate_request_event_cursor(&mut transaction, now, &tenant_id, &key_id, &request_id)
                .await?;
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', $6, $7, 0, 0, 0)",
        )
        .bind(&event.event_id)
        .bind(&tenant_id)
        .bind(&key_id)
        .bind(&request_id)
        .bind(event.event_at)
        .bind(&request.protocol)
        .bind(&request.model)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn record_request_finished(&self, request: FinishRequest) -> Result<(), AppError> {
        let mut tx = self.begin_write_transaction().await?;
        sqlx::query("UPDATE request_records SET completed_at = completed_at WHERE id = $1")
            .bind(request.request_id.to_string())
            .execute(&mut *tx)
            .await?;
        if sqlx::query("SELECT 1 FROM pending_proxy_settlements WHERE request_id = $1")
            .bind(request.request_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .is_some()
        {
            return Err(AppError::Conflict(
                "pending settlement requires its authoritative receipt".into(),
            ));
        }
        let completed_at = unix_millis();
        record_request_finished_in_transaction(&mut tx, &request, completed_at, true).await?;
        tx.commit().await?;
        Ok(())
    }
}

pub(crate) async fn claim_request_record_locator(
    transaction: &mut Transaction<'_, Any>,
    id: &str,
    created_at: i64,
    tenant_id: &str,
    key_id: &str,
) -> Result<bool, AppError> {
    let claimed = sqlx::query(
        "INSERT INTO request_record_locators (id, created_at, tenant_id, key_id) VALUES ($1, $2, $3, $4) ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(created_at)
    .bind(tenant_id)
    .bind(key_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 1 {
        return Ok(true);
    }
    let existing = sqlx::query(
        "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut **transaction)
    .await?;
    let matches = existing.try_get::<i64, _>("created_at")? == created_at
        && existing.try_get::<String, _>("tenant_id")? == tenant_id
        && existing.try_get::<String, _>("key_id")? == key_id;
    if matches {
        Ok(false)
    } else {
        Err(AppError::BadRequest(
            "request id is already owned by a different request locator".into(),
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RequestEventCursor {
    pub event_id: String,
    pub event_at: i64,
}

const REQUEST_EVENT_CURSOR_LOCK_NAMESPACE: i32 = 0x4d54_4345;
const REQUEST_EVENT_CURSOR_LOCK_KEY: i32 = 1;
const UUID_V7_RANDOM_B_MASK: u64 = (1_u64 << 62) - 1;

pub(crate) async fn allocate_request_event_cursor(
    transaction: &mut Transaction<'_, Any>,
    now: i64,
    tenant_id: &str,
    key_id: &str,
    request_id: &str,
) -> Result<RequestEventCursor, AppError> {
    if transaction.as_mut().backend_name() == "PostgreSQL" {
        // One fixed namespace/key pair serializes the global SSE cursor until
        // the caller commits. It is deliberately independent of tenant data.
        sqlx::query("SELECT pg_advisory_xact_lock($1, $2)")
            .bind(REQUEST_EVENT_CURSOR_LOCK_NAMESPACE)
            .bind(REQUEST_EVENT_CURSOR_LOCK_KEY)
            .execute(&mut **transaction)
            .await?;
    }

    let last = sqlx::query(
        "SELECT event_at, event_id FROM request_events ORDER BY event_at DESC, event_id DESC LIMIT 1",
    )
    .fetch_optional(&mut **transaction)
    .await?;
    let cursor = match last {
        None => RequestEventCursor {
            event_id: fresh_v7_at(now)?,
            event_at: now,
        },
        Some(last) => {
            let last_at: i64 = last.try_get("event_at")?;
            let last_id: String = last.try_get("event_id")?;
            if now > last_at {
                RequestEventCursor {
                    event_id: fresh_v7_at(now)?,
                    event_at: now,
                }
            } else if let Some(event_id) = increment_v7_random_b(&last_id, last_at) {
                RequestEventCursor {
                    event_id,
                    event_at: last_at,
                }
            } else {
                let event_at = last_at.checked_add(1).ok_or(AppError::internal(
                    "lifecycle_allocate_request_event_cursor_1",
                ))?;
                RequestEventCursor {
                    event_id: fresh_v7_at(event_at)?,
                    event_at,
                }
            }
        }
    };

    let claimed = sqlx::query(
        "INSERT INTO request_event_locators (id, created_at, tenant_id, key_id, request_id) VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(&cursor.event_id)
    .bind(cursor.event_at)
    .bind(tenant_id)
    .bind(key_id)
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() != 1 {
        return Err(AppError::internal(
            "lifecycle_allocate_request_event_cursor_2",
        ));
    }
    Ok(cursor)
}

fn fresh_v7_at(event_at: i64) -> Result<String, AppError> {
    let millis =
        u64::try_from(event_at).map_err(|_| AppError::internal("lifecycle_fresh_v7_at_1"))?;
    if millis > 0x0000_ffff_ffff_ffff {
        return Err(AppError::internal("lifecycle_fresh_v7_at_1"));
    }
    let timestamp = millis.to_be_bytes();
    let mut bytes = *Uuid::now_v7().as_bytes();
    bytes[..6].copy_from_slice(&timestamp[2..]);
    bytes[6] = (bytes[6] & 0x0f) | 0x70;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Uuid::from_bytes(bytes).to_string())
}

fn increment_v7_random_b(last_id: &str, last_at: i64) -> Option<String> {
    let last = Uuid::parse_str(last_id).ok()?;
    if last.to_string() != last_id
        || last.get_version_num() != 7
        || last.get_variant() != uuid::Variant::RFC4122
    {
        return None;
    }
    let mut bytes = *last.as_bytes();
    let encoded_millis = u64::from_be_bytes([
        0, 0, bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5],
    ]);
    if encoded_millis != u64::try_from(last_at).ok()? {
        return None;
    }
    let random_b = u64::from_be_bytes(bytes[8..].try_into().ok()?) & UUID_V7_RANDOM_B_MASK;
    let next = random_b.checked_add(1)?;
    if next > UUID_V7_RANDOM_B_MASK {
        return None;
    }
    bytes[8..].copy_from_slice(&(next | (0b10_u64 << 62)).to_be_bytes());
    Some(Uuid::from_bytes(bytes).to_string())
}

#[cfg(test)]
pub(crate) async fn claim_request_event_locator(
    transaction: &mut Transaction<'_, Any>,
    id: &str,
    created_at: i64,
    tenant_id: &str,
    key_id: &str,
    request_id: &str,
) -> Result<bool, AppError> {
    let claimed = sqlx::query(
        "INSERT INTO request_event_locators (id, created_at, tenant_id, key_id, request_id) VALUES ($1, $2, $3, $4, $5) ON CONFLICT(id) DO NOTHING",
    )
    .bind(id)
    .bind(created_at)
    .bind(tenant_id)
    .bind(key_id)
    .bind(request_id)
    .execute(&mut **transaction)
    .await?;
    if claimed.rows_affected() == 1 {
        return Ok(true);
    }
    let existing = sqlx::query(
        "SELECT created_at, tenant_id, key_id, request_id FROM request_event_locators WHERE id = $1",
    )
    .bind(id)
    .fetch_one(&mut **transaction)
    .await?;
    let matches = existing.try_get::<i64, _>("created_at")? == created_at
        && existing.try_get::<String, _>("tenant_id")? == tenant_id
        && existing.try_get::<String, _>("key_id")? == key_id
        && existing.try_get::<String, _>("request_id")? == request_id;
    if matches {
        Ok(false)
    } else {
        Err(AppError::BadRequest(
            "request event id is already owned by a different event locator".into(),
        ))
    }
}

pub(crate) async fn record_request_started_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request: &NewRequest,
    now: i64,
) -> Result<(), AppError> {
    insert_request_started_record_in_transaction(transaction, request, now).await?;
    insert_request_started_event_in_transaction(transaction, request, now).await
}

async fn insert_request_started_record_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request: &NewRequest,
    now: i64,
) -> Result<(), AppError> {
    let request_id = request.request_id.to_string();
    let tenant_id = request.tenant_id.to_string();
    let key_id = request.key_id.to_string();
    let reservation_id = request.reservation_id.to_string();
    let upstream_account_id = request.upstream_account_id.map(|id| id.to_string());
    let model_route_id = request.model_route_id.map(|id| id.to_string());
    let claimed =
        claim_request_record_locator(transaction, &request_id, now, &tenant_id, &key_id).await?;
    if !claimed {
        return Err(AppError::BadRequest(
            "request id is already owned by an existing request".into(),
        ));
    }
    sqlx::query(
        "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, currency, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE((SELECT account.currency FROM usage_reservations reservation JOIN credit_accounts account ON account.id = reservation.account_id WHERE reservation.id = $8 AND reservation.key_id = $3), (SELECT currency FROM key_records WHERE id = $3), ''), 0, 0, 0)",
    )
    .bind(&request_id)
    .bind(&tenant_id)
    .bind(&key_id)
    .bind(now)
    .bind(&request.protocol)
    .bind(&request.model)
    .bind(&request.request_object)
    .bind(&reservation_id)
    .bind(&upstream_account_id)
    .bind(&model_route_id)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn insert_request_started_event_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request: &NewRequest,
    now: i64,
) -> Result<(), AppError> {
    let request_id = request.request_id.to_string();
    let tenant_id = request.tenant_id.to_string();
    let key_id = request.key_id.to_string();
    let event =
        allocate_request_event_cursor(transaction, now, &tenant_id, &key_id, &request_id).await?;
    sqlx::query(
        "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) SELECT $1, $2, $3, $4, $5, 'started', $6, $7, 0, 0, 0 WHERE EXISTS (SELECT 1 FROM request_records WHERE id = $4 AND tenant_id = $2 AND key_id = $3 AND completed_at IS NULL)",
    )
    .bind(&event.event_id)
    .bind(&tenant_id)
    .bind(&key_id)
    .bind(&request_id)
    .bind(event.event_at)
    .bind(&request.protocol)
    .bind(&request.model)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) async fn record_request_finished_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request: &FinishRequest,
    completed_at: i64,
    project_aggregates: bool,
) -> Result<bool, AppError> {
    record_request_finished_with_basis_in_transaction(
        tx,
        request,
        completed_at,
        project_aggregates,
        None,
    )
    .await
}

pub(super) async fn record_early_terminal_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request: &FinishRequest,
    completed_at: i64,
) -> Result<bool, AppError> {
    record_request_finished_with_basis_and_metering_in_transaction(
        transaction,
        request,
        completed_at,
        Some(true),
        Some(crate::model::RequestUsageBasis::NotObserved),
        None,
        TerminalMetadata {
            cause: Some(crate::model::RequestTerminalCause::GatewayPreAdmission),
            cache_coverage: None,
        },
    )
    .await
}

async fn record_request_finished_with_basis_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    request: &FinishRequest,
    completed_at: i64,
    project_aggregates: bool,
    usage_basis: Option<crate::model::RequestUsageBasis>,
) -> Result<bool, AppError> {
    record_request_finished_with_basis_and_metering_in_transaction(
        tx,
        request,
        completed_at,
        Some(project_aggregates),
        usage_basis,
        None,
        TerminalMetadata::default(),
    )
    .await
}

async fn record_request_finished_with_basis_and_metering_in_transaction(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    request: &FinishRequest,
    completed_at: i64,
    project_aggregates: Option<bool>,
    usage_basis: Option<crate::model::RequestUsageBasis>,
    metered_usage: Option<MeteredRequestUsage<'_>>,
    metadata: TerminalMetadata,
) -> Result<bool, AppError> {
    let TerminalMetadata {
        cause: terminal_cause,
        cache_coverage,
    } = metadata;
    if project_aggregates.is_some() {
        lock_request_stats_projection_writer_in_transaction(tx).await?;
    }
    let request_id = request.request_id.to_string();
    let locator = sqlx::query(
        "SELECT created_at, tenant_id, key_id FROM request_record_locators WHERE id = $1",
    )
    .bind(&request_id)
    .fetch_optional(&mut **tx)
    .await?;
    let Some(locator) = locator else {
        return Ok(false);
    };
    let created_at: i64 = locator.try_get("created_at")?;
    let tenant_id: String = locator.try_get("tenant_id")?;
    let key_id: String = locator.try_get("key_id")?;
    let billed_units = metered_usage
        .as_ref()
        .map(|usage| usage.billed_units)
        .unwrap_or_default();
    let billing_unit = metered_usage
        .as_ref()
        .map(|usage| usage.billing_unit)
        .unwrap_or("");
    let updated = sqlx::query(
        "UPDATE request_records SET status_code = $1, duration_ms = $2, input_tokens = $3, cached_input_tokens = $4, cache_write_tokens = $5, output_tokens = $6, service_tier = $7, cost_micros = $8, error_code = $9, response_object = $10, completed_at = $11, first_output_ms = $14, generation_duration_ms = $15, usage_basis = $16, billed_units = $17, billing_unit = $18, terminal_cause_code = $19, cache_known_read_tokens = $20, cache_known_input_tokens = $21 WHERE id = $12 AND created_at = $13 AND completed_at IS NULL",
    )
    .bind(request.status_code)
    .bind(request.duration_ms)
    .bind(request.input_tokens)
    .bind(request.cached_input_tokens)
    .bind(request.cache_write_tokens)
    .bind(request.output_tokens)
    .bind(request.service_tier.as_deref().unwrap_or("default"))
    .bind(request.cost_micros)
    .bind(&request.error_code)
    .bind(&request.response_object)
    .bind(completed_at)
    .bind(&request_id)
    .bind(created_at)
    .bind(request.first_output_ms)
    .bind(request.generation_duration_ms)
    .bind(usage_basis.map(crate::model::RequestUsageBasis::as_str))
    .bind(billed_units)
    .bind(billing_unit)
    .bind(terminal_cause.map(crate::model::RequestTerminalCause::as_str))
    .bind(cache_coverage.map(|value| value.read_tokens))
    .bind(cache_coverage.and_then(|value| value.inclusive_input_tokens))
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    super::super::billing::publish_text_settlement_in_transaction(tx, request.request_id).await?;
    let Some(project_aggregates) = project_aggregates else {
        super::terminal_projection::enqueue_terminal_projection_in_transaction(tx, &request_id)
            .await?;
        return Ok(true);
    };
    if project_aggregates {
        sqlx::query(
            "INSERT INTO usage_daily_aggregates (key_id, day_bucket, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros) SELECT key_id, created_at / 86400000, model, CASE WHEN status_code >= 200 AND status_code < 400 AND COALESCE(error_code, '') = '' THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), 1, CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE input_tokens END, CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE output_tokens END, CASE WHEN ((status_code < 200 OR status_code >= 400) OR COALESCE(error_code, '') <> '') AND COALESCE(usage_basis, '') <> 'provider_reported' THEN 0 ELSE cost_micros END FROM request_records WHERE id = $1 AND created_at = $2 ON CONFLICT(key_id, day_bucket, model, status_class, error_code) DO UPDATE SET requests = usage_daily_aggregates.requests + 1, input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens, cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros",
        )
        .bind(&request_id)
        .bind(created_at)
        .execute(&mut **tx)
        .await?;
    }
    let fact_inserted = sqlx::query(
        "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, generation_units, billing_unit, service_tier, currency, cost_micros, session_id) SELECT id, tenant_id, key_id, created_at, model, protocol, CASE WHEN status_code BETWEEN 200 AND 399 AND COALESCE(error_code, '') = '' THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), COALESCE(upstream_account_id, ''), COALESCE(model_route_id, ''), COALESCE(duration_ms, 0), input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, billed_units, billing_unit, service_tier, currency, CASE WHEN ((status_code < 200 OR status_code >= 400) OR COALESCE(error_code, '') <> '') AND COALESCE(usage_basis, '') <> 'provider_reported' THEN 0 ELSE cost_micros END, COALESCE(conversation_cluster_id, 'unlinked:' || key_id) FROM request_records WHERE id = $1 AND created_at = $2 AND completed_at IS NOT NULL AND status_code IS NOT NULL ON CONFLICT(request_id) DO NOTHING",
    )
    .bind(&request_id)
    .bind(created_at)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if fact_inserted && project_aggregates {
        sqlx::query(
            "INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, cache_known_read_tokens, cache_known_input_tokens, cache_eligible_requests, cache_unknown_requests, cache_reported_read_tokens, cache_reported_requests, duration_count, duration_sum_ms, cost_micros) SELECT tenant_id, key_id, created_at / 86400000, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, 1, CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE input_tokens END, CASE WHEN protocol = 'audio-transcription' THEN 0 ELSE output_tokens END, cached_input_tokens, cache_write_tokens, CASE WHEN cache_known_read_tokens IS NOT NULL AND cache_known_input_tokens IS NOT NULL THEN cache_known_read_tokens ELSE 0 END, CASE WHEN cache_known_read_tokens IS NOT NULL AND cache_known_input_tokens IS NOT NULL THEN cache_known_input_tokens ELSE 0 END, CASE WHEN cache_known_read_tokens IS NOT NULL AND cache_known_input_tokens IS NOT NULL THEN 1 ELSE 0 END, CASE WHEN cache_known_read_tokens IS NOT NULL AND cache_known_input_tokens IS NOT NULL THEN 0 ELSE 1 END, COALESCE(cache_known_read_tokens, 0), CASE WHEN cache_known_read_tokens IS NOT NULL THEN 1 ELSE 0 END, 1, duration_ms, cost_micros FROM request_stats_facts WHERE request_id = $1 ON CONFLICT(tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency) DO UPDATE SET requests = request_daily_aggregates.requests + 1, input_tokens = request_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = request_daily_aggregates.output_tokens + excluded.output_tokens, cached_input_tokens = request_daily_aggregates.cached_input_tokens + excluded.cached_input_tokens, cache_write_tokens = request_daily_aggregates.cache_write_tokens + excluded.cache_write_tokens, cache_known_read_tokens = request_daily_aggregates.cache_known_read_tokens + excluded.cache_known_read_tokens, cache_known_input_tokens = request_daily_aggregates.cache_known_input_tokens + excluded.cache_known_input_tokens, cache_eligible_requests = request_daily_aggregates.cache_eligible_requests + excluded.cache_eligible_requests, cache_unknown_requests = COALESCE(request_daily_aggregates.cache_unknown_requests, request_daily_aggregates.requests) + excluded.cache_unknown_requests, cache_reported_read_tokens = request_daily_aggregates.cache_reported_read_tokens + excluded.cache_reported_read_tokens, cache_reported_requests = request_daily_aggregates.cache_reported_requests + excluded.cache_reported_requests, duration_count = request_daily_aggregates.duration_count + excluded.duration_count, duration_sum_ms = request_daily_aggregates.duration_sum_ms + excluded.duration_sum_ms, cost_micros = request_daily_aggregates.cost_micros + excluded.cost_micros",
        )
        .bind(&request_id)
        .execute(&mut **tx)
        .await?;
        add_request_fact_to_session_projection_in_transaction(tx, &request_id).await?;
        sqlx::query(
            r#"INSERT INTO usage_analysis_hourly (
                   tenant_id, key_id, hour_bucket, source_kind, model, protocol, status_class,
                   error_code, upstream_account_id, model_route_id, service_tier, currency,
                   requests, input_tokens, output_tokens, cached_input_tokens,
                   cache_write_tokens, generation_units, duration_count, duration_sum_ms,
                   duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
                   duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
                   duration_bucket_8, duration_bucket_9, duration_bucket_10,
                   duration_bucket_11, cost_micros)
               SELECT tenant_id, key_id, created_at / 3600000,
                      CASE WHEN protocol = 'audio-transcription' THEN 'generation' ELSE 'request' END,
                      model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                           WHEN protocol = 'audio-transcription' THEN 'audio-transcription'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      service_tier, currency, 1,
                      CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                           THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                      output_tokens, cached_input_tokens, cache_write_tokens, generation_units,
                      1, duration_ms,
                      CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END,
                      cost_micros
                 FROM request_stats_facts WHERE request_id = $1
               ON CONFLICT (tenant_id, key_id, hour_bucket, source_kind, model, protocol,
                            status_class, error_code, upstream_account_id, model_route_id,
                            service_tier, currency)
               DO UPDATE SET requests = usage_analysis_hourly.requests + excluded.requests,
                   input_tokens = usage_analysis_hourly.input_tokens + excluded.input_tokens,
                   output_tokens = usage_analysis_hourly.output_tokens + excluded.output_tokens,
                   cached_input_tokens = usage_analysis_hourly.cached_input_tokens + excluded.cached_input_tokens,
                   cache_write_tokens = usage_analysis_hourly.cache_write_tokens + excluded.cache_write_tokens,
                   generation_units = usage_analysis_hourly.generation_units + excluded.generation_units,
                   duration_count = usage_analysis_hourly.duration_count + excluded.duration_count,
                   duration_sum_ms = usage_analysis_hourly.duration_sum_ms + excluded.duration_sum_ms,
                   duration_bucket_0 = usage_analysis_hourly.duration_bucket_0 + excluded.duration_bucket_0,
                   duration_bucket_1 = usage_analysis_hourly.duration_bucket_1 + excluded.duration_bucket_1,
                   duration_bucket_2 = usage_analysis_hourly.duration_bucket_2 + excluded.duration_bucket_2,
                   duration_bucket_3 = usage_analysis_hourly.duration_bucket_3 + excluded.duration_bucket_3,
                   duration_bucket_4 = usage_analysis_hourly.duration_bucket_4 + excluded.duration_bucket_4,
                   duration_bucket_5 = usage_analysis_hourly.duration_bucket_5 + excluded.duration_bucket_5,
                   duration_bucket_6 = usage_analysis_hourly.duration_bucket_6 + excluded.duration_bucket_6,
                   duration_bucket_7 = usage_analysis_hourly.duration_bucket_7 + excluded.duration_bucket_7,
                   duration_bucket_8 = usage_analysis_hourly.duration_bucket_8 + excluded.duration_bucket_8,
                   duration_bucket_9 = usage_analysis_hourly.duration_bucket_9 + excluded.duration_bucket_9,
                   duration_bucket_10 = usage_analysis_hourly.duration_bucket_10 + excluded.duration_bucket_10,
                   duration_bucket_11 = usage_analysis_hourly.duration_bucket_11 + excluded.duration_bucket_11,
                   cost_micros = usage_analysis_hourly.cost_micros + excluded.cost_micros"#,
        )
        .bind(&request_id)
        .execute(&mut **tx)
        .await?;
        sqlx::query(
            r#"INSERT INTO usage_analysis_daily (
                   tenant_id, key_id, day_bucket, source_kind, model, protocol, status_class,
                   error_code, upstream_account_id, model_route_id, service_tier, currency,
                   requests, input_tokens, output_tokens, cached_input_tokens,
                   cache_write_tokens, generation_units, duration_count, duration_sum_ms,
                   duration_bucket_0, duration_bucket_1, duration_bucket_2, duration_bucket_3,
                   duration_bucket_4, duration_bucket_5, duration_bucket_6, duration_bucket_7,
                   duration_bucket_8, duration_bucket_9, duration_bucket_10,
                   duration_bucket_11, cost_micros)
               SELECT tenant_id, key_id, created_at / 86400000,
                      CASE WHEN protocol = 'audio-transcription' THEN 'generation' ELSE 'request' END,
                      model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                           WHEN protocol = 'audio-transcription' THEN 'audio-transcription'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      service_tier, currency, 1,
                      CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                           THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                      output_tokens, cached_input_tokens, cache_write_tokens, generation_units,
                      1, duration_ms,
                      CASE WHEN duration_ms <= 10 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 10 AND duration_ms <= 50 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 50 AND duration_ms <= 100 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 100 AND duration_ms <= 250 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 250 AND duration_ms <= 500 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 500 AND duration_ms <= 1000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 1000 AND duration_ms <= 2500 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 2500 AND duration_ms <= 5000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 5000 AND duration_ms <= 10000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 10000 AND duration_ms <= 30000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 30000 AND duration_ms <= 60000 THEN 1 ELSE 0 END,
                      CASE WHEN duration_ms > 60000 THEN 1 ELSE 0 END,
                      cost_micros
                 FROM request_stats_facts WHERE request_id = $1
               ON CONFLICT (tenant_id, key_id, day_bucket, source_kind, model, protocol,
                            status_class, error_code, upstream_account_id, model_route_id,
                            service_tier, currency)
               DO UPDATE SET requests = usage_analysis_daily.requests + excluded.requests,
                   input_tokens = usage_analysis_daily.input_tokens + excluded.input_tokens,
                   output_tokens = usage_analysis_daily.output_tokens + excluded.output_tokens,
                   cached_input_tokens = usage_analysis_daily.cached_input_tokens + excluded.cached_input_tokens,
                   cache_write_tokens = usage_analysis_daily.cache_write_tokens + excluded.cache_write_tokens,
                   generation_units = usage_analysis_daily.generation_units + excluded.generation_units,
                   duration_count = usage_analysis_daily.duration_count + excluded.duration_count,
                   duration_sum_ms = usage_analysis_daily.duration_sum_ms + excluded.duration_sum_ms,
                   duration_bucket_0 = usage_analysis_daily.duration_bucket_0 + excluded.duration_bucket_0,
                   duration_bucket_1 = usage_analysis_daily.duration_bucket_1 + excluded.duration_bucket_1,
                   duration_bucket_2 = usage_analysis_daily.duration_bucket_2 + excluded.duration_bucket_2,
                   duration_bucket_3 = usage_analysis_daily.duration_bucket_3 + excluded.duration_bucket_3,
                   duration_bucket_4 = usage_analysis_daily.duration_bucket_4 + excluded.duration_bucket_4,
                   duration_bucket_5 = usage_analysis_daily.duration_bucket_5 + excluded.duration_bucket_5,
                   duration_bucket_6 = usage_analysis_daily.duration_bucket_6 + excluded.duration_bucket_6,
                   duration_bucket_7 = usage_analysis_daily.duration_bucket_7 + excluded.duration_bucket_7,
                   duration_bucket_8 = usage_analysis_daily.duration_bucket_8 + excluded.duration_bucket_8,
                   duration_bucket_9 = usage_analysis_daily.duration_bucket_9 + excluded.duration_bucket_9,
                   duration_bucket_10 = usage_analysis_daily.duration_bucket_10 + excluded.duration_bucket_10,
                   duration_bucket_11 = usage_analysis_daily.duration_bucket_11 + excluded.duration_bucket_11,
                   cost_micros = usage_analysis_daily.cost_micros + excluded.cost_micros"#,
        )
        .bind(&request_id)
        .execute(&mut **tx)
        .await?;
        for (table, bucket_column, divisor) in [
            (
                "generation_usage_dimensions_hourly",
                "hour_bucket",
                3_600_000_i64,
            ),
            (
                "generation_usage_dimensions_daily",
                "day_bucket",
                86_400_000_i64,
            ),
        ] {
            let statement = format!(
                r#"INSERT INTO {table} (
                       tenant_id, key_id, {bucket_column}, model, status_class, error_code,
                       upstream_account_id, model_route_id, modality, billing_unit,
                       currency, units)
                   SELECT tenant_id, key_id, created_at / {divisor}, model, status_class,
                          error_code, upstream_account_id, model_route_id, 'audio', billing_unit,
                          currency, generation_units
                     FROM request_stats_facts
                    WHERE request_id = $1 AND protocol = 'audio-transcription'
                   ON CONFLICT (
                       tenant_id, key_id, {bucket_column}, model, status_class, error_code,
                       upstream_account_id, model_route_id, modality, billing_unit, currency)
                   DO UPDATE SET units = {table}.units + excluded.units"#,
            );
            sqlx::query(sqlx::AssertSqlSafe(statement))
                .bind(&request_id)
                .execute(&mut **tx)
                .await?;
        }
    }
    let event =
        allocate_request_event_cursor(tx, completed_at, &tenant_id, &key_id, &request_id).await?;
    let inserted = sqlx::query(
        "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $3 AND created_at = $4",
    )
    .bind(&event.event_id)
    .bind(event.event_at)
    .bind(&request_id)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() != 1 {
        return Err(AppError::internal("lifecycle_site_8"));
    }
    Ok(true)
}
