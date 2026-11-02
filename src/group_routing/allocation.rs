//! Shared request allocations and stable session preferences.
use super::*;
use serde_json::{Value, json};

pub(super) struct Reservation {
    tenant: Uuid,
    revision: i64,
    request: Uuid,
    session: Option<Uuid>,
    now: i64,
    pub context: Option<Value>,
    pub preferred: Option<String>,
}

impl Reservation {
    pub async fn read(
        state: &AppState,
        tenant: Uuid,
        request: Uuid,
        session: Uuid,
        input: &GroupRoutingInput,
    ) -> Result<Self, AppError> {
        let now = crate::db::unix_millis();
        let revision = state.db.allocation_revision(tenant).await?;
        let session = (session != request).then_some(session);
        let preferred = if let Some(session) = session {
            state
                .db
                .allocation_session(tenant, session, now)
                .await?
                .filter(|(id, generation)| {
                    input.candidates.iter().any(|candidate| {
                        candidate.account_id == *id
                            && (*generation == u64::MAX || candidate.generation == *generation)
                            && candidate.health == GroupRoutingHealth::Healthy
                    })
                })
                .map(|(id, _)| id)
        } else {
            None
        };
        if preferred.is_some() {
            return Ok(Self {
                tenant,
                revision,
                request,
                session,
                now,
                context: None,
                preferred,
            });
        }
        let identities: BTreeMap<_, _> = input
            .candidates
            .iter()
            .map(|row| (row.account_id.clone(), row.generation))
            .collect();
        let ids: Vec<_> = identities.keys().cloned().collect();
        let usage = state.db.allocation_usage(tenant, &ids, now).await?;
        let mut accounts = Vec::new();
        for (id, generation) in identities {
            let rows: Vec<_> = usage.iter().filter(|row| row.account == id).collect();
            let requests = rows
                .iter()
                .fold(0_u64, |sum, row| sum.saturating_add(row.requests));
            let tokens = rows
                .iter()
                .fold(0_u64, |sum, row| sum.saturating_add(row.tokens));
            let mut baselines = serde_json::Map::new();
            if let Some(quota) = input.quota_context.as_ref().and_then(|context| {
                context
                    .accounts
                    .iter()
                    .find(|row| row.account_id == id && row.generation == generation)
            }) {
                for window in &quota.windows {
                    let start = window
                        .reset_at
                        .zip(window.period_seconds)
                        .map(|(reset, seconds)| reset.saturating_sub(seconds.saturating_mul(1000)));
                    // The boundary hour is shared with the new cycle; older complete hours are excluded.
                    let baseline = start
                        .map(|start| {
                            rows.iter()
                                .filter(|row| {
                                    row.hour < start.div_euclid(crate::db::allocation::HOUR_MS)
                                })
                                .fold(0_u64, |sum, row| sum.saturating_add(row.requests))
                        })
                        .unwrap_or(0);
                    baselines.insert(
                        window.id.clone(),
                        json!({"reset_at":window.reset_at,"baseline":baseline}),
                    );
                }
            }
            accounts.push(json!({"account_id":id,"generation":generation,"recent_requests":requests,"recent_tokens":tokens,"window_baselines":baselines}));
        }
        let period_start =
            (now / crate::db::allocation::HOUR_MS - 23) * crate::db::allocation::HOUR_MS;
        let context = json!({"version":"account-history-v1","period_start":period_start,"now_ms":now,"session_account":preferred,"accounts":accounts});
        Ok(Self {
            tenant,
            revision,
            request,
            session,
            now,
            context: Some(context),
            preferred,
        })
    }

    pub async fn commit(
        self,
        state: &AppState,
        selected: &GroupRoutingCandidate,
    ) -> Result<bool, AppError> {
        state
            .db
            .allocation_reserve(
                self.tenant,
                self.preferred.is_none().then_some(self.revision),
                self.request,
                self.session,
                (&selected.account_id, selected.generation),
                self.now,
            )
            .await
    }
}

pub(super) fn session_plan(
    input: &GroupRoutingInput,
    preferred: &str,
) -> crate::plugin::routing::GroupRoutingExecutionPlan {
    use crate::plugin::routing::{GroupRoutingExecutionDirective, GroupRoutingExecutionPlan};
    let mut candidates: Vec<_> = input
        .candidates
        .iter()
        .map(|candidate| GroupRoutingExecutionDirective {
            directive: GroupRoutingDirective {
                tenant_id: candidate.tenant_id.clone(),
                route_id: candidate.route_id.clone(),
                account_id: candidate.account_id.clone(),
                generation: candidate.generation,
                allow_transient_probe: false,
                cooldown_ms: 0,
                recovery_wait_ms: 0,
                recheck_ms: 0,
                stickiness: false,
            },
            transient_policy: None,
        })
        .collect();
    candidates.sort_by_key(|candidate| candidate.directive.account_id != preferred);
    GroupRoutingExecutionPlan { candidates }
}
