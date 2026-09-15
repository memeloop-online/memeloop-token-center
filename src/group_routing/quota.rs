//! Projection of an immutable, credential-free observation into one authorized
//! group. Missing evidence does not grant authority or change native health.
use super::*;
use crate::{
    plugin::routing::{
        GROUP_ROUTING_QUOTA_VERSION, GroupRoutingQuotaAccount, GroupRoutingQuotaContext,
        GroupRoutingQuotaWindow,
    },
    upstream_quota::observations::RoutingQuotaObservation,
};

pub(super) fn context_for_bucket(
    members: &[BucketMember],
    observations: &BTreeMap<(Uuid, i64), RoutingQuotaObservation>,
    now_ms: i64,
) -> Option<GroupRoutingQuotaContext> {
    let mut accounts = BTreeMap::new();
    for (_, candidate, _, _) in members {
        let identity = (candidate.account_id, candidate.credential_generation);
        let observation = observations.get(&identity)?;
        // The reader already binds tenant/config/generation. Recheck against
        // this request's authorized snapshot before disclosing any fields.
        if observation.account_id != candidate.account_id
            || observation.generation != candidate.credential_generation
            || observation.config_revision != candidate.transport_revision
            || observation.provider != candidate.driver
            || observation.observed_at < 0
            || observation.observed_at > now_ms
            || observation.valid_until <= now_ms
            || observation.windows.is_empty()
            || observation
                .windows
                .iter()
                .any(|window| window.reset_at.is_some_and(|reset| reset <= now_ms))
        {
            return None;
        }
        accounts
            .entry(identity)
            .or_insert_with(|| GroupRoutingQuotaAccount {
                account_id: candidate.account_id.to_string(),
                generation: candidate.credential_generation as u64,
                provider: observation.provider.clone(),
                observed_at: observation.observed_at,
                valid_until: observation.valid_until,
                windows: observation
                    .windows
                    .iter()
                    .map(|window| GroupRoutingQuotaWindow {
                        id: window.id.clone(),
                        period_seconds: window.period_seconds,
                        reset_at: window.reset_at,
                        reset_is_estimated: window.reset_is_estimated,
                        remaining_fraction: window.remaining_fraction,
                        exhausted: window.exhausted,
                    })
                    .collect(),
            });
    }
    Some(GroupRoutingQuotaContext {
        version: GROUP_ROUTING_QUOTA_VERSION.into(),
        now_ms,
        accounts: accounts.into_values().collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream_quota::observations::RoutingQuotaWindow;

    #[test]
    fn bucket_projection_deduplicates_routes_and_never_reuses_stale_or_outside_evidence() {
        let account = Uuid::from_u128(1);
        let candidate = |route| AuthorizedUpstreamCandidate {
            route_id: Uuid::from_u128(route),
            account_id: account,
            driver: "kimi-oauth".into(),
            transport_revision: 9,
            credential_generation: 7,
        };
        let member = |route| {
            (
                0,
                candidate(route),
                crate::db::GroupRoutingStrategy {
                    plugin_id: "quota-order".into(),
                    config: serde_json::json!({}),
                },
                1,
            )
        };
        let members = vec![member(2), member(3)];
        let observation = RoutingQuotaObservation {
            account_id: account,
            generation: 7,
            config_revision: 9,
            provider: "kimi-oauth".into(),
            observed_at: 900,
            valid_until: 1100,
            windows: vec![RoutingQuotaWindow {
                id: "summary".into(),
                period_seconds: Some(604800),
                reset_at: Some(2000),
                reset_is_estimated: false,
                remaining_fraction: Some(0.5),
                exhausted: Some(false),
            }],
        };
        let mut observations = BTreeMap::from([((account, 7), observation.clone())]);
        let outside = Uuid::from_u128(99);
        observations.insert(
            (outside, 7),
            RoutingQuotaObservation {
                account_id: outside,
                ..observation.clone()
            },
        );
        let context = context_for_bucket(&members, &observations, 1000).unwrap();
        assert_eq!(context.accounts.len(), 1);
        assert_eq!(context.accounts[0].account_id, account.to_string());
        assert_eq!(context.now_ms, 1000);
        let mut new_scope = members.clone();
        new_scope[1].1.account_id = Uuid::from_u128(100);
        assert!(context_for_bucket(&new_scope, &observations, 1000).is_none());
        for variant in 0..7 {
            let mut changed = observation.clone();
            match variant {
                0 => changed.generation += 1,
                1 => changed.config_revision += 1,
                2 => changed.provider = "another-driver".into(),
                3 => changed.valid_until = 1000,
                4 => changed.observed_at = 1001,
                5 => changed.windows[0].reset_at = Some(1000),
                _ => changed.account_id = outside,
            }
            observations.insert((account, 7), changed);
            assert!(
                context_for_bucket(&members, &observations, 1000).is_none(),
                "variant {variant}"
            );
        }
        // One request holds its cloned evidence even when the next observation changes.
        assert_eq!(context.accounts[0].windows[0].remaining_fraction, Some(0.5));
    }
}
