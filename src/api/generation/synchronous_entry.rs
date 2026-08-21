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
    let downstream_idempotency_key = image_idempotency_key(&headers)?;
    let existing_idempotency = match downstream_idempotency_key.as_deref() {
        Some(idempotency_key) => {
            state
                .db
                .has_synchronous_image_idempotency(key.key_id, idempotency_key)
                .await?
        }
        None => false,
    };
    let mut applied = if existing_idempotency {
        apply_traffic_plugin_for_existing_idempotency(&state, &key, "openai-image", request_json)
            .await?
    } else {
        apply_openai_image_traffic_policy(&state, &key, request_json).await?
    };
    let image_count = normalize_openai_image_request(&mut applied.request_json)?;
    let image_idempotency = downstream_idempotency_key.map(|key| GenerationJobIdempotency {
        key,
        request_hash: crate::generation::generation_request_hash(
            &applied.model,
            &applied.request_json,
        ),
    });
    if existing_idempotency {
        match state
            .db
            .lookup_synchronous_image_idempotency(
                key.key_id,
                image_idempotency.as_ref().ok_or(AppError::Internal)?,
            )
            .await?
        {
            Some(SynchronousImageIdempotencyClaim::Claimed) | None => {
                // An expired owner needs takeover, or the row disappeared
                // between the two read-only probes. Both paths must regain
                // normal requested/effective route authorization before any
                // claim can be created or mutated.
                authorize_applied_traffic_policy(&state, &key, "generation", &applied).await?;
            }
            Some(replay) => return image_idempotency_replay_response(&state, replay).await,
        }
    }
    let AppliedTraffic {
        request_json,
        model,
        upstream_account_hint,
        ..
    } = applied;
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
                    upstream_account_hint,
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

async fn apply_openai_image_traffic_policy(
    state: &AppState,
    key: &AuthenticatedKey,
    request_json: Value,
) -> Result<AppliedTraffic, AppError> {
    apply_traffic_policy(
        state,
        key,
        TrafficPolicyProtocols {
            client: "openai-image",
            routing: "generation",
        },
        request_json,
    )
    .await
}

fn image_idempotency_key(headers: &HeaderMap) -> Result<Option<String>, AppError> {
    headers
        .get("idempotency-key")
        .map(|value| {
            value
                .to_str()
                .map(str::to_owned)
                .map_err(|_| AppError::BadRequest("Idempotency-Key must be valid ASCII".into()))
        })
        .transpose()
}

