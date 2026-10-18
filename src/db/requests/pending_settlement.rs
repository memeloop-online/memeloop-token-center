//! Recovery authority for an already admitted request. Registration happens
//! before dispatch. Delivery does not wait for the terminal receipt's DB ACK.
//! An absent journal ACK is unknown evidence, never permission to charge zero.
use super::super::*;

const RESERVATION_CONTRACT_SQL: &str = "SELECT r.account_id,r.enforcement_mode,r.reserved_micros,r.reserved_tokens,r.reserved_units,r.price_snapshot_json,q.upstream_account_id,q.model_route_id,q.upstream_model FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE r.id = $1 AND r.key_id = $2 AND q.id = $3 AND r.status = 'reserved'";

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingProxySettlementClaim {
    pub request_id: Uuid,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub account_id: Uuid,
    pub reservation_id: Uuid,
    pub owner_token: Uuid,
    pub fence: Uuid,
    pub contract_digest: String,
    pub input_token_ceiling: i64,
    pub output_token_ceiling: i64,
    pub requested_service_tier: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PendingProxySettlementEvidence {
    ProviderReported,
    /// Explicit upstream rejection/non-execution evidence, not a timeout,
    /// cancellation, missing journal, expired lease or process crash.
    ConfirmedNotExecuted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PendingProxySettlementAck {
    pub request_id: Uuid,
    pub reservation_id: Uuid,
    pub fence: Uuid,
    pub receipt_id: Uuid,
    pub receipt_digest: String,
    pub cost_micros: i64,
}

pub(super) struct Receipt<'a> {
    pub claim: &'a PendingProxySettlementClaim,
    pub id: Uuid,
    pub digest: String,
}

fn conflict() -> AppError {
    AppError::Conflict("pending settlement authority or receipt mismatch".into())
}

fn digest(value: &Value) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}

fn contract(
    owner: &ProxyRequestOwner,
    reservation: &sqlx::any::AnyRow,
    input_ceiling: i64,
    output_ceiling: i64,
    tier: &str,
) -> Result<Value, AppError> {
    if input_ceiling.checked_add(output_ceiling)
        != Some(reservation.try_get::<i64, _>("reserved_tokens")?)
        || reservation.try_get::<i64, _>("reserved_units")? != 0
    {
        return Err(conflict());
    }
    // Legacy NULL prices are supported by the legacy finalizer's caller-price
    // fallback, but cannot establish an authoritative immutable receipt claim.
    let price_snapshot: String = reservation
        .try_get::<Option<String>, _>("price_snapshot_json")?
        .ok_or_else(conflict)?;
    serde_json::from_str::<ModelPrice>(&price_snapshot).map_err(|_| conflict())?;
    Ok(serde_json::json!({
        "version": 1, "request_id": owner.request_id, "tenant_id": owner.tenant_id,
        "key_id": owner.key_id,
        "account_id": parse_uuid(reservation.try_get("account_id")?)?,
        "reservation_id": owner.reservation_id,
        "mode": reservation.try_get::<String, _>("enforcement_mode")?,
        "reserved_micros": reservation.try_get::<i64, _>("reserved_micros")?,
        "price_snapshot": price_snapshot,
        "upstream_account_id": reservation.try_get::<Option<String>, _>("upstream_account_id")?,
        "model_route_id": reservation.try_get::<Option<String>, _>("model_route_id")?,
        "upstream_model": reservation.try_get::<Option<String>, _>("upstream_model")?,
        "input_ceiling": input_ceiling, "output_ceiling": output_ceiling, "tier": tier,
    }))
}

