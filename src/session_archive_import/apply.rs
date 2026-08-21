use super::*;

pub(super) async fn apply_validated_plan(
    db: &Database,
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
    plan: &mut ValidatedImportPlan,
) -> Result<ApplyStats, Box<dyn std::error::Error + Send + Sync>> {
    let mut applied = ApplyStats::default();
    let mut seen = 0_u64;
    let mut rows = sqlx::query(
        "SELECT sequence, source_started_at, source_checkpoint_ms, external_request_id, record_json FROM import_plan_records ORDER BY source_checkpoint_ms ASC, external_request_id ASC, sequence ASC",
    )
    .fetch(&mut plan.connection);
    while let Some(row) = rows.try_next().await? {
        seen += 1;
        let encoded: Vec<u8> = row.try_get("record_json")?;
        let record: ImportPlanRecord = serde_json::from_slice(&encoded)?;
        validate_plan_record(&record)?;
        if row.try_get::<i64, _>("source_started_at")? != record.source_started_at
            || row.try_get::<i64, _>("source_checkpoint_ms")? != record.source_checkpoint_ms
            || row.try_get::<String, _>("external_request_id")? != record.external_request_id
        {
            return Err(plan_changed_error().into());
        }
        let request_json = load_structured_plan_request(archive, options, &record).await?;
        let committed = match &record.correlation {
            ImportPlanCorrelation::Exact {
                target,
                identity_proof_kind,
                identity_proof_digest,
                correlation_proof_digest,
            } => {
                let target: SessionArchiveTarget = target.clone().into();
                db.commit_session_archive_request(SessionArchiveCommitInput {
                    tenant_external_id: options.tenant_external_id,
                    archive_source: options.archive_source,
                    external_request_id: &record.external_request_id,
                    target: &target,
                    record_digest: &record.record_digest,
                    request_digest: record.request_digest.as_deref(),
                    response_digest: record.response_digest.as_deref(),
                    request_object: record.request_object.as_deref(),
                    response_object: record.response_object.as_deref(),
                    request_json: request_json.as_ref(),
                    conversation_hints: &record.conversation_hints,
                    client_name: record.client_name.as_deref(),
                    source_started_at: record.source_started_at,
                    source_completed_at: record.source_completed_at,
                    identity_proof_kind,
                    identity_proof_digest,
                    correlation_proof_digest,
                })
                .await?
            }
            ImportPlanCorrelation::Unlinked { target } => {
                let target: SessionArchiveUnlinkedTarget = target.clone().into();
                db.commit_session_archive_unlinked_request(SessionArchiveUnlinkedCommitInput {
                    tenant_external_id: options.tenant_external_id,
                    archive_source: options.archive_source,
                    external_request_id: &record.external_request_id,
                    target: &target,
                    record_digest: &record.record_digest,
                    request_digest: record.request_digest.as_deref(),
                    response_digest: record.response_digest.as_deref(),
                    request_object: record.request_object.as_deref(),
                    response_object: record.response_object.as_deref(),
                    request_json: request_json.as_ref(),
                    conversation_hints: &record.conversation_hints,
                    client_name: record.client_name.as_deref(),
                    source_started_at: record.source_started_at,
                    metadata: SessionArchiveUnlinkedMetadata {
                        source_completed_at: record.source_completed_at,
                        protocol: &record.protocol,
                        model: &record.model,
                        status_code: record.status_code,
                        duration_ms: record.duration_ms,
                        input_tokens: record.input_tokens,
                        output_tokens: record.output_tokens,
                        error_code: record.error_code.as_deref(),
                    },
                })
                .await?
            }
            ImportPlanCorrelation::Quarantined { target } => {
                let target: SessionArchiveQuarantineTarget = target.clone().into();
                let header = &plan.header;
                let batch_id = header.quarantine_batch_id.ok_or_else(plan_changed_error)?;
                let source_size_bytes = i64::try_from(header.source_size_bytes)?;
                let eligible_records = i64::try_from(header.record_count)?;
                let quarantine_records = i64::try_from(header.quarantine_records)?;
                let sequence: i64 = row.try_get("sequence")?;
                let committed = db
                    .commit_session_archive_quarantine(SessionArchiveQuarantineCommitInput {
                        batch: SessionArchiveQuarantineBatchInput {
                            batch_id,
                            tenant_external_id: options.tenant_external_id,
                            archive_source: options.archive_source,
                            cpamp_source: options.cpamp_source,
                            source_digest: &header.source_blake3,
                            source_size_bytes,
                            eligible_records,
                            quarantine_records,
                            tenant_binding_kind: header
                                .tenant_binding_kind
                                .as_deref()
                                .ok_or_else(plan_changed_error)?,
                            tenant_binding_proof: header
                                .tenant_binding_proof
                                .as_deref()
                                .ok_or_else(plan_changed_error)?,
                            approved_by_service_id: header.approved_by_service_id,
                        },
                        sequence,
                        target: &target,
                        external_request_id: &record.external_request_id,
                        record_digest: &record.record_digest,
                        source_started_at: record.source_started_at,
                        source_completed_at: record.source_completed_at,
                        protocol: &record.protocol,
                        model: &record.model,
                        status_code: record.status_code,
                        duration_ms: record.duration_ms,
                        input_tokens: record.input_tokens,
                        output_tokens: record.output_tokens,
                        error_code: record.error_code.as_deref(),
                        request_digest: record.request_digest.as_deref(),
                        response_digest: record.response_digest.as_deref(),
                        request_object: record.request_object.as_deref(),
                        response_object: record.response_object.as_deref(),
                    })
                    .await?;
                applied.quarantine_imported += u64::from(committed);
                applied.quarantine_replayed += u64::from(!committed);
                false
            }
        };
        applied.imported += u64::from(committed);
    }
    if seen != plan.record_count {
        return Err(plan_changed_error().into());
    }
    Ok(applied)
}

#[derive(Default)]
pub(super) struct ApplyStats {
    pub(super) imported: u64,
    pub(super) quarantine_imported: u64,
    pub(super) quarantine_replayed: u64,
}

pub(super) async fn load_structured_plan_request(
    archive: &ArchiveStore,
    options: &SessionArchiveImportOptions<'_>,
    record: &ImportPlanRecord,
) -> Result<Option<Value>, Box<dyn std::error::Error + Send + Sync>> {
    if !record.request_is_structured {
        return Ok(None);
    }
    let location = record
        .request_object
        .as_deref()
        .ok_or_else(plan_changed_error)?;
    let expected_digest = record
        .request_digest
        .as_deref()
        .ok_or_else(plan_changed_error)?;
    let body = archive
        .get_bounded(location, options.max_line_bytes)
        .await?;
    if digest(&body) != expected_digest {
        return Err("planned request CAS object failed its content digest".into());
    }
    let value: Value = serde_json::from_slice(&body)?;
    if !matches!(value, Value::Array(_) | Value::Object(_)) {
        return Err(plan_changed_error().into());
    }
    Ok(Some(value))
}
