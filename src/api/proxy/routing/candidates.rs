use super::*;

pub(in crate::api::proxy) fn retain_pinned_text_candidates(
    state: &AppState,
    pinned_route: Option<Uuid>,
    candidates: &mut Vec<AuthorizedUpstreamCandidate>,
) -> Result<(), AppError> {
    let Some(route_id) = pinned_route else {
        return Ok(());
    };
    candidates.retain(|candidate| {
        candidate.route_id == route_id
            && state
                .providers
                .get(&candidate.driver)
                .is_some_and(|provider| {
                    provider
                        .modalities
                        .iter()
                        .any(|modality| modality == "text")
                })
    });
    if candidates.is_empty() {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

pub(in crate::api::proxy) fn candidate_reservation_bounds(
    planned: &PlannedProxyRoute,
    original_body_length: usize,
    output_choice_count: i64,
) -> Result<(i64, i64), AppError> {
    let candidate_output_token_ceiling = planned
        .output_token_ceiling
        .checked_mul(output_choice_count)
        .filter(|ceiling| (0..=MAX_REPORTED_TOKENS).contains(ceiling))
        .ok_or_else(|| {
            AppError::BadRequest(
                "aggregate OpenAI Chat output token reservation is outside the supported range"
                    .into(),
            )
        })?;
    let request_body_ceiling = planned.request_body_ceiling(original_body_length)?;
    let candidate_ceiling = input_reservation_bound(&planned.route, request_body_ceiling)?;
    Ok((candidate_ceiling, candidate_output_token_ceiling))
}

pub(in crate::api::proxy) fn prepared_input_reservation_bound(
    prepared: &PreparedProxyRoute,
    original_body_length: usize,
) -> Result<i64, AppError> {
    let prepared_body_length = prepared
        .component_request
        .as_ref()
        .map(|(request, _)| request.body.len())
        .unwrap_or_default();
    let request_body_ceiling = original_body_length
        .max(prepared.forwarded_body.len())
        .max(prepared_body_length);
    input_reservation_bound(&prepared.route, request_body_ceiling)
}

fn input_reservation_bound(
    route: &ResolvedUpstream,
    request_body_ceiling: usize,
) -> Result<i64, AppError> {
    let body_ceiling = i64::try_from(request_body_ceiling).unwrap_or(i64::MAX);
    body_ceiling
        .checked_add(trusted_input_token_overhead_ceiling(
            Some(&route.driver),
            Some(&route.config),
        )?)
        .filter(|ceiling| *ceiling <= MAX_REPORTED_TOKENS)
        .ok_or_else(|| {
            AppError::Upstream(
                "upstream input token reservation is outside the supported range".into(),
            )
        })
}

#[derive(Default)]
pub(in crate::api::proxy) struct CandidatePreparationSummary {
    skipped_incompatible_strict_route: bool,
    skipped_local_protocol_mismatch: bool,
    skipped_kimi_protocol_mismatch: bool,
}

pub(in crate::api::proxy) async fn next_planned_proxy_candidate(
    request: ProxyRequestContext<'_>,
    candidates: &mut std::vec::IntoIter<AuthorizedUpstreamCandidate>,
    strict_choice_count_is_incompatible: bool,
    summary: &mut CandidatePreparationSummary,
) -> Result<Option<PlannedProxyRoute>, AppError> {
    for candidate in candidates.by_ref() {
        if !request.state.providers.is_public(&candidate.driver) {
            tracing::warn!(
                %request.request_id,
                route_id = %candidate.route_id,
                upstream_account_id = %candidate.account_id,
                stage = "candidate_retired_provider",
                "proxy skipped a candidate whose provider is not public"
            );
            continue;
        }
        let route = match request
            .state
            .db
            .materialize_authorized_upstream_candidate(
                &candidate,
                request.state.config.key_pepper.as_bytes(),
            )
            .await
        {
            Ok(Some(route)) => route,
            Ok(None) => continue,
            Err(error) => {
                tracing::warn!(
                    %request.request_id,
                    route_id = %candidate.route_id,
                    upstream_account_id = %candidate.account_id,
                    error = %error,
                    stage = "candidate_materialize",
                    "selected authorized proxy candidate is invalid"
                );
                return Err(AppError::Upstream(
                    "selected upstream candidate is invalid".into(),
                ));
            }
        };
        if candidate_compatibility(request.protocol, &route)
            == CandidateCompatibility::ProtocolMismatch
        {
            if route.driver == crate::oauth::managed::kimi::PROVIDER_DRIVER {
                summary.skipped_kimi_protocol_mismatch = true;
            } else {
                summary.skipped_local_protocol_mismatch = true;
            }
            continue;
        }
        if strict_choice_count_is_incompatible
            && requires_strict_openai_chat_usage(
                request.protocol,
                &route.driver,
                &route.config,
                request.request_json,
            )
        {
            summary.skipped_incompatible_strict_route = true;
            continue;
        }
        let route_id = route.route_id;
        let account_id = route.account_id;
        return plan_proxy_route(ProxyRoutePlanInput {
            request,
            route,
            preparation_now: unix_millis(),
        })
        .map(Some)
        .inspect_err(|error| {
            tracing::warn!(
                %request.request_id,
                %route_id,
                upstream_account_id = %account_id,
                error_category = error.diagnostic_category(),
                stage = "candidate_prepare",
                "selected authorized proxy candidate is unusable"
            );
        });
    }
    Ok(None)
}

pub(in crate::api::proxy) fn exhausted_candidate_error(
    protocol: Protocol,
    request_json: &Value,
    summary: &CandidatePreparationSummary,
) -> Result<AppError, AppError> {
    if summary.skipped_local_protocol_mismatch {
        codex_transport::validate_protocol(protocol)?;
    }
    if summary.skipped_kimi_protocol_mismatch {
        return Ok(AppError::BadRequest(
            "native Kimi OAuth does not support this request protocol".into(),
        ));
    }
    if summary.skipped_incompatible_strict_route {
        validate_openai_chat_choice_count(request_json)?;
    }
    Ok(AppError::Overloaded)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_component_body_expands_the_input_reservation_bound() {
        const ORIGINAL_BODY_LENGTH: usize = 64;
        const REPORTED_INPUT_TOKENS: i64 = 4_096;
        const PREPARED_BODY_LENGTH: usize = 8_192;

        let prepared = PreparedProxyRoute {
            route: ResolvedUpstream {
                route_id: Uuid::nil(),
                account_id: Uuid::nil(),
                transport_revision: 1,
                credential_generation: 1,
                driver: "component-test".into(),
                base_url: "https://example.com".into(),
                config: json!({}),
                upstream_model: "component-model".into(),
                credential: UpstreamCredential::None,
            },
            forwarded_body: b"{}".to_vec(),
            upstream_stream: false,
            codex_downstream_stream: false,
            codex_store_disabled: false,
            codex_session_id: None,
            component_request: Some((
                PreparedProviderRequest {
                    method: reqwest::Method::POST,
                    path: "/infer".into(),
                    headers: Default::default(),
                    body: vec![b'x'; PREPARED_BODY_LENGTH],
                },
                RequestContext {
                    tenant_id: "tenant".into(),
                    principal_id: "principal".into(),
                    key_id: "key".into(),
                    protocol: "openai".into(),
                    model: "public-model".into(),
                    config_json: "{}".into(),
                },
            )),
            kimi_response: None,
        };

        let initial_bound = input_reservation_bound(&prepared.route, ORIGINAL_BODY_LENGTH).unwrap();
        let prepared_bound =
            prepared_input_reservation_bound(&prepared, ORIGINAL_BODY_LENGTH).unwrap();

        assert!(REPORTED_INPUT_TOKENS > initial_bound);
        assert!(REPORTED_INPUT_TOKENS <= prepared_bound);
        assert_eq!(prepared_bound, PREPARED_BODY_LENGTH as i64);
    }
}
