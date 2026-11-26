//! Evidence from an actual supplier request, never from normalized gateway errors.
use crate::{AppState, provider::UpstreamCredential};
use http::{HeaderMap, StatusCode, header};
use uuid::Uuid;

pub(super) fn uses_account_bearer(headers: &HeaderMap, credential: &UpstreamCredential) -> bool {
    let UpstreamCredential::OAuth { access_token, .. } = credential else {
        return false;
    };
    let mut values = headers.get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() {
        return false;
    }
    let bytes = value.as_bytes();
    bytes.len() > 7
        && bytes[..7].eq_ignore_ascii_case(b"Bearer ")
        && &bytes[7..] == access_token.as_bytes()
}

pub(super) async fn observe(
    state: &AppState,
    account_id: Uuid,
    generation: i64,
    status: StatusCode,
    account_bearer: bool,
) {
    // Called only after execute returned a supplier HTTP response. Local auth,
    // transport failures and 403 permissions/limits cannot enter this branch.
    // Successful inference has no database query and no additional task.
    if status != StatusCode::UNAUTHORIZED
        || !account_bearer
        || crate::api::proxy::independent_probe::TARGET
            .try_with(|_| ())
            .is_ok()
    {
        return;
    }
    state
        .upstream_quota
        .oauth_rejections
        .submit(account_id, generation);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn inference_rejection_records_only_supplier_401_for_the_selected_generation() {
        let directory = tempfile::tempdir().unwrap();
        let state = AppState::initialize(crate::config::Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("inference-rejection.db").display()
        )))
        .await
        .unwrap();
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-access".into(),
            refresh_token: Some("fixture-refresh".into()),
            expires_at: Some(crate::db::unix_millis() + 86_400_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let account = state
            .db
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: "inference-rejection".into(),
                    name: "fixture".into(),
                    driver: "http-json".into(),
                    config: serde_json::json!({"base_url":"https://api.example.test"}),
                    credential: credential.clone(),
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some("cursor".into()),
                    oauth_refresh_url: Some("https://refresh.example.test".into()),
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        for (status, bearer) in [
            (200, true),
            (403, true),
            (407, true),
            (429, true),
            (502, true),
            (401, false),
        ] {
            observe(
                &state,
                account.id,
                1,
                StatusCode::from_u16(status).unwrap(),
                bearer,
            )
            .await;
            state.upstream_quota.oauth_rejections.flush(&state).await;
            assert!(
                !state
                    .db
                    .has_rejected_oauth_generation(account.id, 1)
                    .await
                    .unwrap()
            );
        }
        let target = crate::api::proxy::independent_probe::IndependentProbeTarget::new(
            account.id,
            "fixture-model".into(),
            1,
            account.updated_at,
        );
        crate::api::proxy::independent_probe::TARGET
            .scope(
                target,
                observe(&state, account.id, 1, StatusCode::UNAUTHORIZED, true),
            )
            .await;
        state.upstream_quota.oauth_rejections.flush(&state).await;
        assert!(
            !state
                .db
                .has_rejected_oauth_generation(account.id, 1)
                .await
                .unwrap()
        );
        observe(&state, account.id, 1, StatusCode::UNAUTHORIZED, true).await;
        state.upstream_quota.oauth_rejections.flush(&state).await;
        assert_eq!(
            state
                .db
                .list_managed_oauth_refresh_candidates(crate::db::unix_millis(), 20)
                .await
                .unwrap(),
            vec![(account.id, 1)]
        );
        let rotated = state
            .db
            .rotate_upstream_credential(
                account.id,
                credential,
                "fixture-rotation",
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        observe(&state, account.id, 1, StatusCode::UNAUTHORIZED, true).await;
        state.upstream_quota.oauth_rejections.flush(&state).await;
        assert!(
            !state
                .db
                .has_rejected_oauth_generation(account.id, rotated.credential_generation)
                .await
                .unwrap()
        );
        assert!(
            state
                .db
                .list_managed_oauth_refresh_candidates(crate::db::unix_millis(), 20)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn only_the_actual_account_oauth_bearer_is_evidence() {
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-access".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        for (value, expected) in [
            ("Bearer fixture-access", true),
            ("bearer fixture-access", true),
            ("Bearer gateway-token", false),
            ("Basic fixture-access", false),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(header::AUTHORIZATION, value.parse().unwrap());
            assert_eq!(uses_account_bearer(&headers, &credential), expected);
            assert!(!uses_account_bearer(&headers, &UpstreamCredential::None));
        }
        let mut headers = HeaderMap::new();
        headers.append(
            header::AUTHORIZATION,
            "Bearer fixture-access".parse().unwrap(),
        );
        headers.append(
            header::AUTHORIZATION,
            "Bearer gateway-token".parse().unwrap(),
        );
        assert!(!uses_account_bearer(&headers, &credential));
    }
}
