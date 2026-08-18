use super::*;

#[path = "codex_transport.rs"]
mod codex_transport;

#[cfg(test)]
mod tests;

const PROXY_BODY_CHANNEL_CAPACITY: usize = 1;

pub(super) async fn proxy(
    state: AppState,
    headers: HeaderMap,
    body: Bytes,
    protocol: Protocol,
) -> Result<Response, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let original_request_json: Value = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("request body must be valid JSON".into()))?;
    let applied =
        apply_traffic_policy(&state, &key, protocol.name(), original_request_json.clone()).await?;
    let request_json = applied.request_json;
    let model = applied.model;
    let resolved_route = state
        .db
        .resolve_upstream_with_hint(
            key.tenant_id,
            &model,
            protocol.name(),
            applied.upstream_account_hint,
            state.config.key_pepper.as_bytes(),
        )
        .await?;
    let (
        base_url,
        upstream_credential,
        legacy_upstream_key,
        upstream_model,
        upstream_account_id,
        model_route_id,
        route_driver,
        route_config,
    ) = if let Some(route) = resolved_route {
        if !state.providers.contains(&route.driver) {
            return Err(AppError::Upstream(format!(
                "provider driver {} is not loaded",
                route.driver
            )));
        }
        route.credential.validate(unix_millis())?;
        (
            route.base_url,
            Some(route.credential),
            None,
            route.upstream_model,
            Some(route.account_id),
            Some(route.route_id),
            Some(route.driver),
            Some(route.config),
        )
    } else {
        let (base_url, upstream_key) = if protocol.is_openai() {
            (
                state.config.upstream_openai_url.clone(),
                state.config.upstream_openai_key.clone(),
            )
        } else {
            (
                state.config.upstream_anthropic_url.clone(),
                state.config.upstream_anthropic_key.clone(),
            )
        };
        (
            base_url.ok_or_else(|| AppError::Upstream(protocol.name().into()))?,
            None,
            upstream_key,
            model.clone(),
            None,
            None,
            None,
            None,
        )
    };
    if route_driver.as_deref() == Some("cpa-subscription-bridge") {
        if !matches!(protocol, Protocol::OpenAiChat) {
            return Err(AppError::BadRequest(
                "CPA subscription bridge supports OpenAI chat completions only".into(),
            ));
        }
        let valid_provider = route_config
            .as_ref()
            .and_then(|config| config.get("provider"))
            .and_then(Value::as_str)
            .is_some_and(|provider| matches!(provider, "copilot" | "cursor"));
        let has_handle = upstream_credential
            .as_ref()
            .and_then(UpstreamCredential::subscription_bridge_handle)
            .is_some();
        if !valid_provider || !has_handle {
            return Err(AppError::Upstream(
                "subscription bridge account configuration is invalid".into(),
            ));
        }
    }
    let is_codex_route = route_driver.as_deref() == Some(codex_transport::DRIVER);
    if is_codex_route {
        codex_transport::validate_protocol(protocol)?;
        if base_url != codex_transport::BASE_URL {
            return Err(AppError::BadRequest(
                "CPA Codex OAuth account has an invalid fixed base URL".into(),
            ));
        }
        let credential = upstream_credential.as_ref().ok_or_else(|| {
            AppError::BadRequest("CPA Codex OAuth account has no credential".into())
        })?;
        codex_transport::validate_credential_contract(credential)?;
        codex_transport::validate_route_config(route_config.as_ref().ok_or_else(|| {
            AppError::BadRequest("CPA Codex OAuth account has no configuration".into())
        })?)?;
    }
    let mut forwarded_json = request_json.clone();
    if let Some(value) = forwarded_json.get_mut("model") {
        *value = Value::String(upstream_model.clone());
    }
    let codex_plan = if is_codex_route {
        Some(codex_transport::prepare_request(
            &mut forwarded_json,
            &upstream_model,
            route_config.as_ref().ok_or_else(|| {
                AppError::BadRequest("CPA Codex OAuth account has no configuration".into())
            })?,
        )?)
    } else {
        None
    };
    let output_token_ceiling = match codex_plan.as_ref() {
        Some(plan) => plan.output_token_ceiling,
        None => inject_controlled_output_ceiling(protocol, &mut forwarded_json)?,
    };
    let forwarded_body = serde_json::to_vec(&forwarded_json).map_err(|_| AppError::Internal)?;
    let component_adapter = route_driver.as_deref().and_then(|driver| {
        state
            .providers
            .get(driver)
            .and_then(|provider| provider.component_adapter.as_ref())
    });
    let component_request = if component_adapter.is_some() {
        match forwarded_json.get("stream") {
            Some(Value::Bool(false)) | None => {}
            Some(Value::Bool(true)) => {
                return Err(AppError::BadRequest(
                    "component providers support buffered requests only; stream=true is unavailable"
                        .into(),
                ));
            }
            Some(_) => {
                return Err(AppError::BadRequest("stream must be a boolean".into()));
            }
        }
        let driver = route_driver.as_deref().ok_or(AppError::Internal)?;
        let config = route_config.clone().ok_or_else(|| {
            AppError::Upstream("component provider route has no account configuration".into())
        })?;
        let component_context = RequestContext {
            tenant_id: key.tenant_id.to_string(),
            principal_id: key.principal_id.to_string(),
            key_id: key.key_id.to_string(),
            protocol: protocol.name().to_owned(),
            model: model.clone(),
            config_json: serde_json::to_string(&config).map_err(|_| AppError::Internal)?,
        };
        let prepared = prepare_component_provider(
            &state,
            driver,
            component_context.clone(),
            config,
            forwarded_json.clone(),
        )
        .await?;
        Some((prepared, component_context))
    } else {
        None
    };
    let price = state.db.model_price(&model, &key.currency).await?;
    let input_token_ceiling = i64::try_from(
        body.len().max(forwarded_body.len()).max(
            component_request
                .as_ref()
                .map(|(request, _)| request.body.len())
                .unwrap_or_default(),
        ),
    )
    .unwrap_or(i64::MAX);
    let requested_service_tier = match forwarded_json.get("service_tier") {
        None => None,
        Some(Value::String(tier)) if is_supported_service_tier(tier) => Some(tier.clone()),
        Some(_) => {
            return Err(AppError::BadRequest(
                "service_tier must be default, auto, priority, flex, scale, batch, or standard_only"
                    .into(),
            ));
        }
    };
    if let Some(tier) = requested_service_tier.as_deref()
        && !matches!(tier, "auto" | "standard_only")
        && !(tier == "default" && price.tiers.is_empty())
        && !price
            .tiers
            .iter()
            .any(|price_tier| price_tier.service_tier == tier)
    {
        return Err(AppError::BadRequest(
            "the requested service_tier has no configured price".into(),
        ));
    }
    let request_id = Uuid::now_v7();
    let request_digest = blake3::hash(&body).to_hex();
    let admitted_request_object = format!("gap://{request_id}/request");
    let request_archive_attempt =
        begin_proxy_archive_attempt(&state.db, request_id, ArchiveStagingPurpose::Request).await?;
    let reservation = match state
        .db
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling,
            output_token_ceiling,
            protocol: protocol.name(),
            model: &model,
            request_object: &admitted_request_object,
            upstream_account_id,
            model_route_id,
        })
        .await
    {
        Ok(reservation) => reservation,
        Err(error) => {
            abandon_proxy_archive_attempt(&state.db, &request_archive_attempt).await;
            return Err(error);
        }
    };
    let conversation_hints = conversation_hints(&headers, &original_request_json);
    let client_name = client_name(&headers);
    let conversation = matches!(
        protocol,
        Protocol::OpenAiChat | Protocol::OpenAiResponses | Protocol::AnthropicMessages
    )
    .then(|| ProxyConversation {
        key: key.clone(),
        request_json: original_request_json,
        hints: conversation_hints,
        client_name,
    });

    let started = Instant::now();
    let buffered_request = BufferedRequest {
        state: &state,
        reservation,
        request_id,
        started,
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier,
        conversation,
        protocol,
        tenant_id: key.tenant_id,
    };
    let mut request_writer = match state
        .archive
        .start_writer(&request_archive_attempt.object_locator)
        .await
    {
        Ok(writer) => writer,
        Err(_) => {
            abandon_proxy_archive_attempt(&state.db, &request_archive_attempt).await;
            tracing::warn!(%request_id, stage = "request_archive_start", "proxy archive failed");
            return finish_proxy_failure(&buffered_request, "request_archive").await;
        }
    };
    if request_writer.write(body.clone()).await.is_err() {
        let _ = request_writer.abort().await;
        abandon_proxy_archive_attempt(&state.db, &request_archive_attempt).await;
        tracing::warn!(%request_id, stage = "request_archive_write", "proxy archive failed");
        return finish_proxy_failure(&buffered_request, "request_archive").await;
    }
    let staged_request = match request_writer.finish_staged().await {
        Ok(staged) if staged.blake3_digest == request_digest.as_str() => staged,
        Ok(_) | Err(_) => {
            abandon_proxy_archive_attempt(&state.db, &request_archive_attempt).await;
            tracing::warn!(%request_id, stage = "request_archive_finish", "proxy archive failed");
            return finish_proxy_failure(&buffered_request, "request_archive").await;
        }
    };
    if staged_request.object_locator != request_archive_attempt.object_locator {
        abandon_proxy_archive_attempt(&state.db, &request_archive_attempt).await;
        tracing::warn!(%request_id, stage = "request_archive_locator", "proxy archive failed");
        return finish_proxy_failure(&buffered_request, "request_archive").await;
    }
    let attach_result = attach_proxy_archive_with_retry(
        &state.db,
        request_id,
        key.tenant_id,
        buffered_request.reservation.id,
        &admitted_request_object,
        &request_archive_attempt,
    )
    .await;
    if let Err(error) = attach_result {
        if !matches!(error, AppError::Internal) {
            abandon_proxy_archive_attempt(&state.db, &request_archive_attempt).await;
        }
        tracing::warn!(%request_id, stage = "request_archive_attach", "proxy archive failed");
        return finish_proxy_failure(&buffered_request, "request_archive").await;
    }
    if let Some((prepared, component_context)) = component_request {
        let Some(driver) = route_driver.as_deref() else {
            return finish_proxy_failure(&buffered_request, "provider_configuration").await;
        };
        let Some(config) = route_config.as_ref() else {
            return finish_proxy_failure(&buffered_request, "provider_configuration").await;
        };
        let Some(credential) = upstream_credential.as_ref() else {
            return finish_proxy_failure(&buffered_request, "provider_credential").await;
        };
        return execute_component_provider(
            buffered_request,
            driver,
            &base_url,
            config,
            credential,
            prepared,
            component_context,
        )
        .await;
    }
    let bridge_stream = forwarded_json
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let codex_downstream_stream = codex_plan
        .as_ref()
        .is_some_and(|plan| plan.downstream_stream);
    let (request_path, request_body) = if is_codex_route {
        (codex_transport::RESPONSES_PATH, forwarded_body)
    } else if route_driver.as_deref() == Some("cpa-subscription-bridge") {
        let Some(provider) = route_config
            .as_ref()
            .and_then(|config| config.get("provider"))
            .and_then(Value::as_str)
            .filter(|provider| matches!(*provider, "copilot" | "cursor"))
        else {
            return finish_proxy_failure(&buffered_request, "provider_configuration").await;
        };
        let Some(handle) = upstream_credential
            .as_ref()
            .and_then(UpstreamCredential::subscription_bridge_handle)
        else {
            return finish_proxy_failure(&buffered_request, "provider_credential").await;
        };
        let bridge_body = match serde_json::to_vec(&json!({
            "provider": provider,
            "handle": handle,
            "model": forwarded_json.get("model").and_then(Value::as_str),
            "stream": bridge_stream,
            "payload": forwarded_json
        })) {
            Ok(body) => body,
            Err(_) => return finish_proxy_failure(&buffered_request, "provider_request").await,
        };
        ("/v1/execute", bridge_body)
    } else {
        (protocol.path(), forwarded_body)
    };
    let outbound_base_url = if is_codex_route {
        codex_transport::outbound_base_url(&base_url)
    } else {
        base_url.clone()
    };
    let outbound_http = match route_config.as_ref() {
        Some(config) => match network::client_for_config_url(
            &state.http,
            &outbound_base_url,
            config,
            state.config.allow_oauth_loopback,
        )
        .await
        {
            Ok(client) => client,
            Err(_) => {
                tracing::warn!(%request_id, stage = "network_client", "proxy setup failed");
                return finish_proxy_failure(&buffered_request, "upstream_connection").await;
            }
        },
        // Legacy environment configuration is cluster-administrator owned and
        // may intentionally name an internal CPA service.
        None => state.http.clone(),
    };
    let mut request = outbound_http
        .post(format!(
            "{}{}",
            outbound_base_url.trim_end_matches('/'),
            request_path
        ))
        .body(request_body);
    if is_codex_route {
        let Some(credential) = upstream_credential.as_ref() else {
            return finish_proxy_failure(&buffered_request, "provider_credential").await;
        };
        request = match codex_transport::apply_wire_headers(request, credential, request_id) {
            Ok(request) => request,
            Err(_) => {
                tracing::warn!(%request_id, stage = "codex_headers", "proxy setup failed");
                return finish_proxy_failure(&buffered_request, "provider_credential").await;
            }
        };
    } else if let Some(credential) = upstream_credential.as_ref() {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                headers
                    .get(header::ACCEPT)
                    .cloned()
                    .unwrap_or(HeaderValue::from_static("application/json")),
            );
        request = match credential.apply(request, unix_millis()) {
            Ok(request) => request,
            Err(_) => {
                tracing::warn!(%request_id, stage = "credential_apply", "proxy setup failed");
                return finish_proxy_failure(&buffered_request, "provider_credential").await;
            }
        };
    } else if let Some(upstream_key) = legacy_upstream_key.as_ref() {
        request = request
            .header(header::CONTENT_TYPE, "application/json")
            .header(
                header::ACCEPT,
                headers
                    .get(header::ACCEPT)
                    .cloned()
                    .unwrap_or(HeaderValue::from_static("application/json")),
            );
        request = if protocol.is_openai() {
            request.bearer_auth(upstream_key)
        } else {
            request.header("x-api-key", upstream_key)
        };
    }
    if !is_codex_route {
        if let Some(version) = headers.get("anthropic-version") {
            request = request.header("anthropic-version", version);
        }
        if let Some(beta) = headers.get("anthropic-beta") {
            request = request.header("anthropic-beta", beta);
        }
    }

    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        route_driver.as_deref().unwrap_or("legacy"),
        "proxy",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        started.elapsed(),
    );
    let upstream = match upstream_result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                %request_id,
                stage = "send",
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                "proxy upstream request failed"
            );
            return finish_proxy_failure(&buffered_request, "upstream_connection").await;
        }
    };
    if route_driver.as_deref() == Some("cpa-subscription-bridge") {
        return finish_subscription_bridge_response(buffered_request, upstream, bridge_stream)
            .await;
    }
    let status = upstream.status();
    if !status.is_success() {
        drop(upstream);
        return finish_buffered_request(
            &buffered_request,
            status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"upstream rejected the request\",\"type\":\"upstream_error\"}}",
            ),
            "application/json",
            TokenUsage::default(),
            Some(format!("http_{}", status.as_u16())),
        )
        .await;
    }
    let content_type = upstream.headers().get(header::CONTENT_TYPE).cloned();
    if is_codex_route && !codex_transport::is_event_stream(&upstream) {
        drop(upstream);
        return finish_proxy_failure(&buffered_request, "upstream_invalid_content_type").await;
    }
    if is_codex_route && !codex_downstream_stream {
        let buffered = match codex_transport::buffer_response(upstream).await {
            Ok(buffered) => buffered,
            Err(error_code) => {
                tracing::warn!(%request_id, stage = error_code, "Codex upstream response failed");
                return finish_proxy_failure(&buffered_request, error_code).await;
            }
        };
        return finish_buffered_request(
            &buffered_request,
            StatusCode::OK,
            buffered.body,
            "application/json",
            buffered.usage,
            None,
        )
        .await;
    }
    let is_sse = bridge_stream
        || content_type
            .as_ref()
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(';').next())
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("text/event-stream"));
    let capture_json_usage = should_capture_buffered_usage(is_sse, content_type.as_ref());
    let mut response_archive_attempt =
        match begin_proxy_archive_attempt(&state.db, request_id, ArchiveStagingPurpose::Response)
            .await
        {
            Ok(attempt) => Some(attempt),
            Err(_) => {
                tracing::warn!(%request_id, stage = "response_archive_begin", "proxy archive gap");
                None
            }
        };
    let archive_writer = if let Some(attempt) = response_archive_attempt.as_ref() {
        match state.archive.start_writer(&attempt.object_locator).await {
            Ok(writer) => Some(writer),
            Err(_) => {
                abandon_proxy_archive_attempt(&state.db, attempt).await;
                response_archive_attempt = None;
                tracing::warn!(%request_id, stage = "response_archive", "proxy archive gap");
                None
            }
        }
    } else {
        None
    };
    let (body_sender, body_receiver) = tokio::sync::mpsc::channel(PROXY_BODY_CHANNEL_CAPACITY);
    let background_state = state.clone();
    let status_code = i64::from(status.as_u16());
    let BufferedRequest {
        reservation,
        started,
        input_token_ceiling,
        output_token_ceiling,
        requested_service_tier,
        conversation,
        tenant_id,
        ..
    } = buffered_request;
    tokio::spawn(async move {
        let mut upstream_stream = upstream.bytes_stream();
        let mut archive_writer = archive_writer;
        let mut response_archive_attempt = response_archive_attempt;
        let mut usage_capture = Vec::new();
        let mut sse_capture = is_sse.then(|| {
            if is_codex_route {
                ResponsesSseCapture::for_codex()
            } else {
                ResponsesSseCapture::default()
            }
        });
        let mut codex_streaming_sanitizer =
            is_codex_route.then(codex_transport::CodexStreamingSanitizer::default);
        let mut transport_error: Option<&'static str> = None;
        let mut response_bytes = 0_usize;
        let mut delivered_any = false;
        let mut delivered_billable = false;
        let hard_deadline = tokio::time::Instant::now() + MAX_PROXY_LIFETIME;
        let (archive_lease_lost_sender, mut archive_lease_lost_receiver) =
            tokio::sync::mpsc::channel(1);
        let archive_heartbeat_task = response_archive_attempt.clone().map(|mut attempt| {
            let heartbeat_database = background_state.db.clone();
            tokio::spawn(async move {
                let mut heartbeat = tokio::time::interval(Duration::from_millis(
                    u64::try_from(ARCHIVE_STAGING_WRITE_HEARTBEAT_MILLIS).unwrap_or(20_000),
                ));
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                heartbeat.tick().await;
                loop {
                    heartbeat.tick().await;
                    if !heartbeat_proxy_archive_attempt(&heartbeat_database, &mut attempt)
                        .await
                        .unwrap_or(false)
                    {
                        let _ = archive_lease_lost_sender.send(()).await;
                        break;
                    }
                }
            })
        });
        loop {
            let polled = tokio::select! {
                biased;
                _ = archive_lease_lost_receiver.recv(), if response_archive_attempt.is_some() => None,
                next = tokio::time::timeout_at(hard_deadline, upstream_stream.next()) => Some(next),
            };
            let Some(polled) = polled else {
                if let Some(writer) = archive_writer.take() {
                    drop(tokio::spawn(async move {
                        let _ = writer.abort().await;
                    }));
                }
                response_archive_attempt = None;
                tracing::warn!(%request_id, stage = "response_archive_heartbeat", "proxy archive lease lost; downstream response continues with an archive gap");
                continue;
            };
            let next = match polled {
                Ok(next) => next,
                Err(_) => {
                    transport_error = Some("upstream_timeout");
                    if let Some(writer) = archive_writer.take() {
                        let _ = writer.abort().await;
                    }
                    let _ = tokio::time::timeout(
                        MAX_DOWNSTREAM_SEND_WAIT,
                        body_sender.send(Err(std::io::Error::other("upstream stream timed out"))),
                    )
                    .await;
                    break;
                }
            };
            let Some(next) = next else {
                if codex_streaming_sanitizer
                    .as_ref()
                    .is_some_and(|sanitizer| !sanitizer.is_complete())
                {
                    transport_error = Some("upstream_incomplete_response");
                }
                break;
            };
            match next {
                Ok(raw_chunk) => {
                    response_bytes = response_bytes.saturating_add(raw_chunk.len());
                    if response_bytes > MAX_PROXY_RESPONSE_BODY {
                        transport_error = Some("upstream_response_too_large");
                        if let Some(writer) = archive_writer.take() {
                            let _ = writer.abort().await;
                        }
                        let _ = tokio::time::timeout(
                            MAX_DOWNSTREAM_SEND_WAIT,
                            body_sender.send(Err(std::io::Error::other(
                                "upstream response exceeded the size limit",
                            ))),
                        )
                        .await;
                        break;
                    }
                    let (chunk, chunk_billable) =
                        if let Some(sanitizer) = codex_streaming_sanitizer.as_mut() {
                            match sanitizer.push(&raw_chunk) {
                                Ok(chunk) => {
                                    let billable = sanitizer.last_push_billable();
                                    (chunk, billable)
                                }
                                Err(error_code) => {
                                    transport_error = Some(error_code);
                                    if let Some(writer) = archive_writer.take() {
                                        let _ = writer.abort().await;
                                    }
                                    let _ = tokio::time::timeout(
                                        MAX_DOWNSTREAM_SEND_WAIT,
                                        body_sender.send(Err(std::io::Error::other(
                                            "upstream stream violated the Responses protocol",
                                        ))),
                                    )
                                    .await;
                                    break;
                                }
                            }
                        } else {
                            (raw_chunk, true)
                        };
                    if capture_json_usage {
                        append_bounded(&mut usage_capture, &chunk, 2 * 1024 * 1024);
                    }
                    if let Some(capture) = sse_capture.as_mut() {
                        capture.push(&chunk);
                    }
                    if let Some(mut writer) = archive_writer.take() {
                        match writer.write(chunk.clone()).await {
                            Ok(()) => archive_writer = Some(writer),
                            Err(_) => {
                                tracing::warn!(%request_id, stage = "response_archive_stream", "proxy archive gap");
                                let _ = writer.abort().await;
                                if let Some(attempt) = response_archive_attempt.take() {
                                    abandon_proxy_archive_attempt(&background_state.db, &attempt)
                                        .await;
                                }
                            }
                        }
                    }
                    if chunk.is_empty() {
                        continue;
                    }
                    if !delivered_any {
                        match tokio::time::timeout(MAX_DOWNSTREAM_SEND_WAIT, body_sender.reserve())
                            .await
                        {
                            Ok(Ok(permit)) => {
                                match prepare_proxy_delivery_with_retry(
                                    &background_state.db,
                                    request_id,
                                    tenant_id,
                                    &reservation,
                                    input_token_ceiling,
                                    output_token_ceiling,
                                    requested_service_tier.as_deref(),
                                )
                                .await
                                {
                                    Ok(()) => {
                                        let initial_length =
                                            chunk.len().min(MAX_UNCONFIRMED_DELIVERY_BYTES);
                                        let initial = chunk.slice(..initial_length);
                                        let remaining = chunk.slice(initial_length..);
                                        permit.send(Ok::<Bytes, std::io::Error>(initial));
                                        record_delivered_chunk(
                                            &mut delivered_any,
                                            &mut delivered_billable,
                                            chunk_billable,
                                        );
                                        if confirm_proxy_delivery_with_retry(
                                            &background_state.db,
                                            request_id,
                                            tenant_id,
                                            &reservation,
                                        )
                                        .await
                                        .is_err()
                                        {
                                            transport_error = Some("delivery_state");
                                        } else if !remaining.is_empty() {
                                            match tokio::time::timeout(
                                                MAX_DOWNSTREAM_SEND_WAIT,
                                                body_sender
                                                    .send(Ok::<Bytes, std::io::Error>(remaining)),
                                            )
                                            .await
                                            {
                                                Ok(Ok(())) => {}
                                                Ok(Err(_)) => {
                                                    transport_error =
                                                        Some("downstream_disconnected")
                                                }
                                                Err(_) => {
                                                    transport_error =
                                                        Some("downstream_backpressure")
                                                }
                                            }
                                        }
                                    }
                                    Err(_) => {
                                        drop(permit);
                                        transport_error = Some("delivery_state");
                                        let _ = tokio::time::timeout(
                                            MAX_DOWNSTREAM_SEND_WAIT,
                                            body_sender.send(Err(std::io::Error::other(
                                                "response delivery could not be recorded",
                                            ))),
                                        )
                                        .await;
                                    }
                                }
                            }
                            Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                            Err(_) => transport_error = Some("downstream_backpressure"),
                        }
                    } else {
                        match tokio::time::timeout(
                            MAX_DOWNSTREAM_SEND_WAIT,
                            body_sender.send(Ok::<Bytes, std::io::Error>(chunk)),
                        )
                        .await
                        {
                            Ok(Ok(())) => record_delivered_chunk(
                                &mut delivered_any,
                                &mut delivered_billable,
                                chunk_billable,
                            ),
                            Ok(Err(_)) => transport_error = Some("downstream_disconnected"),
                            Err(_) => transport_error = Some("downstream_backpressure"),
                        }
                    }
                    if transport_error.is_some() {
                        if let Some(writer) = archive_writer.take() {
                            let _ = writer.abort().await;
                        }
                        break;
                    }
                }
                Err(_) => {
                    transport_error = Some("upstream_stream");
                    if let Some(writer) = archive_writer.take() {
                        let _ = writer.abort().await;
                    }
                    let _ = tokio::time::timeout(
                        MAX_DOWNSTREAM_SEND_WAIT,
                        body_sender.send(Err(std::io::Error::other("upstream stream failed"))),
                    )
                    .await;
                    break;
                }
            }
        }
        if transport_error.is_some() {
            if let Some(writer) = archive_writer.take() {
                let _ = writer.abort().await;
            }
            if let Some(attempt) = response_archive_attempt.take() {
                abandon_proxy_archive_attempt(&background_state.db, &attempt).await;
            }
        }
        let sse_summary = sse_capture.map(ResponsesSseCapture::finish_summary);
        let stored_response = if transport_error.is_none()
            && let Some(writer) = archive_writer
        {
            match writer.finish_staged().await {
                Ok(staged)
                    if response_archive_attempt
                        .as_ref()
                        .is_some_and(|attempt| attempt.object_locator == staged.object_locator) =>
                {
                    staged.object_locator
                }
                Ok(_) | Err(_) => {
                    if let Some(attempt) = response_archive_attempt.take() {
                        abandon_proxy_archive_attempt(&background_state.db, &attempt).await;
                    }
                    tracing::warn!(%request_id, stage = "response_archive_finish", "proxy archive gap");
                    format!("gap://{request_id}/response")
                }
            }
        } else {
            format!("gap://{request_id}/response")
        };
        let protocol_error = match sse_summary.as_ref().map(|summary| &summary.outcome) {
            Some(ResponsesSseOutcome::Failed) => Some("upstream_failed_response"),
            Some(ResponsesSseOutcome::Incomplete) => Some("upstream_incomplete_response"),
            Some(ResponsesSseOutcome::Completed { .. }) | None => None,
        };
        let mut terminal_status = status_code;
        let mut error_code = transport_error.or(protocol_error);
        if error_code.is_some() {
            terminal_status = 502;
        }
        let full_contract_usage = || TokenUsage {
            input_tokens: input_token_ceiling,
            output_tokens: output_token_ceiling,
            ..TokenUsage::default()
        };
        let mut charge_contract_ceiling = delivered_billable && error_code.is_some();
        let mut usage = if error_code.is_some() {
            if delivered_billable {
                full_contract_usage()
            } else {
                TokenUsage::default()
            }
        } else {
            let extracted_usage = match sse_summary.as_ref() {
                Some(summary) if summary.usage_invalid => ExtractedUsage::Invalid,
                Some(summary) => summary.usage.clone().map_or_else(
                    || {
                        if is_codex_route {
                            ExtractedUsage::Invalid
                        } else {
                            ExtractedUsage::Missing
                        }
                    },
                    ExtractedUsage::Valid,
                ),
                None => extract_usage_checked(&usage_capture),
            };
            match extracted_usage {
                ExtractedUsage::Valid(usage) => usage,
                ExtractedUsage::Missing => {
                    charge_contract_ceiling = delivered_billable;
                    if delivered_billable {
                        full_contract_usage()
                    } else {
                        TokenUsage::default()
                    }
                }
                ExtractedUsage::Invalid => {
                    terminal_status = 502;
                    error_code = Some("upstream_invalid_usage");
                    charge_contract_ceiling = delivered_billable;
                    if delivered_billable {
                        full_contract_usage()
                    } else {
                        TokenUsage::default()
                    }
                }
            }
        };
        match crate::db::normalize_proxy_usage(
            &usage,
            input_token_ceiling,
            output_token_ceiling,
            requested_service_tier.as_deref(),
        ) {
            Ok(normalized) => usage = normalized,
            Err(AppError::Upstream(_)) => {
                terminal_status = 502;
                error_code = Some("upstream_invalid_usage");
                charge_contract_ceiling = delivered_billable;
                usage = if delivered_billable {
                    full_contract_usage()
                } else {
                    TokenUsage::default()
                };
            }
            Err(_) => {
                terminal_status = 502;
                error_code = Some("upstream_invalid_usage");
                charge_contract_ceiling = delivered_billable;
                usage = if delivered_billable {
                    full_contract_usage()
                } else {
                    TokenUsage::default()
                };
            }
        }
        let response_id = if (200..400).contains(&terminal_status)
            && matches!(protocol, Protocol::OpenAiResponses)
        {
            match sse_summary.as_ref().map(|summary| &summary.outcome) {
                Some(ResponsesSseOutcome::Completed { response_id }) => response_id.clone(),
                None => extract_response_id(&usage_capture),
                Some(ResponsesSseOutcome::Failed | ResponsesSseOutcome::Incomplete) => None,
            }
        } else {
            None
        };
        let conversation_input = conversation
            .as_ref()
            .map(|conversation| ProxyConversationInput {
                key: &conversation.key,
                request_json: &conversation.request_json,
                hints: &conversation.hints,
                client_name: conversation.client_name.as_deref(),
                upstream_response_id: response_id.as_deref(),
            });
        let terminal_result = finish_proxy_request_with_retry(
            &background_state.db,
            FinishProxyRequest {
                request_id,
                tenant_id,
                reservation: &reservation,
                input_token_ceiling,
                output_token_ceiling,
                requested_service_tier: requested_service_tier.as_deref(),
                status_code: terminal_status,
                duration_ms: started.elapsed().as_millis() as i64,
                usage,
                charge_contract_ceiling,
                error_code,
                response_object: &stored_response,
                conversation: conversation_input,
            },
            response_archive_attempt.as_ref(),
        )
        .await;
        if response_archive_requires_cleanup(&terminal_result, &stored_response)
            && let Some(attempt) = response_archive_attempt.as_ref()
        {
            abandon_proxy_archive_attempt(&background_state.db, attempt).await;
        }
        if let Some(task) = archive_heartbeat_task {
            task.abort();
        }
        if terminal_result.is_err() {
            // The commit can be durable even when its acknowledgement is lost.
            // Preserve this request-scoped archive until its database owner is
            // known; deleting it here could leave a committed row dangling.
            tracing::error!(%request_id, stage = "terminal_transaction", "proxy request finalization failed");
        }
    });
    let mut response = Response::builder()
        .status(status)
        .header(REQUEST_ID_HEADER, request_id.to_string());
    if let Some(content_type) = content_type {
        response = response.header(header::CONTENT_TYPE, content_type);
    }
    response
        .body(Body::from_stream(ReceiverStream::new(body_receiver)))
        .map_err(|_| AppError::Internal)
}

