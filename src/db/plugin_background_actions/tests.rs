use super::*;
use crate::upstream_quota::reset::BackgroundResetGuard;

const PLUGIN: &str = "mtc-codex-expiring-reset";
const PEPPER: &[u8] = b"synthetic-background-fixture";

async fn fixture(db: &Database) -> (UpstreamAccountView, BackgroundResetGuard, String) {
    let tenant = format!("background-{}", Uuid::now_v7());
    let account = db.create_upstream_account(CreateUpstreamAccountInput {
        tenant_external_id: tenant.clone(), name: tenant.clone(), driver: "openai-codex".into(),
        config: serde_json::json!({}), credential: UpstreamCredential::OAuth {
            access_token: "synthetic-only".into(), refresh_token: None, expires_at: None,
            header: "authorization".into(), prefix: "Bearer ".into(), proxy_url: None,
            proxy_network_scope: None, adapter_state: Some(serde_json::json!({"schema":"openai-codex-oauth-v1","account_id":"synthetic"})),
        }, oauth_session_id: None, oauth_driver: None, oauth_refresh_url: None,
    }, PEPPER).await.unwrap();
    let issued = db
        .create_service_token(
            CreateServiceTokenInput {
                name: tenant.clone(),
                scopes: vec!["providers:write".into(), "plugins:write".into()],
                tenant_external_id: Some(tenant.clone()),
            },
            PEPPER,
        )
        .await
        .unwrap();
    let actor = db
        .authenticate_service_token(&issued.token, PEPPER)
        .await
        .unwrap();
    let grant = db
        .put_background_action_grant(PLUGIN, &tenant, account.id, true, 0, &actor, "")
        .await
        .unwrap();
    let now = unix_millis();
    assert!(
        db.claim_background_action_tick(&grant, "owner", now, 60_000)
            .await
            .unwrap()
    );
    let prepared = db
        .prepare_quota_reset(PrepareQuotaReset {
            id: Uuid::now_v7(),
            account: account.clone(),
            actor: format!("plugin:{PLUGIN}"),
            confirmation_hash: "confirmation".into(),
            prepare_idempotency_hash: "stable-business-key".into(),
            available: 1,
            applicable: 1,
            observed_at: now,
        })
        .await
        .unwrap();
    (
        account,
        BackgroundResetGuard {
            grant,
            lease_owner: "owner".into(),
            credit_fingerprint: "credit".into(),
            inventory_fingerprint: "inventory".into(),
            not_before_ms: now - 1,
            deadline_ms: now + 600_000,
        },
        prepared.operation.id,
    )
}

async fn claim(
    db: &Database,
    account: &UpstreamAccountView,
    guard: &BackgroundResetGuard,
    id: &str,
) -> Result<QuotaResetClaim, AppError> {
    db.claim_quota_reset_guarded(
        account,
        id,
        &format!("plugin:{PLUGIN}"),
        "confirmation",
        "stable-confirm-key",
        Some(guard),
    )
    .await
}

