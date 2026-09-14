//! Human evidence closes unknown image submissions without another upstream call.
use super::*;

#[derive(Debug, Serialize)]
pub struct ImageGenerationQuarantineView {
    pub status: &'static str,
    pub resolution: Option<ImageGenerationQuarantineResolution>,
    pub request_id: Uuid,
    pub tenant_external_id: String,
    pub model: String,
    pub currency: String,
    pub reserved_micros: i64,
    pub submission_started_at: i64,
    pub submission_uncertain_at: i64,
    pub revision: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct ImageGenerationQuarantineResolution {
    pub resolution_id: Uuid,
    pub request_id: Uuid,
    pub tenant_external_id: String,
    pub action: String,
    pub confirmed_cost_micros: i64,
    pub currency: String,
    pub evidence_digest: String,
    pub resolved_by_service_id: Uuid,
    pub resulting_status: String,
    pub created_at: i64,
}

pub struct ResolveImageGenerationQuarantine<'a> {
    pub tenant_external_id: &'a str,
    pub request_id: Uuid,
    pub actor_service_id: Uuid,
    pub idempotency_hash: &'a str,
    pub expected_revision: &'a str,
    pub action: &'a str,
    pub confirmed_cost_micros: i64,
    pub currency: &'a str,
    pub evidence_digest: &'a str,
}

const PROJECTION: &str = "SELECT q.id, q.tenant_id, q.key_id, q.reservation_id, q.created_at, q.model, q.completed_at, q.submission_started_at, q.submission_uncertain_at, q.error_code, t.external_id AS tenant_external_id, r.account_id, r.key_id AS reservation_key_id, r.enforcement_mode, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, a.currency, receipt.result_json AS resolution_json FROM request_records q JOIN tenants t ON t.id = q.tenant_id JOIN usage_reservations r ON r.id = q.reservation_id AND r.key_id = q.key_id JOIN credit_accounts a ON a.id = r.account_id AND a.tenant_id = q.tenant_id LEFT JOIN image_generation_quarantine_resolutions receipt ON receipt.request_id = q.id AND receipt.tenant_id = q.tenant_id";

