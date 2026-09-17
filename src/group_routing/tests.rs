use super::*;

#[tokio::test]
async fn blocked_snapshot_with_maximum_candidates_cannot_extend_frozen_deadline() {
    let directory = tempfile::tempdir().unwrap();
    let mut state = AppState::initialize(crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("blocked-groups.db").display()
    )))
    .await
    .unwrap();
    let held = state.db.hold_group_snapshot_pool_for_tests().await;
    let tenant = Uuid::now_v7();
    let mut candidates = (1..=1024)
        .map(|id| AuthorizedUpstreamCandidate {
            route_id: Uuid::from_u128(id),
            account_id: Uuid::from_u128(id + 2048),
            driver: "http-json".into(),
            transport_revision: 1,
            credential_generation: 1,
        })
        .collect::<Vec<_>>();
    let before = candidates.clone();
    tokio::time::pause();
    let started = tokio::time::Instant::now();
    let deadline = started + Duration::from_millis(25);
    // The real proxy entrance with no installed hooks must not query this
    // exhausted pool or clone candidates before freezing its core budget.
    assert!(candidate_snapshot_if_enabled(&state, &candidates).is_none());
    assert_eq!(tokio::time::Instant::now(), started);
    prepare(
        &mut state,
        tenant,
        Uuid::nil(),
        Uuid::now_v7(),
        deadline,
        &mut candidates,
    )
    .await
    .unwrap();
    // Tokio's timer wheel rounds a deadline up to the next millisecond tick,
    // even with paused time; this is not additional policy/DB wait budget.
    let completed = tokio::time::Instant::now();
    assert!(completed >= deadline);
    assert!(completed <= deadline + Duration::from_millis(1));
    assert_eq!(candidates, before);
    assert!(state.group_routing.is_none());
    drop(held);
}

#[test]
fn reversed_plan_retains_exact_generation_and_health_snapshot() {
    let tenant = Uuid::from_u128(1);
    let route = Uuid::from_u128(2);
    let account = Uuid::from_u128(3);
    let first = policy(tenant, route, account);
    let mut second = first.clone();
    second.candidate.generation = 4;
    second.candidate.health = GroupRoutingHealth::Authentication;
    second.directive.generation = 4;
    second.directive.allow_transient_probe = false;
    let input = GroupRoutingInput {
        tenant_id: tenant.to_string(),
        seed: 1,
        remaining_deadline_ms: 1000,
        config: serde_json::json!({}),
        candidates: vec![first.candidate.clone(), second.candidate.clone()],
        quota_context: None,
    };
    assert_eq!(
        planned_candidate(&input, &second.directive),
        Some(&second.candidate)
    );
    assert_eq!(
        planned_candidate(&input, &first.directive),
        Some(&first.candidate)
    );
}

#[test]
fn failed_high_priority_bucket_keeps_slots_before_success_and_native_tail() {
    let member = |id| {
        (
            0,
            AuthorizedUpstreamCandidate {
                route_id: Uuid::from_u128(id),
                account_id: Uuid::from_u128(id + 100),
                driver: "http-json".into(),
                transport_revision: 1,
                credential_generation: 1,
            },
            crate::db::GroupRoutingStrategy {
                plugin_id: "test".into(),
                config: serde_json::json!({}),
            },
            1,
        )
    };
    let high = vec![member(2), member(1)];
    let low = vec![member(3)];
    let mut ranks = BTreeMap::new();
    let mut next = 0;
    assert_eq!(reserve_native_bucket_ranks(&high, &mut ranks, &mut next), 0);
    // No valid plan for high: its native member order must stay reserved.
    assert_eq!(reserve_native_bucket_ranks(&low, &mut ranks, &mut next), 2);
    let mut candidates = [member(99).1, member(3).1, member(1).1, member(2).1];
    candidates.sort_by_key(|candidate| {
        ranks
            .get(&(
                candidate.route_id,
                candidate.account_id,
                candidate.credential_generation,
            ))
            .copied()
            .unwrap_or(usize::MAX)
    });
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.route_id.as_u128())
            .collect::<Vec<_>>(),
        vec![2, 1, 3, 99]
    );
}

#[test]
fn equal_priority_buckets_order_by_uuid_before_kind() {
    let low_route = format!("{}:route", Uuid::from_u128(1));
    let high_provider = format!("{}:provider", Uuid::from_u128(100));
    let mut buckets = StrategyBuckets::new();
    buckets.insert((std::cmp::Reverse(5), high_provider), Vec::new());
    buckets.insert((std::cmp::Reverse(5), low_route.clone()), Vec::new());
    assert_eq!(buckets.first_key_value().unwrap().0.1, low_route);
}

