use super::super::archive_staging::bind_archive_staging_attempt_in_transaction;
use super::super::*;
use crate::archive_staging::{
    ArchiveStagingOwner, ArchiveStagingPurpose, ArchiveStagingWriteLease,
};

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
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub reservation: &'a UsageReservation,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub requested_service_tier: Option<&'a str>,
    pub status_code: i64,
    pub duration_ms: i64,
    pub usage: TokenUsage,
    pub charge_contract_ceiling: bool,
    pub error_code: Option<&'a str>,
    pub response_object: &'a str,
    pub conversation: Option<ProxyConversationInput<'a>>,
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
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let reservation = reserve_usage_in_transaction(
            &mut transaction,
            input.key,
            input.price,
            input.input_token_ceiling,
            input.output_token_ceiling,
            now,
        )
        .await?;
        if let Err(error) = record_request_started_in_transaction(
            &mut transaction,
            &NewRequest {
                request_id: input.request_id,
                key_id: input.key.key_id,
                tenant_id: input.key.tenant_id,
                protocol: input.protocol.to_owned(),
                model: input.model.to_owned(),
                request_object: input.request_object.to_owned(),
                reservation_id: reservation.id,
                upstream_account_id: input.upstream_account_id,
                model_route_id: input.model_route_id,
            },
            now,
        )
        .await
        {
            transaction.rollback().await?;
            return Err(error);
        }
        transaction.commit().await?;
        Ok(reservation)
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
        let mut transaction = self.pool.begin().await?;
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
        let mut transaction = self.pool.begin().await?;
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

    pub async fn finish_proxy_request(
        &self,
        input: FinishProxyRequest<'_>,
    ) -> Result<FinishProxyRequestResult, AppError> {
        self.finish_proxy_request_with_archive_staging(input, None)
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
        if let Some(lease) = response_archive_lease
            && (lease.key.owner != ArchiveStagingOwner::ProxyRequest(input.request_id)
                || lease.key.purpose != ArchiveStagingPurpose::Response)
        {
            return Err(AppError::BadRequest(
                "proxy response archive lease does not match its request owner".into(),
            ));
        }
        let now = unix_millis();
        let request_id = input.request_id.to_string();
        let tenant_id = input.tenant_id.to_string();
        let key_id = input.reservation.key_id.to_string();
        let reservation_id = input.reservation.id.to_string();
        let mut transaction = self.pool.begin().await?;

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
                    .ok_or(AppError::Internal)?,
                cost_micros: existing.try_get("cost_micros")?,
                error_code: existing.try_get("error_code")?,
                response_object: existing
                    .try_get::<Option<String>, _>("response_object")?
                    .ok_or(AppError::Internal)?,
            };
            transaction.commit().await?;
            return Ok(result);
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
            transaction.rollback().await?;
            return Err(AppError::Conflict(
                "proxy response archive writer was fenced".into(),
            ));
        }

        let reservation_row = sqlx::query(
            "SELECT account_id, key_id, reserved_micros, reserved_tokens, rate_window_start, status, actual_micros, price_snapshot_json FROM usage_reservations WHERE id = $1",
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
        let price_snapshot_json: Option<String> = reservation_row.try_get("price_snapshot_json")?;
        let (input_micros_per_million, output_micros_per_million, price_tiers) =
            if let Some(snapshot) = price_snapshot_json {
                let price: ModelPrice =
                    serde_json::from_str(&snapshot).map_err(|_| AppError::Internal)?;
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
            reserved_micros: reservation_row.try_get("reserved_micros")?,
            input_micros_per_million,
            output_micros_per_million,
            price_tiers,
            rate_window_start: reservation_row.try_get("rate_window_start")?,
            reserved_tokens,
        };

        let (usage, status_code, error_code, response_object, usage_invalid) =
            match normalize_proxy_usage(
                &input.usage,
                input.input_token_ceiling,
                input.output_token_ceiling,
                input.requested_service_tier,
            ) {
                Ok(usage) => (
                    usage,
                    input.status_code,
                    input.error_code.map(str::to_owned),
                    input.response_object.to_owned(),
                    false,
                ),
                Err(AppError::Upstream(_)) => (
                    TokenUsage::default(),
                    502,
                    Some("upstream_invalid_usage".to_owned()),
                    "{\"error\":{\"message\":\"upstream returned invalid usage\",\"type\":\"upstream_error\"}}".to_owned(),
                    true,
                ),
                Err(error) => return Err(error),
            };

        let reservation_status: String = reservation_row.try_get("status")?;
        let cost_micros = match reservation_status.as_str() {
            "reserved" => {
                settle_token_usage_in_transaction_with_charge(
                    &mut transaction,
                    &trusted_reservation,
                    &usage,
                    now,
                    input
                        .charge_contract_ceiling
                        .then(|| {
                            proxy_contract_ceiling_micros(
                                &trusted_reservation,
                                input.input_token_ceiling,
                                input.output_token_ceiling,
                                input.requested_service_tier,
                            )
                        })
                        .transpose()?,
                )
                .await?
            }
            // Repair compatibility for a request left pending by the old
            // split settle/finish implementation. Never settle it twice.
            "settled" => reservation_row
                .try_get::<Option<i64>, _>("actual_micros")?
                .ok_or(AppError::Internal)?,
            _ => {
                return Err(AppError::Conflict(
                    "request reservation is not finishable".into(),
                ));
            }
        };
        // Attach the proven logical conversation before materializing session rollups.
        // This keeps the request in its inferred cluster instead of first accounting it
        // under the explicit `unlinked:<stable-key-id>` sentinel.
        if let Some(conversation) = input.conversation {
            if conversation.key.tenant_id != input.tenant_id
                || conversation.key.key_id != input.reservation.key_id
            {
                return Err(AppError::NotFound);
            }
            self.record_conversation_observation_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key: conversation.key,
                    request_id: input.request_id,
                    request_json: conversation.request_json,
                    hints: conversation.hints,
                    client_name: conversation.client_name,
                    observed_at: now,
                    attach_request_record: true,
                },
            )
            .await?;
            if let Some(response_id) = conversation.upstream_response_id {
                attach_conversation_upstream_response_in_transaction(
                    &mut transaction,
                    input.request_id,
                    response_id,
                )
                .await?;
            }
        }
        let finished = record_request_finished_in_transaction(
            &mut transaction,
            &FinishRequest {
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
        )
        .await?;
        if !finished {
            return Err(AppError::Conflict(
                "request terminal ownership changed".into(),
            ));
        }
        transaction.commit().await?;
        Ok(FinishProxyRequestResult::Finished {
            cost_micros,
            usage_invalid,
        })
    }

    pub async fn record_request_started(&self, request: NewRequest) -> Result<(), AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
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
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, currency, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE((SELECT currency FROM key_records WHERE id = $3), ''), 0, 0, 0)",
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
        let event_id = Uuid::now_v7().to_string();
        if claim_request_event_locator(
            &mut transaction,
            &event_id,
            now,
            &tenant_id,
            &key_id,
            &request_id,
        )
        .await?
        {
            sqlx::query(
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', $6, $7, 0, 0, 0)",
            )
            .bind(&event_id)
            .bind(&tenant_id)
            .bind(&key_id)
            .bind(&request_id)
            .bind(now)
            .bind(&request.protocol)
            .bind(&request.model)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn record_request_finished(&self, request: FinishRequest) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let completed_at = unix_millis();
        record_request_finished_in_transaction(&mut tx, &request, completed_at).await?;
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
        "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, currency, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, COALESCE((SELECT currency FROM key_records WHERE id = $3), ''), 0, 0, 0)",
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
    let event_id = Uuid::now_v7().to_string();
    if claim_request_event_locator(
        transaction,
        &event_id,
        now,
        &tenant_id,
        &key_id,
        &request_id,
    )
    .await?
    {
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', $6, $7, 0, 0, 0)",
        )
        .bind(&event_id)
        .bind(&tenant_id)
        .bind(&key_id)
        .bind(&request_id)
        .bind(now)
        .bind(&request.protocol)
        .bind(&request.model)
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

pub(crate) async fn record_request_finished_in_transaction(
    tx: &mut Transaction<'_, Any>,
    request: &FinishRequest,
    completed_at: i64,
) -> Result<bool, AppError> {
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
    let updated = sqlx::query(
        "UPDATE request_records SET status_code = $1, duration_ms = $2, input_tokens = $3, cached_input_tokens = $4, cache_write_tokens = $5, output_tokens = $6, service_tier = $7, cost_micros = $8, error_code = $9, response_object = $10, completed_at = $11 WHERE id = $12 AND created_at = $13 AND completed_at IS NULL",
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
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() == 0 {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO usage_daily_aggregates (key_id, day_bucket, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros) SELECT key_id, created_at / 86400000, model, CASE WHEN status_code >= 200 AND status_code < 400 THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), 1, input_tokens, output_tokens, cost_micros FROM request_records WHERE id = $1 AND created_at = $2 ON CONFLICT(key_id, day_bucket, model, status_class, error_code) DO UPDATE SET requests = usage_daily_aggregates.requests + 1, input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens, cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros",
    )
    .bind(&request_id)
    .bind(created_at)
    .execute(&mut **tx)
    .await?;
    let fact_inserted = sqlx::query(
        "INSERT INTO request_stats_facts (request_id, tenant_id, key_id, created_at, model, protocol, status_class, error_code, upstream_account_id, model_route_id, duration_ms, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, service_tier, currency, cost_micros, session_id) SELECT id, tenant_id, key_id, created_at, model, protocol, CASE WHEN status_code BETWEEN 200 AND 399 THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), COALESCE(upstream_account_id, ''), COALESCE(model_route_id, ''), COALESCE(duration_ms, 0), input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, service_tier, currency, cost_micros, COALESCE(conversation_cluster_id, 'unlinked:' || key_id) FROM request_records WHERE id = $1 AND created_at = $2 AND completed_at IS NOT NULL AND status_code IS NOT NULL ON CONFLICT(request_id) DO NOTHING",
    )
    .bind(&request_id)
    .bind(created_at)
    .execute(&mut **tx)
    .await?
    .rows_affected()
        == 1;
    if fact_inserted {
        sqlx::query(
            "INSERT INTO request_daily_aggregates (tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, requests, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, duration_count, duration_sum_ms, cost_micros) SELECT tenant_id, key_id, created_at / 86400000, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency, 1, input_tokens, output_tokens, cached_input_tokens, cache_write_tokens, 1, duration_ms, cost_micros FROM request_stats_facts WHERE request_id = $1 ON CONFLICT(tenant_id, key_id, day_bucket, model, protocol, status_class, error_code, upstream_account_id, model_route_id, service_tier, currency) DO UPDATE SET requests = request_daily_aggregates.requests + 1, input_tokens = request_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = request_daily_aggregates.output_tokens + excluded.output_tokens, cached_input_tokens = request_daily_aggregates.cached_input_tokens + excluded.cached_input_tokens, cache_write_tokens = request_daily_aggregates.cache_write_tokens + excluded.cache_write_tokens, duration_count = request_daily_aggregates.duration_count + excluded.duration_count, duration_sum_ms = request_daily_aggregates.duration_sum_ms + excluded.duration_sum_ms, cost_micros = request_daily_aggregates.cost_micros + excluded.cost_micros",
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
               SELECT tenant_id, key_id, created_at / 3600000, 'request', model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      service_tier, currency, 1,
                      CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                           THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                      output_tokens, cached_input_tokens, cache_write_tokens, 0, 1, duration_ms,
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
               SELECT tenant_id, key_id, created_at / 86400000, 'request', model,
                      CASE WHEN protocol = 'anthropic' OR protocol LIKE 'anthropic-%'
                           THEN 'anthropic' WHEN protocol = 'openai-image' THEN 'openai-image'
                           ELSE 'openai' END,
                      status_class, error_code, upstream_account_id, model_route_id,
                      service_tier, currency, 1,
                      CASE WHEN input_tokens >= cached_input_tokens + cache_write_tokens
                           THEN input_tokens - cached_input_tokens - cache_write_tokens ELSE 0 END,
                      output_tokens, cached_input_tokens, cache_write_tokens, 0, 1, duration_ms,
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
    }
    let event_id = Uuid::now_v7().to_string();
    if claim_request_event_locator(
        tx,
        &event_id,
        completed_at,
        &tenant_id,
        &key_id,
        &request_id,
    )
    .await?
    {
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $3 AND created_at = $4",
        )
        .bind(&event_id)
        .bind(completed_at)
        .bind(&request_id)
        .bind(created_at)
        .execute(&mut **tx)
        .await?;
    }
    Ok(true)
}
