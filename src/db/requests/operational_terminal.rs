//! Runtime completion is not permission to settle an unknown financial result.
//! The gateway writes this observation before returning a finished response so
//! a delivered 502 cannot stay blank. Financial settlement stays asynchronous.
use super::super::parse_uuid;
use super::super::*;

#[cfg(test)]
pub(crate) mod tests;

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

#[derive(Clone, Default)]
pub(crate) struct ExpiredProxyOwnerScan {
    after: Option<(i64, String)>,
    upper: Option<i64>,
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
        self.record_pending_proxy_operational_terminal_inner(
            claim,
            receipt_id,
            terminal,
            upstream_attribution,
            false,
        )
        .await
    }

    async fn record_pending_proxy_operational_terminal_inner(
        &self,
        claim: &PendingProxySettlementClaim,
        receipt_id: Uuid,
        terminal: &PendingProxyOperationalTerminal,
        upstream_attribution: ProxyRequestUpstreamAttribution,
        require_expired: bool,
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
        .map_err(|_| {
            AppError::internal("operational_terminal_record_pending_proxy_operational_terminal_1")
        })?;
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
        if require_expired {
            let now = self.request_owner_now(&mut tx).await?;
            let eligible = sqlx::query("SELECT 1 FROM request_records q WHERE q.id = $1 AND q.created_at = $2 AND q.tenant_id = $3 AND q.key_id = $4 AND q.reservation_id = $5 AND q.proxy_owner_token = $7 AND q.completed_at IS NULL AND q.proxy_owner_expires_at IS NOT NULL AND q.proxy_owner_expires_at <= $6 AND NOT EXISTS (SELECT 1 FROM proxy_operational_terminals o WHERE o.request_id = q.id)")
                .bind(claim.request_id.to_string()).bind(created_at).bind(claim.tenant_id.to_string()).bind(claim.key_id.to_string()).bind(claim.reservation_id.to_string()).bind(now).bind(claim.owner_token.to_string())
                .fetch_optional(&mut *tx).await?;
            if eligible.is_none() {
                return Err(conflict());
            }
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
        let contract: Value = serde_json::from_str(
            &authority.try_get::<String, _>("contract_json")?,
        )
        .map_err(|_| {
            AppError::internal("operational_terminal_record_pending_proxy_operational_terminal_2")
        })?;
        let contract_bytes = serde_json::to_vec(&contract).map_err(|_| {
            AppError::internal("operational_terminal_record_pending_proxy_operational_terminal_3")
        })?;
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
            super::terminal_projection::enqueue_unknown_terminal_projection_in_transaction(
                &mut tx, &claim.request_id.to_string(),
            ).await?;
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
            // Publish the execution receipt atomically, independently of archive
            // permits and financial projection. Legacy storage requires numeric
            // placeholders; the shared read model emits unknown usage until the
            // authoritative financial source is complete.
        super::terminal_projection::enqueue_unknown_terminal_projection_in_transaction(
            &mut tx,
            &claim.request_id.to_string(),
        )
        .await?;
            let draft_cursor = crate::db::RequestEventCursor { event_id: Uuid::nil().to_string(), event_at: terminal.completed_at };
            let pending_started = super::lifecycle::prepare_pending_started_in_transaction(&mut tx, &claim.request_id.to_string()).await?;
            let draft_row = sqlx::query(r#"SELECT CAST($1 AS TEXT) AS event_id, CAST(tenant_id AS TEXT) AS tenant_id, CAST(key_id AS TEXT) AS key_id, CAST(id AS TEXT) AS request_id, CAST($2 AS BIGINT) AS event_at, CAST('finished' AS TEXT) AS event_kind, CAST(protocol AS TEXT) AS protocol, CAST(model AS TEXT) AS model, CAST($3 AS BIGINT) AS status_code, CAST($4 AS BIGINT) AS duration_ms, CAST(0 AS BIGINT) AS input_tokens, CAST(0 AS BIGINT) AS output_tokens, CAST(0 AS BIGINT) AS cost_micros, CAST($5 AS TEXT) AS error_code FROM request_records WHERE id = $6 AND created_at = $7 AND tenant_id = $8 AND key_id = $9"#)
                .bind(&draft_cursor.event_id).bind(draft_cursor.event_at).bind(terminal.status_code)
                .bind(terminal.duration_ms).bind(&terminal.error_code)
                .bind(claim.request_id.to_string()).bind(created_at)
                .bind(claim.tenant_id.to_string()).bind(claim.key_id.to_string())
                .fetch_optional(&mut *tx).await?;
            let prepared = match draft_row.as_ref() {
                Some(row) => Some(crate::db::history_immutable_outbox::EventDraft::from_row(row)?.prepare(&mut tx).await?),
                None => None,
            };
            crate::db::history_immutable_outbox::PreparedEventCapture::publish(&mut tx, pending_started, terminal.completed_at).await?;
        crate::db::history_immutable_outbox::PreparedEventCapture::publish(&mut tx, prepared, terminal.completed_at).await?;
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
        self.close_expired_proxy_owners_page(limit, &mut ExpiredProxyOwnerScan::default())
            .await
    }

    pub(crate) async fn close_expired_proxy_owners_page(
        &self,
        limit: i64,
        scan: &mut ExpiredProxyOwnerScan,
    ) -> Result<u64, AppError> {
        let mut connection = self.pool.acquire().await?;
        let now = self.request_owner_now(&mut connection).await?;
        connection.finish();
        let upper = *scan.upper.get_or_insert(now);
        // Bound work before looking up any partition or applying eligibility.
        let rows = match &scan.after {
            Some((updated, id)) => sqlx::query("SELECT * FROM pending_proxy_settlements WHERE state = 'pending' AND updated_at <= $1 AND (updated_at, request_id) > ($2, $3) ORDER BY updated_at, request_id LIMIT $4")
                .bind(upper).bind(updated).bind(id).bind(limit).fetch_all(&self.pool).await?,
            None => sqlx::query("SELECT * FROM pending_proxy_settlements WHERE state = 'pending' AND updated_at <= $1 ORDER BY updated_at, request_id LIMIT $2")
                .bind(upper).bind(limit).fetch_all(&self.pool).await?,
        };
        if rows.is_empty() {
            scan.after = None;
            scan.upper = None;
        }
        let mut closed = 0_u64;
        for row in rows {
            let request_id_text: String = row.try_get("request_id")?;
            scan.after = Some((row.try_get("updated_at")?, request_id_text.clone()));
            let request_id = parse_uuid(request_id_text)?;
            let tenant: String = row.try_get("tenant_id")?;
            let key: String = row.try_get("key_id")?;
            let created_at: Option<i64> = sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1 AND tenant_id = $2 AND key_id = $3")
                .bind(request_id.to_string()).bind(&tenant).bind(&key).fetch_optional(&self.pool).await?;
            let Some(created_at) = created_at else {
                continue;
            };
            let eligible = sqlx::query("SELECT 1 FROM request_records q WHERE q.id = $1 AND q.created_at = $2 AND q.tenant_id = $3 AND q.key_id = $4 AND q.reservation_id = $5 AND q.completed_at IS NULL AND q.proxy_owner_expires_at IS NOT NULL AND q.proxy_owner_expires_at <= $6 AND NOT EXISTS (SELECT 1 FROM proxy_operational_terminals o WHERE o.request_id = q.id)")
                .bind(request_id.to_string()).bind(created_at).bind(&tenant).bind(&key).bind(row.try_get::<String,_>("reservation_id")?).bind(now).fetch_optional(&self.pool).await?;
            if eligible.is_none() {
                continue;
            }
            let contract: serde_json::Value = serde_json::from_str(
                &row.try_get::<String, _>("contract_json")?,
            )
            .map_err(|_| AppError::internal("operational_terminal_close_expired_proxy_owners_1"))?;
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
                    .ok_or(AppError::internal(
                        "operational_terminal_close_expired_proxy_owners_1",
                    ))?,
                output_token_ceiling: contract
                    .get("output_ceiling")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or(AppError::internal(
                        "operational_terminal_close_expired_proxy_owners_2",
                    ))?,
                requested_service_tier: contract
                    .get("tier")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
            };
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
                .record_pending_proxy_operational_terminal_inner(
                    &claim,
                    receipt_id,
                    &terminal,
                    ProxyRequestUpstreamAttribution::KeepSelected,
                    true,
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
