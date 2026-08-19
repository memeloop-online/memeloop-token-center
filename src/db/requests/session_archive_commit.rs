use super::super::*;

pub struct SessionArchiveUnlinkedMetadata<'a> {
    pub source_completed_at: Option<i64>,
    pub protocol: &'a str,
    pub model: &'a str,
    pub status_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub error_code: Option<&'a str>,
}

pub struct SessionArchiveCommitInput<'a> {
    pub tenant_external_id: &'a str,
    pub archive_source: &'a str,
    pub external_request_id: &'a str,
    pub target: &'a SessionArchiveTarget,
    pub record_digest: &'a str,
    pub request_digest: Option<&'a str>,
    pub response_digest: Option<&'a str>,
    pub request_object: Option<&'a str>,
    pub response_object: Option<&'a str>,
    pub request_json: Option<&'a serde_json::Value>,
    pub conversation_hints: &'a ConversationHints,
    pub client_name: Option<&'a str>,
    pub source_started_at: i64,
    /// The source completion time. `None` is accepted only for legacy/direct
    /// callers and makes the checkpoint cursor fall back to `source_started_at`.
    pub source_completed_at: Option<i64>,
    pub identity_proof_kind: &'a str,
    pub identity_proof_digest: &'a str,
    pub correlation_proof_digest: &'a str,
}

pub struct SessionArchiveUnlinkedCommitInput<'a> {
    pub tenant_external_id: &'a str,
    pub archive_source: &'a str,
    pub external_request_id: &'a str,
    pub target: &'a SessionArchiveUnlinkedTarget,
    pub record_digest: &'a str,
    pub request_digest: Option<&'a str>,
    pub response_digest: Option<&'a str>,
    pub request_object: Option<&'a str>,
    pub response_object: Option<&'a str>,
    pub request_json: Option<&'a serde_json::Value>,
    pub conversation_hints: &'a ConversationHints,
    pub client_name: Option<&'a str>,
    pub source_started_at: i64,
    pub metadata: SessionArchiveUnlinkedMetadata<'a>,
}

