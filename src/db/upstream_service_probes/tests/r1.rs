use super::*;

async fn fixture(db: &Database) -> ResolvedUpstream {
    let id = account(db, &format!("r1-evidence-{}", Uuid::now_v7())).await;
    let context = db.probe_account_context(id).await.unwrap();
    ResolvedUpstream {
        route_id: Uuid::now_v7(),
        account_id: id,
        upstream_model: "exact".into(),
        credential_generation: context.credential_generation,
        transport_revision: context.transport_revision,
        driver: "http-json".into(),
        base_url: "http://127.0.0.1:1".into(),
        config: serde_json::json!({}),
        credential: UpstreamCredential::None,
    }
}

fn failure(exhausted: bool, reset_at: Option<i64>) -> SupplierQuotaEvidence {
    SupplierQuotaEvidence {
        exhausted,
        reset_at,
        reset_source: if reset_at.is_some() {
            "retry_after"
        } else {
            "unknown"
        },
    }
}

fn success(route: &ResolvedUpstream, at: i64) -> Value {
    serde_json::json!({
        "status": "healthy", "upstream_status": 200, "actual_model": route.upstream_model,
        "credential_generation": route.credential_generation,
        "transport_revision": route.transport_revision,
        "started_at": at, "checked_at": at + 1, "request_id": Uuid::now_v7(),
        "source": "model_probe"
    })
}

async fn kind(db: &Database, route: &ResolvedUpstream) -> String {
    sqlx::query_scalar("SELECT failure_kind FROM upstream_supplier_availability WHERE upstream_account_id=$1 AND upstream_model=$2")
        .bind(route.account_id.to_string()).bind(&route.upstream_model)
        .fetch_one(&db.pool).await.unwrap()
}

async fn evidence_merge_orders(db: &Database) {
    let now = super::super::super::unix_millis();
    for enrichment_first in [false, true] {
        let route = fixture(db).await;
        let request = Uuid::now_v7();
        db.store_supplier_quota(&route, request, now, failure(false, None))
            .await
            .unwrap();
        if enrichment_first {
            db.store_supplier_quota(&route, request, now, failure(true, None))
                .await
                .unwrap();
        }
        db.store_supplier_quota(&route, Uuid::now_v7(), now + 1, failure(false, None))
            .await
            .unwrap();
        if !enrichment_first {
            db.store_supplier_quota(&route, request, now, failure(true, None))
                .await
                .unwrap();
        }
        assert_eq!(kind(db, &route).await, "quota_exhausted");
    }
    for success_first in [false, true] {
        let route = fixture(db).await;
        if success_first {
            db.recover_supplier_quota_from_probe(route.account_id, &success(&route, now))
                .await
                .unwrap();
        }
        db.store_supplier_quota(&route, Uuid::now_v7(), now, failure(true, None))
            .await
            .unwrap();
        if !success_first {
            db.recover_supplier_quota_from_probe(route.account_id, &success(&route, now))
                .await
                .unwrap();
        }
        assert_eq!(
            kind(db, &route).await,
            "quota_exhausted",
            "equal-time failure wins"
        );
        db.recover_supplier_quota_from_probe(route.account_id, &success(&route, now + 2))
            .await
            .unwrap();
        db.store_supplier_quota(&route, Uuid::now_v7(), now + 1, failure(true, None))
            .await
            .unwrap();
        assert_eq!(
            kind(db, &route).await,
            "available",
            "older failure stays fenced"
        );
        // A later weak failure must not erase the independent success fence.
        db.store_supplier_quota(&route, Uuid::now_v7(), now + 3, failure(false, None))
            .await
            .unwrap();
        db.store_supplier_quota(&route, Uuid::now_v7(), now, failure(true, None))
            .await
            .unwrap();
        assert_eq!(kind(db, &route).await, "rate_limited");
    }
    // Success arrives after a newer weak failure: retain both facts.
    let route = fixture(db).await;
    db.store_supplier_quota(&route, Uuid::now_v7(), now + 3, failure(false, None))
        .await
        .unwrap();
    db.recover_supplier_quota_from_probe(route.account_id, &success(&route, now + 2))
        .await
        .unwrap();
    db.store_supplier_quota(&route, Uuid::now_v7(), now, failure(true, None))
        .await
        .unwrap();
    assert_eq!(kind(db, &route).await, "rate_limited");
}

