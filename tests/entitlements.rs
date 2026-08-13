use memeloop_token_center::{
    AppState, api,
    config::Config,
    db::{
        CancelEntitlementInput, CreateKeyInput, CreateServiceTokenInput, Database,
        EntitlementOperation, ReconcileEntitlementInput, ReplaceEntitlementInput,
    },
    error::AppError,
    model::KeyPolicy,
};
use rust_decimal::Decimal;
use uuid::Uuid;

const PEPPER: &[u8] = b"entitlement test pepper longer than thirty-two bytes";

struct Fixture {
    _directory: tempfile::TempDir,
    database_url: String,
    database: Database,
    tenant: String,
    account_id: Uuid,
    downstream_key: String,
}

impl Fixture {
    async fn new(label: &str) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("entitlement.db").display()
        );
        let database = Database::connect_with_max(&database_url, 8).await.unwrap();
        database.migrate().await.unwrap();
        let tenant = format!("entitlement-{label}-{}", Uuid::now_v7());
        let key = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: tenant.clone(),
                    principal_external_id: "member".into(),
                    alias: "stable-credit-account".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                PEPPER,
            )
            .await
            .unwrap();
        Self {
            _directory: directory,
            database_url,
            database,
            tenant,
            account_id: key.account_id,
            downstream_key: key.key,
        }
    }

    fn reconcile(
        &self,
        subscription: &str,
        cycle: &str,
        desired_micros: i64,
        version: i64,
    ) -> ReconcileEntitlementInput {
        ReconcileEntitlementInput {
            tenant_external_id: self.tenant.clone(),
            account_id: self.account_id,
            provider: "memeloop-web".into(),
            external_subscription_id: subscription.into(),
            external_cycle_id: cycle.into(),
            period_start: 1_700_000_000_000,
            period_end: 4_100_000_000_000,
            currency: "USD".into(),
            desired_micros,
            version,
            source: "subscription-webhook".into(),
            proration_json: Some(r#"{"reason":"upgrade"}"#.into()),
        }
    }
}

#[tokio::test]
async fn reconcile_upgrade_cancel_and_replay_are_exact() {
    let fixture = Fixture::new("lifecycle").await;
    let first = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile("sub-1", "cycle-1", 10_000_000, 1)),
            "entitlement-create",
        )
        .await
        .unwrap();
    assert_eq!(first.entitlement.desired, "10");
    assert_eq!(first.entitlement.consumed, "0");
    assert_eq!(first.entitlement.remaining, "10");
    assert_eq!(first.ledger_delta, "10");
    assert_eq!(first.entitlement.version, 1);

    let replay = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile("sub-1", "cycle-1", 10_000_000, 1)),
            "entitlement-create",
        )
        .await
        .unwrap();
    assert_eq!(replay, first);
    let conflicting = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile("sub-1", "cycle-1", 11_000_000, 1)),
            "entitlement-create",
        )
        .await;
    assert!(matches!(conflicting, Err(AppError::Conflict(_))));

    let upgraded = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile("sub-1", "cycle-1", 15_000_000, 2)),
            "entitlement-upgrade",
        )
        .await
        .unwrap();
    assert_eq!(upgraded.entitlement.remaining, "15");
    assert_eq!(upgraded.ledger_delta, "5");

    let cancelled = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Cancel(CancelEntitlementInput {
                tenant_external_id: fixture.tenant.clone(),
                provider: "memeloop-web".into(),
                external_subscription_id: "sub-1".into(),
                external_cycle_id: Some("cycle-1".into()),
                version: 3,
                source: "subscription-cancelled".into(),
            }),
            "entitlement-cancel",
        )
        .await
        .unwrap();
    assert_eq!(cancelled.entitlement.status, "cancelled");
    assert_eq!(cancelled.entitlement.remaining, "0");
    assert_eq!(cancelled.ledger_delta, "-15");
    let cancellation_replay = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Cancel(CancelEntitlementInput {
                tenant_external_id: fixture.tenant.clone(),
                provider: "memeloop-web".into(),
                external_subscription_id: "sub-1".into(),
                external_cycle_id: Some("cycle-1".into()),
                version: 3,
                source: "subscription-cancelled".into(),
            }),
            "entitlement-cancel",
        )
        .await
        .unwrap();
    assert_eq!(cancellation_replay, cancelled);
    assert!(matches!(
        fixture
            .database
            .reconcile_entitlement(
                EntitlementOperation::Reconcile(
                    fixture.reconcile("sub-1", "cycle-1", 10_000_000, 3)
                ),
                "accidental-reactivation"
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    let explicit_new_version = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile("sub-1", "cycle-1", 8_000_000, 4)),
            "explicit-reactivation-v4",
        )
        .await
        .unwrap();
    assert_eq!(explicit_new_version.entitlement.status, "active");
    assert_eq!(explicit_new_version.entitlement.remaining, "8");
}

