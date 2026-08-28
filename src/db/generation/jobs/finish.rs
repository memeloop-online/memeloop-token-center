use super::*;

impl Database {
    pub async fn finish_generation_job(
        &self,
        input: FinishGenerationJobInput<'_>,
    ) -> Result<i64, AppError> {
        if !matches!(input.status, "succeeded" | "failed" | "cancelled") {
            return Err(AppError::BadRequest(
                "invalid terminal generation status".into(),
            ));
        }
        if input.status == "succeeded" {
            if input.billed_units <= 0 || input.error_code.is_some() || input.assets.is_empty() {
                return Err(AppError::BadRequest(
                    "a successful generation requires billed units and archived assets".into(),
                ));
            }
            if input.assets.iter().any(|asset| {
                asset.index < 0
                    || asset.object_locator.trim().is_empty()
                    || asset.mime_type.trim().is_empty()
                    || asset.size_bytes <= 0
                    || asset.filename.trim().is_empty()
            }) {
                return Err(AppError::BadRequest(
                    "a successful generation contains an invalid archived asset".into(),
                ));
            }
            if input.assets.iter().enumerate().any(|(index, asset)| {
                input.assets[index + 1..]
                    .iter()
                    .any(|other| other.asset_id == asset.asset_id || other.index == asset.index)
            }) {
                return Err(AppError::BadRequest(
                    "a successful generation contains duplicate archived assets".into(),
                ));
            }
            if input.staged_assets.is_some_and(|staged| {
                staged.billed_units != input.billed_units || staged.assets != input.assets
            }) {
                return Err(AppError::BadRequest(
                    "a successful generation must match its staged asset manifest".into(),
                ));
            }
        } else {
            let allowed_billed_failure = input.error_code
                == Some("upstream_usage_exceeds_contract")
                && input.billed_units > 0;
            if (!allowed_billed_failure && input.billed_units != 0)
                || !input.assets.is_empty()
                || !input
                    .error_code
                    .is_some_and(is_allowed_generation_error_code)
            {
                return Err(AppError::BadRequest(
                    "a failed generation requires a fixed error code and valid billing units"
                        .into(),
                ));
            }
        }
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT j.status, j.lease_owner, j.tenant_id, j.key_id, j.driver, j.created_at, j.estimated_units, j.billed_units, j.cost_micros, j.result_json, j.error_code, j.staged_assets_json, j.reservation_id, j.billing_unit_snapshot, j.micros_per_unit_snapshot, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT j.status, j.lease_owner, j.tenant_id, j.key_id, j.driver, j.created_at, j.estimated_units, j.billed_units, j.cost_micros, j.result_json, j.error_code, j.staged_assets_json, j.reservation_id, j.billing_unit_snapshot, j.micros_per_unit_snapshot, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, r.status AS reservation_status, r.actual_micros FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id WHERE j.id = $1"
            }
        };
        let job = sqlx::query(select)
            .bind(input.job_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        let current_status: String = job.try_get("status")?;
        let persisted_staged_assets = job
            .try_get::<Option<String>, _>("staged_assets_json")?
            .map(|value| serde_json::from_str::<GenerationStagedAssets>(&value))
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let driver: String = job.try_get("driver")?;
        let estimated_units: i64 = job.try_get("estimated_units")?;
        if input.billed_units > estimated_units {
            return Err(AppError::BadRequest(
                "generation billed units exceed the reserved estimate".into(),
            ));
        }
        if input.status == "succeeded"
            && match driver.as_str() {
                "volcengine-seedance" => {
                    input.assets.len() != 1 || !input.assets[0].mime_type.starts_with("video/")
                }
                "comfyui" => !(1..=16).contains(&input.assets.len()),
                "http-json" => {
                    input.assets.len() != 1 || !input.assets[0].mime_type.starts_with("video/")
                }
                _ => true,
            }
        {
            return Err(AppError::BadRequest(
                "generation driver returned an invalid number of archived assets".into(),
            ));
        }

        let key_id = parse_uuid(job.try_get("key_id")?)?;
        let micros_per_unit: i64 = job.try_get("micros_per_unit_snapshot")?;
        let billing_unit: String = job.try_get("billing_unit_snapshot")?;
        let reservation = UsageReservation {
            id: parse_uuid(job.try_get("reservation_id")?)?,
            account_id: parse_uuid(job.try_get("account_id")?)?,
            key_id,
            reserved_micros: job.try_get("reserved_micros")?,
            input_micros_per_million: 0,
            output_micros_per_million: if billing_unit == "megapixel" {
                micros_per_unit
            } else {
                micros_per_unit
                    .checked_mul(1_000_000)
                    .ok_or(AppError::Internal)?
            },
            price_tiers: Vec::new(),
            rate_window_start: job.try_get("rate_window_start")?,
            reserved_tokens: job.try_get("reserved_tokens")?,
        };
        let usage = TokenUsage {
            output_tokens: input.billed_units,
            ..TokenUsage::default()
        };
        let expected_cost_micros = price_token_usage(&reservation, &usage)?;
        let result = (input.status == "succeeded")
            .then(|| safe_generation_result(&driver, input.billed_units, input.assets));
        let result_json = result
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;

        if matches!(
            current_status.as_str(),
            "succeeded" | "failed" | "cancelled"
        ) {
            let existing_billed_units: Option<i64> = job.try_get("billed_units")?;
            let existing_cost_micros: i64 = job.try_get("cost_micros")?;
            let existing_result_json: Option<String> = job.try_get("result_json")?;
            let existing_result = existing_result_json
                .as_deref()
                .map(serde_json::from_str)
                .transpose()
                .map_err(|_| AppError::Internal)?;
            let existing_error_code: Option<String> = job.try_get("error_code")?;
            let reservation_status: String = job.try_get("reservation_status")?;
            let actual_micros: Option<i64> = job.try_get("actual_micros")?;
            let staged_replay_matches = if input.status == "succeeded" {
                persisted_staged_assets.as_ref() == input.staged_assets
            } else {
                persisted_staged_assets.is_none()
            };
            let exact_terminal = current_status == input.status
                && existing_billed_units == Some(input.billed_units)
                && existing_cost_micros == expected_cost_micros
                && existing_result == result
                && existing_error_code.as_deref() == input.error_code
                && staged_replay_matches
                && reservation_status == "settled"
                && actual_micros == Some(existing_cost_micros);
            if !exact_terminal
                || !generation_assets_match(&mut transaction, input.job_id, input.assets).await?
            {
                return Err(AppError::Conflict(
                    "generation job already has a different terminal result".into(),
                ));
            }
            transaction.commit().await?;
            return Ok(existing_cost_micros);
        }
        if !matches!(
            current_status.as_str(),
            "queued" | "running" | "submitting" | "cancelling"
        ) {
            return Err(AppError::Internal);
        }
        let lease_owner: Option<String> = job.try_get("lease_owner")?;
        if lease_owner.as_deref() != Some(input.worker_id) {
            return Err(AppError::NotFound);
        }
        if persisted_staged_assets.as_ref() != input.staged_assets {
            return Err(AppError::NotFound);
        }

        let cost_micros =
            settle_token_usage_in_transaction(&mut transaction, &reservation, &usage, now).await?;
        if cost_micros != expected_cost_micros {
            return Err(AppError::Conflict(
                "generation reservation was settled for a different amount".into(),
            ));
        }
        let expected_staged_assets_json = input
            .staged_assets
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let updated = sqlx::query("UPDATE generation_jobs SET status = $1, billed_units = $2, cost_micros = $3, result_json = $4, error_code = $5, completed_at = $6, updated_at = $7, lease_owner = NULL, lease_expires_at = NULL, staged_assets_json = CASE WHEN $1 = 'succeeded' THEN staged_assets_json ELSE NULL END WHERE id = $8 AND lease_owner = $9 AND status IN ('queued', 'running', 'submitting', 'cancelling') AND ((staged_assets_json IS NULL AND $10 IS NULL) OR staged_assets_json = $10)")
            .bind(input.status)
            .bind(input.billed_units)
            .bind(cost_micros)
            .bind(result_json)
            .bind(input.error_code)
            .bind(now)
            .bind(now)
            .bind(input.job_id.to_string())
            .bind(input.worker_id)
            .bind(expected_staged_assets_json)
            .execute(&mut *transaction)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        if input.status == "succeeded" {
            insert_generation_assets_in_transaction(
                &mut transaction,
                input.job_id,
                input.assets,
                now,
            )
            .await?;
        } else {
            sqlx::query("DELETE FROM generation_assets WHERE job_id = $1")
                .bind(input.job_id.to_string())
                .execute(&mut *transaction)
                .await?;
            if let Some(staged) = input.staged_assets {
                let key = crate::archive_staging::ArchiveStagingKey::new(
                    ArchiveStagingOwner::GenerationJob(input.job_id),
                    ArchiveStagingPurpose::Assets,
                    staged.attempt_nonce,
                )?;
                super::super::cleanup_archive_staging_attempt_in_transaction(&mut transaction, key)
                    .await?;
            } else {
                super::super::cleanup_archive_staging_purpose_in_transaction(
                    &mut transaction,
                    ArchiveStagingOwner::GenerationJob(input.job_id),
                    ArchiveStagingPurpose::Assets,
                )
                .await?;
            }
        }
        aggregate_terminal_generation_job(&mut transaction, &input.job_id.to_string(), now).await?;
        let tenant_id: String = job.try_get("tenant_id")?;
        let key_id = key_id.to_string();
        let request_id = input.job_id.to_string();
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
                "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', 'generation', public_model, CASE WHEN status = 'succeeded' THEN 200 WHEN status = 'cancelled' THEN 499 ELSE 502 END, $3 - created_at, 0, 0, cost_micros, error_code FROM generation_jobs WHERE id = $4",
            )
            .bind(&event_id)
            .bind(now)
            .bind(now)
            .bind(&request_id)
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(cost_micros)
    }
}

