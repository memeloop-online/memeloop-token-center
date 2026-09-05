use super::*;

pub(super) const MAX_UPSTREAM_ATTEMPTS: usize = 3;

pub(super) struct PreparedProxyRoute {
    pub(super) route: ResolvedUpstream,
    forwarded_body: Vec<u8>,
    pub(super) output_token_ceiling: i64,
    pub(super) codex_downstream_stream: bool,
    codex_session_id: Option<String>,
    pub(super) component_request: Option<(PreparedProviderRequest, RequestContext)>,
}

impl PreparedProxyRoute {
    pub(super) fn is_codex(&self) -> bool {
        codex_transport::is_driver(&self.route.driver)
    }

    pub(super) fn is_component(&self, state: &AppState) -> bool {
        state
            .providers
            .get(&self.route.driver)
            .and_then(|provider| provider.component_adapter.as_ref())
            .is_some()
    }

    pub(super) fn request_body_ceiling(&self, original_body_length: usize) -> usize {
        original_body_length.max(self.forwarded_body.len()).max(
            self.component_request
                .as_ref()
                .map(|(request, _)| request.body.len())
                .unwrap_or_default(),
        )
    }
}

pub(super) async fn prepare_proxy_route(
    state: &AppState,
    key: &AuthenticatedKey,
    model: &str,
    protocol: Protocol,
    request_id: Uuid,
    request_json: &Value,
    route: ResolvedUpstream,
) -> Result<PreparedProxyRoute, AppError> {
    if !state.providers.contains(&route.driver) {
        return Err(AppError::Upstream(format!(
            "provider driver {} is not loaded",
            route.driver
        )));
    }
    route.credential.validate(unix_millis())?;
    let is_codex = codex_transport::is_driver(&route.driver);
    if is_codex {
        codex_transport::validate_protocol(protocol)?;
        if route.base_url != codex_transport::BASE_URL {
            return Err(AppError::BadRequest(
                "OpenAI Codex account has an invalid fixed base URL".into(),
            ));
        }
        codex_transport::validate_credential_contract(&route.credential)?;
        codex_transport::validate_route_config(&route.config)?;
    }
    let mut forwarded_json = request_json.clone();
    if let Some(value) = forwarded_json.get_mut("model") {
        *value = Value::String(route.upstream_model.clone());
    }
    let codex_plan = if is_codex {
        Some(codex_transport::prepare_request_with_id(
            &mut forwarded_json,
            &route.upstream_model,
            &route.config,
            request_id,
        )?)
    } else {
        None
    };
    let output_token_ceiling = match codex_plan.as_ref() {
        Some(plan) => plan.output_token_ceiling,
        None => inject_controlled_output_ceiling(protocol, &mut forwarded_json)?,
    };
    let component_adapter = state
        .providers
        .get(&route.driver)
        .and_then(|provider| provider.component_adapter.as_ref());
    let component_request = if component_adapter.is_some() {
        match forwarded_json.get("stream") {
            Some(Value::Bool(false)) | None => {}
            Some(Value::Bool(true)) => {
                return Err(AppError::BadRequest(
                    "component providers support buffered requests only; stream=true is unavailable"
                        .into(),
                ));
            }
            Some(_) => return Err(AppError::BadRequest("stream must be a boolean".into())),
        }
        let context = RequestContext {
            tenant_id: key.tenant_id.to_string(),
            principal_id: key.principal_id.to_string(),
            key_id: key.key_id.to_string(),
            protocol: protocol.name().to_owned(),
            model: model.to_owned(),
            config_json: serde_json::to_string(&route.config).map_err(|_| AppError::Internal)?,
        };
        let prepared = prepare_component_provider(
            state,
            &route.driver,
            context.clone(),
            route.config.clone(),
            forwarded_json.clone(),
        )
        .await?;
        Some((prepared, context))
    } else {
        None
    };
    let forwarded_body = serde_json::to_vec(&forwarded_json).map_err(|_| AppError::Internal)?;
    Ok(PreparedProxyRoute {
        route,
        forwarded_body,
        output_token_ceiling,
        codex_downstream_stream: codex_plan
            .as_ref()
            .is_some_and(|plan| plan.downstream_stream),
        codex_session_id: codex_plan.map(|plan| plan.session_id),
        component_request,
    })
}

pub(super) enum ProxySendError {
    RetryableConnection,
    CandidateUnavailable,
    NonRetryableTransport,
    Credential,
}

