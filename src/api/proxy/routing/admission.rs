use super::*;

pub(in crate::api::proxy) struct NextSendableProxyRouteInput<'a> {
    pub(in crate::api::proxy) request: ProxyRequestContext<'a>,
    pub(in crate::api::proxy) reservation_id: Uuid,
    pub(in crate::api::proxy) assigned_route: &'a mut (Uuid, Uuid),
    pub(in crate::api::proxy) candidates: &'a mut std::vec::IntoIter<ResolvedUpstream>,
    pub(in crate::api::proxy) failover_reason: Option<UpstreamHealthReason>,
    pub(in crate::api::proxy) candidate_rank: &'a mut usize,
    pub(in crate::api::proxy) outbound_attempts: usize,
    pub(in crate::api::proxy) deferred_shared_probes:
        &'a mut std::collections::VecDeque<DeferredSharedProbe>,
}

pub(in crate::api::proxy) struct DeferredSharedProbe {
    pub(in crate::api::proxy) route: ResolvedUpstream,
    pub(in crate::api::proxy) candidate_rank: usize,
    pub(in crate::api::proxy) probe_lease_until: i64,
}

pub(in crate::api::proxy) struct AdmittedProxyRouteInput<'a> {
    pub(in crate::api::proxy) request: ProxyRequestContext<'a>,
    pub(in crate::api::proxy) reservation_id: Uuid,
    pub(in crate::api::proxy) assigned_route: &'a mut (Uuid, Uuid),
    pub(in crate::api::proxy) failover_reason: Option<UpstreamHealthReason>,
    pub(in crate::api::proxy) planned: PlannedProxyRoute,
    pub(in crate::api::proxy) admission: UpstreamAttemptAdmission,
    pub(in crate::api::proxy) shared_probe_permit: Option<SharedProbePermit>,
    pub(in crate::api::proxy) candidate_rank: usize,
    pub(in crate::api::proxy) outbound_attempt: usize,
}

pub(in crate::api::proxy) async fn prepare_admitted_proxy_route(
    input: AdmittedProxyRouteInput<'_>,
) -> Result<(PreparedProxyRoute, UpstreamAttemptGuard, usize, usize), AppError> {
    let AdmittedProxyRouteInput {
        request,
        reservation_id,
        assigned_route,
        failover_reason,
        planned,
        admission,
        shared_probe_permit,
        candidate_rank,
        outbound_attempt,
    } = input;
    let state = request.state;
    let request_id = request.request_id;
    let mut upstream_attempt = UpstreamAttemptGuard::new(
        state,
        request_id,
        planned.route.account_id,
        planned.route.credential_generation,
        admission,
        shared_probe_permit,
    );
    let next_assignment = (planned.route.account_id, planned.route.route_id);
    if *assigned_route != next_assignment {
        if let Err(error) = state
            .db
            .reassign_pending_proxy_upstream(
                request_id,
                request.key.tenant_id,
                reservation_id,
                *assigned_route,
                next_assignment,
            )
            .await
        {
            upstream_attempt
                .complete(UpstreamAttemptTerminal::Inconclusive)
                .await;
            return Err(error);
        }
        tracing::warn!(
            %request_id,
            failed_upstream_account_id = %assigned_route.0,
            next_upstream_account_id = %next_assignment.0,
            candidate_rank,
            outbound_attempt,
            reason = ?failover_reason.unwrap_or(UpstreamHealthReason::Unavailable),
            stage = "upstream_failover",
            "proxy is switching to the next authorized upstream before downstream delivery"
        );
        state.metrics.observe_upstream_health(
            UpstreamHealthEvent::Failover,
            failover_reason.unwrap_or(UpstreamHealthReason::Unavailable),
        );
        *assigned_route = next_assignment;
    }
    let prepared = match materialize_proxy_route(state, planned).await {
        Ok(prepared) => prepared,
        Err(error) => {
            upstream_attempt
                .complete(UpstreamAttemptTerminal::Inconclusive)
                .await;
            return Err(error);
        }
    };
    Ok((prepared, upstream_attempt, candidate_rank, outbound_attempt))
}