fn is_allowed_generation_error_code(error_code: &str) -> bool {
    matches!(
        error_code,
        "retry_exhausted"
            | "generation_timeout"
            | "generation_rejected"
            | "submission_outcome_unknown"
            | "generation_staging_lost"
            | "generation_asset_bytes_exceeded"
            | "upstream_usage_exceeds_contract"
            | "seedance_generation_failed"
            | "seedance_missing_asset"
            | "seedance_invalid_asset"
            | "comfyui_failed"
            | "comfyui_execution_error"
            | "comfyui_missing_assets"
            | "comfyui_asset_limit_exceeded"
            | "siliconflow_video_failed"
            | "siliconflow_video_missing_asset"
            | "siliconflow_video_asset_limit_exceeded"
            | "siliconflow_video_invalid_asset"
            | "cancelled_by_user"
    )
}

fn safe_generation_result(
    driver: &str,
    billed_units: i64,
    assets: &[ArchivedGenerationAsset],
) -> serde_json::Value {
    let provider = match driver {
        "volcengine-seedance" => {
            serde_json::json!({"status": "succeeded", "duration": billed_units})
        }
        "comfyui" => serde_json::json!({"status": "success"}),
        "http-json" => serde_json::json!({"status": "Succeed"}),
        _ => serde_json::json!({"status": "succeeded"}),
    };
    let assets = assets
        .iter()
        .map(|asset| GenerationAssetView {
            asset_id: asset.asset_id,
            index: asset.index,
            mime_type: asset.mime_type.clone(),
            size_bytes: asset.size_bytes,
            filename: asset.filename.clone(),
        })
        .collect::<Vec<_>>();
    serde_json::json!({"provider": provider, "assets": assets})
}

