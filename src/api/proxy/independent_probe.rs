use super::*;

#[derive(Clone, Default)]
pub(in crate::api) struct ProbeObservation {
    pub(in crate::api) model: Option<String>,
    pub(in crate::api) status: Option<u16>,
    pub(in crate::api) quota: Option<crate::db::SupplierQuotaEvidence>,
}

/// Internal-only pin, not a customer header. Also captures what actually
/// reached transport, so an MTC rejection cannot masquerade as provider429.
#[derive(Clone)]
pub(in crate::api) struct IndependentProbeTarget {
    account: Uuid,
    model: String,
    generation: i64,
    revision: i64,
    observed: std::sync::Arc<std::sync::Mutex<ProbeObservation>>,
}

tokio::task_local! {
    pub(super) static TARGET: IndependentProbeTarget;
}

impl IndependentProbeTarget {
    pub(in crate::api) fn new(
        account: Uuid,
        model: String,
        generation: i64,
        revision: i64,
    ) -> Self {
        Self {
            account,
            model,
            generation,
            revision,
            observed: Default::default(),
        }
    }

    pub(in crate::api) fn observation(&self) -> ProbeObservation {
        self.observed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    pub(super) fn validate_target(route: &ResolvedUpstream) -> Result<(), AppError> {
        TARGET
            .try_with(|target| {
                if route.account_id != target.account
                    || route.upstream_model != target.model
                    || route.credential_generation != target.generation
                    || route.transport_revision != target.revision
                {
                    return Err(AppError::Forbidden);
                }
                Ok(())
            })
            .unwrap_or(Ok(()))
    }

    pub(super) fn dispatched(route: &ResolvedUpstream) {
        let _ = TARGET.try_with(|target| {
            target
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .model = Some(route.upstream_model.clone());
        });
    }

    pub(super) fn response(status: StatusCode) {
        let _ = TARGET.try_with(|target| {
            target
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .status = Some(status.as_u16());
        });
    }

    pub(super) fn quota(evidence: crate::db::SupplierQuotaEvidence) {
        let _ = TARGET.try_with(|target| {
            target
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .quota = Some(evidence);
        });
    }
}