fn record_delivered_chunk(
    delivered_any: &mut bool,
    delivered_billable: &mut bool,
    chunk_billable: bool,
) {
    *delivered_any = true;
    *delivered_billable |= chunk_billable;
}

#[derive(Clone)]
struct ProxyConversation {
    key: AuthenticatedKey,
    request_json: Value,
    hints: crate::conversation::ConversationHints,
    client_name: Option<String>,
}

struct BufferedRequest<'a> {
    state: &'a AppState,
    reservation: crate::model::UsageReservation,
    request_id: Uuid,
    started: Instant,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    requested_service_tier: Option<String>,
    conversation: Option<ProxyConversation>,
    protocol: Protocol,
    tenant_id: Uuid,
}

#[allow(clippy::too_many_arguments)]
async fn execute_component_provider(
    request: BufferedRequest<'_>,
    driver: &str,
    base_url: &str,
    config: &Value,
    credential: &UpstreamCredential,
    prepared: PreparedProviderRequest,
    context: RequestContext,
) -> Result<Response, AppError> {
    let target = match component_provider_url(base_url, &prepared.path) {
        Ok(target) => target,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_target", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_unsafe_target").await;
        }
    };
    let outbound_http = match network::client_for_config_url(
        &request.state.http,
        &target,
        config,
        request.state.config.allow_oauth_loopback,
    )
    .await
    {
        Ok(client) => client,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_network_client", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_unsafe_target").await;
        }
    };
    let mut upstream_request = outbound_http
        .request(prepared.method, target)
        .timeout(component_provider_timeout(config))
        .body(prepared.body);
    for (name, value) in prepared.headers {
        let name = match reqwest::header::HeaderName::from_bytes(name.as_bytes()) {
            Ok(name) => name,
            Err(_) => {
                return finish_component_provider_failure(&request, "provider_invalid_request")
                    .await;
            }
        };
        let value = match reqwest::header::HeaderValue::from_str(&value) {
            Ok(value) => value,
            Err(_) => {
                return finish_component_provider_failure(&request, "provider_invalid_request")
                    .await;
            }
        };
        upstream_request = upstream_request.header(name, value);
    }
    upstream_request = match credential.apply(upstream_request, unix_millis()) {
        Ok(request) => request,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_credential", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_credential").await;
        }
    };
    let upstream_started = Instant::now();
    let upstream_result = upstream_request.send().await;
    request.state.metrics.observe_upstream(
        driver,
        "component_provider",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    let upstream = match upstream_result {
        Ok(response) => response,
        Err(error) => {
            tracing::warn!(
                request_id = %request.request_id,
                is_timeout = error.is_timeout(),
                is_connect = error.is_connect(),
                "component provider upstream request failed"
            );
            return finish_component_provider_failure(&request, "upstream_connection").await;
        }
    };
    let upstream_status = upstream.status();
    if !upstream_status.is_success() && !upstream_status.is_redirection() {
        drop(upstream);
        return finish_buffered_request(
            &request,
            upstream_status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"component provider rejected the request\"}}",
            ),
            "application/json",
            TokenUsage::default(),
            Some(format!("http_{}", upstream_status.as_u16())),
        )
        .await;
    }
    let mut upstream_headers = BTreeMap::new();
    for (name, value) in upstream.headers() {
        let value = match value.to_str() {
            Ok(value) => value,
            Err(_) => {
                return finish_component_provider_failure(&request, "upstream_invalid_headers")
                    .await;
            }
        };
        upstream_headers.insert(name.to_string(), value.to_owned());
    }
    let Some(maximum) = request
        .state
        .providers
        .get(driver)
        .and_then(|provider| provider.component_adapter.as_ref())
        .map(|adapter| adapter.max_response_bytes)
    else {
        return finish_component_provider_failure(&request, "provider_configuration").await;
    };
    let upstream_body = match read_bounded_upstream(upstream, maximum).await {
        Ok(body) => body,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_response", "component provider request failed");
            return finish_component_provider_failure(&request, "upstream_response_too_large")
                .await;
        }
    };
    let normalized = match normalize_component_provider(
        request.state,
        driver,
        context,
        upstream_status.as_u16(),
        upstream_headers,
        upstream_body,
    )
    .await
    {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(request_id = %request.request_id, stage = "component_normalize", "component provider request failed");
            return finish_component_provider_failure(&request, "provider_normalize").await;
        }
    };
    let status = match StatusCode::from_u16(normalized.status) {
        Ok(status) => status,
        Err(_) => {
            return finish_component_provider_failure(&request, "provider_invalid_response").await;
        }
    };
    if !status.is_success() {
        return finish_buffered_request(
            &request,
            status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"component provider rejected the request\"}}",
            ),
            "application/json",
            TokenUsage::default(),
            Some(format!("http_{}", status.as_u16())),
        )
        .await;
    }
    let input_tokens = i64::try_from(normalized.input_tokens).ok();
    let output_tokens = i64::try_from(normalized.output_tokens).ok();
    let usage_is_valid = input_tokens.is_some_and(|tokens| {
        (0..=MAX_REPORTED_TOKENS).contains(&tokens) && tokens <= request.input_token_ceiling
    }) && output_tokens.is_some_and(|tokens| {
        (0..=MAX_REPORTED_TOKENS).contains(&tokens) && tokens <= request.output_token_ceiling
    });
    if !usage_is_valid {
        return finish_component_provider_failure(&request, "upstream_invalid_usage").await;
    }
    if normalized.estimated {
        tracing::debug!(request_id = %request.request_id, stage = "component_usage", "component provider reported estimated usage");
    }
    let usage = if status.is_success() {
        TokenUsage {
            input_tokens: input_tokens.unwrap_or_default(),
            output_tokens: output_tokens.unwrap_or_default(),
            ..TokenUsage::default()
        }
    } else {
        TokenUsage::default()
    };
    let content_type = normalized
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .map(|(_, value)| value.as_str())
        .unwrap_or("application/json");
    finish_buffered_request(
        &request,
        status,
        Bytes::from(normalized.body),
        content_type,
        usage,
        None,
    )
    .await
}