pub(super) async fn insert_generation_assets_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    job_id: Uuid,
    assets: &[ArchivedGenerationAsset],
    now: i64,
) -> Result<(), AppError> {
    for asset in assets {
        let inserted = sqlx::query(
            "INSERT INTO generation_assets (id, job_id, asset_index, object_locator, mime_type, size_bytes, filename, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT(job_id, asset_index) DO NOTHING",
        )
        .bind(asset.asset_id.to_string())
        .bind(job_id.to_string())
        .bind(asset.index)
        .bind(&asset.object_locator)
        .bind(&asset.mime_type)
        .bind(asset.size_bytes)
        .bind(&asset.filename)
        .bind(now)
        .execute(&mut **transaction)
        .await?;
        if inserted.rows_affected() == 0
            && !generation_asset_matches(transaction, job_id, asset).await?
        {
            return Err(AppError::Conflict(
                "generation staged asset replay does not match archived metadata".into(),
            ));
        }
    }
    Ok(())
}

async fn generation_asset_matches(
    transaction: &mut Transaction<'_, Any>,
    job_id: Uuid,
    expected: &ArchivedGenerationAsset,
) -> Result<bool, AppError> {
    let row = sqlx::query(
        "SELECT id, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE job_id = $1 AND asset_index = $2",
    )
    .bind(job_id.to_string())
    .bind(expected.index)
    .fetch_optional(&mut **transaction)
    .await?;
    let Some(row) = row else {
        return Ok(false);
    };
    Ok(
        row.try_get::<String, _>("id")? == expected.asset_id.to_string()
            && row.try_get::<String, _>("object_locator")? == expected.object_locator
            && row.try_get::<String, _>("mime_type")? == expected.mime_type
            && row.try_get::<i64, _>("size_bytes")? == expected.size_bytes
            && row.try_get::<String, _>("filename")? == expected.filename,
    )
}

pub(super) async fn generation_assets_match(
    transaction: &mut Transaction<'_, Any>,
    job_id: Uuid,
    expected: &[ArchivedGenerationAsset],
) -> Result<bool, AppError> {
    let rows = sqlx::query(
        "SELECT id, asset_index, object_locator, mime_type, size_bytes, filename FROM generation_assets WHERE job_id = $1 ORDER BY asset_index, id",
    )
    .bind(job_id.to_string())
    .fetch_all(&mut **transaction)
    .await?;
    if rows.len() != expected.len() {
        return Ok(false);
    }
    let mut expected = expected.iter().collect::<Vec<_>>();
    expected.sort_by_key(|asset| (asset.index, asset.asset_id));
    for (row, expected) in rows.iter().zip(expected) {
        let id: String = row.try_get("id")?;
        let index: i64 = row.try_get("asset_index")?;
        let object_locator: String = row.try_get("object_locator")?;
        let mime_type: String = row.try_get("mime_type")?;
        let size_bytes: i64 = row.try_get("size_bytes")?;
        let filename: String = row.try_get("filename")?;
        if id != expected.asset_id.to_string()
            || index != expected.index
            || object_locator != expected.object_locator
            || mime_type != expected.mime_type
            || size_bytes != expected.size_bytes
            || filename != expected.filename
        {
            return Ok(false);
        }
    }
    Ok(true)
}