async fn deadlines_and_reset_verification(db: &Database) {
    let now = super::super::super::unix_millis();
    let deadline = now + 3_600_000;
    let key = billing_key(db).await;
    for exhausted in [false, true] {
        for later in [None, Some(now + 60_000)] {
            for known_first in [false, true] {
                let route = fixture(db).await;
                let public_model = format!("r1-{}", route.account_id);
                let route_id = super::route(db, route.account_id, &public_model, "exact").await;
                grant(db, key.key_id, route_id, None).await;
                let observations = if known_first {
                    [
                        (now, failure(exhausted, Some(deadline))),
                        (now + 1, failure(exhausted, later)),
                    ]
                } else {
                    [
                        (now + 1, failure(exhausted, later)),
                        (now, failure(exhausted, Some(deadline))),
                    ]
                };
                for (at, evidence) in observations {
                    db.store_supplier_quota(&route, Uuid::now_v7(), at, evidence)
                        .await
                        .unwrap();
                }
                assert_eq!(
                    db.supplier_quota_probe_deadline(route.account_id, "exact")
                        .await
                        .unwrap(),
                    Some(deadline)
                );
                assert!(
                    db.supplier_quota_recovery_models(route.account_id)
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert!(
                    db.list_authorized_upstream_candidates_with_hint(
                        key.key_id,
                        key.tenant_id,
                        &public_model,
                        "openai",
                        selection(Some(route.account_id)),
                    )
                    .await
                    .unwrap()
                    .is_empty(),
                    "retained provider timing excludes customer dispatch"
                );
            }
        }
    }
    let route = fixture(db).await;
    sqlx::query("UPDATE upstream_accounts SET driver='openai-codex' WHERE id=$1")
        .bind(route.account_id.to_string())
        .execute(&db.pool)
        .await
        .unwrap();
    let mut unrelated = route.clone();
    unrelated.upstream_model = "unrelated".into();
    for target in [&route, &unrelated] {
        db.store_supplier_quota(target, Uuid::now_v7(), now, failure(true, Some(deadline)))
            .await
            .unwrap();
    }
    db.schedule_supplier_quota_verification(
        route.account_id,
        route.credential_generation,
        route.transport_revision,
        now + 1,
    )
    .await
    .unwrap();
    assert_eq!(
        db.supplier_quota_probe_deadline(route.account_id, "exact")
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        kind(db, &route).await,
        "quota_exhausted",
        "readback only schedules verification"
    );
    db.recover_supplier_quota_from_probe(route.account_id, &success(&route, now + 2))
        .await
        .unwrap();
    assert_eq!(kind(db, &route).await, "available");
    assert_eq!(
        kind(db, &unrelated).await,
        "quota_exhausted",
        "exact success cannot heal unrelated scope"
    );
    db.store_supplier_quota(
        &unrelated,
        Uuid::now_v7(),
        now + 3,
        failure(true, Some(deadline)),
    )
    .await
    .unwrap();
    db.schedule_supplier_quota_verification(
        route.account_id,
        route.credential_generation,
        route.transport_revision,
        now + 3,
    )
    .await
    .unwrap();
    assert_eq!(
        db.supplier_quota_probe_deadline(route.account_id, "unrelated")
            .await
            .unwrap(),
        Some(deadline),
        "newer/concurrent failure retains deadline"
    );
    db.schedule_supplier_quota_verification(
        route.account_id,
        route.credential_generation - 1,
        route.transport_revision,
        now + 4,
    )
    .await
    .unwrap();
    assert_eq!(
        db.supplier_quota_probe_deadline(route.account_id, "unrelated")
            .await
            .unwrap(),
        Some(deadline)
    );
}

async fn legacy_deadline_and_provenance(db: &Database) {
    for disabled in [false, true] {
        let route = fixture(db).await;
        let now = super::super::super::unix_millis();
        sqlx::query("INSERT INTO upstream_account_health (upstream_account_id, consecutive_failures, cooldown_until, probe_lease_until, probe_lease_token, credential_generation, transport_revision, last_failure_kind, updated_at) VALUES ($1,1,$2,0,'',$3,$4,'quota_exhausted',$5)")
            .bind(route.account_id.to_string()).bind(now + 60_000).bind(route.credential_generation).bind(route.transport_revision).bind(now)
            .execute(&db.pool).await.unwrap();
        assert_eq!(
            db.supplier_quota_probe_deadline(route.account_id, "any-model")
                .await
                .unwrap(),
            Some(now + 60_000)
        );
        // Migration149 leaves old columns intact and new provenance NULL.
        sqlx::query("INSERT INTO upstream_service_probes (upstream_account_id,status,error_code,latency_ms,checked_at) VALUES ($1,'healthy',NULL,17,1)")
            .bind(route.account_id.to_string()).execute(&db.pool).await.unwrap();
        if disabled {
            sqlx::query("UPDATE upstream_accounts SET status='disabled' WHERE id=$1")
                .bind(route.account_id.to_string())
                .execute(&db.pool)
                .await
                .unwrap();
        }
        let saved = db
            .upstream_service_probe(route.account_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(saved["source"], "legacy_unverified");
        assert_eq!(saved["status"], "unknown");
        assert_eq!(saved["historical_result"]["status"], "healthy");
        assert_eq!(saved["historical_result"]["latency_ms"], 17);
        assert!(saved.get("actual_model").is_none());
        if disabled {
            assert!(db.probe_account_context(route.account_id).await.is_err());
            assert!(
                !db.upstream_service_probes_due(i64::MAX, i64::MAX, 1000)
                    .await
                    .unwrap()
                    .contains(&route.account_id)
            );
        }
    }
}

async fn contracts(db: &Database) {
    evidence_merge_orders(db).await;
    retained_evidence_success_orders(db).await;
    deadlines_and_reset_verification(db).await;
    legacy_deadline_and_provenance(db).await;
}

/// R2-F2: H(T), W(T+3), S(T+2) must have the same routing meaning in
/// either persistence order, without throwing away W or the success fence.
async fn retained_evidence_success_orders(db: &Database) {
    let now = super::super::super::unix_millis();
    let key = billing_key(db).await;
    for hard in [false, true] {
        for reset in [None, Some(now + 3_600_000)] {
            for success_first in [false, true] {
                let target = fixture(db).await;
                let public = format!("r2-{}", target.account_id);
                let route_id = route(db, target.account_id, &public, "exact").await;
                grant(db, key.key_id, route_id, None).await;
                let weak_request = Uuid::now_v7();
                if success_first {
                    db.recover_supplier_quota_from_probe(
                        target.account_id,
                        &success(&target, now + 2),
                    )
                    .await
                    .unwrap();
                } else {
                    db.store_supplier_quota(&target, Uuid::now_v7(), now, failure(hard, reset))
                        .await
                        .unwrap();
                }
                db.store_supplier_quota(&target, weak_request, now + 3, failure(false, None))
                    .await
                    .unwrap();
                if success_first {
                    db.store_supplier_quota(&target, Uuid::now_v7(), now, failure(hard, reset))
                        .await
                        .unwrap();
                } else {
                    db.recover_supplier_quota_from_probe(
                        target.account_id,
                        &success(&target, now + 2),
                    )
                    .await
                    .unwrap();
                }
                let row = sqlx::query(
                    "SELECT * FROM upstream_supplier_availability WHERE upstream_account_id=$1",
                )
                .bind(target.account_id.to_string())
                .fetch_one(&db.pool)
                .await
                .unwrap();
                assert_eq!(row.get::<String, _>("failure_kind"), "rate_limited");
                assert_eq!(row.get::<i64, _>("observed_at"), now + 3);
                assert_eq!(row.get::<String, _>("request_id"), weak_request.to_string());
                assert_eq!(row.get::<Option<i64>, _>("success_at"), Some(now + 2));
                assert_eq!(row.get::<Option<i64>, _>("hard_observed_at"), None);
                assert_eq!(row.get::<Option<i64>, _>("reset_observed_at"), None);
                assert_eq!(row.get::<Option<i64>, _>("reset_at"), None);
                assert_eq!(row.get::<String, _>("reset_source"), "unknown");
                assert_eq!(
                    db.supplier_quota_probe_deadline(target.account_id, "exact")
                        .await
                        .unwrap(),
                    None
                );
                assert!(
                    db.supplier_quota_recovery_models(target.account_id)
                        .await
                        .unwrap()
                        .is_empty()
                );
                assert_eq!(
                    db.list_authorized_upstream_candidates_with_hint(
                        key.key_id,
                        key.tenant_id,
                        &public,
                        "openai",
                        selection(Some(target.account_id)),
                    )
                    .await
                    .unwrap()
                    .len(),
                    1
                );
                // Older success and subsequently arriving pre-success hard facts
                // cannot roll the success fence or routing state back.
                db.recover_supplier_quota_from_probe(target.account_id, &success(&target, now + 1))
                    .await
                    .unwrap();
                db.store_supplier_quota(&target, Uuid::now_v7(), now + 1, failure(true, reset))
                    .await
                    .unwrap();
                assert_eq!(kind(db, &target).await, "rate_limited");
            }
        }
    }
    // Hard and reset facts age independently; equal-time failure still wins.
    for evidence_at in [now + 2, now + 3] {
        for new_hard in [false, true] {
            let target = fixture(db).await;
            db.store_supplier_quota(
                &target,
                Uuid::now_v7(),
                now,
                failure(true, Some(now + 3_600_000)),
            )
            .await
            .unwrap();
            db.store_supplier_quota(
                &target,
                Uuid::now_v7(),
                evidence_at,
                failure(new_hard, (!new_hard).then_some(now + 7_200_000)),
            )
            .await
            .unwrap();
            db.recover_supplier_quota_from_probe(target.account_id, &success(&target, now + 2))
                .await
                .unwrap();
            let row = sqlx::query(
                "SELECT * FROM upstream_supplier_availability WHERE upstream_account_id=$1",
            )
            .bind(target.account_id.to_string())
            .fetch_one(&db.pool)
            .await
            .unwrap();
            assert_eq!(
                row.get::<String, _>("failure_kind"),
                if new_hard {
                    "quota_exhausted"
                } else {
                    "rate_limited"
                }
            );
            assert_eq!(
                row.get::<Option<i64>, _>("hard_observed_at"),
                new_hard.then_some(evidence_at)
            );
            assert_eq!(
                row.get::<Option<i64>, _>("reset_at"),
                (!new_hard).then_some(now + 7_200_000)
            );
            assert_eq!(
                row.get::<Option<i64>, _>("reset_observed_at"),
                (!new_hard).then_some(evidence_at)
            );
            assert_eq!(
                db.supplier_quota_probe_deadline(target.account_id, "exact")
                    .await
                    .unwrap(),
                (!new_hard).then_some(now + 7_200_000)
            );
            assert_eq!(
                db.supplier_quota_recovery_models(target.account_id)
                    .await
                    .unwrap(),
                if new_hard {
                    vec!["exact".to_owned()]
                } else {
                    vec![]
                }
            );
        }
    }
}

#[tokio::test]
async fn sqlite_r1_evidence_order_timing_reset_and_history() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("r1.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    contracts(&db).await;
    db.pool.close().await;
}

#[tokio::test]
async fn postgres_r1_evidence_order_timing_reset_and_history() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let db = Database::connect(&url).await.unwrap();
    db.migrate().await.unwrap();
    contracts(&db).await;
    db.pool.close().await;
}

#[tokio::test]
async fn migration149_retains_disabled_legacy_success_without_inventing_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("upgrade.db").display()
    ))
    .await
    .unwrap();
    sqlx::raw_sql(
        "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, status TEXT NOT NULL);
         CREATE TABLE upstream_service_probes (upstream_account_id TEXT PRIMARY KEY, status TEXT NOT NULL, error_code TEXT, latency_ms BIGINT, checked_at BIGINT NOT NULL);
         INSERT INTO upstream_accounts VALUES ('00000000-0000-0000-0000-000000000000','disabled');
         INSERT INTO upstream_service_probes VALUES ('00000000-0000-0000-0000-000000000000','healthy',NULL,17,123);"
    ).execute(&db.pool).await.unwrap();
    sqlx::raw_sql(include_str!(
        "../../../../migrations/common/0149_scoped_supplier_availability.sql"
    ))
    .execute(&db.pool)
    .await
    .unwrap();
    let historical = db
        .upstream_service_probe(Uuid::nil())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(historical["status"], "unknown");
    assert_eq!(historical["source"], "legacy_unverified");
    assert_eq!(historical["historical_result"]["status"], "healthy");
    assert_eq!(historical["historical_result"]["checked_at"], 123);
    let status: String = sqlx::query_scalar("SELECT status FROM upstream_accounts")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(status, "disabled");
    db.pool.close().await;
}