async fn finish_component_provider_failure(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    finish_buffered_request(
        request,
        StatusCode::BAD_GATEWAY,
        Bytes::from_static(b"{\"error\":{\"message\":\"component provider request failed\"}}"),
        "application/json",
        TokenUsage::default(),
        Some(error_code.to_owned()),
    )
    .await
}

async fn finish_proxy_failure(
    request: &BufferedRequest<'_>,
    error_code: &str,
) -> Result<Response, AppError> {
    finish_buffered_request(
        request,
        StatusCode::BAD_GATEWAY,
        Bytes::from_static(
            b"{\"error\":{\"message\":\"upstream request failed\",\"type\":\"upstream_error\"}}",
        ),
        "application/json",
        TokenUsage::default(),
        Some(error_code.to_owned()),
    )
    .await
}

async fn finish_subscription_bridge_response(
    request: BufferedRequest<'_>,
    upstream: reqwest::Response,
    stream: bool,
) -> Result<Response, AppError> {
    let request_id = request.request_id;
    let upstream_status = upstream.status();
    if !upstream_status.is_success() {
        drop(upstream);
        return finish_buffered_request(
            &request,
            upstream_status,
            Bytes::from_static(
                b"{\"error\":{\"message\":\"subscription bridge rejected the request\"}}",
            ),
            "application/json",
            TokenUsage::default(),
            Some(format!("http_{}", upstream_status.as_u16())),
        )
        .await;
    }
    let raw = match read_bounded_upstream(upstream, MAX_SUBSCRIPTION_BRIDGE_RESPONSE).await {
        Ok(raw) => raw,
        Err(_) => {
            tracing::warn!(%request_id, stage = "bridge_response", "subscription bridge response failed");
            return finish_buffered_request(
                &request,
                StatusCode::BAD_GATEWAY,
                Bytes::from_static(
                    b"{\"error\":{\"message\":\"subscription bridge response failed\"}}",
                ),
                "application/json",
                TokenUsage::default(),
                Some("upstream_stream".to_owned()),
            )
            .await;
        }
    };
    let wrapper: Value = match serde_json::from_slice(&raw) {
        Ok(wrapper) => wrapper,
        Err(_) => {
            return finish_buffered_request(
                &request,
                StatusCode::BAD_GATEWAY,
                Bytes::from_static(
                    b"{\"error\":{\"message\":\"subscription bridge returned invalid JSON\"}}",
                ),
                "application/json",
                TokenUsage::default(),
                Some("upstream_invalid_json".to_owned()),
            )
            .await;
        }
    };
    let (body, content_type) = match unwrap_subscription_bridge_body(&wrapper, stream) {
        Ok(response) => response,
        Err(_) => {
            tracing::warn!(%request_id, stage = "bridge_shape", "subscription bridge response failed");
            return finish_buffered_request(
                &request,
                StatusCode::BAD_GATEWAY,
                Bytes::from_static(b"{\"error\":{\"message\":\"subscription bridge returned an invalid response\"}}"),
                "application/json",
                TokenUsage::default(),
                Some("upstream_invalid_response".to_owned()),
            )
            .await;
        }
    };
    let usage = match extract_usage_checked(&body) {
        ExtractedUsage::Invalid => {
            return finish_proxy_failure(&request, "upstream_invalid_usage").await;
        }
        ExtractedUsage::Valid(usage) if usage.total_tokens() > 0 => usage,
        ExtractedUsage::Valid(_) | ExtractedUsage::Missing => TokenUsage {
            input_tokens: estimated_tokens(request.input_token_ceiling),
            output_tokens: estimated_tokens(i64::try_from(body.len()).unwrap_or(i64::MAX))
                .min(request.output_token_ceiling),
            ..TokenUsage::default()
        },
    };
    finish_buffered_request(&request, StatusCode::OK, body, content_type, usage, None).await
}

