use super::*;

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
    let body_ceiling = i64::try_from(request_body_ceiling).unwrap_or(i64::MAX);
    let candidate_ceiling = body_ceiling
        .checked_add(trusted_input_token_overhead_ceiling(
            Some(&planned.route.driver),
            Some(&planned.route.config),
        )?)
        .filter(|ceiling| *ceiling <= MAX_REPORTED_TOKENS)
        .ok_or_else(|| {
            AppError::Upstream(
                "upstream input token reservation is outside the supported range".into(),
            )
        })?;
    Ok((candidate_ceiling, candidate_output_token_ceiling))
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
