use super::super::*;
use super::synchronous_image::{
    SyncImageRequest, execute_synchronous_image_request, fail_image_request,
    image_idempotency_replay_response, responses_tool_image_request,
    scoped_upstream_image_idempotency,
};

#[derive(Debug, Deserialize)]
pub(in crate::api) struct CreateGenerationRequest {
    pub(in crate::api) model: String,
    pub(in crate::api) input: Value,
}

pub(in crate::api) async fn create_image_generation(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let value: Value = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("request body must be valid JSON".into()))?;
    if value.get("input").is_some() {
        let request = serde_json::from_value(value)
            .map_err(|_| AppError::BadRequest("model and generation input are required".into()))?;
        return create_generation(State(state), headers, Json(request)).await;
    }
    proxy_openai_image_generation(state, headers, body, value).await
}

async fn proxy_openai_image_generation(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    request_json: Value,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let applied = apply_traffic_policy(&state, &key, "openai-image", request_json).await?;
    let mut request_json = applied.request_json;
    let model = applied.model;
    let prompt = request_json
        .get("prompt")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty() && value.len() <= 32_000)
        .ok_or_else(|| AppError::BadRequest("prompt is required".into()))?;
    if prompt.contains('\0') {
        return Err(AppError::BadRequest("prompt contains invalid data".into()));
    }
    let image_count = match request_json.get("n") {
        None => 1,
        Some(Value::Number(value)) if value.is_i64() => value
            .as_i64()
            .ok_or_else(|| AppError::BadRequest("n must be an integer between 1 and 10".into()))?,
        Some(_) => {
            return Err(AppError::BadRequest(
                "n must be an integer between 1 and 10".into(),
            ));
        }
    };
    if !(1..=10).contains(&image_count) {
        return Err(AppError::BadRequest("n must be between 1 and 10".into()));
    }
    request_json["n"] = json!(image_count);
    let image_idempotency = headers
        .get("idempotency-key")
        .map(|value| {
            let value = value
                .to_str()
                .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))?;
            Ok::<_, AppError>(GenerationJobIdempotency {
                key: value.to_owned(),
                request_hash: crate::generation::generation_request_hash(&model, &request_json),
            })
        })
        .transpose()?;
    let request_id = Uuid::now_v7();
    if let Some(idempotency) = image_idempotency.as_ref() {
        match state
            .db
            .claim_synchronous_image_idempotency(key.key_id, idempotency, request_id)
            .await?
        {
            SynchronousImageIdempotencyClaim::Claimed => {}
            replay => return image_idempotency_replay_response(&state, replay).await,
        }
    }
    let preparation = async {
        let route = state
            .db
            .resolve_authorized_upstream_with_hint(
                key.key_id,
                key.tenant_id,
                &model,
                "generation",
                RouteSelectionOptions {
                    upstream_account_hint: applied.upstream_account_hint,
                    selection_seed: request_id,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await?
            .ok_or_else(|| AppError::Upstream("image generation route is not configured".into()))?;
        if route.driver != "http-json" {
            return Err(AppError::Upstream(format!(
                "generation driver {} does not implement the OpenAI Images API",
                route.driver
            )));
        }
        let price = state.db.generation_price(&model, &key.currency).await?;
        if !matches!(price.billing_unit.as_str(), "image" | "job") {
            return Err(AppError::BadRequest(
                "OpenAI image generation must be billed per image or job".into(),
            ));
        }
        let billed_units = if price.billing_unit == "job" {
            1
        } else {
            image_count
        };
        let responses_tool_mode = route
            .config
            .get("image_api_mode")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "responses-tool");
        if responses_tool_mode && image_count != 1 {
            return Err(AppError::BadRequest(
                "responses-tool image routes currently require n=1".into(),
            ));
        }
        let (upstream_path, forwarded) = if responses_tool_mode {
            (
                "/v1/responses",
                responses_tool_image_request(&route.config, &route.upstream_model, &request_json)?,
            )
        } else {
            let mut forwarded = request_json;
            forwarded["model"] = Value::String(route.upstream_model.clone());
            ("/v1/images/generations", forwarded)
        };
        let outbound_http = network::client_for_config_url(
            &state.http,
            &route.base_url,
            &route.config,
            state.config.allow_oauth_loopback,
        )
        .await?;
        let upstream_idempotency = image_idempotency.as_ref().map(|idempotency| {
            scoped_upstream_image_idempotency(
                state.config.key_pepper.as_bytes(),
                key.tenant_id,
                key.key_id,
                route.route_id,
                upstream_path,
                &idempotency.key,
            )
        });
        let reservation_price = price
            .reservation_price()
            .ok_or_else(|| AppError::BadRequest("generation price is too large".into()))?;
        let mut request = outbound_http
            .post(format!(
                "{}{}",
                route.base_url.trim_end_matches('/'),
                upstream_path
            ))
            .json(&forwarded);
        if let Some(upstream_idempotency) = upstream_idempotency.as_deref() {
            request = request.header("idempotency-key", upstream_idempotency);
        }
        request = route.credential.apply(request, unix_millis())?;
        Ok::<_, AppError>((
            route,
            billed_units,
            reservation_price,
            request,
            responses_tool_mode,
        ))
    }
    .await;
    let (route, billed_units, reservation_price, request, responses_tool_mode) = match preparation {
        Ok(prepared) => prepared,
        Err(error) => {
            if let Some(idempotency) = image_idempotency.as_ref() {
                state
                    .db
                    .release_synchronous_image_idempotency_claim(
                        key.key_id,
                        &idempotency.key,
                        request_id,
                    )
                    .await?;
            }
            return Err(error);
        }
    };
    // Hashing is local and bounded by MAX_IMAGE_REQUEST_BODY. The relational
    // admission transaction is deliberately completed before the first object
    // store write, so rejected/replayed requests cannot consume archive space.
    let staged_request_object = format!("pending://synchronous/{request_id}/request");
    let started = state
        .db
        .start_synchronous_image_request(StartSynchronousImageRequest {
            request_id,
            key: &key,
            price: &reservation_price,
            input_token_ceiling: 0,
            output_token_ceiling: billed_units,
            idempotency: image_idempotency.as_ref(),
            protocol: "openai-image",
            model: &model,
            request_object: &staged_request_object,
            upstream_account_id: Some(route.account_id),
            model_route_id: Some(route.route_id),
        })
        .await;
    let reservation = match started {
        Ok(StartSynchronousImageResult::Started(reservation)) => reservation,
        Ok(StartSynchronousImageResult::Replay(replay)) => {
            return image_idempotency_replay_response(&state, replay).await;
        }
        Err(error) => {
            if let Some(idempotency) = image_idempotency.as_ref() {
                state
                    .db
                    .release_synchronous_image_idempotency_claim(
                        key.key_id,
                        &idempotency.key,
                        request_id,
                    )
                    .await?;
            }
            return Err(error);
        }
    };
    let started = Instant::now();
    let context = SyncImageRequest {
        state: &state,
        reservation: &reservation,
        request_id,
        started,
        billed_units,
        expected_image_count: image_count,
        key_id: key.key_id,
        idempotency_key: image_idempotency.as_ref().map(|value| value.key.as_str()),
    };
    match tokio::time::timeout(
        SYNCHRONOUS_IMAGE_DEADLINE,
        execute_synchronous_image_request(
            &context,
            body,
            &staged_request_object,
            &route,
            request,
            responses_tool_mode,
        ),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => fail_image_request(&context, "image_deadline_exceeded").await,
    }
}