fn unwrap_subscription_bridge_body(
    wrapper: &Value,
    stream: bool,
) -> Result<(Bytes, &'static str), AppError> {
    if stream {
        let chunks = wrapper
            .get("chunks")
            .and_then(Value::as_array)
            .ok_or_else(|| AppError::Upstream("subscription bridge returned no chunks".into()))?;
        let mut body = Vec::new();
        for chunk in chunks {
            let chunk = chunk.as_str().ok_or_else(|| {
                AppError::Upstream("subscription bridge returned an invalid chunk".into())
            })?;
            if body.len().saturating_add(chunk.len()) > MAX_SUBSCRIPTION_BRIDGE_RESPONSE {
                return Err(AppError::Upstream(
                    "subscription bridge stream is too large".into(),
                ));
            }
            body.extend_from_slice(chunk.as_bytes());
        }
        Ok((Bytes::from(body), "text/event-stream"))
    } else {
        let payload = wrapper
            .get("payload")
            .ok_or_else(|| AppError::Upstream("subscription bridge returned no payload".into()))?;
        Ok((
            Bytes::from(serde_json::to_vec(payload).map_err(|_| AppError::Internal)?),
            "application/json",
        ))
    }
}

async fn finish_buffered_request(
    request: &BufferedRequest<'_>,
    mut status: StatusCode,
    mut body: Bytes,
    content_type: &str,
    usage: TokenUsage,
    mut error_code: Option<String>,
) -> Result<Response, AppError> {
    let request_id = request.request_id;
    let usage = match crate::db::normalize_proxy_usage(
        &usage,
        request.input_token_ceiling,
        request.output_token_ceiling,
        request.requested_service_tier.as_deref(),
    ) {
        Ok(usage) => usage,
        Err(AppError::Upstream(_)) => {
            status = StatusCode::BAD_GATEWAY;
            body = Bytes::from_static(
                b"{\"error\":{\"message\":\"upstream returned invalid usage\",\"type\":\"upstream_error\"}}",
            );
            error_code = Some("upstream_invalid_usage".to_owned());
            TokenUsage::default()
        }
        Err(error) => return Err(error),
    };
    let response_id = (status.is_success()
        && error_code.is_none()
        && matches!(request.protocol, Protocol::OpenAiResponses))
    .then(|| extract_response_id(&body))
    .flatten();
    let mut response_archive_attempt = match begin_proxy_archive_attempt(
        &request.state.db,
        request_id,
        ArchiveStagingPurpose::Response,
    )
    .await
    {
        Ok(attempt) => Some(attempt),
        Err(_) => {
            tracing::warn!(%request_id, stage = "buffered_response_archive_begin", "proxy archive gap");
            None
        }
    };
    let stored_response = if let Some(attempt) = response_archive_attempt.as_ref() {
        match request
            .state
            .archive
            .start_writer(&attempt.object_locator)
            .await
        {
            Ok(mut writer) => {
                if writer.write(body.clone()).await.is_err() {
                    let _ = writer.abort().await;
                    abandon_proxy_archive_attempt(&request.state.db, attempt).await;
                    response_archive_attempt = None;
                    tracing::warn!(%request_id, stage = "buffered_response_archive_write", "proxy archive gap");
                    format!("gap://{request_id}/response")
                } else {
                    match writer.finish_staged().await {
                        Ok(staged) if staged.object_locator == attempt.object_locator => {
                            staged.object_locator
                        }
                        Ok(_) | Err(_) => {
                            abandon_proxy_archive_attempt(&request.state.db, attempt).await;
                            response_archive_attempt = None;
                            tracing::warn!(%request_id, stage = "buffered_response_archive_finish", "proxy archive gap");
                            format!("gap://{request_id}/response")
                        }
                    }
                }
            }
            Err(_) => {
                abandon_proxy_archive_attempt(&request.state.db, attempt).await;
                response_archive_attempt = None;
                tracing::warn!(%request_id, stage = "buffered_response_archive_start", "proxy archive gap");
                format!("gap://{request_id}/response")
            }
        }
    } else {
        format!("gap://{request_id}/response")
    };
    let conversation = request
        .conversation
        .as_ref()
        .map(|conversation| ProxyConversationInput {
            key: &conversation.key,
            request_json: &conversation.request_json,
            hints: &conversation.hints,
            client_name: conversation.client_name.as_deref(),
            upstream_response_id: response_id.as_deref(),
        });
    let result = finish_proxy_request_with_retry(
        &request.state.db,
        FinishProxyRequest {
            request_id,
            tenant_id: request.tenant_id,
            reservation: &request.reservation,
            input_token_ceiling: request.input_token_ceiling,
            output_token_ceiling: request.output_token_ceiling,
            requested_service_tier: request.requested_service_tier.as_deref(),
            status_code: i64::from(status.as_u16()),
            duration_ms: request.started.elapsed().as_millis() as i64,
            usage,
            charge_contract_ceiling: false,
            error_code: error_code.as_deref(),
            response_object: &stored_response,
            conversation,
        },
        response_archive_attempt.as_ref(),
    )
    .await;
    if response_archive_requires_cleanup(&result, &stored_response)
        && let Some(attempt) = response_archive_attempt.as_ref()
    {
        abandon_proxy_archive_attempt(&request.state.db, attempt).await;
    }
    let result = result?;
    if matches!(result, FinishProxyRequestResult::AlreadyFinished { .. }) {
        tracing::debug!(%request_id, stage = "terminal_replay", "proxy request already finalized");
    }
    Response::builder()
        .status(status)
        .header(REQUEST_ID_HEADER, request_id.to_string())
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .map_err(|_| AppError::Internal)
}