#[test]
fn seeded_sticky_order_is_repeatable_and_nonsticky_order_is_preserved() {
    let tenant = Uuid::from_u128(1);
    let mut directives: Vec<_> = (2..18)
        .map(|id| {
            let mut directive =
                policy(tenant, Uuid::from_u128(id), Uuid::from_u128(id + 100)).directive;
            directive.stickiness = id % 2 == 0;
            directive
        })
        .collect();
    let native_tail: Vec<_> = directives
        .iter()
        .filter(|d| !d.stickiness)
        .cloned()
        .collect();
    let seed = Uuid::from_u128(123);
    let mut repeat = directives.clone();
    sort_plan_candidates(seed, &mut directives);
    sort_plan_candidates(seed, &mut repeat);
    assert_eq!(directives, repeat);
    assert!(directives[..8].iter().all(|d| d.stickiness));
    assert_eq!(directives[8..], native_tail);
    // Sticky ordering depends on identity + request seed, not guest ordering.
    repeat[..8].reverse();
    sort_plan_candidates(seed, &mut repeat);
    assert_eq!(directives, repeat);
    assert!((124..140).all(|seed| {
        let mut other = directives.clone();
        sort_plan_candidates(Uuid::from_u128(seed), &mut other);
        other[8..] == native_tail
    }));
    assert!(
        (124..140).any(|seed| {
            let mut other = directives.clone();
            sort_plan_candidates(Uuid::from_u128(seed), &mut other);
            other[..8] != directives[..8]
        }),
        "different session seeds must not all select one fixed sticky ranking"
    );
}

fn snapshot() -> RequestGroupRouting {
    let started = tokio::time::Instant::now();
    RequestGroupRouting {
        tenant_id: Uuid::now_v7(),
        seed: 7,
        started,
        deadline: started + Duration::from_secs(30),
        policies: BTreeMap::new(),
    }
}

fn policy(tenant: Uuid, route: Uuid, account: Uuid) -> CandidatePolicy {
    CandidatePolicy {
        group_id: "test-group".into(),
        strategy_version: 1,
        plugin_id: "test-policy".into(),
        config: serde_json::json!({}),
        candidate: GroupRoutingCandidate {
            tenant_id: tenant.to_string(),
            route_id: route.to_string(),
            account_id: account.to_string(),
            generation: 3,
            health: GroupRoutingHealth::Transient,
        },
        directive: GroupRoutingDirective {
            tenant_id: tenant.to_string(),
            route_id: route.to_string(),
            account_id: account.to_string(),
            generation: 3,
            allow_transient_probe: false,
            cooldown_ms: 42,
            recovery_wait_ms: 500,
            recheck_ms: 100,
            stickiness: false,
        },
        transient_policy: None,
        transient_signal_enabled: false,
        transient_signal: None,
    }
}

#[tokio::test]
async fn native_fallback_has_no_implicit_controls_and_policy_identity_is_exact() {
    let mut snapshot = snapshot();
    let route = Uuid::now_v7();
    let account = Uuid::now_v7();
    assert!(snapshot.policy(route, account, 3).is_none());
    snapshot.policies.insert(
        (route, account, 3),
        policy(snapshot.tenant_id, route, account),
    );
    let selected = snapshot.policy(route, account, 3).unwrap();
    assert!(!selected.allow_probe());
    assert_eq!(selected.cooldown_ms(), 42);
    assert!(!snapshot.uses_transient_signal(route, account, 3));
    assert!(snapshot.policy(route, account, 4).is_none());
    assert!(snapshot.policy(Uuid::now_v7(), account, 3).is_none());
    assert!(snapshot.policy(route, Uuid::now_v7(), 3).is_none());
}

#[tokio::test(start_paused = true)]
async fn strategy_wait_never_replenishes_elapsed_time_or_extends_core_deadline() {
    let snapshot = snapshot();
    let mut policy = policy(snapshot.tenant_id, Uuid::now_v7(), Uuid::now_v7());
    let expected = snapshot.started + Duration::from_millis(500);
    assert_eq!(policy.wait_deadline(&snapshot, snapshot.deadline), expected);
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(policy.wait_deadline(&snapshot, snapshot.deadline), expected);
    assert!(policy.wait_deadline(&snapshot, snapshot.deadline) < tokio::time::Instant::now());
    let core = snapshot.started + Duration::from_millis(250);
    assert_eq!(policy.wait_deadline(&snapshot, core), core);
    policy.directive.recovery_wait_ms = 0;
    assert_eq!(policy.wait_deadline(&snapshot, core), snapshot.started);
}

#[tokio::test]
async fn recheck_is_bounded_independently_of_guest_values() {
    let snapshot = snapshot();
    let mut policy = policy(snapshot.tenant_id, Uuid::now_v7(), Uuid::now_v7());
    policy.directive.recheck_ms = 0;
    assert_eq!(policy.recheck(), Duration::from_millis(25));
    policy.directive.recheck_ms = u64::MAX;
    assert_eq!(policy.recheck(), Duration::from_secs(5));
}
