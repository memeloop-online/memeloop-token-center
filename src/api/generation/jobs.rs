use super::super::*;

pub(in crate::api) async fn create_generation(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CreateGenerationRequest>,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let applied = apply_traffic_policy(
        &state,
        &key,
        "generation",
        json!({"model": body.model, "input": body.input}),
    )
    .await?;
    let mut body: CreateGenerationRequest =
        serde_json::from_value(applied.request_json).map_err(|_| {
            AppError::BadRequest("plugin-rewritten generation request is invalid".into())
        })?;
    if !body.input.is_object() {
        return Err(AppError::BadRequest(
            "generation input must be a JSON object".into(),
        ));
    }
    let route = state
        .db
        .resolve_upstream_with_hint(
            key.tenant_id,
            &body.model,
            "generation",
            applied.upstream_account_hint,
            state.config.key_pepper.as_bytes(),
        )
        .await?
        .ok_or_else(|| AppError::Upstream("generation route is not configured".into()))?;
    if !matches!(route.driver.as_str(), "volcengine-seedance" | "comfyui") {
        return Err(AppError::Upstream(format!(
            "generation driver {} cannot execute asynchronous jobs",
            route.driver
        )));
    }
    if route.driver == "volcengine-seedance" {
        normalize_seedance_duration(&mut body.input)?;
    }
    // Hash and archive exactly the normalized request that the worker will
    // submit. This prevents alternate `duration`/`--dur` spellings from
    // reserving one amount while asking a permissive provider for another.
    let request_hash = crate::generation::generation_request_hash(&body.model, &body.input);
    let idempotency = headers
        .get("idempotency-key")
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))?;
            Ok::<_, AppError>(GenerationJobIdempotency {
                key: value.to_owned(),
                request_hash: request_hash.clone(),
            })
        })
        .transpose()?;
    if let Some(existing) = match idempotency.as_ref() {
        Some(idempotency) => {
            state
                .db
                .generation_job_by_idempotency(key.key_id, idempotency)
                .await?
        }
        None => None,
    } {
        return Ok((StatusCode::OK, Json(existing)).into_response());
    }
    let generation_price = state
        .db
        .generation_price(&body.model, &key.currency)
        .await?;
    let estimated_units =
        estimated_generation_units(&route.driver, &generation_price.billing_unit, &body.input)?;
    let reservation_price = generation_price
        .reservation_price()
        .ok_or_else(|| AppError::BadRequest("generation price is too large".into()))?;
    let job_id = Uuid::now_v7();
    let archived = serde_json::to_vec(&json!({
        "model": body.model,
        "input": body.input
    }))
    .map_err(|_| AppError::Internal)?;
    let key_id = key.key_id;
    let started = state
        .db
        .start_generation_job(
            StartGenerationJobInput {
                job_id,
                key,
                upstream_account_id: route.account_id,
                reservation_price,
                public_model: body.model,
                upstream_model: route.upstream_model,
                driver: route.driver,
                request_hash: request_hash.clone(),
                estimated_units,
                billing_unit: generation_price.billing_unit,
                micros_per_unit: generation_price.micros_per_unit,
            },
            idempotency.as_ref(),
        )
        .await?;
    let preparing = match started {
        CreateGenerationJobResult::Replayed(job) => {
            return Ok((StatusCode::OK, Json(job)).into_response());
        }
        CreateGenerationJobResult::Created(job) => job,
    };
    // Admission and a durable intent precede the first object-store write. The
    // request stays in a unique, reaper-owned prefix instead of the shared CAS
    // namespace, whose reference count cannot be inferred from a prefix scan.
    let mut request_lease = match crate::generation::begin_generation_staging_attempt(
        &state,
        crate::archive_staging::ArchiveStagingOwner::GenerationJob(preparing.job_id),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        Uuid::now_v7(),
    )
    .await
    {
        Ok(lease) => lease,
        Err(error) => {
            state
                .db
                .fail_generation_job_preparation(
                    key_id,
                    preparing.job_id,
                    "generation_archive_failed",
                )
                .await?;
            return Err(error);
        }
    };
    let request_object = match crate::generation::write_generation_staging_bytes(
        &state,
        &mut request_lease,
        "request.json",
        Bytes::from(archived),
    )
    .await
    {
        Ok(staged) => staged.object_locator,
        Err(error) => {
            state
                .db
                .abandon_archive_staging_attempt(&request_lease)
                .await?;
            state
                .db
                .fail_generation_job_preparation(
                    key_id,
                    preparing.job_id,
                    "generation_archive_failed",
                )
                .await?;
            return Err(error);
        }
    };
    match state
        .db
        .attach_generation_job_request_staged(
            key_id,
            preparing.job_id,
            &request_hash,
            &request_object,
            &request_lease,
        )
        .await
    {
        Ok(AttachGenerationJobResult::Attached(job)) => {
            Ok((StatusCode::ACCEPTED, Json(job)).into_response())
        }
        Ok(AttachGenerationJobResult::Indeterminate) => {
            // Admission and the request CAS are already durable. Returning the
            // known job identity as accepted prevents a no-idempotency retry
            // from duplicating a queued job after a lost database commit ACK.
            Ok((StatusCode::ACCEPTED, Json(preparing)).into_response())
        }
        Err(error) => {
            let _ = state
                .db
                .fail_generation_job_preparation(
                    key_id,
                    preparing.job_id,
                    "generation_archive_attach_failed",
                )
                .await;
            Err(error)
        }
    }
}