async fn read_bounded_upstream(
    response: reqwest::Response,
    maximum: usize,
) -> Result<Vec<u8>, AppError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum as u64)
    {
        return Err(AppError::Upstream("upstream response is too large".into()));
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| AppError::Upstream(error.to_string()))?;
        if body.len().saturating_add(chunk.len()) > maximum {
            return Err(AppError::Upstream("upstream response is too large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

fn estimated_tokens(bytes: i64) -> i64 {
    bytes.max(0).saturating_add(3) / 4
}

enum ExtractedUsage {
    Missing,
    Valid(TokenUsage),
    Invalid,
}

fn merge_streaming_usage(current: &mut TokenUsage, next: TokenUsage) -> Result<(), ()> {
    current.input_tokens = current.input_tokens.max(next.input_tokens);
    current.cached_input_tokens = current.cached_input_tokens.max(next.cached_input_tokens);
    current.cache_write_tokens = current.cache_write_tokens.max(next.cache_write_tokens);
    current.output_tokens = current.output_tokens.max(next.output_tokens);
    if let Some(next_tier) = next.service_tier {
        match current.service_tier.as_deref() {
            None => current.service_tier = Some(next_tier),
            Some(current_tier) if current_tier == next_tier => {}
            Some(_) => return Err(()),
        }
    }
    Ok(())
}

fn extract_usage_checked(body: &[u8]) -> ExtractedUsage {
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return match usage_from_value_checked(&value) {
            Ok(Some(usage)) => ExtractedUsage::Valid(usage),
            Ok(None) => ExtractedUsage::Missing,
            Err(()) => ExtractedUsage::Invalid,
        };
    }
    let mut result: Option<TokenUsage> = None;
    for line in body.split(|byte| *byte == b'\n') {
        let Some(line) = line
            .strip_prefix(b"data: ")
            .or_else(|| line.strip_prefix(b"data:"))
        else {
            continue;
        };
        let line = trim_ascii_whitespace(line);
        if line.is_empty() || line == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(line) else {
            return ExtractedUsage::Invalid;
        };
        match usage_from_value_checked(&value) {
            Err(()) => return ExtractedUsage::Invalid,
            Ok(None) => continue,
            Ok(Some(next)) => {
                let current = result.get_or_insert_with(TokenUsage::default);
                if merge_streaming_usage(current, next).is_err() {
                    return ExtractedUsage::Invalid;
                }
            }
        }
    }
    match result {
        Some(usage) => ExtractedUsage::Valid(usage),
        None => ExtractedUsage::Missing,
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ResponsesSseEventKind {
    Lifecycle,
    Completed,
    Failed,
    Other,
}

impl ResponsesSseEventKind {
    fn from_name(name: &[u8]) -> Self {
        match trim_ascii_whitespace(name) {
            b"response.created" | b"response.in_progress" => Self::Lifecycle,
            b"response.completed" | b"message_stop" => Self::Completed,
            b"response.failed" | b"response.incomplete" | b"error" | b"response.error" => {
                Self::Failed
            }
            _ => Self::Other,
        }
    }

    fn is_response_lifecycle(self) -> bool {
        matches!(self, Self::Lifecycle | Self::Completed | Self::Failed)
    }
}

#[derive(Default)]
struct ResponsesSseCapture {
    line: Vec<u8>,
    data: Vec<u8>,
    event_kind: Option<ResponsesSseEventKind>,
    response_id: Option<String>,
    discard_line: bool,
    discard_event: bool,
    invalid: bool,
    terminal_success: bool,
    terminal_failure: bool,
    usage: Option<TokenUsage>,
    usage_invalid: bool,
    require_explicit_completed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum ResponsesSseOutcome {
    Completed { response_id: Option<String> },
    Failed,
    Incomplete,
}

struct ResponsesSseSummary {
    outcome: ResponsesSseOutcome,
    usage: Option<TokenUsage>,
    usage_invalid: bool,
}

impl ResponsesSseCapture {
    fn for_codex() -> Self {
        Self {
            require_explicit_completed: true,
            ..Self::default()
        }
    }

    fn push(&mut self, chunk: &[u8]) {
        for &byte in chunk {
            if byte == b'\n' {
                self.finish_line();
            } else if self.discard_line {
                continue;
            } else if self.line.len() >= MAX_RESPONSES_SSE_EVENT_BYTES {
                self.line.clear();
                self.discard_line = true;
                self.discard_event = true;
                self.invalid = true;
            } else {
                self.line.push(byte);
            }
        }
    }

    fn finish_summary(mut self) -> ResponsesSseSummary {
        if !self.discard_line && !self.line.is_empty() {
            self.finish_line();
        }
        if !self.data.is_empty() || self.discard_event || self.event_kind.is_some() {
            self.dispatch_event();
        }
        let outcome = if self.terminal_failure {
            ResponsesSseOutcome::Failed
        } else if self.invalid {
            ResponsesSseOutcome::Incomplete
        } else if self.terminal_success {
            ResponsesSseOutcome::Completed {
                response_id: self.response_id,
            }
        } else {
            ResponsesSseOutcome::Incomplete
        };
        ResponsesSseSummary {
            outcome,
            usage: self.usage,
            usage_invalid: self.usage_invalid,
        }
    }

    #[cfg(test)]
    fn finish(self) -> ResponsesSseOutcome {
        self.finish_summary().outcome
    }

    fn finish_line(&mut self) {
        if self.discard_line {
            self.discard_line = false;
            self.line.clear();
            return;
        }
        let mut line = std::mem::take(&mut self.line);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            self.dispatch_event();
            return;
        }
        if self.discard_event {
            return;
        }
        if line == b"data" || line.starts_with(b"data:") {
            let value = if line == b"data" {
                &[][..]
            } else {
                line[5..].strip_prefix(b" ").unwrap_or(&line[5..])
            };
            let separator = usize::from(!self.data.is_empty());
            if self
                .data
                .len()
                .saturating_add(separator)
                .saturating_add(value.len())
                > MAX_RESPONSES_SSE_EVENT_BYTES
            {
                self.data.clear();
                self.discard_event = true;
                self.invalid = true;
                return;
            }
            if separator == 1 {
                self.data.push(b'\n');
            }
            self.data.extend_from_slice(value);
        } else if line == b"event" || line.starts_with(b"event:") {
            let value = if line == b"event" {
                &[][..]
            } else {
                line[6..].strip_prefix(b" ").unwrap_or(&line[6..])
            };
            self.event_kind = Some(ResponsesSseEventKind::from_name(value));
        }
    }

    fn dispatch_event(&mut self) {
        let data = std::mem::take(&mut self.data);
        let event_kind = self.event_kind.take();
        let discard = std::mem::take(&mut self.discard_event);
        if discard {
            match event_kind {
                Some(ResponsesSseEventKind::Completed) => self.terminal_success = true,
                Some(ResponsesSseEventKind::Failed) => self.terminal_failure = true,
                Some(ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other) | None => {}
            }
            return;
        }
        if data.is_empty() {
            match event_kind {
                Some(ResponsesSseEventKind::Completed) => self.terminal_success = true,
                Some(ResponsesSseEventKind::Failed) => self.terminal_failure = true,
                Some(ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other) | None => {}
            }
            return;
        }
        let data = trim_ascii_whitespace(&data);
        if data == b"[DONE]" {
            if matches!(event_kind, Some(ResponsesSseEventKind::Failed)) {
                if self.require_explicit_completed && self.terminal_failure {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            } else if !self.require_explicit_completed {
                self.terminal_success = true;
            }
            return;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            self.invalid = true;
            return;
        };
        match usage_from_value_checked(&value) {
            Err(()) => self.usage_invalid = true,
            Ok(None) => {}
            Ok(Some(next)) => {
                let current = self.usage.get_or_insert_with(TokenUsage::default);
                if merge_streaming_usage(current, next).is_err() {
                    self.usage_invalid = true;
                }
            }
        }
        if value.get("error").is_some() || value.pointer("/response/error").is_some() {
            self.terminal_failure = true;
        }
        let payload_kind = value
            .get("type")
            .and_then(Value::as_str)
            .map(|name| ResponsesSseEventKind::from_name(name.as_bytes()));
        if self.require_explicit_completed
            && let (Some(event_kind), Some(payload_kind)) = (event_kind, payload_kind)
            && event_kind != payload_kind
            && (event_kind.is_response_lifecycle() || payload_kind.is_response_lifecycle())
        {
            self.invalid = true;
            if matches!(event_kind, ResponsesSseEventKind::Failed)
                || matches!(payload_kind, ResponsesSseEventKind::Failed)
            {
                self.terminal_failure = true;
            }
            return;
        }
        let kind = payload_kind
            .or(event_kind)
            .unwrap_or(ResponsesSseEventKind::Other);
        if kind.is_response_lifecycle()
            && let Some(response_id) = value
                .pointer("/response/id")
                .or_else(|| value.get("id"))
                .and_then(Value::as_str)
                .and_then(safe_conversation_hint)
        {
            match self.response_id.as_deref() {
                None => self.response_id = Some(response_id),
                Some(current) if current == response_id => {}
                Some(_) => self.invalid = true,
            }
        }
        match kind {
            ResponsesSseEventKind::Completed => {
                if self.require_explicit_completed
                    && (self.terminal_success || self.terminal_failure)
                {
                    self.invalid = true;
                }
                self.terminal_success = true;
            }
            ResponsesSseEventKind::Failed => {
                if self.require_explicit_completed
                    && (self.terminal_success || self.terminal_failure)
                {
                    self.invalid = true;
                }
                self.terminal_failure = true;
            }
            ResponsesSseEventKind::Lifecycle | ResponsesSseEventKind::Other => {}
        }
    }
}

fn trim_ascii_whitespace(mut value: &[u8]) -> &[u8] {
    while value.first().is_some_and(u8::is_ascii_whitespace) {
        value = &value[1..];
    }
    while value.last().is_some_and(u8::is_ascii_whitespace) {
        value = &value[..value.len() - 1];
    }
    value
}

fn should_capture_buffered_usage(is_sse: bool, content_type: Option<&HeaderValue>) -> bool {
    if is_sse {
        return false;
    }
    let Some(media_type) = content_type
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(str::trim)
    else {
        // Some compatible upstreams omit Content-Type even though their body
        // is JSON. Preserve the existing usage parsing contract for them.
        return content_type.is_none();
    };
    media_type.eq_ignore_ascii_case("application/json")
        || media_type
            .get(media_type.len().saturating_sub("+json".len())..)
            .is_some_and(|suffix| suffix.eq_ignore_ascii_case("+json"))
}

#[cfg(test)]
fn completed_response_id(
    status: StatusCode,
    transport_complete: bool,
    responses_sse: bool,
    streamed_response_id: Option<String>,
    buffered_tail: &[u8],
) -> Option<String> {
    if !status.is_success() || !transport_complete {
        return None;
    }
    if responses_sse {
        streamed_response_id
    } else {
        extract_response_id(buffered_tail)
    }
}

fn extract_response_id(body: &[u8]) -> Option<String> {
    const MAX_RESPONSE_ID_SCAN_BYTES: usize = 2 * 1024 * 1024;

    fn id_from_value(value: &Value) -> Option<String> {
        value
            .pointer("/response/id")
            .or_else(|| value.get("id"))
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
    }

    let body = body.get(..body.len().min(MAX_RESPONSE_ID_SCAN_BYTES))?;
    if let Ok(value) = serde_json::from_slice::<Value>(body) {
        return id_from_value(&value);
    }

    let mut top_level_id = None;
    for line in body.split(|byte| *byte == b'\n') {
        let Some(data) = line.strip_prefix(b"data:") else {
            continue;
        };
        let data = data.strip_prefix(b" ").unwrap_or(data);
        let data = data.strip_suffix(b"\r").unwrap_or(data);
        if data == b"[DONE]" {
            continue;
        }
        let Ok(value) = serde_json::from_slice::<Value>(data) else {
            continue;
        };
        if let Some(response_id) = value
            .pointer("/response/id")
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
        {
            return Some(response_id);
        }
        if top_level_id.is_none() {
            top_level_id = value
                .get("id")
                .and_then(Value::as_str)
                .and_then(safe_conversation_hint);
        }
    }
    top_level_id
}

fn usage_from_value_checked(value: &Value) -> Result<Option<TokenUsage>, ()> {
    let Some(usage) = value
        .get("usage")
        .or_else(|| value.pointer("/message/usage"))
        .or_else(|| value.pointer("/response/usage"))
    else {
        return Ok(None);
    };
    if usage.is_null() {
        return Ok(None);
    }
    let usage = usage.as_object().ok_or(())?;
    let integer = |field: &str| -> Result<Option<i64>, ()> {
        usage
            .get(field)
            .map(|value| value.as_i64().ok_or(()))
            .transpose()
    };
    let input = match integer("input_tokens")? {
        Some(value) => Some(value),
        None => integer("prompt_tokens")?,
    };
    let output = match integer("output_tokens")? {
        Some(value) => Some(value),
        None => integer("completion_tokens")?,
    };
    let (reported_input, output) = match (input, output) {
        (Some(input), Some(output)) => (input, output),
        (Some(input), None) => (input, 0),
        (None, Some(output)) => (0, output),
        (None, None) => return Err(()),
    };
    let details_integer = |details_field: &str| -> Result<Option<i64>, ()> {
        let Some(details) = usage.get(details_field) else {
            return Ok(None);
        };
        let details = details.as_object().ok_or(())?;
        details
            .get("cached_tokens")
            .map(|value| value.as_i64().ok_or(()))
            .transpose()
    };
    let cached_input = match details_integer("input_tokens_details")? {
        Some(value) => value,
        None => match details_integer("prompt_tokens_details")? {
            Some(value) => value,
            None => integer("cache_read_input_tokens")?.unwrap_or_default(),
        },
    };
    let cache_write = if let Some(value) = integer("cache_creation_input_tokens")? {
        value
    } else if let Some(details) = usage.get("cache_creation") {
        let details = details.as_object().ok_or(())?;
        let detail_integer = |field: &str| -> Result<i64, ()> {
            details
                .get(field)
                .map(|value| value.as_i64().ok_or(()))
                .transpose()
                .map(Option::unwrap_or_default)
        };
        detail_integer("ephemeral_5m_input_tokens")?
            .checked_add(detail_integer("ephemeral_1h_input_tokens")?)
            .ok_or(())?
    } else {
        0
    };
    // OpenAI prompt/input counts include cached tokens; Anthropic input_tokens
    // excludes its separately reported cache read/write counters.
    let input_includes_cache = usage.contains_key("input_tokens_details")
        || usage.contains_key("prompt_tokens_details")
        || usage.contains_key("prompt_tokens");
    let uncached_input = if input_includes_cache {
        reported_input.checked_sub(cached_input).ok_or(())?
    } else {
        reported_input
    };
    let service_tier_value = value
        .get("service_tier")
        .or_else(|| value.pointer("/response/service_tier"));
    let service_tier = match service_tier_value {
        None => None,
        Some(value) => {
            let tier = value.as_str().ok_or(())?;
            if !is_supported_service_tier(tier) {
                return Err(());
            }
            Some(tier.to_owned())
        }
    };
    let parsed = TokenUsage {
        input_tokens: uncached_input,
        cached_input_tokens: cached_input,
        cache_write_tokens: cache_write,
        output_tokens: output,
        service_tier,
    };
    if [
        parsed.input_tokens,
        parsed.cached_input_tokens,
        parsed.cache_write_tokens,
        parsed.output_tokens,
    ]
    .into_iter()
    .all(|tokens| (0..=MAX_REPORTED_TOKENS).contains(&tokens))
        && parsed
            .input_tokens
            .checked_add(parsed.cached_input_tokens)
            .and_then(|tokens| tokens.checked_add(parsed.cache_write_tokens))
            .is_some()
    {
        Ok(Some(parsed))
    } else {
        Err(())
    }
}

#[cfg(test)]
fn usage_from_value(value: &Value) -> Option<TokenUsage> {
    usage_from_value_checked(value).ok().flatten()
}

fn is_supported_service_tier(tier: &str) -> bool {
    matches!(
        tier,
        "default" | "auto" | "priority" | "flex" | "scale" | "batch" | "standard_only"
    )
}

fn append_bounded(capture: &mut Vec<u8>, chunk: &[u8], maximum: usize) {
    if chunk.len() >= maximum {
        capture.clear();
        capture.extend_from_slice(&chunk[chunk.len() - maximum..]);
        return;
    }
    let overflow = capture
        .len()
        .saturating_add(chunk.len())
        .saturating_sub(maximum);
    if overflow > 0 {
        capture.drain(..overflow);
    }
    capture.extend_from_slice(chunk);
}

fn conversation_hints(headers: &HeaderMap, body: &Value) -> crate::conversation::ConversationHints {
    let session_id = first_hint_header(
        headers,
        &[
            "x-mtc-conversation-id",
            "x-claude-code-session-id",
            "x-codex-session-id",
            "x-conversation-id",
            "x-session-id",
        ],
    )
    .or_else(|| {
        first_hint_pointer(
            body,
            &[
                "/metadata/conversation_id",
                "/metadata/session_id",
                "/metadata/thread_id",
                "/conversation_id",
                "/session_id",
                "/thread_id",
                "/prompt_cache_key",
            ],
        )
    });
    let turn_id = first_hint_header(headers, &["x-mtc-turn-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/turn_id", "/metadata/message_id"]));
    let parent_turn_id = first_hint_header(headers, &["x-mtc-parent-turn-id"]).or_else(|| {
        first_hint_pointer(
            body,
            &[
                "/metadata/parent_turn_id",
                "/metadata/previous_response_id",
                "/previous_response_id",
            ],
        )
    });
    let branch_id = first_hint_header(headers, &["x-mtc-branch-id"])
        .or_else(|| first_hint_pointer(body, &["/metadata/branch_id", "/branch_id"]));
    let compaction = headers
        .get("x-mtc-compaction")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes"))
        || body
            .pointer("/metadata/compaction")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        || body
            .get("type")
            .and_then(Value::as_str)
            .is_some_and(|value| value == "compaction");

    crate::conversation::ConversationHints {
        session_id,
        turn_id,
        parent_turn_id,
        branch_id,
        compaction,
    }
}

fn client_name(headers: &HeaderMap) -> Option<String> {
    first_hint_header(headers, &["x-mtc-client-name", header::USER_AGENT.as_str()])
}

fn first_hint_header(headers: &HeaderMap, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .and_then(safe_conversation_hint)
    })
}

fn first_hint_pointer(body: &Value, pointers: &[&str]) -> Option<String> {
    pointers.iter().find_map(|pointer| {
        body.pointer(pointer)
            .and_then(Value::as_str)
            .and_then(safe_conversation_hint)
    })
}

fn safe_conversation_hint(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        None
    } else {
        Some(value.to_owned())
    }
}
