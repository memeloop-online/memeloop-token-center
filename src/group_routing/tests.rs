use super::*;

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