fn estimated_generation_units(
    driver: &str,
    billing_unit: &str,
    input: &Value,
) -> Result<i64, AppError> {
    match (driver, billing_unit) {
        ("volcengine-seedance", "second") => {
            let units = input
                .get("duration")
                .and_then(Value::as_i64)
                .ok_or(AppError::Internal)?;
            if !(1..=60).contains(&units) {
                return Err(AppError::Internal);
            }
            Ok(units)
        }
        ("comfyui", "job") => Ok(1),
        ("volcengine-seedance", _) => Err(AppError::BadRequest(
            "Seedance generation price must use second billing".into(),
        )),
        ("comfyui", _) => Err(AppError::BadRequest(
            "ComfyUI generation price must use job billing".into(),
        )),
        _ => Err(AppError::BadRequest("unsupported generation driver".into())),
    }
}

pub(in crate::api) fn normalize_seedance_duration(input: &mut Value) -> Result<i64, AppError> {
    let object = input
        .as_object_mut()
        .ok_or_else(|| AppError::BadRequest("generation input must be a JSON object".into()))?;
    let explicit = match object.get("duration") {
        None => None,
        Some(Value::Number(value)) => Some(value.as_i64().ok_or_else(|| {
            AppError::BadRequest("Seedance duration must be a JSON integer".into())
        })?),
        Some(_) => {
            return Err(AppError::BadRequest(
                "Seedance duration must be a JSON integer".into(),
            ));
        }
    };
    if explicit.is_some_and(|duration| !(1..=60).contains(&duration)) {
        return Err(AppError::BadRequest(
            "Seedance duration must be between 1 and 60 seconds".into(),
        ));
    }

    let mut content_duration = None;
    if let Some(content) = object.get_mut("content").and_then(Value::as_array_mut) {
        for item in content {
            let Some(text) = item.get_mut("text") else {
                continue;
            };
            let Some(original) = text.as_str() else {
                continue;
            };
            let tokens = original.split_whitespace().collect::<Vec<_>>();
            let mut normalized = Vec::with_capacity(tokens.len());
            let mut index = 0;
            let mut removed_duration = false;
            while index < tokens.len() {
                let token = tokens[index];
                if token == "--dur" {
                    if content_duration.is_some() {
                        return Err(AppError::BadRequest(
                            "Seedance content must contain at most one --dur option".into(),
                        ));
                    }
                    let raw = tokens.get(index + 1).ok_or_else(|| {
                        AppError::BadRequest(
                            "Seedance content --dur must be followed by an integer".into(),
                        )
                    })?;
                    if raw.is_empty() || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err(AppError::BadRequest(
                            "Seedance content --dur must be followed by an integer".into(),
                        ));
                    }
                    let duration = raw.parse::<i64>().map_err(|_| {
                        AppError::BadRequest(
                            "Seedance content --dur must be followed by an integer".into(),
                        )
                    })?;
                    if !(1..=60).contains(&duration) {
                        return Err(AppError::BadRequest(
                            "Seedance duration must be between 1 and 60 seconds".into(),
                        ));
                    }
                    content_duration = Some(duration);
                    removed_duration = true;
                    index += 2;
                    continue;
                }
                if token.starts_with("--dur") {
                    return Err(AppError::BadRequest(
                        "Seedance content contains a malformed --dur option".into(),
                    ));
                }
                normalized.push(token);
                index += 1;
            }
            if removed_duration {
                *text = Value::String(normalized.join(" "));
            }
        }
    }
    if explicit.is_some() && content_duration.is_some() && explicit != content_duration {
        return Err(AppError::BadRequest(
            "Seedance duration conflicts with content --dur".into(),
        ));
    }
    let duration = explicit.or(content_duration).unwrap_or(5);
    object.insert("duration".to_owned(), Value::from(duration));
    Ok(duration)
}