pub(super) async fn send_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
) -> Result<(UpstreamResponse, crate::metrics::ActivityGuard), ProxySendError> {
    if route.is_codex() {
        return send_codex_proxy_route(state, headers, route).await;
    }
    send_reqwest_proxy_route(state, headers, protocol, request_id, route).await
}
async fn send_codex_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    route: &PreparedProxyRoute,
) -> Result<(UpstreamResponse, crate::metrics::ActivityGuard), ProxySendError> {
    let outbound_base_url = codex_transport::outbound_base_url(&route.route.base_url);
    network::validate_codex_transport(
        &outbound_base_url,
        &route.route.config,
        route.route.credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let target_url = network::upstream_api_url(&outbound_base_url, codex_transport::RESPONSES_PATH);
    let mut request = state
        .codex_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    if let Some((proxy_url, _)) = route.route.credential.proxy() {
        let proxy =
            wreq::Proxy::all(proxy_url).map_err(|_| ProxySendError::CandidateUnavailable)?;
        request = request.proxy(proxy);
    }
    let session_id = route
        .codex_session_id
        .as_deref()
        .ok_or(ProxySendError::Credential)?;
    request = codex_transport::apply_wreq_wire_headers(
        request,
        headers,
        &route.route.credential,
        session_id,
    )
    .map_err(|_| ProxySendError::Credential)?;
    let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
    let upstream_started = Instant::now();
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(wreq::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => Ok((UpstreamResponse::Codex(response), upstream_activity)),
        Err(error)
            if error.is_connect()
                || error.is_proxy_connect()
                || error.is_dns()
                || error.is_tls() =>
        {
            Err(ProxySendError::RetryableConnection)
        }
        Err(_) => Err(ProxySendError::NonRetryableTransport),
    }
}
async fn send_reqwest_proxy_route(
    state: &AppState,
    headers: &HeaderMap,
    protocol: Protocol,
    request_id: Uuid,
    route: &PreparedProxyRoute,
) -> Result<(UpstreamResponse, crate::metrics::ActivityGuard), ProxySendError> {
    let outbound_base_url = route.route.base_url.clone();
    let outbound_http = network::client_for_config_url(
        &state.http,
        &outbound_base_url,
        &route.route.config,
        route.route.credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| ProxySendError::CandidateUnavailable)?;
    let target_url = network::upstream_api_url(&outbound_base_url, protocol.path());
    let mut request = outbound_http
        .post(target_url)
        .body(route.forwarded_body.clone());
    request = request
        .header(header::CONTENT_TYPE, "application/json")
        .header(
            header::ACCEPT,
            headers
                .get(header::ACCEPT)
                .cloned()
                .unwrap_or(HeaderValue::from_static("application/json")),
        );
    request = route
        .route
        .credential
        .apply(request, unix_millis())
        .map_err(|_| ProxySendError::Credential)?;
    if route.route.driver == crate::oauth::copilot::PROVIDER_DRIVER {
        let product = format!("memeloop-token-center/{}", env!("CARGO_PKG_VERSION"));
        request = request
            .header(header::USER_AGENT, &product)
            .header("X-GitHub-Api-Version", "2026-06-01")
            .header("X-Request-Id", request_id.to_string())
            .header("Editor-Version", &product)
            .header("Editor-Plugin-Version", &product);
    }
    if let Some(version) = headers.get("anthropic-version") {
        request = request.header("anthropic-version", version);
    }
    if route.route.driver == crate::oauth::claude::PROVIDER_DRIVER {
        request = request.header("anthropic-beta", crate::oauth::claude::OAUTH_BETA_HEADER);
    } else if let Some(beta) = headers.get("anthropic-beta") {
        request = request.header("anthropic-beta", beta);
    }
    let upstream_activity = state.metrics.active_upstream(&route.route.driver, "proxy");
    let upstream_started = Instant::now();
    let upstream_result = request.send().await;
    state.metrics.observe_upstream(
        &route.route.driver,
        "proxy",
        upstream_result.as_ref().ok().map(reqwest::Response::status),
        upstream_started.elapsed(),
    );
    match upstream_result {
        Ok(response) => Ok((UpstreamResponse::Reqwest(response), upstream_activity)),
        Err(error) if error.is_connect() => Err(ProxySendError::RetryableConnection),
        // Do not replay ambiguous POST delivery.
        Err(_) => Err(ProxySendError::NonRetryableTransport),
    }
}

pub(super) fn retryable_upstream_status(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
    )
}
