use super::*;
use crate::{db::Database, model::UsageReservation};

pub(in crate::api::proxy) struct CancellationGuard {
    owner: Option<CancelledRequest>,
    lost: tokio::sync::watch::Sender<bool>,
}

impl Default for CancellationGuard {
    fn default() -> Self {
        Self {
            owner: None,
            lost: tokio::sync::watch::channel(false).0,
        }
    }
}

struct CancelledRequest {
    database: Database,
    pending: std::sync::Arc<crate::proxy_lifecycle::pending_terminal::Journal>,
    context: proxy_diagnostics::Context,
    tenant_id: Uuid,
    reservation: UsageReservation,
    permit: crate::proxy_lifecycle::ProxyLifecycleActivity,
    identity: crate::db::ProxyRequestOwner,
    stage: &'static str,
}

impl CancellationGuard {
    pub(in crate::api::proxy) async fn acquire_owner(&mut self) -> Result<(), AppError> {
        let owner = self.owner.as_mut().ok_or(AppError::internal("cancellation_acquire_owner_1"))?;
        owner
            .permit
            .acquire_owner(
                owner.database.clone(),
                owner.identity.clone(),
                self.lost.clone(),
            )
            .await
    }

    pub(in crate::api::proxy) fn arm(
        &mut self,
        database: Database,
        pending: std::sync::Arc<crate::proxy_lifecycle::pending_terminal::Journal>,
        context: proxy_diagnostics::Context,
        tenant_id: Uuid,
        reservation: UsageReservation,
        permit: crate::proxy_lifecycle::ProxyLifecycleActivity,
    ) {
        let identity = crate::db::ProxyRequestOwner {
            request_id: context.request_id,
            tenant_id,
            key_id: reservation.key_id,
            reservation_id: reservation.id,
            token: Uuid::now_v7(),
        };
        self.owner = Some(CancelledRequest {
            database,
            pending,
            context,
            tenant_id,
            reservation,
            permit,
            identity,
            stage: "upstream_response_headers",
        });
    }

    pub(in crate::api::proxy) async fn prepare_dispatch(
        &mut self,
        request: &BufferedRequest<'_>,
    ) -> Result<(), AppError> {
        let owner = self.owner.as_mut().ok_or(AppError::internal("cancellation_prepare_dispatch_1"))?;
        let claim = owner
            .database
            .claim_pending_proxy_settlement(
                &owner.identity,
                request.input_token_ceiling,
                request.output_token_ceiling,
                request.requested_service_tier.as_deref(),
            )
            .await?;
        owner.reservation = request.reservation.clone();
        owner.stage = "upstream_response_headers";
        owner.pending.register_claim(claim);
        let request_id = owner.context.request_id;
        if let Err(error) = owner
            .database
            .record_proxy_in_flight_stage(
                request_id,
                owner.tenant_id,
                owner.reservation.key_id,
                owner.reservation.id,
                owner.stage,
            )
            .await
        {
            tracing::warn!(
                %request_id,
                stage = owner.stage,
                error_category = error.diagnostic_category(),
                "in-flight request stage was not stored"
            );
        }
        Ok(())
    }

    pub(in crate::api::proxy) fn response_headers_received(&mut self) {
        if let Some(owner) = self.owner.as_mut() {
            owner.stage = "upstream_response_body";
        }
    }

    pub(in crate::api::proxy) fn record_dispatch_failure(&self, response: &Response) {
        let Some(owner) = self.owner.as_ref() else {
            return;
        };
        let Some(error_code) = response.extensions().get::<crate::error::PublicErrorCode>() else {
            return;
        };
        owner.pending.submit_unobserved(
            owner.context.request_id,
            &owner.reservation,
            owner.context.elapsed_millis_at(Instant::now()),
            (i64::from(response.status().as_u16()), error_code.0),
            None,
            "upstream_response_headers",
        );
    }

    pub(in crate::api::proxy) fn disarm(&mut self) {
        if let Some(owner) = self.owner.take() {
            if owner.pending.has_claim(owner.context.request_id) {
                owner.pending.mark_unknown(owner.context.request_id);
            }
            let _ = owner
                .pending
                .attach_activity(owner.context.request_id, owner.permit);
        }
    }

    pub(in crate::api::proxy) fn handoff(
        &mut self,
    ) -> Result<crate::proxy_lifecycle::ProxyLifecycleActivity, AppError> {
        self.owner
            .take()
            .map(|owner| owner.permit)
            .ok_or(AppError::internal("cancellation_handoff_1"))
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        let request_id = owner.context.request_id;
        let permit = match owner.pending.attach_activity(request_id, owner.permit) {
            Ok(()) => return,
            Err(permit) => permit,
        };
        if owner.pending.has_claim(request_id) {
            owner.pending.submit_cancelled(
                request_id,
                &owner.reservation,
                owner.context.elapsed_millis_at(Instant::now()),
                owner.stage,
            );
            let _ = owner.pending.attach_activity(request_id, permit);
            let pending = owner.pending.clone();
            let database = owner.database.clone();
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn(async move {
                    if let Err(error) = pending
                        .record_operational_observation(&database, request_id)
                        .await
                    {
                        tracing::error!(
                            %request_id,
                            error_category = error.diagnostic_category(),
                            "cancelled request terminal was not durable"
                        );
                    }
                });
            }
            return;
        }
        let duration_ms = owner.context.elapsed_millis_at(Instant::now());
        tracing::warn!(%request_id, stage = "request_owner_cancelled", "proxy request owner cancelled before response handoff");
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let settlement = async move {
            let _permit = permit;
            for delay_ms in [0, 10, 50, 200] {
                if delay_ms != 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                match owner
                    .database
                    .finish_cancelled_proxy_request(
                        request_id,
                        owner.tenant_id,
                        &owner.reservation,
                        duration_ms,
                    )
                    .await
                {
                    Ok(_) => {
                        tracing::info!(%request_id, stage = "request_cancellation_settled", "proxy cancellation settlement completed");
                        return;
                    }
                    Err(AppError::Internal { code, .. }) if code == "cancellation_drop_1" => {}
                    Err(error) => {
                        tracing::error!(%request_id, stage = "request_cancellation_reconcile_failed", error_category = error.diagnostic_category(), "proxy cancellation requires orphan reconciliation");
                        return;
                    }
                }
            }
            tracing::error!(%request_id, stage = "request_cancellation_reconcile_failed", "proxy cancellation requires orphan reconciliation");
        };
        runtime.spawn(async move {
            if tokio::time::timeout(MAX_PROXY_FINALIZATION_LIFETIME, settlement)
                .await
                .is_err()
            {
                tracing::error!(%request_id, stage = "request_cancellation_reconcile_timeout", "proxy cancellation requires orphan reconciliation");
            }
        });
    }
}