impl Database {
    pub async fn list_image_generation_quarantine(
        &self,
        tenant: &str,
        limit: i64,
        after_id: Option<Uuid>,
    ) -> Result<Vec<ImageGenerationQuarantineView>, AppError> {
        if !(1..=100).contains(&limit) {
            return Err(AppError::BadRequest(
                "limit must be between 1 and 100".into(),
            ));
        }
        let sql = format!(
            "{PROJECTION} WHERE t.external_id = $1 AND q.completed_at IS NULL AND q.submission_started_at IS NOT NULL AND q.submission_uncertain_at IS NOT NULL AND q.id > $2 ORDER BY q.id LIMIT $3"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(tenant)
            .bind(after_id.map(|id| id.to_string()).unwrap_or_default())
            .bind(limit)
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(view)
            .collect()
    }

    pub async fn image_generation_quarantine(
        &self,
        tenant: &str,
        request_id: Uuid,
    ) -> Result<ImageGenerationQuarantineView, AppError> {
        let sql = format!(
            "{PROJECTION} WHERE t.external_id = $1 AND q.id = $2 AND q.submission_started_at IS NOT NULL AND q.submission_uncertain_at IS NOT NULL"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(tenant)
            .bind(request_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        view(&row)
    }

    pub async fn resolve_image_generation_quarantine(
        &self,
        input: ResolveImageGenerationQuarantine<'_>,
    ) -> Result<ImageGenerationQuarantineResolution, AppError> {
        validate(&input)?;
        let digest = blake3::hash(
            &serde_json::to_vec(&(
                input.tenant_external_id,
                input.request_id,
                input.actor_service_id,
                input.expected_revision,
                input.action,
                input.confirmed_cost_micros,
                input.currency,
                input.evidence_digest,
            ))
            .map_err(|_| AppError::Internal)?,
        )
        .to_hex()
        .to_string();
        let mut tx = self.begin_write_transaction().await?;
        // Lock current service identity/credential, and recheck permission even
        // for replay. Revocation or tenant reassignment cannot race the write.
        let actor = sqlx::query("UPDATE service_principals SET updated_at = updated_at WHERE id = $1 AND status = 'active'")
            .bind(input.actor_service_id.to_string()).execute(&mut *tx).await?;
        if actor.rows_affected() != 1 {
            return Err(AppError::Forbidden);
        }
        sqlx::query("UPDATE service_credentials SET created_at = created_at WHERE service_principal_id = $1 AND generation = (SELECT credential_generation FROM service_principals WHERE id = $1)")
            .bind(input.actor_service_id.to_string()).execute(&mut *tx).await?;
        let credential = sqlx::query("SELECT c.scopes_json, c.tenant_external_id FROM service_credentials c JOIN service_principals s ON s.id = c.service_principal_id AND s.credential_generation = c.generation WHERE s.id = $1 AND s.status = 'active' AND c.revoked_at IS NULL")
            .bind(input.actor_service_id.to_string()).fetch_optional(&mut *tx).await?.ok_or(AppError::Forbidden)?;
        let scopes: Vec<String> =
            serde_json::from_str(&credential.try_get::<String, _>("scopes_json")?)
                .map_err(|_| AppError::Internal)?;
        if !scopes
            .iter()
            .any(|s| s == "*" || s == "generations:reconcile")
            || credential
                .try_get::<Option<String>, _>("tenant_external_id")?
                .as_deref()
                != Some(input.tenant_external_id)
        {
            return Err(AppError::Forbidden);
        }
        // Same lock order as live image completion: idempotency, request,
        // reservation. The late live owner can only replay our failed receipt.
        sqlx::query("UPDATE synchronous_image_idempotency SET lease_expires_at = lease_expires_at WHERE request_id = $1 AND key_id IN (SELECT q.key_id FROM request_records q JOIN tenants t ON t.id = q.tenant_id WHERE q.id = $1 AND t.external_id = $2)")
            .bind(input.request_id.to_string()).bind(input.tenant_external_id).execute(&mut *tx).await?;
        let locked = sqlx::query("UPDATE request_records SET completed_at = completed_at WHERE id = $1 AND tenant_id = (SELECT id FROM tenants WHERE external_id = $2)")
            .bind(input.request_id.to_string()).bind(input.tenant_external_id).execute(&mut *tx).await?;
        if locked.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let sql = format!("{PROJECTION} WHERE q.id = $1 AND t.external_id = $2");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(input.request_id.to_string())
            .bind(input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?;
        let tenant_id: String = row.try_get("tenant_id")?;
        if let Some(replay) = sqlx::query("SELECT request_digest, result_json FROM image_generation_quarantine_resolutions WHERE tenant_id = $1 AND actor_service_id = $2 AND idempotency_hash = $3")
            .bind(&tenant_id).bind(input.actor_service_id.to_string()).bind(input.idempotency_hash).fetch_optional(&mut *tx).await? {
            if replay.try_get::<String, _>("request_digest")? != digest { return Err(AppError::Conflict("Idempotency-Key was used for a different resolution".into())); }
            let result = serde_json::from_str(&replay.try_get::<String, _>("result_json")?).map_err(|_| AppError::Internal)?;
            tx.commit().await?;
            return Ok(result);
        }
        if row.try_get::<Option<i64>, _>("completed_at")?.is_some()
            || row
                .try_get::<Option<i64>, _>("submission_uncertain_at")?
                .is_none()
            || row
                .try_get::<Option<i64>, _>("submission_started_at")?
                .is_none()
            || row.try_get::<String, _>("reservation_status")? != "reserved"
        {
            return Err(AppError::Conflict(
                "request is not pending image quarantine reconciliation".into(),
            ));
        }
        let current = view(&row)?;
        if current.revision != input.expected_revision {
            return Err(AppError::Conflict(
                "quarantine revision changed; read it again".into(),
            ));
        }
        if current.currency != input.currency {
            return Err(AppError::BadRequest(
                "confirmed currency does not match reservation account".into(),
            ));
        }
        let reservation = UsageReservation {
            id: parse_uuid(row.try_get("reservation_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            key_id: parse_uuid(row.try_get("reservation_key_id")?)?,
            enforcement_mode: EnforcementMode::from_storage(
                &row.try_get::<String, _>("enforcement_mode")?,
            )
            .ok_or(AppError::Internal)?,
            reserved_micros: row.try_get("reserved_micros")?,
            reserved_tokens: row.try_get("reserved_tokens")?,
            rate_window_start: row.try_get("rate_window_start")?,
            input_micros_per_million: 0,
            output_micros_per_million: 0,
            price_tiers: Vec::new(),
        };
        let locked_reservation = sqlx::query("UPDATE usage_reservations SET status = status WHERE id = $1 AND key_id = $2 AND account_id = $3 AND status = 'reserved'")
            .bind(reservation.id.to_string()).bind(reservation.key_id.to_string()).bind(reservation.account_id.to_string()).execute(&mut *tx).await?;
        if locked_reservation.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "reservation changed during resolution".into(),
            ));
        }
        let now = unix_millis();
        let result = ImageGenerationQuarantineResolution {
            resolution_id: Uuid::now_v7(),
            request_id: input.request_id,
            tenant_external_id: input.tenant_external_id.into(),
            action: input.action.into(),
            confirmed_cost_micros: input.confirmed_cost_micros,
            currency: input.currency.into(),
            evidence_digest: input.evidence_digest.into(),
            resolved_by_service_id: input.actor_service_id,
            resulting_status: "failed".into(),
            created_at: now,
        };
        let inserted = sqlx::query("INSERT INTO image_generation_quarantine_resolutions (id,tenant_id,request_id,actor_service_id,idempotency_hash,request_digest,expected_revision,action,confirmed_cost_micros,currency,evidence_digest,result_json,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING")
            .bind(result.resolution_id.to_string()).bind(&tenant_id).bind(input.request_id.to_string()).bind(input.actor_service_id.to_string()).bind(input.idempotency_hash).bind(digest).bind(input.expected_revision).bind(input.action).bind(input.confirmed_cost_micros).bind(input.currency).bind(input.evidence_digest).bind(serde_json::to_string(&result).map_err(|_| AppError::Internal)?).bind(now).execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "request or Idempotency-Key already resolved".into(),
            ));
        }
        let cost = crate::db::requests::settle_confirmed_image_charge_in_transaction(
            &mut tx,
            &reservation,
            now,
            input.confirmed_cost_micros,
        )
        .await?;
        let error_code = if input.action == "not_delivered" {
            "image_not_delivered_confirmed"
        } else {
            "image_charge_confirmed_no_result"
        };
        let finished = record_request_finished_in_transaction(
            &mut tx,
            &FinishRequest {
                first_output_ms: None,
                generation_duration_ms: None,
                request_id: input.request_id,
                status_code: 502,
                duration_ms: now.saturating_sub(row.try_get::<i64, _>("created_at")?),
                input_tokens: 0,
                cached_input_tokens: 0,
                cache_write_tokens: 0,
                output_tokens: 0,
                service_tier: None,
                cost_micros: cost,
                error_code: Some(error_code.into()),
                response_object: format!("gap://{}/response", input.request_id),
            },
            now,
            reservation.enforcement_mode.enforces_prepaid_limits(),
        )
        .await?;
        if !finished {
            return Err(AppError::Conflict(
                "request completed during resolution".into(),
            ));
        }
        sqlx::query("UPDATE synchronous_image_idempotency SET status = 'failed', response_status = 502, response_object = NULL, error_code = $1, completed_at = $2 WHERE request_id = $3 AND key_id = $4 AND reservation_id = $5 AND status = 'pending'")
            .bind(error_code).bind(now).bind(input.request_id.to_string()).bind(reservation.key_id.to_string()).bind(reservation.id.to_string()).execute(&mut *tx).await?;
        // Keep existing immutable request/result evidence for audit; no fake
        // asset rows or upstream re-dispatch are produced by reconciliation.
        tx.commit().await?;
        Ok(result)
    }
}

