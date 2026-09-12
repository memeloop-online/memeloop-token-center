//! Evidence-bearing operator decisions; never dispatches or settles credit.
use super::super::*;

#[derive(Debug, Serialize)]
pub struct GenerationQuarantineView {
    pub job_id: Uuid,
    pub tenant_external_id: String,
    pub model: String,
    pub driver: String,
    pub attempt_count: i64,
    pub updated_at: i64,
    pub lease_expires_at: Option<i64>,
    pub revision: String,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct GenerationQuarantineResolution {
    pub resolution_id: Uuid,
    pub job_id: Uuid,
    pub tenant_external_id: String,
    pub action: String,
    pub upstream_job_id: Option<String>,
    pub evidence_digest: String,
    pub resolved_by_service_id: Uuid,
    pub resulting_status: String,
    pub delivery_confirmed_at: Option<i64>,
    pub reconciliation_deadline_at: Option<i64>,
    pub created_at: i64,
}

pub struct ResolveGenerationQuarantine<'a> {
    pub tenant_external_id: &'a str,
    pub job_id: Uuid,
    pub actor_service_id: Uuid,
    pub idempotency_hash: &'a str,
    pub expected_revision: &'a str,
    pub action: &'a str,
    pub upstream_job_id: Option<&'a str>,
    pub evidence_digest: &'a str,
}

const QUARANTINE_COLUMNS: &str = "j.id, j.tenant_id, t.external_id AS tenant_external_id, j.public_model, j.driver, j.attempt_count, j.updated_at, j.lease_expires_at, j.submission_nonce";

impl Database {
    pub async fn list_generation_quarantine(
        &self,
        tenant: &str,
        limit: i64,
        after_id: Option<Uuid>,
    ) -> Result<Vec<GenerationQuarantineView>, AppError> {
        if !(1..=100).contains(&limit) {
            return Err(AppError::BadRequest(
                "limit must be between 1 and 100".into(),
            ));
        }
        let rows = sqlx::query(&format!("SELECT {QUARANTINE_COLUMNS} FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id WHERE t.external_id = $1 AND j.status = 'submitting' AND j.error_code = 'shutdown_delivery_unknown' AND j.upstream_job_id IS NULL AND j.id > $2 ORDER BY j.id LIMIT $3"))
            .bind(tenant).bind(after_id.map(|id| id.to_string()).unwrap_or_default()).bind(limit)
            .fetch_all(&self.pool).await?;
        rows.iter().map(quarantine_view).collect()
    }

    pub async fn generation_quarantine(
        &self,
        tenant: &str,
        job_id: Uuid,
    ) -> Result<GenerationQuarantineView, AppError> {
        let row = sqlx::query(&format!("SELECT {QUARANTINE_COLUMNS} FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id WHERE t.external_id = $1 AND j.id = $2 AND j.status = 'submitting' AND j.error_code = 'shutdown_delivery_unknown' AND j.upstream_job_id IS NULL"))
            .bind(tenant).bind(job_id.to_string()).fetch_optional(&self.pool).await?
            .ok_or(AppError::NotFound)?;
        quarantine_view(&row)
    }