// API journal integration is a separate owner/PR. These entry points must land
// without requiring an output-path writer to edit the database implementation.
#[allow(dead_code)]
impl Database {
    pub(crate) async fn claim_pending_proxy_settlement(
        &self,
        owner: &ProxyRequestOwner,
        input_token_ceiling: i64,
        output_token_ceiling: i64,
        requested_service_tier: Option<&str>,
    ) -> Result<PendingProxySettlementClaim, AppError> {
        if input_token_ceiling < 0 || output_token_ceiling < 0 {
            return Err(conflict());
        }
        let tier = requested_service_tier.unwrap_or("default");
        validate_service_tier(tier)?;
        let mut tx = self.begin_write_transaction().await?;
        // Same first row as finalization and the orphan CAS: a stale orphan
        // selection must recheck our responsibility after acquiring this row.
        let owned = sqlx::query("UPDATE request_records SET proxy_owner_token = proxy_owner_token WHERE id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND proxy_owner_token = $5 AND completed_at IS NULL AND submission_started_at IS NULL RETURNING proxy_owner_expires_at")
            .bind(owner.request_id.to_string()).bind(owner.tenant_id.to_string())
            .bind(owner.key_id.to_string()).bind(owner.reservation_id.to_string())
            .bind(owner.token.to_string()).fetch_optional(&mut *tx).await?
            .ok_or_else(conflict)?;
        let now = self.request_owner_now(&mut tx).await?;
        if owned
            .try_get::<Option<i64>, _>("proxy_owner_expires_at")?
            .is_none_or(|expiry| expiry <= now)
        {
            return Err(conflict());
        }
        let reservation = sqlx::query(RESERVATION_CONTRACT_SQL)
            .bind(owner.reservation_id.to_string())
            .bind(owner.key_id.to_string())
            .bind(owner.request_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or_else(conflict)?;
        let account_id = parse_uuid(reservation.try_get("account_id")?)?;
        let contract = contract(
            owner,
            &reservation,
            input_token_ceiling,
            output_token_ceiling,
            tier,
        )?;
        let contract_digest = digest(&contract)?;
        let existing = sqlx::query("SELECT owner_token, fence, contract_digest, state, receipt_id FROM pending_proxy_settlements WHERE request_id = $1")
            .bind(owner.request_id.to_string()).fetch_optional(&mut *tx).await?;
        let fence = if let Some(existing) = existing {
            if existing.try_get::<String, _>("owner_token")? != owner.token.to_string()
                || existing.try_get::<String, _>("state")? != "pending"
                || existing
                    .try_get::<Option<String>, _>("receipt_id")?
                    .is_some()
            {
                return Err(conflict());
            }
            if existing.try_get::<String, _>("contract_digest")? == contract_digest {
                parse_uuid(existing.try_get("fence")?)?
            } else {
                // A newly selected candidate has a different immutable contract.
                // Its old receipt is fenced; this is not another provider send.
                Uuid::now_v7()
            }
        } else {
            Uuid::now_v7()
        };
        sqlx::query("INSERT INTO pending_proxy_settlements (request_id, tenant_id, key_id, account_id, reservation_id, owner_token, fence, contract_digest, contract_json, state, created_at, updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,'pending',$10,$10) ON CONFLICT(request_id) DO UPDATE SET fence = excluded.fence, contract_digest = excluded.contract_digest, contract_json = excluded.contract_json, updated_at = excluded.updated_at")
            .bind(owner.request_id.to_string()).bind(owner.tenant_id.to_string())
            .bind(owner.key_id.to_string()).bind(account_id.to_string()).bind(owner.reservation_id.to_string())
            .bind(owner.token.to_string()).bind(fence.to_string()).bind(&contract_digest)
            .bind(contract.to_string()).bind(now).execute(&mut *tx).await?;
        // Do not write delivery_prepared/started: registration is neither
        // downstream delivery evidence nor a prohibition on route failover.
        tx.commit().await?;
        Ok(PendingProxySettlementClaim {
            request_id: owner.request_id,
            tenant_id: owner.tenant_id,
            key_id: owner.key_id,
            account_id,
            reservation_id: owner.reservation_id,
            owner_token: owner.token,
            fence,
            contract_digest,
            input_token_ceiling,
            output_token_ceiling,
            requested_service_tier: tier.to_owned(),
        })
    }

    pub(crate) async fn mark_pending_proxy_settlement_unknown(
        &self,
        claim: &PendingProxySettlementClaim,
    ) -> Result<(), AppError> {
        let marked = sqlx::query("UPDATE pending_proxy_settlements SET unknown_reason = 'authoritative_usage_unavailable', updated_at = $7 WHERE request_id = $1 AND tenant_id = $2 AND key_id = $3 AND reservation_id = $4 AND fence = $5 AND contract_digest = $6 AND state = 'pending'")
            .bind(claim.request_id.to_string()).bind(claim.tenant_id.to_string()).bind(claim.key_id.to_string())
            .bind(claim.reservation_id.to_string()).bind(claim.fence.to_string()).bind(&claim.contract_digest)
            .bind(unix_millis()).execute(&self.pool).await?;
        if marked.rows_affected() != 1 {
            return Err(conflict());
        }
        Ok(())
    }

    pub(crate) async fn finish_pending_proxy_settlement(
        &self,
        claim: &PendingProxySettlementClaim,
        receipt_id: Uuid,
        evidence: PendingProxySettlementEvidence,
        input: FinishProxyRequest<'_>,
        upstream_attribution: ProxyRequestUpstreamAttribution,
    ) -> Result<PendingProxySettlementAck, AppError> {
        if input.request_id != claim.request_id
            || input.tenant_id != claim.tenant_id
            || input.reservation.id != claim.reservation_id
            || input.reservation.key_id != claim.key_id
            || input.reservation.account_id != claim.account_id
            || input.input_token_ceiling != claim.input_token_ceiling
            || input.output_token_ceiling != claim.output_token_ceiling
            || input.requested_service_tier.unwrap_or("default") != claim.requested_service_tier
        {
            return Err(conflict());
        }
        if let Some(conversation) = input.conversation.as_ref()
            && (conversation.key.tenant_id != claim.tenant_id
                || conversation.key.key_id != claim.key_id
                || conversation.key.account_id != claim.account_id)
        {
            return Err(conflict());
        }
        match evidence {
            PendingProxySettlementEvidence::ProviderReported
                if input.usage_basis != Some(crate::model::RequestUsageBasis::ProviderReported) =>
            {
                return Err(conflict());
            }
            PendingProxySettlementEvidence::ConfirmedNotExecuted
                if input.usage != TokenUsage::default()
                    || !(400..=599).contains(&input.status_code)
                    || input.usage_basis != Some(crate::model::RequestUsageBasis::NotObserved) =>
            {
                return Err(conflict());
            }
            _ => {}
        }
        let attribution = match upstream_attribution {
            ProxyRequestUpstreamAttribution::KeepSelected => {
                serde_json::json!({"kind": "keep_selected"})
            }
            ProxyRequestUpstreamAttribution::LastDispatched(ids) => {
                serde_json::json!({"kind": "last_dispatched", "ids": ids})
            }
        };
        let value = serde_json::json!({
            "version": 1, "claim": claim, "evidence": evidence,
            "status": input.status_code, "error": input.error_code, "response": input.response_object,
            "duration": input.duration_ms, "first_output": input.first_output_ms,
            "generation_duration": input.generation_duration_ms,
            "cause": input.terminal_cause.map(|cause| cause.as_str()),
            "input": input.usage.input_tokens, "cached": input.usage.cached_input_tokens,
            "cache_write": input.usage.cache_write_tokens, "output": input.usage.output_tokens,
            "tier": input.usage.service_tier, "cache_coverage": input.usage.cache_coverage.map(|c| (c.read_tokens, c.inclusive_input_tokens)),
            "routing_session": input.routing_session_id, "routing_at": input.routing_terminal_observed_at,
            "attribution": attribution,
            "conversation": input.conversation.as_ref().map(|c| serde_json::json!({
                "key": c.key.key_id, "tenant": c.key.tenant_id, "principal": c.key.principal_id,
                // Match every field persisted as key_snapshot_json by the
                // conversation outbox, including on the applied replay path.
                "key_snapshot": {
                    "account_id": c.key.account_id, "alias": c.key.alias,
                    "currency": c.key.currency, "credential_generation": c.key.credential_generation,
                    "policy": c.key.policy,
                },
                "body": c.request_json, "hints": c.hints, "client": c.client_name, "response": c.upstream_response_id,
            })),
        });
        let receipt = Receipt {
            claim,
            id: receipt_id,
            digest: digest(&value)?,
        };
        self.finish_proxy_request_with_pending_receipt(input, upstream_attribution, &receipt)
            .await?;
        // This read is required even after AlreadyFinished; only the matching
        // financial receipt is an ACK allowing the independent journal deletion.
        let cost_micros: i64 = sqlx::query_scalar("SELECT cost_micros FROM pending_proxy_settlements WHERE request_id = $1 AND reservation_id = $2 AND fence = $3 AND receipt_id = $4 AND receipt_digest = $5 AND state = 'applied'")
            .bind(claim.request_id.to_string()).bind(claim.reservation_id.to_string()).bind(claim.fence.to_string())
            .bind(receipt_id.to_string()).bind(&receipt.digest).fetch_optional(&self.pool).await?.ok_or_else(conflict)?;
        Ok(PendingProxySettlementAck {
            request_id: claim.request_id,
            reservation_id: claim.reservation_id,
            fence: claim.fence,
            receipt_id,
            receipt_digest: receipt.digest,
            cost_micros,
        })
    }
}

/// Called with the request owner row already locked. Legacy cancellation and a
/// reaper selected before registration must both pass this same guard.
pub(super) async fn guard(
    tx: &mut Transaction<'_, Any>,
    input: &FinishProxyRequest<'_>,
    receipt: Option<&Receipt<'_>>,
) -> Result<Option<FinishProxyRequestResult>, AppError> {
    let row = sqlx::query("SELECT tenant_id,key_id,account_id,reservation_id,owner_token,fence,contract_digest,state,receipt_id,receipt_digest,cost_micros,status_code,error_code,response_object FROM pending_proxy_settlements WHERE request_id = $1")
        .bind(input.request_id.to_string()).fetch_optional(&mut **tx).await?;
    let Some(row) = row else {
        return if receipt.is_none() {
            Ok(None)
        } else {
            Err(conflict())
        };
    };
    let Some(receipt) = receipt else {
        return Err(conflict());
    };
    let claim = receipt.claim;
    for (column, expected) in [
        ("tenant_id", claim.tenant_id.to_string()),
        ("key_id", claim.key_id.to_string()),
        ("account_id", claim.account_id.to_string()),
        ("reservation_id", claim.reservation_id.to_string()),
        ("owner_token", claim.owner_token.to_string()),
        ("fence", claim.fence.to_string()),
        ("contract_digest", claim.contract_digest.clone()),
    ] {
        if row.try_get::<String, _>(column)? != expected {
            return Err(conflict());
        }
    }
    if row.try_get::<String, _>("state")? == "applied" {
        if row.try_get::<Option<String>, _>("receipt_id")?.as_deref()
            != Some(receipt.id.to_string().as_str())
            || row
                .try_get::<Option<String>, _>("receipt_digest")?
                .as_deref()
                != Some(receipt.digest.as_str())
        {
            return Err(conflict());
        }
        return Ok(Some(FinishProxyRequestResult::AlreadyFinished {
            status_code: row.try_get("status_code")?,
            cost_micros: row.try_get("cost_micros")?,
            error_code: row.try_get("error_code")?,
            response_object: row.try_get("response_object")?,
        }));
    }
    let reservation = sqlx::query(RESERVATION_CONTRACT_SQL)
        .bind(claim.reservation_id.to_string())
        .bind(claim.key_id.to_string())
        .bind(claim.request_id.to_string())
        .fetch_optional(&mut **tx)
        .await?
        .ok_or_else(conflict)?;
    let owner = ProxyRequestOwner {
        request_id: claim.request_id,
        tenant_id: claim.tenant_id,
        key_id: claim.key_id,
        reservation_id: claim.reservation_id,
        token: claim.owner_token,
    };
    if digest(&contract(
        &owner,
        &reservation,
        claim.input_token_ceiling,
        claim.output_token_ceiling,
        &claim.requested_service_tier,
    )?)? != claim.contract_digest
    {
        return Err(conflict());
    }
    Ok(None)
}

pub(super) async fn acknowledge(
    tx: &mut Transaction<'_, Any>,
    receipt: &Receipt<'_>,
    input: &FinishProxyRequest<'_>,
    cost_micros: i64,
    now: i64,
) -> Result<(), AppError> {
    let acknowledged = sqlx::query("UPDATE pending_proxy_settlements SET state = 'applied', receipt_id = $1, receipt_digest = $2, cost_micros = $3, status_code = $4, error_code = $5, response_object = $6, applied_at = $7, updated_at = $7 WHERE request_id = $8 AND fence = $9 AND state = 'pending'")
        .bind(receipt.id.to_string()).bind(&receipt.digest).bind(cost_micros)
        .bind(input.status_code).bind(input.error_code).bind(input.response_object).bind(now)
        .bind(input.request_id.to_string()).bind(receipt.claim.fence.to_string()).execute(&mut **tx).await?;
    if acknowledged.rows_affected() != 1 {
        return Err(conflict());
    }
    Ok(())
}