async fn contract(db: &Database) {
    let (account, guard, id) = fixture(db).await;
    // Simulate a Pod dying for >120s before dispatch without a wall-clock sleep.
    sqlx::query("UPDATE upstream_quota_reset_operations SET expires_at=$1 WHERE id=$2")
        .bind(unix_millis() - 120_001)
        .bind(&id)
        .execute(&db.pool)
        .await
        .unwrap();
    let expired = db
        .quota_reset_prepare_replay(&account, &format!("plugin:{PLUGIN}"), "stable-business-key")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(expired.state, "expired");
    db.refresh_background_quota_reset_prepare(&account, &id, 2, 2, unix_millis(), &guard)
        .await
        .unwrap();
    let refreshed = db
        .quota_reset_operation(
            &guard.grant.tenant_id,
            &guard.grant.upstream_account_id,
            &id,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.id, id);
    assert_eq!(
        (
            refreshed.state.as_str(),
            refreshed.available_credits,
            refreshed.applicable_credits
        ),
        ("prepared", 2, 2)
    );
    // Also recover inventory changes before the original preparation expires.
    db.refresh_background_quota_reset_prepare(&account, &id, 3, 3, unix_millis(), &guard)
        .await
        .unwrap();
    let refreshed = db
        .quota_reset_operation(
            &guard.grant.tenant_id,
            &guard.grant.upstream_account_id,
            &id,
        )
        .await
        .unwrap();
    assert_eq!(
        (refreshed.available_credits, refreshed.applicable_credits),
        (3, 3)
    );
    let (left, right) = tokio::join!(
        claim(db, &account, &guard, &id),
        claim(db, &account, &guard, &id)
    );
    let claims = [left.unwrap(), right.unwrap()];
    assert_eq!(
        claims
            .iter()
            .filter(|claim| matches!(claim, QuotaResetClaim::Claimed { .. }))
            .count(),
        1
    );
    assert_eq!(
        claims
            .iter()
            .filter(|claim| matches!(claim, QuotaResetClaim::Replayed(_)))
            .count(),
        1
    );
    for accepted in [false, true] {
        db.finish_quota_reset(&id, accepted, (!accepted).then_some("synthetic_ambiguous"))
            .await
            .unwrap();
        db.refresh_background_quota_reset_prepare(&account, &id, 9, 9, unix_millis(), &guard)
            .await
            .unwrap();
        let operation = db
            .quota_reset_operation(
                &guard.grant.tenant_id,
                &guard.grant.upstream_account_id,
                &id,
            )
            .await
            .unwrap();
        assert_ne!(operation.state, "prepared");
        assert_eq!(operation.available_credits, 3);
        assert!(matches!(
            claim(db, &account, &guard, &id).await.unwrap(),
            QuotaResetClaim::Replayed(_)
        ));
    }
    // Racing renewal and claim retain the same irreversible operation.
    let (account, guard, id) = fixture(db).await;
    let (renew, claimed) = tokio::join!(
        db.refresh_background_quota_reset_prepare(&account, &id, 2, 2, unix_millis(), &guard),
        claim(db, &account, &guard, &id)
    );
    renew.unwrap();
    assert!(matches!(claimed.unwrap(), QuotaResetClaim::Claimed { .. }));
    db.refresh_background_quota_reset_prepare(&account, &id, 9, 9, unix_millis(), &guard)
        .await
        .unwrap();
    assert!(matches!(
        claim(db, &account, &guard, &id).await.unwrap(),
        QuotaResetClaim::Replayed(_)
    ));

    // Every authority fence is checked again at the irreversible claim.
    for scenario in [
        "grant",
        "lease",
        "actor_generation",
        "actor_revoked",
        "actor_disabled",
        "provider_scope",
        "plugin_scope",
        "actor_tenant",
        "credential",
        "account",
        "tenant",
    ] {
        let (account, guard, id) = fixture(db).await;
        let actor = &guard.grant.actor_service_id;
        let query = match scenario {
            "grant" => sqlx::query("UPDATE plugin_background_action_grants SET enabled=0,revision=revision+1 WHERE upstream_account_id=$1").bind(account.id.to_string()),
            "lease" => sqlx::query("UPDATE plugin_background_action_grants SET lease_until=0 WHERE upstream_account_id=$1").bind(account.id.to_string()),
            "actor_generation" => sqlx::query("UPDATE service_principals SET credential_generation=credential_generation+1 WHERE id=$1").bind(actor),
            "actor_revoked" => sqlx::query("UPDATE service_credentials SET revoked_at=1 WHERE service_principal_id=$1").bind(actor),
            "actor_disabled" => sqlx::query("UPDATE service_principals SET status='disabled' WHERE id=$1").bind(actor),
            "provider_scope" => sqlx::query("UPDATE service_credentials SET scopes_json='[\"plugins:write\"]' WHERE service_principal_id=$1").bind(actor),
            "plugin_scope" => sqlx::query("UPDATE service_credentials SET scopes_json='[\"providers:write\"]' WHERE service_principal_id=$1").bind(actor),
            "actor_tenant" => sqlx::query("UPDATE service_credentials SET tenant_external_id='wrong-tenant' WHERE service_principal_id=$1").bind(actor),
            "credential" => sqlx::query("UPDATE upstream_accounts SET credential_generation=credential_generation+1 WHERE id=$1").bind(account.id.to_string()),
            "account" => sqlx::query("UPDATE upstream_accounts SET status='disabled' WHERE id=$1").bind(account.id.to_string()),
            _ => sqlx::query("UPDATE tenants SET status='disabled' WHERE id=$1").bind(account.tenant_id.to_string()),
        };
        query.execute(&db.pool).await.unwrap();
        assert!(
            claim(db, &account, &guard, &id).await.is_err(),
            "{scenario} must fence claim"
        );
        let state: String =
            sqlx::query_scalar("SELECT state FROM upstream_quota_reset_operations WHERE id=$1")
                .bind(&id)
                .fetch_one(&db.pool)
                .await
                .unwrap();
        assert_eq!(state, "prepared", "{scenario}");
    }
}

#[tokio::test]
async fn sqlite_background_action_recovery_and_authority_fences() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("background.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    contract(&db).await;
}

#[tokio::test]
async fn postgres_background_action_recovery_and_authority_fences() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let fixture = crate::db::test_support::PostgresTestDatabase::create(&url).await;
    let db = Database::connect(fixture.url()).await.unwrap();
    db.migrate().await.unwrap();
    contract(&db).await;
    db.close().await;
    fixture.cleanup().await;
}
