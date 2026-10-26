//! Runtime completion is not permission to settle an unknown financial result.
//! The gateway writes this observation before returning a finished response so
//! a delivered 502 cannot stay blank. Financial settlement stays asynchronous.
use super::super::parse_uuid;
use super::super::*;

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub(crate) struct PendingProxyOperationalTerminal {
    pub status_code: i64,
    pub error_code: Option<String>,
    pub terminal_cause: Option<String>,
    pub stage: Option<String>,
    pub completed_at: i64,
    pub duration_ms: i64,
    pub first_output_ms: Option<i64>,
    pub generation_duration_ms: Option<i64>,
    pub response_object: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct PendingProxyOperationalTerminalAck {
    pub request_id: Uuid,
    pub reservation_id: Uuid,
    pub fence: Uuid,
    pub receipt_id: Uuid,
    pub operational_digest: String,
}

fn conflict() -> AppError {
    AppError::Conflict("operational terminal authority or receipt mismatch".into())
}

fn validate(input: &PendingProxyOperationalTerminal) -> Result<(), AppError> {
    if !(200..=599).contains(&input.status_code)
        || input.duration_ms < 0
        || input.first_output_ms.is_some_and(|value| value < 0)
        || input.generation_duration_ms.is_some_and(|value| value < 0)
        || input.error_code.as_ref().is_some_and(|value| {
            value.is_empty()
                || value.len() > 128
                || !value
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        })
        || input.terminal_cause.as_deref().is_some_and(|value| {
            crate::model::failure_diagnostic(Some(value)).is_none()
                || value == "gateway_pre_admission"
        })
        || input.stage.as_deref().is_some_and(|stage| {
            !matches!(
                stage,
                "upstream_response_headers"
                    | "upstream_response_body"
                    | "response_delivery"
                    | "buffered_response_ready"
            )
        })
        || input.response_object.is_empty()
        || input.response_object.chars().any(char::is_control)
    {
        return Err(conflict());
    }
    Ok(())
}

#[allow(dead_code)] // API journal integration is owned separately.
impl Database {
    /// An ACK proves only that this exact runtime observation is durable. It
    /// cannot release a hold or retire a journal awaiting financial settlement.
    pub(crate) async fn record_pending_proxy_operational_terminal(
        &self,
        claim: &PendingProxySettlementClaim,
        receipt_id: Uuid,
        terminal: &PendingProxyOperationalTerminal,
        upstream_attribution: ProxyRequestUpstreamAttribution,
    ) -> Result<PendingProxyOperationalTerminalAck, AppError> {
        validate(terminal)?;
        let attribution = match upstream_attribution {
            ProxyRequestUpstreamAttribution::KeepSelected => {
                serde_json::json!({"kind": "keep_selected"})
            }
            ProxyRequestUpstreamAttribution::LastDispatched(ids) => {
                serde_json::json!({"kind": "last_dispatched", "ids": ids})
            }
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1, "claim": claim, "receipt_id": receipt_id,
            "terminal": terminal, "attribution": attribution,
        }))
        .map_err(|_| AppError::Internal)?;
        let operational_digest = blake3::hash(&bytes).to_hex().to_string();
        let mut tx = self.begin_write_transaction().await?;
        let locator = sqlx::query(
            "SELECT created_at FROM request_record_locators WHERE id = $1 AND tenant_id = $2 AND key_id = $3",
        )
        .bind(claim.request_id.to_string())
        .bind(claim.tenant_id.to_string())
        .bind(claim.key_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(conflict)?;
        let created_at: i64 = locator.try_get("created_at")?;
        if terminal.completed_at < created_at {
            return Err(conflict());
        }
        // Same private request row first as claim/finalizer. No completed_at
        // predicate: an exact operational receipt may arrive after accounting.
        let owned = sqlx::query(
            "UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND created_at = $2 AND tenant_id = $3 AND key_id = $4 AND reservation_id = $5",
        )
        .bind(claim.request_id.to_string())
        .bind(created_at)
        .bind(claim.tenant_id.to_string())
        .bind(claim.key_id.to_string())
        .bind(claim.reservation_id.to_string())
        .execute(&mut *tx)
        .await?;
        if owned.rows_affected() != 1 {
            return Err(conflict());
        }
        let authority = sqlx::query(
            "SELECT tenant_id,key_id,account_id,reservation_id,owner_token,fence,contract_digest,contract_json,state FROM pending_proxy_settlements WHERE request_id = $1",
        )
        .bind(claim.request_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(conflict)?;
        for (column, expected) in [
            ("tenant_id", claim.tenant_id.to_string()),
            ("key_id", claim.key_id.to_string()),
            ("account_id", claim.account_id.to_string()),
            ("reservation_id", claim.reservation_id.to_string()),
            ("owner_token", claim.owner_token.to_string()),
            ("fence", claim.fence.to_string()),
            ("contract_digest", claim.contract_digest.clone()),
        ] {
            if authority.try_get::<String, _>(column)? != expected {
                return Err(conflict());
            }
        }
        let contract: Value =
            serde_json::from_str(&authority.try_get::<String, _>("contract_json")?)
                .map_err(|_| AppError::Internal)?;
        let contract_bytes = serde_json::to_vec(&contract).map_err(|_| AppError::Internal)?;
        if blake3::hash(&contract_bytes).to_hex().as_str() != claim.contract_digest {
            return Err(conflict());
        }
        if contract["input_ceiling"].as_i64() != Some(claim.input_token_ceiling)
            || contract["output_ceiling"].as_i64() != Some(claim.output_token_ceiling)
            || contract["tier"].as_str() != Some(claim.requested_service_tier.as_str())
            || !matches!(
                authority.try_get::<String, _>("state")?.as_str(),
                "pending" | "applied"
            )
        {
            return Err(conflict());
        }
        if let Some(existing) = sqlx::query(
            "SELECT receipt_id, operational_digest FROM proxy_operational_terminals WHERE request_id = $1",
        )
        .bind(claim.request_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        {
            if existing.try_get::<String, _>("receipt_id")? != receipt_id.to_string()
                || existing.try_get::<String, _>("operational_digest")? != operational_digest
            {
                return Err(conflict());
            }
        } else {
            // Use the frozen contract, not a mutable request/candidate lookup,
            // so recovery has identical attribution after financial completion.
            let ids = match upstream_attribution {
                ProxyRequestUpstreamAttribution::KeepSelected => (
                    contract["upstream_account_id"].as_str().map(str::to_owned),
                    contract["model_route_id"].as_str().map(str::to_owned),
                ),
                ProxyRequestUpstreamAttribution::LastDispatched(ids) => (
                    ids.map(|pair| pair.0.to_string()),
                    ids.map(|pair| pair.1.to_string()),
                ),
            };
            sqlx::query(
                "INSERT INTO proxy_operational_terminals (request_id,request_created_at,tenant_id,key_id,account_id,reservation_id,owner_token,fence,contract_digest,receipt_id,operational_digest,status_code,error_code,terminal_cause_code,stage,completed_at,duration_ms,first_output_ms,generation_duration_ms,response_object,upstream_account_id,model_route_id,recorded_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21,$22,$23)",
            )
            .bind(claim.request_id.to_string())
            .bind(created_at)
            .bind(claim.tenant_id.to_string())
            .bind(claim.key_id.to_string())
            .bind(claim.account_id.to_string())
            .bind(claim.reservation_id.to_string())
            .bind(claim.owner_token.to_string())
            .bind(claim.fence.to_string())
            .bind(&claim.contract_digest)
            .bind(receipt_id.to_string())
            .bind(&operational_digest)
            .bind(terminal.status_code)
            .bind(&terminal.error_code)
            .bind(&terminal.terminal_cause)
            .bind(&terminal.stage)
            .bind(terminal.completed_at)
            .bind(terminal.duration_ms)
            .bind(terminal.first_output_ms)
            .bind(terminal.generation_duration_ms)
            .bind(&terminal.response_object)
            .bind(ids.0)
            .bind(ids.1)
            .bind(unix_millis())
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(PendingProxyOperationalTerminalAck {
            request_id: claim.request_id,
            reservation_id: claim.reservation_id,
            fence: claim.fence,
            receipt_id,
            operational_digest,
        })
    }

    /// A gateway that died after admitting a request stops renewing its owner
    /// lease. Record that the connection is over so the list does not keep
    /// showing the request as still running. Billing stays unknown.
    pub(crate) async fn close_expired_proxy_owners(&self, limit: i64) -> Result<u64, AppError> {
        let now = self
            .request_owner_now(&mut *self.pool.acquire().await?)
            .await?;
        let rows = sqlx::query(
            "SELECT p.request_id, p.tenant_id, p.key_id, p.account_id, p.reservation_id, p.owner_token, p.fence, p.contract_digest, p.contract_json, q.created_at FROM pending_proxy_settlements p JOIN request_records q ON q.id = p.request_id AND q.tenant_id = p.tenant_id AND q.key_id = p.key_id AND q.reservation_id = p.reservation_id WHERE p.state = 'pending' AND q.completed_at IS NULL AND q.proxy_owner_expires_at IS NOT NULL AND q.proxy_owner_expires_at <= $1 AND NOT EXISTS (SELECT 1 FROM proxy_operational_terminals o WHERE o.request_id = p.request_id AND o.reservation_id = p.reservation_id) ORDER BY q.proxy_owner_expires_at, p.request_id LIMIT $2",
        )
        .bind(now)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;
        let mut closed = 0_u64;
        for row in rows {
            let request_id = parse_uuid(row.try_get("request_id")?)?;
            let contract: serde_json::Value =
                serde_json::from_str(&row.try_get::<String, _>("contract_json")?)
                    .map_err(|_| AppError::Internal)?;
            let claim = PendingProxySettlementClaim {
                request_id,
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                reservation_id: parse_uuid(row.try_get("reservation_id")?)?,
                owner_token: parse_uuid(row.try_get("owner_token")?)?,
                fence: parse_uuid(row.try_get("fence")?)?,
                contract_digest: row.try_get("contract_digest")?,
                input_token_ceiling: contract
                    .get("input_ceiling")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or(AppError::Internal)?,
                output_token_ceiling: contract
                    .get("output_ceiling")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or(AppError::Internal)?,
                requested_service_tier: contract
                    .get("tier")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
            };
            let created_at: i64 = row.try_get("created_at")?;
            let terminal = PendingProxyOperationalTerminal {
                status_code: 502,
                error_code: Some("gateway_owner_lost".to_owned()),
                terminal_cause: None,
                stage: Some("response_delivery".to_owned()),
                completed_at: now.max(created_at),
                duration_ms: now.saturating_sub(created_at).max(0),
                first_output_ms: None,
                generation_duration_ms: None,
                response_object: format!("gap://{request_id}/response"),
            };
            let receipt_id = Uuid::now_v7();
            match self
                .record_pending_proxy_operational_terminal(
                    &claim,
                    receipt_id,
                    &terminal,
                    ProxyRequestUpstreamAttribution::KeepSelected,
                )
                .await
            {
                Ok(_) => {
                    if self
                        .mark_pending_proxy_settlement_unknown(&claim)
                        .await
                        .is_err()
                    {
                        tracing::error!(%request_id, "expired proxy owner terminal was recorded but its settlement remains unmarked");
                    }
                    closed += 1;
                }
                Err(AppError::Conflict(_)) => {}
                Err(error) => {
                    tracing::error!(%request_id, error_category = error.diagnostic_category(), "expired proxy owner could not be closed");
                }
            }
        }
        Ok(closed)
    }
}