#[tokio::test]
async fn replace_is_atomic_and_preserves_the_credit_account() {
    let fixture = Fixture::new("replace").await;
    let old = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile(
                "sub-old",
                "old-cycle",
                10_000_000,
                1,
            )),
            "create-old",
        )
        .await
        .unwrap();
    let replacement = fixture.reconcile("sub-new", "new-cycle", 7_500_000, 1);
    let replaced = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Replace(ReplaceEntitlementInput {
                tenant_external_id: fixture.tenant.clone(),
                provider: "memeloop-web".into(),
                external_subscription_id: "sub-old".into(),
                version: 2,
                source: "subscription-replaced".into(),
                replacement,
            }),
            "replace-old-with-new",
        )
        .await
        .unwrap();
    assert_eq!(
        replaced.replaced_entitlement_id,
        Some(old.entitlement.entitlement_id)
    );
    assert_eq!(replaced.entitlement.external_subscription_id, "sub-new");
    assert_eq!(replaced.entitlement.account_id, fixture.account_id);
    assert_eq!(replaced.entitlement.remaining, "7.5");
    assert_eq!(replaced.ledger_delta, "-2.5");

    let rows = fixture
        .database
        .list_entitlements(Some(&fixture.tenant), None, None)
        .await
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows.iter().any(|row| row.status == "replaced"));
    assert!(rows.iter().any(|row| row.status == "active"));
}

#[tokio::test]
async fn cancellation_revokes_only_unconsumed_entitlement_credit() {
    let fixture = Fixture::new("consumption").await;
    fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(fixture.reconcile(
                "sub-consumed",
                "cycle-consumed",
                10_000_000,
                1,
            )),
            "create-consumed",
        )
        .await
        .unwrap();
    let key = fixture
        .database
        .authenticate_key(&fixture.downstream_key, PEPPER)
        .await
        .unwrap();
    let price = fixture
        .database
        .upsert_model_price(
            "entitlement-meter-test",
            "USD",
            Decimal::from(1_000_000),
            Decimal::ZERO,
        )
        .await
        .unwrap();
    let reservation = fixture
        .database
        .reserve_usage(&key, &price, 4, 0)
        .await
        .unwrap();
    assert_eq!(
        fixture
            .database
            .settle_usage(&reservation, 4, 0)
            .await
            .unwrap(),
        4_000_000
    );
    let cancelled = fixture
        .database
        .reconcile_entitlement(
            EntitlementOperation::Cancel(CancelEntitlementInput {
                tenant_external_id: fixture.tenant.clone(),
                provider: "memeloop-web".into(),
                external_subscription_id: "sub-consumed".into(),
                external_cycle_id: Some("cycle-consumed".into()),
                version: 2,
                source: "cancel-after-usage".into(),
            }),
            "cancel-consumed",
        )
        .await
        .unwrap();
    assert_eq!(cancelled.entitlement.desired, "10");
    assert_eq!(cancelled.entitlement.consumed, "4");
    assert_eq!(cancelled.entitlement.remaining, "0");
    assert_eq!(cancelled.ledger_delta, "-6");
}