fn normalize_openai_image_request(request_json: &mut Value) -> Result<i64, AppError> {
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
    Ok(image_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        db::{CreateGroupInput, GroupKind, ReplaceGroupMembersInput},
    };
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    async fn post_openai_image(
        state: &AppState,
        credential: &str,
        idempotency_key: &str,
        prompt: &str,
    ) -> Response {
        router_for_role(state.clone(), RuntimeRole::Gateway)
            .oneshot(
                Request::post("/v1/images/generations")
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::AUTHORIZATION, format!("Bearer {credential}"))
                    .header("idempotency-key", idempotency_key)
                    .body(Body::from(
                        serde_json::to_vec(&json!({
                            "model": "image-replay-model",
                            "prompt": prompt,
                            "n": 1,
                            "size": "1024x1024"
                        }))
                        .expect("image request JSON"),
                    ))
                    .expect("image request"),
            )
            .await
            .expect("image response")
    }

    #[tokio::test]
    async fn openai_images_use_generation_grants_and_ignore_credential_groups() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("image-routing.db").display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .expect("test state");
        let tenant = "image-routing-protocol";
        let model = "image-routing-model";
        let upstream = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "image upstream".to_owned(),
                    driver: "http-json".to_owned(),
                    config: json!({
                        "base_url": "https://images.example.invalid",
                        "network_scope": "public"
                    }),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("upstream account");
        let route = state
            .db
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.to_owned(),
                public_model: model.to_owned(),
                upstream_account_id: upstream.id,
                upstream_model: model.to_owned(),
                protocol: "generation".to_owned(),
                priority: 0,
            })
            .await
            .expect("generation route");
        let key_input = |alias: &str| CreateKeyInput {
            tenant_external_id: tenant.to_owned(),
            principal_external_id: alias.to_owned(),
            alias: alias.to_owned(),
            currency: "USD".to_owned(),
            policy: KeyPolicy::default(),
            initial_balance: Decimal::ONE,
            idempotency_key: None,
        };
        let granted = state
            .db
            .create_key_with_routing(
                key_input("granted"),
                &[route.id],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("granted credential");
        let ungranted = state
            .db
            .create_key_with_routing(
                key_input("ungranted"),
                &[],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("ungranted credential");

        // Both credentials deliberately share the same presentation-only
        // group. Only the explicit generation-route grant may authorize one.
        let credential_group = state
            .db
            .create_group(
                GroupKind::Credential,
                CreateGroupInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "Image testers".to_owned(),
                },
            )
            .await
            .expect("credential group");
        state
            .db
            .replace_group_members(
                GroupKind::Credential,
                credential_group.id,
                ReplaceGroupMembersInput {
                    tenant_external_id: tenant.to_owned(),
                    member_ids: vec![granted.key_id, ungranted.key_id],
                    expected_updated_at: credential_group.updated_at,
                },
            )
            .await
            .expect("credential group members");

        let granted_key = state
            .db
            .authenticate_key(&granted.key, state.config.key_pepper.as_bytes())
            .await
            .expect("authenticate granted credential");
        let applied = apply_openai_image_traffic_policy(
            &state,
            &granted_key,
            json!({"model": model, "prompt": "draw a fox"}),
        )
        .await
        .expect("generation grant authorizes OpenAI image admission");
        assert_eq!(applied.model, model);

        let ungranted_key = state
            .db
            .authenticate_key(&ungranted.key, state.config.key_pepper.as_bytes())
            .await
            .expect("authenticate ungranted credential");
        assert!(matches!(
            apply_openai_image_traffic_policy(
                &state,
                &ungranted_key,
                json!({"model": model, "prompt": "draw a fox"}),
            )
            .await,
            Err(AppError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn completed_image_replay_precedes_route_state_but_not_key_authentication() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/images/generations"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "created": 1,
                "data": [{"b64_json": "bW9jay1wbmc="}]
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let directory = tempfile::tempdir().expect("temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("image-replay-order.db").display()
        );
        let state = AppState::initialize(Config::for_test(database_url))
            .await
            .expect("test state");
        let tenant = "image-replay-order";
        let upstream_account = state
            .db
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.to_owned(),
                    name: "image replay upstream".to_owned(),
                    driver: "http-json".to_owned(),
                    config: json!({
                        "base_url": upstream.uri(),
                        "network_scope": "private"
                    }),
                    credential: UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("upstream account");
        let route = state
            .db
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.to_owned(),
                public_model: "image-replay-model".to_owned(),
                upstream_account_id: upstream_account.id,
                upstream_model: "image-replay-upstream".to_owned(),
                protocol: "generation".to_owned(),
                priority: 0,
            })
            .await
            .expect("generation route");
        state
            .db
            .upsert_generation_price("image-replay-model", "USD", "image", Decimal::new(3, 1))
            .await
            .expect("generation price");
        let key_input = |alias: &str| CreateKeyInput {
            tenant_external_id: tenant.to_owned(),
            principal_external_id: alias.to_owned(),
            alias: alias.to_owned(),
            currency: "USD".to_owned(),
            policy: KeyPolicy::default(),
            initial_balance: Decimal::ONE,
            idempotency_key: None,
        };
        let granted = state
            .db
            .create_key_with_routing(
                key_input("replay-granted"),
                &[route.id],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("granted credential");
        let ungranted = state
            .db
            .create_key_with_routing(
                key_input("replay-ungranted"),
                &[],
                &[],
                state.config.key_pepper.as_bytes(),
            )
            .await
            .expect("ungranted credential");

        let first = post_openai_image(
            &state,
            &granted.key,
            "stable-image-replay",
            "draw a compact fox",
        )
        .await;
        assert_eq!(first.status(), StatusCode::OK);
        let first_body = axum::body::to_bytes(first.into_body(), 64 * 1024)
            .await
            .expect("first image response");

        state
            .db
            .set_model_route_enabled(route.id, tenant, false, route.updated_at)
            .await
            .expect("disable completed route");
        let replay = post_openai_image(
            &state,
            &granted.key,
            "stable-image-replay",
            "draw a compact fox",
        )
        .await;
        assert_eq!(replay.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(replay.into_body(), 64 * 1024)
                .await
                .expect("replayed image response"),
            first_body
        );

        let mismatch = post_openai_image(
            &state,
            &granted.key,
            "stable-image-replay",
            "draw a different fox",
        )
        .await;
        assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);

        let new_ungranted = post_openai_image(
            &state,
            &ungranted.key,
            "new-disabled-image",
            "draw a compact fox",
        )
        .await;
        assert_eq!(new_ungranted.status(), StatusCode::FORBIDDEN);

        state
            .db
            .set_key_status(granted.key_id, "suspended")
            .await
            .expect("suspend credential");
        let suspended = post_openai_image(
            &state,
            &granted.key,
            "stable-image-replay",
            "draw a compact fox",
        )
        .await;
        assert_eq!(suspended.status(), StatusCode::UNAUTHORIZED);
    }
}