fn view(row: &AnyRow) -> Result<ImageGenerationQuarantineView, AppError> {
    let request_id = parse_uuid(row.try_get("id")?)?;
    let started: i64 = row.try_get("submission_started_at")?;
    let uncertain: i64 = row.try_get("submission_uncertain_at")?;
    let completed_at: Option<i64> = row.try_get("completed_at")?;
    let resolution: Option<ImageGenerationQuarantineResolution> = row
        .try_get::<Option<String>, _>("resolution_json")?
        .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
        .transpose()?;
    let revision = blake3::hash(
        &serde_json::to_vec(&(
            request_id,
            row.try_get::<String, _>("tenant_id")?,
            row.try_get::<String, _>("reservation_id")?,
            started,
            uncertain,
            row.try_get::<i64, _>("reserved_micros")?,
            row.try_get::<String, _>("currency")?,
            completed_at,
            resolution.as_ref().map(|receipt| receipt.resolution_id),
        ))
        .map_err(|_| AppError::Internal)?,
    )
    .to_hex()
    .to_string();
    Ok(ImageGenerationQuarantineView {
        status: if completed_at.is_some() {
            "resolved"
        } else {
            "awaiting_confirmation"
        },
        resolution,
        request_id,
        tenant_external_id: row.try_get("tenant_external_id")?,
        model: row.try_get("model")?,
        currency: row.try_get("currency")?,
        reserved_micros: row.try_get("reserved_micros")?,
        submission_started_at: started,
        submission_uncertain_at: uncertain,
        revision,
    })
}

fn validate(input: &ResolveImageGenerationQuarantine<'_>) -> Result<(), AppError> {
    let hex = |v: &str| {
        v.len() == 64
            && v.bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !hex(input.idempotency_hash)
        || !hex(input.expected_revision)
        || !hex(input.evidence_digest)
        || !matches!(input.action, "not_delivered" | "settle_confirmed")
        || !(0..=9_007_199_254_740_991).contains(&input.confirmed_cost_micros)
        || (input.action == "not_delivered" && input.confirmed_cost_micros != 0)
        || input.currency.len() != 3
        || !input.currency.bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err(AppError::BadRequest(
            "invalid image quarantine resolution evidence, action, amount, currency, or revision"
                .into(),
        ));
    }
    Ok(())
}