#[tokio::test]
async fn account_and_subscription_identity_cannot_cross_tenants() {
    let first = Fixture::new("tenant-a").await;
    let second = Fixture::new("tenant-b").await;
    let mut forged = first.reconcile("sub-cross", "cycle", 10_000_000, 1);
    forged.account_id = second.account_id;
    assert!(matches!(
        first
            .database
            .reconcile_entitlement(
                EntitlementOperation::Reconcile(forged),
                "cross-tenant-account"
            )
            .await,
        Err(AppError::Forbidden)
    ));
}

#[tokio::test]
async fn entitlement_http_api_enforces_scopes_and_tenant_boundary() {
    let first = Fixture::new("http-scope").await;
    let second_tenant = format!("other-tenant-{}", Uuid::now_v7());
    let second_key = first
        .database
        .create_key(
            CreateKeyInput {
                tenant_external_id: second_tenant,
                principal_external_id: "other-member".into(),
                alias: "other-account".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ZERO,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    let scoped = first
        .database
        .create_service_token(
            CreateServiceTokenInput {
                name: "subscription-reconciler".into(),
                scopes: vec!["entitlements:read".into(), "entitlements:write".into()],
                tenant_external_id: Some(first.tenant.clone()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let wrong_scope = first
        .database
        .create_service_token(
            CreateServiceTokenInput {
                name: "read-requests-only".into(),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some(first.tenant.clone()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let mut config = Config::for_test(first.database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).unwrap();
    let state = AppState::initialize(config).await.unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, api::router(state)).await });
    let client = reqwest::Client::new();
    let url = format!("http://{address}/internal/v1/entitlements");

    let body = serde_json::json!({
        "account_id": first.account_id,
        "provider": "memeloop-web",
        "external_subscription_id": "sub-http",
        "external_cycle_id": "cycle-http",
        "period_start": 1700000000000_i64,
        "period_end": 4100000000000_i64,
        "currency": "USD",
        "desired": "5",
        "version": 1,
        "source": "http-test"
    });
    let response = client
        .put(&url)
        .bearer_auth(&scoped.token)
        .header("idempotency-key", "http-entitlement-create")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::CREATED);
    let response: serde_json::Value = response.json().await.unwrap();
    assert_eq!(response["desired"], "5");
    assert_eq!(response["remaining"], "5");
    assert_eq!(response["ledger_delta"], "5");
    let replay: serde_json::Value = client
        .put(&url)
        .bearer_auth(&scoped.token)
        .header("idempotency-key", "http-entitlement-create")
        .json(&body)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay, response);
    let mut changed_body = body.clone();
    changed_body["desired"] = serde_json::Value::String("6".into());
    let conflict = client
        .put(&url)
        .bearer_auth(&scoped.token)
        .header("idempotency-key", "http-entitlement-create")
        .json(&changed_body)
        .send()
        .await
        .unwrap();
    assert_eq!(conflict.status(), reqwest::StatusCode::CONFLICT);

    let denied = client
        .get(&url)
        .bearer_auth(&wrong_scope.token)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), reqwest::StatusCode::FORBIDDEN);
    let cross_tenant = client
        .put(&url)
        .bearer_auth(&scoped.token)
        .header("idempotency-key", "http-entitlement-cross-tenant")
        .json(&serde_json::json!({
            "tenant_external_id": first.tenant,
            "account_id": second_key.account_id,
            "provider": "memeloop-web",
            "external_subscription_id": "sub-cross-http",
            "external_cycle_id": "cycle-cross-http",
            "period_start": 1700000000000_i64,
            "period_end": 4100000000000_i64,
            "currency": "USD",
            "desired": "5",
            "version": 1,
            "source": "http-test"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(cross_tenant.status(), reqwest::StatusCode::FORBIDDEN);

    let own_rows: serde_json::Value = client
        .get(&url)
        .bearer_auth(&scoped.token)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(own_rows.as_array().unwrap().len(), 1);
    assert_eq!(own_rows[0]["tenant_external_id"], first.tenant);
    server.abort();
}