impl Database {
    pub async fn commit_session_archive_request(
        &self,
        input: SessionArchiveCommitInput<'_>,
    ) -> Result<bool, AppError> {
        let checkpoint_ms =
            session_archive_checkpoint_ms(input.source_started_at, input.source_completed_at)?;
        if input.target.tenant_id != input.target.key.tenant_id
            || !is_sha256_hex(input.record_digest)
            || !is_sha256_hex(input.identity_proof_digest)
            || !is_sha256_hex(input.correlation_proof_digest)
            || !valid_archive_identifier(input.archive_source, 256)
            || !valid_archive_identifier(input.external_request_id, 512)
            || !valid_archive_identifier(input.identity_proof_kind, 128)
            || input
                .request_digest
                .is_some_and(|digest| !is_sha256_hex(digest))
            || input
                .response_digest
                .is_some_and(|digest| !is_sha256_hex(digest))
        {
            return Err(AppError::BadRequest(
                "exact archive correlation proof is invalid".into(),
            ));
        }
        let tenant_matches: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tenants WHERE id = $1 AND external_id = $2")
                .bind(input.target.tenant_id.to_string())
                .bind(input.tenant_external_id)
                .fetch_one(&self.pool)
                .await?;
        if tenant_matches != 1 {
            return Err(AppError::NotFound);
        }

        // Refuse protected targets before creating semantic atoms or conversation
        // projections. The transactional check below is repeated to close the race.
        let protected = sqlx::query(
            "SELECT request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND tenant_id = $3",
        )
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(input.target.tenant_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let protected_request: String = protected.try_get("request_object")?;
        let protected_response: Option<String> = protected.try_get("response_object")?;
        replacement_for_gap(&protected_request, input.request_object)?;
        if let Some(current) = protected_response.as_deref() {
            replacement_for_gap(current, input.response_object)?;
        }

        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let current = sqlx::query(
            "SELECT request_object, response_object FROM request_records WHERE id = $1 AND created_at = $2 AND tenant_id = $3",
        )
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(input.target.tenant_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let current_request: String = current.try_get("request_object")?;
        let current_response: Option<String> = current.try_get("response_object")?;
        let next_request = replacement_for_gap(&current_request, input.request_object)?;
        let next_response = match (current_response.as_deref(), input.response_object) {
            (Some(current), replacement) => replacement_for_gap(current, replacement)?,
            (None, Some(replacement)) => Some(replacement.to_owned()),
            (None, None) => None,
        };

        let correlation_inserted = sqlx::query(
            "INSERT INTO session_archive_correlations (tenant_id, source, external_request_id, disposition, key_id, principal_id, target_request_id, target_request_created_at, external_event_hash, record_digest, proof_digest, identity_proof_kind, identity_proof_digest, source_model, source_started_at, correlated_at) VALUES ($1, $2, $3, 'exact', $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15) ON CONFLICT(tenant_id, source, external_request_id) DO NOTHING",
        )
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .bind(input.target.key.key_id.to_string())
        .bind(input.target.key.principal_id.to_string())
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(&input.target.external_event_hash)
        .bind(input.record_digest)
        .bind(input.correlation_proof_digest)
        .bind(input.identity_proof_kind)
        .bind(input.identity_proof_digest)
        .bind(&input.target.source_model)
        .bind(input.source_started_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if correlation_inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT disposition, key_id, principal_id, target_request_id, target_request_created_at, external_event_hash, record_digest, proof_digest, identity_proof_kind, identity_proof_digest, source_model, source_started_at FROM session_archive_correlations WHERE tenant_id = $1 AND source = $2 AND external_request_id = $3",
            )
            .bind(input.target.tenant_id.to_string())
            .bind(input.archive_source)
            .bind(input.external_request_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Internal)?;
            let expected_target_request_id = input.target.target_request_id.to_string();
            let compatible = existing.try_get::<String, _>("disposition")? == "exact"
                && existing.try_get::<String, _>("key_id")? == input.target.key.key_id.to_string()
                && existing.try_get::<String, _>("principal_id")?
                    == input.target.key.principal_id.to_string()
                && existing
                    .try_get::<Option<String>, _>("target_request_id")?
                    .as_deref()
                    == Some(expected_target_request_id.as_str())
                && existing.try_get::<Option<i64>, _>("target_request_created_at")?
                    == Some(input.target.request_created_at)
                && existing
                    .try_get::<Option<String>, _>("external_event_hash")?
                    .as_deref()
                    == Some(input.target.external_event_hash.as_str())
                && existing.try_get::<String, _>("record_digest")? == input.record_digest
                && existing.try_get::<String, _>("proof_digest")? == input.correlation_proof_digest
                && existing.try_get::<String, _>("identity_proof_kind")?
                    == input.identity_proof_kind
                && existing.try_get::<String, _>("identity_proof_digest")?
                    == input.identity_proof_digest
                && existing.try_get::<String, _>("source_model")? == input.target.source_model
                && existing.try_get::<i64, _>("source_started_at")? == input.source_started_at;
            if !compatible {
                return Err(AppError::BadRequest(
                    "archive correlation changed while it was being imported".into(),
                ));
            }
        }

        let inserted = sqlx::query(
            "INSERT INTO session_archive_import_records (tenant_id, source, external_request_id, target_request_id, external_event_hash, record_digest, request_digest, response_digest, request_object, response_object, source_started_at, imported_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12) ON CONFLICT(tenant_id, source, external_request_id) DO NOTHING",
        )
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .bind(input.target.target_request_id.to_string())
        .bind(&input.target.external_event_hash)
        .bind(input.record_digest)
        .bind(input.request_digest)
        .bind(input.response_digest)
        .bind(input.request_object)
        .bind(input.response_object)
        .bind(input.source_started_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT target_request_id, external_event_hash, record_digest, request_digest, response_digest, request_object, response_object, source_started_at FROM session_archive_import_records WHERE tenant_id = $1 AND source = $2 AND external_request_id = $3",
            )
            .bind(input.target.tenant_id.to_string())
            .bind(input.archive_source)
            .bind(input.external_request_id)
            .fetch_optional(&mut *tx)
            .await?;
            let compatible_replay = existing
                .map(|row| {
                    Ok::<_, AppError>(
                        row.try_get::<String, _>("target_request_id")?
                            == input.target.target_request_id.to_string()
                            && row.try_get::<String, _>("external_event_hash")?
                                == input.target.external_event_hash
                            && row.try_get::<String, _>("record_digest")? == input.record_digest
                            && row
                                .try_get::<Option<String>, _>("request_digest")?
                                .as_deref()
                                == input.request_digest
                            && row
                                .try_get::<Option<String>, _>("response_digest")?
                                .as_deref()
                                == input.response_digest
                            && row
                                .try_get::<Option<String>, _>("request_object")?
                                .as_deref()
                                == input.request_object
                            && row
                                .try_get::<Option<String>, _>("response_object")?
                                .as_deref()
                                == input.response_object
                            && row.try_get::<i64, _>("source_started_at")?
                                == input.source_started_at,
                    )
                })
                .transpose()?
                .unwrap_or(false);
            return if compatible_replay {
                advance_session_archive_checkpoint(
                    &mut tx,
                    input.target.tenant_id,
                    input.archive_source,
                    checkpoint_ms,
                    input.external_request_id,
                    false,
                    now,
                )
                .await?;
                tx.commit().await?;
                Ok(false)
            } else {
                tx.rollback().await?;
                Err(AppError::BadRequest(
                    "archive request changed while it was being imported".into(),
                ))
            };
        }

        if let Some(request_json) = input.request_json {
            let existing = sqlx::query(
                "SELECT cluster_id FROM conversation_observations WHERE request_id = $1",
            )
            .bind(input.target.target_request_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            if existing.is_none() {
                self.record_conversation_observation_in_transaction(
                    &mut tx,
                    ConversationObservationInput {
                        key: &input.target.key,
                        request_id: input.target.target_request_id,
                        request_json,
                        hints: input.conversation_hints,
                        client_name: input.client_name,
                        observed_at: input.source_started_at,
                        attach_request_record: true,
                    },
                )
                .await?;
            }
        }

        let updated = sqlx::query(
            "UPDATE request_records SET request_object = $1, response_object = $2 WHERE id = $3 AND created_at = $4 AND tenant_id = $5 AND request_object = $6 AND ((response_object IS NULL AND $7 IS NULL) OR response_object = $7)",
        )
        .bind(next_request.unwrap_or_else(|| current_request.clone()))
        .bind(next_response)
        .bind(input.target.target_request_id.to_string())
        .bind(input.target.request_created_at)
        .bind(input.target.tenant_id.to_string())
        .bind(&current_request)
        .bind(current_response.as_deref())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            tx.rollback().await?;
            return Err(AppError::BadRequest(
                "archive target changed after preflight".into(),
            ));
        }
        advance_session_archive_checkpoint(
            &mut tx,
            input.target.tenant_id,
            input.archive_source,
            checkpoint_ms,
            input.external_request_id,
            true,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    pub async fn commit_session_archive_unlinked_request(
        &self,
        input: SessionArchiveUnlinkedCommitInput<'_>,
    ) -> Result<bool, AppError> {
        let checkpoint_ms = session_archive_checkpoint_ms(
            input.source_started_at,
            input.metadata.source_completed_at,
        )?;
        if input.target.tenant_id != input.target.key.tenant_id
            || !is_sha256_hex(input.record_digest)
            || !is_sha256_hex(&input.target.identity_proof_digest)
            || !is_sha256_hex(&input.target.correlation_proof_digest)
            || !valid_archive_identifier(input.archive_source, 256)
            || !valid_archive_identifier(input.external_request_id, 512)
            || !valid_archive_identifier(&input.target.identity_proof_kind, 128)
            || input
                .request_digest
                .is_some_and(|digest| !is_sha256_hex(digest))
            || input
                .response_digest
                .is_some_and(|digest| !is_sha256_hex(digest))
            || input.metadata.protocol.trim().is_empty()
            || input.metadata.model.trim().is_empty()
            || input.metadata.protocol.len() > 256
            || input.metadata.model.len() > 512
            || input.metadata.input_tokens < 0
            || input.metadata.output_tokens < 0
        {
            return Err(AppError::BadRequest(
                "archive-only request metadata or proof is invalid".into(),
            ));
        }
        let tenant_matches: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM tenants WHERE id = $1 AND external_id = $2")
                .bind(input.target.tenant_id.to_string())
                .bind(input.tenant_external_id)
                .fetch_one(&self.pool)
                .await?;
        if tenant_matches != 1 {
            return Err(AppError::NotFound);
        }

        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let inserted = sqlx::query(
            "INSERT INTO session_archive_correlations (tenant_id, source, external_request_id, disposition, key_id, principal_id, target_request_id, target_request_created_at, external_event_hash, record_digest, proof_digest, identity_proof_kind, identity_proof_digest, source_model, source_started_at, correlated_at) VALUES ($1, $2, $3, 'unlinked', $4, $5, NULL, NULL, NULL, $6, $7, $8, $9, $10, $11, $12) ON CONFLICT(tenant_id, source, external_request_id) DO NOTHING",
        )
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .bind(input.target.key.key_id.to_string())
        .bind(input.target.key.principal_id.to_string())
        .bind(input.record_digest)
        .bind(&input.target.correlation_proof_digest)
        .bind(&input.target.identity_proof_kind)
        .bind(&input.target.identity_proof_digest)
        .bind(input.metadata.model)
        .bind(input.source_started_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT c.disposition, c.key_id, c.principal_id, c.record_digest, c.proof_digest, c.identity_proof_kind, c.identity_proof_digest, c.source_model, c.source_started_at, u.archive_request_id, u.source_completed_at, u.protocol, u.model, u.status_code, u.duration_ms, u.input_tokens, u.output_tokens, u.error_code, u.request_digest, u.response_digest, u.request_object, u.response_object FROM session_archive_correlations c LEFT JOIN session_archive_unlinked_requests u ON u.tenant_id = c.tenant_id AND u.source = c.source AND u.external_request_id = c.external_request_id WHERE c.tenant_id = $1 AND c.source = $2 AND c.external_request_id = $3",
            )
            .bind(input.target.tenant_id.to_string())
            .bind(input.archive_source)
            .bind(input.external_request_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Internal)?;
            let expected_archive_request_id = input.target.archive_request_id.to_string();
            let compatible = existing.try_get::<String, _>("disposition")? == "unlinked"
                && existing.try_get::<String, _>("key_id")? == input.target.key.key_id.to_string()
                && existing.try_get::<String, _>("principal_id")?
                    == input.target.key.principal_id.to_string()
                && existing.try_get::<String, _>("record_digest")? == input.record_digest
                && existing.try_get::<String, _>("proof_digest")?
                    == input.target.correlation_proof_digest
                && existing.try_get::<String, _>("identity_proof_kind")?
                    == input.target.identity_proof_kind
                && existing.try_get::<String, _>("identity_proof_digest")?
                    == input.target.identity_proof_digest
                && existing.try_get::<String, _>("source_model")? == input.metadata.model
                && existing.try_get::<i64, _>("source_started_at")? == input.source_started_at
                && existing
                    .try_get::<Option<String>, _>("archive_request_id")?
                    .as_deref()
                    == Some(expected_archive_request_id.as_str())
                && existing.try_get::<Option<i64>, _>("source_completed_at")?
                    == input.metadata.source_completed_at
                && existing
                    .try_get::<Option<String>, _>("protocol")?
                    .as_deref()
                    == Some(input.metadata.protocol)
                && existing.try_get::<Option<String>, _>("model")?.as_deref()
                    == Some(input.metadata.model)
                && existing.try_get::<Option<i64>, _>("status_code")? == input.metadata.status_code
                && existing.try_get::<Option<i64>, _>("duration_ms")? == input.metadata.duration_ms
                && existing.try_get::<Option<i64>, _>("input_tokens")?
                    == Some(input.metadata.input_tokens)
                && existing.try_get::<Option<i64>, _>("output_tokens")?
                    == Some(input.metadata.output_tokens)
                && existing
                    .try_get::<Option<String>, _>("error_code")?
                    .as_deref()
                    == input.metadata.error_code
                && existing
                    .try_get::<Option<String>, _>("request_digest")?
                    .as_deref()
                    == input.request_digest
                && existing
                    .try_get::<Option<String>, _>("response_digest")?
                    .as_deref()
                    == input.response_digest
                && existing
                    .try_get::<Option<String>, _>("request_object")?
                    .as_deref()
                    == input.request_object
                && existing
                    .try_get::<Option<String>, _>("response_object")?
                    .as_deref()
                    == input.response_object;
            if compatible {
                advance_session_archive_checkpoint(
                    &mut tx,
                    input.target.tenant_id,
                    input.archive_source,
                    checkpoint_ms,
                    input.external_request_id,
                    false,
                    now,
                )
                .await?;
                tx.commit().await?;
                return Ok(false);
            }
            tx.rollback().await?;
            return Err(AppError::BadRequest(
                "archive-only request changed while it was being imported".into(),
            ));
        }

        sqlx::query(
            "INSERT INTO session_archive_unlinked_requests (tenant_id, source, external_request_id, archive_request_id, key_id, principal_id, conversation_cluster_id, source_started_at, source_completed_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, error_code, request_digest, response_digest, request_object, response_object, imported_at) VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20)",
        )
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .bind(input.target.archive_request_id.to_string())
        .bind(input.target.key.key_id.to_string())
        .bind(input.target.key.principal_id.to_string())
        .bind(input.source_started_at)
        .bind(input.metadata.source_completed_at)
        .bind(input.metadata.protocol)
        .bind(input.metadata.model)
        .bind(input.metadata.status_code)
        .bind(input.metadata.duration_ms)
        .bind(input.metadata.input_tokens)
        .bind(input.metadata.output_tokens)
        .bind(input.metadata.error_code)
        .bind(input.request_digest)
        .bind(input.response_digest)
        .bind(input.request_object)
        .bind(input.response_object)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let empty_request = serde_json::Value::Null;
        let cluster_id = self
            .record_conversation_observation_in_transaction(
                &mut tx,
                ConversationObservationInput {
                    key: &input.target.key,
                    request_id: input.target.archive_request_id,
                    request_json: input.request_json.unwrap_or(&empty_request),
                    hints: input.conversation_hints,
                    client_name: input.client_name,
                    observed_at: input.source_started_at,
                    attach_request_record: false,
                },
            )
            .await?;
        let attached = sqlx::query(
            "UPDATE session_archive_unlinked_requests SET conversation_cluster_id = $1 WHERE tenant_id = $2 AND source = $3 AND external_request_id = $4 AND conversation_cluster_id IS NULL",
        )
        .bind(cluster_id.to_string())
        .bind(input.target.tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .execute(&mut *tx)
        .await?;
        if attached.rows_affected() != 1 {
            return Err(AppError::Internal);
        }

        advance_session_archive_checkpoint(
            &mut tx,
            input.target.tenant_id,
            input.archive_source,
            checkpoint_ms,
            input.external_request_id,
            true,
            now,
        )
        .await?;
        tx.commit().await?;
        Ok(true)
    }
}

fn replacement_for_gap(
    current: &str,
    replacement: Option<&str>,
) -> Result<Option<String>, AppError> {
    let Some(replacement) = replacement else {
        return Ok(Some(current.to_owned()));
    };
    if current.starts_with("gap://") || current == replacement {
        return Ok(Some(replacement.to_owned()));
    }
    Err(AppError::BadRequest(
        "archive import refused to overwrite an existing object".into(),
    ))
}

pub(super) fn session_archive_checkpoint_ms(
    source_started_at: i64,
    source_completed_at: Option<i64>,
) -> Result<i64, AppError> {
    if source_started_at < 0
        || source_completed_at.is_some_and(|completed_at| completed_at < source_started_at)
    {
        return Err(AppError::BadRequest(
            "archive request time range is invalid".into(),
        ));
    }
    // A CPA archive record always carries completed_at. None remains a narrowly
    // defined compatibility path for legacy/direct callers and advances by start.
    Ok(source_completed_at.unwrap_or(source_started_at))
}

pub(super) async fn advance_session_archive_checkpoint(
    tx: &mut Transaction<'_, Any>,
    tenant_id: Uuid,
    archive_source: &str,
    checkpoint_ms: i64,
    external_request_id: &str,
    imported: bool,
    now: i64,
) -> Result<(), AppError> {
    if imported {
        sqlx::query(
            "INSERT INTO session_archive_import_checkpoints (tenant_id, source, watermark_ms, watermark_request_id, imported_records, updated_at) VALUES ($1, $2, $3, $4, 1, $5) ON CONFLICT(tenant_id, source) DO UPDATE SET watermark_ms = CASE WHEN excluded.watermark_ms > session_archive_import_checkpoints.watermark_ms THEN excluded.watermark_ms ELSE session_archive_import_checkpoints.watermark_ms END, watermark_request_id = CASE WHEN excluded.watermark_ms > session_archive_import_checkpoints.watermark_ms OR (excluded.watermark_ms = session_archive_import_checkpoints.watermark_ms AND excluded.watermark_request_id > session_archive_import_checkpoints.watermark_request_id) THEN excluded.watermark_request_id ELSE session_archive_import_checkpoints.watermark_request_id END, imported_records = session_archive_import_checkpoints.imported_records + 1, updated_at = excluded.updated_at",
        )
        .bind(tenant_id.to_string())
        .bind(archive_source)
        .bind(checkpoint_ms)
        .bind(external_request_id)
        .bind(now)
        .execute(&mut **tx)
        .await?;
        return Ok(());
    }

    // A replay must never change imported_records. It may only upgrade a legacy
    // start-based checkpoint to the completed-at cursor already bound by the
    // immutable record digest. A missing checkpoint is corrupt importer state.
    let updated = sqlx::query(
        "UPDATE session_archive_import_checkpoints SET watermark_ms = CASE WHEN $3 > watermark_ms THEN $3 ELSE watermark_ms END, watermark_request_id = CASE WHEN $3 > watermark_ms OR ($3 = watermark_ms AND $4 > watermark_request_id) THEN $4 ELSE watermark_request_id END, updated_at = $5 WHERE tenant_id = $1 AND source = $2",
    )
    .bind(tenant_id.to_string())
    .bind(archive_source)
    .bind(checkpoint_ms)
    .bind(external_request_id)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    if updated.rows_affected() != 1 {
        return Err(AppError::Internal);
    }
    Ok(())
}
