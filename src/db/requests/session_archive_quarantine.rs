use super::super::*;
use super::session_archive::{archive_proof_digest, deterministic_archive_request_id};

#[derive(Clone, Debug)]
pub struct SessionArchiveQuarantineFilter<'a> {
    pub tenant_external_id: &'a str,
    pub state: Option<&'a str>,
    pub limit: i64,
    pub before_started_at: Option<i64>,
    pub before_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionArchiveQuarantineRecordView {
    pub id: Uuid,
    pub tenant_external_id: String,
    pub source: String,
    pub external_request_id: String,
    pub record_digest: String,
    pub reason_code: String,
    pub source_started_at: i64,
    pub source_completed_at: Option<i64>,
    pub protocol: String,
    pub model: String,
    pub status_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error_code: Option<String>,
    pub state: String,
}

pub struct SessionArchiveQuarantineResolutionInput<'a> {
    pub tenant_external_id: &'a str,
    pub quarantine_id: Uuid,
    pub action: &'a str,
    pub key_id: Option<Uuid>,
    pub expected_record_digest: &'a str,
    pub evidence_digest: &'a str,
    pub note: Option<&'a str>,
    pub idempotency_key: &'a str,
    pub resolved_by_service_id: Uuid,
}

#[derive(Clone, Debug, Serialize)]
pub struct SessionArchiveQuarantineResolutionView {
    pub id: Uuid,
    pub quarantine_id: Uuid,
    pub action: String,
    pub key_id: Option<Uuid>,
    pub evidence_digest: String,
    pub created_at: i64,
}

impl Database {
    pub async fn list_session_archive_quarantine(
        &self,
        filter: SessionArchiveQuarantineFilter<'_>,
    ) -> Result<Vec<SessionArchiveQuarantineRecordView>, AppError> {
        if filter
            .state
            .is_some_and(|state| !matches!(state, "pending" | "resolved" | "dismissed"))
        {
            return Err(AppError::BadRequest("invalid quarantine state".into()));
        }
        let before_started_at = filter.before_started_at.unwrap_or(i64::MAX);
        let before_id = filter.before_id.unwrap_or(Uuid::max()).to_string();
        let rows = sqlx::query(
            "SELECT q.id,t.external_id AS tenant_external_id,q.source,q.external_request_id,q.record_digest,q.reason_code,q.source_started_at,q.source_completed_at,q.protocol,q.model,q.status_code,q.duration_ms,q.input_tokens,q.output_tokens,q.error_code,CASE WHEN r.action='dismiss' THEN 'dismissed' WHEN r.id IS NOT NULL THEN 'resolved' ELSE 'pending' END AS state FROM session_archive_quarantine_record_heads h JOIN session_archive_quarantine_record_versions q ON q.id=h.quarantine_id JOIN tenants t ON t.id=q.tenant_id LEFT JOIN session_archive_quarantine_resolutions r ON r.quarantine_id=q.id WHERE t.external_id=$1 AND ($2='' OR CASE WHEN r.action='dismiss' THEN 'dismissed' WHEN r.id IS NOT NULL THEN 'resolved' ELSE 'pending' END=$2) AND (q.source_started_at<$3 OR (q.source_started_at=$3 AND q.id<$4)) ORDER BY q.source_started_at DESC,q.id DESC LIMIT $5",
        )
        .bind(filter.tenant_external_id)
        .bind(filter.state.unwrap_or_default())
        .bind(before_started_at)
        .bind(before_id)
        .bind(filter.limit.clamp(1, 100) + 1)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .take(filter.limit.clamp(1, 100) as usize)
            .map(quarantine_view)
            .collect()
    }