    pub async fn resolve_generation_quarantine(
        &self,
        input: ResolveGenerationQuarantine<'_>,
    ) -> Result<GenerationQuarantineResolution, AppError> {
        validate_resolution(&input)?;
        let request_digest = blake3::hash(
            &serde_json::to_vec(&(
                input.tenant_external_id,
                input.job_id,
                input.actor_service_id,
                input.expected_revision,
                input.action,
                input.upstream_job_id,
                input.evidence_digest,
            ))
            .map_err(|_| AppError::Internal)?,
        )
        .to_hex()
        .to_string();
        let mut transaction = self.begin_write_transaction().await?;
        let lock = match self.backend {
            DatabaseBackend::PostgreSql => " FOR UPDATE OF j",
            DatabaseBackend::Sqlite => "",
        };
        let row = sqlx::query(&format!("SELECT {QUARANTINE_COLUMNS}, j.status, j.error_code, j.upstream_job_id FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id WHERE t.external_id = $1 AND j.id = $2{lock}"))
            .bind(input.tenant_external_id).bind(input.job_id.to_string())
            .fetch_optional(&mut *transaction).await?.ok_or(AppError::NotFound)?;
        let tenant_id: String = row.try_get("tenant_id")?;
        let replay = sqlx::query("SELECT request_digest, result_json FROM generation_quarantine_resolutions WHERE tenant_id = $1 AND actor_service_id = $2 AND idempotency_hash = $3")
            .bind(&tenant_id).bind(input.actor_service_id.to_string()).bind(input.idempotency_hash)
            .fetch_optional(&mut *transaction).await?;
        if let Some(replay) = replay {
            if replay.try_get::<String, _>("request_digest")? != request_digest {
                return Err(AppError::Conflict(
                    "Idempotency-Key was used for a different resolution".into(),
                ));
            }
            let result = serde_json::from_str(&replay.try_get::<String, _>("result_json")?)
                .map_err(|_| AppError::Internal)?;
            transaction.commit().await?;
            return Ok(result);
        }
        if row.try_get::<String, _>("status")? != "submitting"
            || row.try_get::<Option<String>, _>("error_code")?.as_deref()
                != Some("shutdown_delivery_unknown")
            || row
                .try_get::<Option<String>, _>("upstream_job_id")?
                .is_some()
        {
            return Err(AppError::Conflict(
                "job is not pending quarantine reconciliation".into(),
            ));
        }
        let current = quarantine_view(&row)?;
        if current.revision != input.expected_revision {
            return Err(AppError::Conflict(
                "quarantine revision changed; read it again".into(),
            ));
        }
        let now = unix_millis();
        if current.lease_expires_at.is_some_and(|expiry| expiry >= now) {
            return Err(AppError::Conflict(
                "the submitting worker still has an active lease".into(),
            ));
        }
        let nonce: String = row.try_get("submission_nonce")?;
        let status = if input.action == "confirmed_not_submitted" {
            "queued"
        } else {
            "running"
        };
        let delivery_confirmed_at = (input.action == "confirmed_submitted").then_some(now);
        let reconciliation_deadline_at =
            delivery_confirmed_at.map(|time| time.saturating_add(24 * 60 * 60 * 1_000));
        let result = GenerationQuarantineResolution {
            resolution_id: Uuid::now_v7(),
            job_id: input.job_id,
            tenant_external_id: input.tenant_external_id.to_owned(),
            action: input.action.to_owned(),
            upstream_job_id: input.upstream_job_id.map(str::to_owned),
            evidence_digest: input.evidence_digest.to_owned(),
            resolved_by_service_id: input.actor_service_id,
            resulting_status: status.to_owned(),
            created_at: now,
            delivery_confirmed_at,
            reconciliation_deadline_at,
        };
        // One immutable resolution per submission attempt. The insert and the
        // fenced state transition commit together, including their replay data.
        let inserted = sqlx::query("INSERT INTO generation_quarantine_resolutions (id, tenant_id, job_id, submission_nonce, actor_service_id, idempotency_hash, request_digest, expected_revision, action, evidence_digest, upstream_job_id, result_json, created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13) ON CONFLICT DO NOTHING")
            .bind(result.resolution_id.to_string()).bind(&tenant_id).bind(input.job_id.to_string())
            .bind(&nonce).bind(input.actor_service_id.to_string()).bind(input.idempotency_hash)
            .bind(request_digest).bind(input.expected_revision).bind(input.action).bind(input.evidence_digest)
            .bind(input.upstream_job_id).bind(serde_json::to_string(&result).map_err(|_| AppError::Internal)?)
            .bind(now).execute(&mut *transaction).await?;
        if inserted.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "submission or Idempotency-Key already resolved".into(),
            ));
        }
        let updated = sqlx::query("UPDATE generation_jobs SET status = $1, upstream_job_id = $2, submission_nonce = NULL, error_code = NULL, failure_count = 0, next_attempt_at = $3, lease_owner = NULL, lease_expires_at = NULL, updated_at = $3, delivery_confirmed_at = $9, reconciliation_deadline_at = $10 WHERE id = $4 AND tenant_id = $5 AND status = 'submitting' AND error_code = 'shutdown_delivery_unknown' AND upstream_job_id IS NULL AND submission_nonce = $6 AND updated_at = $7 AND attempt_count = $8 AND (lease_expires_at IS NULL OR lease_expires_at < $3)")
            .bind(status).bind(input.upstream_job_id).bind(now).bind(input.job_id.to_string()).bind(&tenant_id)
            .bind(nonce).bind(current.updated_at).bind(current.attempt_count)
            .bind(delivery_confirmed_at).bind(reconciliation_deadline_at).execute(&mut *transaction).await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "quarantine changed during resolution".into(),
            ));
        }
        transaction.commit().await?;
        Ok(result)
    }
}

fn quarantine_view(row: &sqlx::any::AnyRow) -> Result<GenerationQuarantineView, AppError> {
    let job_id = parse_uuid(row.try_get("id")?)?;
    let attempt_count: i64 = row.try_get("attempt_count")?;
    let updated_at: i64 = row.try_get("updated_at")?;
    let lease_expires_at: Option<i64> = row.try_get("lease_expires_at")?;
    let nonce: String = row.try_get("submission_nonce")?;
    let revision = blake3::hash(
        &serde_json::to_vec(&(job_id, nonce, attempt_count, updated_at, lease_expires_at))
            .map_err(|_| AppError::Internal)?,
    )
    .to_hex()
    .to_string();
    Ok(GenerationQuarantineView {
        job_id,
        tenant_external_id: row.try_get("tenant_external_id")?,
        model: row.try_get("public_model")?,
        driver: row.try_get("driver")?,
        attempt_count,
        updated_at,
        lease_expires_at,
        revision,
    })
}

fn validate_resolution(input: &ResolveGenerationQuarantine<'_>) -> Result<(), AppError> {
    let digest = |value: &str| {
        value.len() == 64
            && value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    };
    if !digest(input.expected_revision)
        || !digest(input.evidence_digest)
        || !digest(input.idempotency_hash)
    {
        return Err(AppError::BadRequest(
            "revision and evidence must be lowercase 64-character digests".into(),
        ));
    }
    match (input.action, input.upstream_job_id) {
        ("confirmed_not_submitted", None) => Ok(()),
        ("confirmed_submitted", Some(id)) if !id.is_empty() && id.len() <= 256
            && !matches!(id, "." | "..")
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':')) => Ok(()),
        _ => Err(AppError::BadRequest("choose confirmed_not_submitted without an upstream ID or confirmed_submitted with a valid upstream ID".into())),
    }
}
