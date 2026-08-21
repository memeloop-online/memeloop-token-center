use memeloop_token_center::{
    db::{
        BeginOAuthLoginSession, CreateUpstreamAccountInput, Database, OAuthLoginClaim,
        OAuthLoginSessionReference,
    },
    provider::UpstreamCredential,
};
use uuid::Uuid;

#[tokio::test]
async fn two_postgres_pools_share_single_poll_and_finalize_owners() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let first = Database::connect(&database_url).await.unwrap();
    first.migrate().await.unwrap();
    let second = Database::connect(&database_url).await.unwrap();
    let suffix = Uuid::now_v7();
    let tenant = format!("oauth-pg-{suffix}");
    let session_id = Uuid::now_v7();
    let now = memeloop_token_center::db::unix_millis();
    let expires_at = now + 60_000;
    first
        .begin_oauth_login_session(BeginOAuthLoginSession {
            session_id,
            tenant_external_id: tenant.clone(),
            operator_service_id: None,
            state_ciphertext: "encrypted-state".to_owned(),
            next_poll_at: now,
            expires_at,
        })
        .await
        .unwrap();
    let reference = OAuthLoginSessionReference {
        session_id,
        tenant_external_id: tenant.clone(),
        operator_service_id: None,
        expires_at,
    };
    let other_reference = reference.clone();
    let (left, right) = tokio::join!(
        first.claim_oauth_login_poll(&reference, now, 5),
        second.claim_oauth_login_poll(&other_reference, now, 5)
    );
    let poll_owner = exactly_one_poll_owner(left.unwrap(), right.unwrap());
    first
        .stage_oauth_login_ready(session_id, poll_owner, "encrypted-ready".to_owned(), now)
        .await
        .unwrap();

    let other_reference = reference.clone();
    let (left, right) = tokio::join!(
        first.claim_oauth_login_poll(&reference, now, 5),
        second.claim_oauth_login_poll(&other_reference, now, 5)
    );
    let finalize_owner = exactly_one_finalize_owner(left.unwrap(), right.unwrap());
    let account = first
        .create_upstream_account(
            CreateUpstreamAccountInput {
                tenant_external_id: tenant.clone(),
                name: format!("native-codex-{suffix}"),
                driver: "openai-codex".to_owned(),
                config: serde_json::json!({
                    "base_url": "https://chatgpt.com/backend-api/codex",
                    "network_scope": "public",
                    "reservation_token_bounds": {}
                }),
                credential: UpstreamCredential::OAuth {
                    access_token: "access-token".to_owned(),
                    refresh_token: Some("refresh-token".to_owned()),
                    expires_at: Some(expires_at),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(serde_json::json!({
                        "schema": "openai-codex-oauth-v1",
                        "account_id": format!("account-{suffix}")
                    })),
                },
                oauth_session_id: Some(session_id),
                oauth_driver: Some("openai_codex_device".to_owned()),
                oauth_refresh_url: Some("https://auth.openai.com/oauth/token".to_owned()),
            },
            b"oauth-postgres-pepper-at-least-32-bytes",
        )
        .await
        .unwrap();
    second
        .finish_oauth_login_session(session_id, finalize_owner, account.id, now)
        .await
        .unwrap();
    assert!(matches!(
        first
            .claim_oauth_login_poll(&reference, now, 5)
            .await
            .unwrap(),
        OAuthLoginClaim::Consumed { account_id } if account_id == account.id
    ));

    let disabled = first
        .set_upstream_account_status(account.id, &tenant, "disabled", account.updated_at)
        .await
        .unwrap();
    first
        .delete_upstream_account(account.id, &tenant, disabled.updated_at)
        .await
        .unwrap();
}

fn exactly_one_poll_owner(left: OAuthLoginClaim, right: OAuthLoginClaim) -> Uuid {
    match (left, right) {
        (
            OAuthLoginClaim::Claimed {
                lease_owner,
                state_ciphertext,
            },
            OAuthLoginClaim::Pending { .. },
        )
        | (
            OAuthLoginClaim::Pending { .. },
            OAuthLoginClaim::Claimed {
                lease_owner,
                state_ciphertext,
            },
        ) => {
            assert_eq!(state_ciphertext, "encrypted-state");
            lease_owner
        }
        other => panic!("expected exactly one PostgreSQL poll owner: {other:?}"),
    }
}

fn exactly_one_finalize_owner(left: OAuthLoginClaim, right: OAuthLoginClaim) -> Uuid {
    match (left, right) {
        (
            OAuthLoginClaim::Ready {
                lease_owner,
                ready_ciphertext,
            },
            OAuthLoginClaim::Pending { .. },
        )
        | (
            OAuthLoginClaim::Pending { .. },
            OAuthLoginClaim::Ready {
                lease_owner,
                ready_ciphertext,
            },
        ) => {
            assert_eq!(ready_ciphertext, "encrypted-ready");
            lease_owner
        }
        other => panic!("expected exactly one PostgreSQL finalize owner: {other:?}"),
    }
}
