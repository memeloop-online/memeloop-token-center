use super::*;
use crate::model::UsageReservation;

#[derive(Default)]
pub(in crate::api::proxy) struct CancellationGuard {
    owner: Option<CancelledRequest>,
}

struct CancelledRequest {
    database: Database,
    context: proxy_diagnostics::Context,
    tenant_id: Uuid,
    reservation: UsageReservation,
    permit: tokio::sync::OwnedSemaphorePermit,
}

impl CancellationGuard {
    pub(in crate::api::proxy) fn arm(
        &mut self,
        database: Database,
        context: proxy_diagnostics::Context,
        tenant_id: Uuid,
        reservation: UsageReservation,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) {
        self.owner = Some(CancelledRequest {
            database,
            context,
            tenant_id,
            reservation,
            permit,
        });
    }

    pub(in crate::api::proxy) fn disarm(&mut self) {
        self.owner = None;
    }

    pub(in crate::api::proxy) fn handoff(
        &mut self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, AppError> {
        self.owner
            .take()
            .map(|owner| owner.permit)
            .ok_or(AppError::Internal)
    }
}

impl Drop for CancellationGuard {
    fn drop(&mut self) {
        let Some(owner) = self.owner.take() else {
            return;
        };
        let request_id = owner.context.request_id;
        let duration_ms = owner.context.elapsed_millis_at(Instant::now());
        tracing::warn!(%request_id, stage = "request_owner_cancelled", "proxy request owner cancelled before response handoff");
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _permit = owner.permit;
            for delay_ms in [0, 10, 50, 200] {
                if delay_ms != 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                match owner.database.finish_cancelled_proxy_request(
                    request_id, owner.tenant_id, &owner.reservation, duration_ms,
                ).await {
                    Ok(_) => {
                        tracing::info!(%request_id, stage = "request_cancellation_settled", "proxy cancellation settlement completed");
                        return;
                    }
                    Err(AppError::Internal) => {}
                    Err(error) => {
                        tracing::error!(%request_id, stage = "request_cancellation_reconcile_failed", error_category = error.diagnostic_category(), "proxy cancellation requires orphan reconciliation");
                        return;
                    }
                }
            }
            tracing::error!(%request_id, stage = "request_cancellation_reconcile_failed", "proxy cancellation requires orphan reconciliation");
        });
    }
}