    pub async fn get_session_archive_quarantine(
        &self,
        tenant_external_id: &str,
        quarantine_id: Uuid,
    ) -> Result<SessionArchiveQuarantineRecordView, AppError> {
        let row = sqlx::query(
            "SELECT q.id,t.external_id AS tenant_external_id,q.source,q.external_request_id,q.record_digest,q.reason_code,q.source_started_at,q.source_completed_at,q.protocol,q.model,q.status_code,q.duration_ms,q.input_tokens,q.output_tokens,q.error_code,CASE WHEN r.action='dismiss' THEN 'dismissed' WHEN r.id IS NOT NULL THEN 'resolved' ELSE 'pending' END AS state FROM session_archive_quarantine_record_versions q JOIN tenants t ON t.id=q.tenant_id LEFT JOIN session_archive_quarantine_resolutions r ON r.quarantine_id=q.id WHERE t.external_id=$1 AND q.id=$2",
        )
        .bind(tenant_external_id)
        .bind(quarantine_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        quarantine_view(row)
    }

    pub async fn resolve_session_archive_quarantine(
        &self,
        input: SessionArchiveQuarantineResolutionInput<'_>,
    ) -> Result<SessionArchiveQuarantineResolutionView, AppError> {
        validate_resolution(&input)?;
        let tenant_id = tenant_id(self, input.tenant_external_id).await?;
        let now = unix_millis();
        let mut tx = self.begin_write_transaction().await?;
        let quarantine = sqlx::query("SELECT source,external_request_id,source_session_id,record_digest,proof_digest,source_started_at,source_completed_at,protocol,model,status_code,duration_ms,input_tokens,output_tokens,error_code,request_digest,response_digest,request_object,response_object FROM session_archive_quarantine_record_versions WHERE id=$1 AND tenant_id=$2")
            .bind(input.quarantine_id.to_string()).bind(tenant_id.to_string())
            .fetch_optional(&mut *tx).await?.ok_or(AppError::NotFound)?;
        if quarantine.try_get::<String, _>("record_digest")? != input.expected_record_digest {
            return Err(AppError::Conflict(
                "quarantine record changed before resolution".into(),
            ));
        }
        let (key_id, principal_id) = match (input.action, input.key_id) {
            ("associate", Some(key_id)) => {
                let row = sqlx::query(
                    "SELECT principal_id FROM key_records WHERE id=$1 AND tenant_id=$2",
                )
                .bind(key_id.to_string())
                .bind(tenant_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .ok_or(AppError::NotFound)?;
                (
                    Some(key_id),
                    Some(parse_uuid(row.try_get("principal_id")?)?),
                )
            }
            ("dismiss", None) => (None, None),
            _ => return Err(AppError::BadRequest("invalid quarantine resolution".into())),
        };
        let resolution_id = deterministic_resolution_id(input.quarantine_id, input.idempotency_key);
        let request_digest = resolution_request_digest(&input, key_id);
        let inserted = sqlx::query("INSERT INTO session_archive_quarantine_resolutions (id,tenant_id,quarantine_id,action,key_id,evidence_digest,note,idempotency_key,request_digest,resolved_by_service_id,created_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11) ON CONFLICT(quarantine_id) DO NOTHING")
            .bind(resolution_id.to_string()).bind(tenant_id.to_string())
            .bind(input.quarantine_id.to_string()).bind(input.action)
            .bind(key_id.map(|id| id.to_string())).bind(input.evidence_digest)
            .bind(input.note).bind(input.idempotency_key).bind(&request_digest)
            .bind(input.resolved_by_service_id.to_string()).bind(now)
            .execute(&mut *tx).await?;
        if inserted.rows_affected() == 0 {
            let existing =
                ensure_resolution_replay(&mut tx, &input, resolution_id, key_id, &request_digest)
                    .await?;
            tx.commit().await?;
            return Ok(existing);
        }
        if let (Some(key_id), Some(principal_id)) = (key_id, principal_id) {
            let source: String = quarantine.try_get("source")?;
            let external_request_id: String = quarantine.try_get("external_request_id")?;
            let record_digest: String = quarantine.try_get("record_digest")?;
            let source_started_at: i64 = quarantine.try_get("source_started_at")?;
            let model: String = quarantine.try_get("model")?;
            let identity_proof_digest = archive_proof_digest(
                "memeloop-session-archive-operator-resolution-v1",
                &[
                    input.tenant_external_id,
                    &input.quarantine_id.to_string(),
                    input.evidence_digest,
                    &key_id.to_string(),
                    &principal_id.to_string(),
                ],
            );
            let correlation_proof_digest = archive_proof_digest(
                "memeloop-session-archive-correlation-v1",
                &[
                    input.tenant_external_id,
                    &source,
                    &external_request_id,
                    "unlinked",
                    &key_id.to_string(),
                    &principal_id.to_string(),
                    input.expected_record_digest,
                    &identity_proof_digest,
                ],
            );
            sqlx::query("INSERT INTO session_archive_correlations (tenant_id,source,external_request_id,disposition,key_id,principal_id,target_request_id,target_request_created_at,external_event_hash,record_digest,proof_digest,identity_proof_kind,identity_proof_digest,source_model,source_started_at,correlated_at) VALUES ($1,$2,$3,'unlinked',$4,$5,NULL,NULL,NULL,$6,$7,'operator-evidence-v1',$8,$9,$10,$11)")
                .bind(tenant_id.to_string()).bind(&source).bind(&external_request_id)
                .bind(key_id.to_string()).bind(principal_id.to_string()).bind(&record_digest)
                .bind(&correlation_proof_digest).bind(&identity_proof_digest).bind(&model)
                .bind(source_started_at).bind(now).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO session_archive_unlinked_requests (tenant_id,source,external_request_id,archive_request_id,key_id,principal_id,conversation_cluster_id,source_started_at,source_completed_at,protocol,model,status_code,duration_ms,input_tokens,output_tokens,error_code,request_digest,response_digest,request_object,response_object,imported_at,source_session_id) VALUES ($1,$2,$3,$4,$5,$6,NULL,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19,$20,$21)")
                .bind(tenant_id.to_string()).bind(&source).bind(&external_request_id)
                .bind(input.quarantine_id.to_string()).bind(key_id.to_string()).bind(principal_id.to_string())
                .bind(source_started_at).bind(quarantine.try_get::<Option<i64>,_>("source_completed_at")?)
                .bind(quarantine.try_get::<String,_>("protocol")?).bind(&model)
                .bind(quarantine.try_get::<Option<i64>,_>("status_code")?).bind(quarantine.try_get::<Option<i64>,_>("duration_ms")?)
                .bind(quarantine.try_get::<i64,_>("input_tokens")?).bind(quarantine.try_get::<i64,_>("output_tokens")?)
                .bind(quarantine.try_get::<Option<String>,_>("error_code")?)
                .bind(quarantine.try_get::<Option<String>,_>("request_digest")?).bind(quarantine.try_get::<Option<String>,_>("response_digest")?)
                .bind(quarantine.try_get::<Option<String>,_>("request_object")?).bind(quarantine.try_get::<Option<String>,_>("response_object")?)
                .bind(now).bind(quarantine.try_get::<String,_>("source_session_id")?)
                .execute(&mut *tx).await?;
            add_archive_record_to_session_projection_in_transaction(
                &mut tx,
                tenant_id,
                key_id,
                &source,
                &external_request_id,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(SessionArchiveQuarantineResolutionView {
            id: resolution_id,
            quarantine_id: input.quarantine_id,
            action: input.action.to_owned(),
            key_id,
            evidence_digest: input.evidence_digest.to_owned(),
            created_at: now,
        })
    }
}

async fn tenant_id(db: &Database, external_id: &str) -> Result<Uuid, AppError> {
    let value: String = sqlx::query_scalar("SELECT id FROM tenants WHERE external_id=$1")
        .bind(external_id)
        .fetch_optional(&db.pool)
        .await?
        .ok_or(AppError::NotFound)?;
    parse_uuid(value)
}

fn validate_resolution(
    input: &SessionArchiveQuarantineResolutionInput<'_>,
) -> Result<(), AppError> {
    validate_idempotency_key(input.idempotency_key, "Idempotency-Key")?;
    if !matches!(input.action, "associate" | "dismiss")
        || !is_sha256_hex(input.expected_record_digest)
        || !is_sha256_hex(input.evidence_digest)
        || input.note.is_some_and(|note| {
            note.len() > 2_000 || note.bytes().any(|byte| byte.is_ascii_control())
        })
    {
        return Err(AppError::BadRequest(
            "invalid quarantine resolution proof".into(),
        ));
    }
    Ok(())
}

fn resolution_request_digest(
    input: &SessionArchiveQuarantineResolutionInput<'_>,
    key_id: Option<Uuid>,
) -> String {
    let quarantine_id = input.quarantine_id.to_string();
    let key_id = key_id.map(|id| id.to_string());
    let service_id = input.resolved_by_service_id.to_string();
    archive_proof_digest(
        "memeloop-session-archive-quarantine-resolution-request-v1",
        &[
            input.tenant_external_id,
            &quarantine_id,
            input.action,
            key_id.as_deref().unwrap_or("none"),
            input.expected_record_digest,
            input.evidence_digest,
            input.note.unwrap_or(""),
            input.idempotency_key,
            &service_id,
        ],
    )
}

fn deterministic_resolution_id(quarantine_id: Uuid, idempotency_key: &str) -> Uuid {
    deterministic_archive_request_id(&quarantine_id.to_string(), "resolution", idempotency_key)
}

async fn ensure_resolution_replay(
    tx: &mut Transaction<'static, Any>,
    input: &SessionArchiveQuarantineResolutionInput<'_>,
    resolution_id: Uuid,
    key_id: Option<Uuid>,
    request_digest: &str,
) -> Result<SessionArchiveQuarantineResolutionView, AppError> {
    let row = sqlx::query("SELECT id,action,key_id,evidence_digest,note,idempotency_key,request_digest,resolved_by_service_id,created_at FROM session_archive_quarantine_resolutions WHERE quarantine_id=$1")
        .bind(input.quarantine_id.to_string()).fetch_one(&mut **tx).await?;
    let expected_service_id = input.resolved_by_service_id.to_string();
    let compatible = row.try_get::<String, _>("id")? == resolution_id.to_string()
        && row.try_get::<String, _>("action")? == input.action
        && row.try_get::<Option<String>, _>("key_id")? == key_id.map(|id| id.to_string())
        && row.try_get::<String, _>("evidence_digest")? == input.evidence_digest
        && row.try_get::<Option<String>, _>("note")?.as_deref() == input.note
        && row.try_get::<String, _>("idempotency_key")? == input.idempotency_key
        && row.try_get::<String, _>("request_digest")? == request_digest
        && row
            .try_get::<Option<String>, _>("resolved_by_service_id")?
            .as_deref()
            == Some(expected_service_id.as_str());
    if !compatible {
        return Err(AppError::Conflict(
            "quarantine already has a different final resolution".into(),
        ));
    }
    Ok(SessionArchiveQuarantineResolutionView {
        id: resolution_id,
        quarantine_id: input.quarantine_id,
        action: input.action.to_owned(),
        key_id,
        evidence_digest: input.evidence_digest.to_owned(),
        created_at: row.try_get("created_at")?,
    })
}

fn quarantine_view(row: AnyRow) -> Result<SessionArchiveQuarantineRecordView, AppError> {
    Ok(SessionArchiveQuarantineRecordView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        source: row.try_get("source")?,
        external_request_id: row.try_get("external_request_id")?,
        record_digest: row.try_get("record_digest")?,
        reason_code: row.try_get("reason_code")?,
        source_started_at: row.try_get("source_started_at")?,
        source_completed_at: row.try_get("source_completed_at")?,
        protocol: row.try_get("protocol")?,
        model: row.try_get("model")?,
        status_code: row.try_get("status_code")?,
        duration_ms: row.try_get("duration_ms")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        error_code: row.try_get("error_code")?,
        state: row.try_get("state")?,
    })
}
