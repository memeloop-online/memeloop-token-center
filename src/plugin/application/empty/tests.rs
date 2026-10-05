use super::*;
use crate::{AppState, config::Config};
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn fixture() -> (tempfile::TempDir, AppState) {
    let directory = tempfile::tempdir().unwrap();
    let inventory = directory.path().join("inventory.json");
    std::fs::write(&inventory, b"{}").unwrap();
    let mut config = Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("empty.db").display()
    ));
    config.plugin_inventory_file = Some(inventory.to_str().unwrap().into());
    let state = AppState::initialize(config).await.unwrap();
    (directory, state)
}

async fn register(
    state: &AppState,
    token: &str,
    body: serde_json::Value,
    key: Option<&str>,
) -> StatusCode {
    let mut request = Request::post("/internal/v1/plugin-runtime/empty-inventories")
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/json");
    if let Some(key) = key {
        request = request.header("idempotency-key", key);
    }
    crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::Control)
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

#[tokio::test]
async fn empty_inventory_http_registration_replay_audit_and_restart() {
    let (directory, state) = fixture().await;
    let authority = state.application_plugins.as_ref().unwrap();
    assert!(authority.status().await.unwrap().current.is_none());
    for _ in 0..2 {
        assert_eq!(
            register(
                &state,
                &state.config.service_token,
                json!({"inventory_id":"recovery"}),
                Some("empty-register")
            )
            .await,
            StatusCode::NO_CONTENT
        );
    }
    let status = authority.status().await.unwrap();
    assert!(status.current.is_none());
    assert_eq!(status.candidates.len(), 1);
    assert!(status.candidates[0].staged);
    assert!(status.candidates[0].plugins.is_empty());
    let audit = state.db.plugin_audit(None).await.unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0].action, "register_empty");
    assert_eq!(audit[0].outcome, "staged");
    assert_eq!(audit[0].actor, "bootstrap");
    assert!(audit[0].revision.is_none());
    let entries: BTreeMap<String, PreinstalledInventory> =
        serde_json::from_slice(&std::fs::read(directory.path().join("inventory.json")).unwrap())
            .unwrap();
    assert!(entries["recovery"].grants.is_empty());
    assert_eq!(entries["recovery"].root.parent(), Some(directory.path()));
    assert_eq!(
        std::fs::read_dir(&entries["recovery"].root)
            .unwrap()
            .count(),
        0
    );
    assert_eq!(
        register(
            &state,
            &state.config.service_token,
            json!({"inventory_id":"different"}),
            Some("empty-register")
        )
        .await,
        StatusCode::CONFLICT
    );
    assert_eq!(authority.status().await.unwrap().candidates.len(), 1);
    let restarted = AppState::initialize((*state.config).clone()).await.unwrap();
    let restarted_authority = restarted.application_plugins.as_ref().unwrap();
    assert!(restarted_authority.status().await.unwrap().candidates[0].staged);
    let published = restarted_authority
        .publish(
            PublishApplicationPlugin {
                inventory_id: "recovery".into(),
                expected_revision: 0,
            },
            "empty-first-positive-revision",
        )
        .await
        .unwrap();
    assert_eq!(published.revision, 1);
    let gateway = AppState::initialize((*state.config).clone()).await.unwrap();
    let pinned = gateway.pin_application_plugins().await.unwrap();
    assert!(pinned.plugins.manifests().is_empty());
    assert_eq!(
        pinned.pinned_application_plugins.unwrap().receipt.revision,
        1
    );
}

#[tokio::test]
async fn empty_inventory_rejects_authority_path_grants_and_missing_key() {
    let (_directory, state) = fixture().await;
    let body = json!({"inventory_id":"recovery"});
    assert_eq!(
        register(&state, "invalid", body.clone(), Some("key")).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        register(&state, &state.config.service_token, body.clone(), None).await,
        StatusCode::BAD_REQUEST
    );
    state.db.create_tenant("empty-test", None).await.unwrap();
    for tenant in [None, Some("empty-test".to_owned())] {
        let issued = state
            .db
            .create_service_token(
                crate::db::CreateServiceTokenInput {
                    name: "empty-limited".into(),
                    scopes: if tenant.is_some() {
                        vec!["plugins:write".into()]
                    } else {
                        vec!["plugins:read".into()]
                    },
                    tenant_external_id: tenant,
                },
                state.config.key_pepper.as_bytes(),
            )
            .await
            .unwrap();
        assert_eq!(
            register(&state, &issued.token, body.clone(), Some("key")).await,
            StatusCode::FORBIDDEN
        );
    }
    for body in [
        json!({"inventory_id":"recovery","root":"/tmp/untrusted"}),
        json!({"inventory_id":"recovery","grants":{}}),
        json!({"inventory_id":"recovery","packages":[]}),
    ] {
        assert_eq!(
            register(&state, &state.config.service_token, body, Some("key")).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        register(
            &state,
            &state.config.service_token,
            json!({"inventory_id":"../escape"}),
            Some("key")
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    let authority = state.application_plugins.as_ref().unwrap();
    assert!(authority.status().await.unwrap().candidates.is_empty());
    assert!(state.db.plugin_audit(None).await.unwrap().is_empty());
}

async fn concurrent_registration_and_cas(state: &AppState) {
    let second = AppState::initialize((*state.config).clone()).await.unwrap();
    let (first, replay) = tokio::join!(
        register(
            state,
            &state.config.service_token,
            json!({"inventory_id":"empty"}),
            Some("register-once")
        ),
        register(
            &second,
            &second.config.service_token,
            json!({"inventory_id":"empty"}),
            Some("register-once")
        ),
    );
    assert_eq!(first, StatusCode::NO_CONTENT);
    assert_eq!(replay, StatusCode::NO_CONTENT);
    assert_eq!(state.db.plugin_audit(None).await.unwrap().len(), 1);
    let first = state.application_plugins.as_ref().unwrap();
    let second = second.application_plugins.as_ref().unwrap();
    let request = || PublishApplicationPlugin {
        inventory_id: "empty".into(),
        expected_revision: 0,
    };
    let (left, right) = tokio::join!(
        first.publish(request(), "publish-left"),
        second.publish(request(), "publish-right")
    );
    let winning_key = match (left, right) {
        (Ok(receipt), Err(AppError::Conflict(_))) => {
            assert_eq!(receipt.revision, 1);
            "publish-left"
        }
        (Err(AppError::Conflict(_)), Ok(receipt)) => {
            assert_eq!(receipt.revision, 1);
            "publish-right"
        }
        _ => panic!("exactly one CAS publication must win"),
    };
    assert_eq!(
        first
            .publish(request(), winning_key)
            .await
            .unwrap()
            .revision,
        1
    );
    assert_eq!(
        state
            .db
            .application_plugin_history(None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(
        first
            .rollback(
                RollbackApplicationPlugin {
                    target_revision: 0,
                    expected_revision: 1
                },
                "not-rev-zero"
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn empty_inventory_sqlite_concurrent_registration_and_publication_cas() {
    let (_directory, state) = fixture().await;
    concurrent_registration_and_cas(&state).await;
}

#[tokio::test]
async fn empty_inventory_postgres_concurrent_registration_and_publication_cas() {
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    sqlx::any::install_default_drivers();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    let schema = format!("empty_inventory_{}", uuid::Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&pool)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-c search_path={schema}"));
    let directory = tempfile::tempdir().unwrap();
    let inventory = directory.path().join("inventory.json");
    std::fs::write(&inventory, b"{}").unwrap();
    let mut config = Config::for_test(isolated.to_string());
    config.plugin_inventory_file = Some(inventory.to_str().unwrap().into());
    let state = AppState::initialize(config).await.unwrap();
    concurrent_registration_and_cas(&state).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&pool)
        .await
        .unwrap();
}

#[tokio::test]
async fn empty_inventory_immutable_collision_and_install_lock_fail_closed() {
    let (directory, state) = fixture().await;
    let authority = state.application_plugins.as_ref().unwrap();
    let occupied = directory.path().join("occupied");
    std::fs::create_dir(&occupied).unwrap();
    installation::append_inventory_file(
        &directory.path().join("inventory.json"),
        "occupied",
        &PreinstalledInventory {
            root: occupied,
            grants: BTreeMap::new(),
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        authority
            .register_empty(
                RegisterEmptyInventory {
                    inventory_id: "occupied".into()
                },
                "collision",
                "bootstrap"
            )
            .await,
        Err(AppError::Conflict(_))
    ));
    assert!(state.db.plugin_audit(None).await.unwrap().is_empty());
    state
        .db
        .begin_plugin_installation(
            "installing",
            &json!(["synthetic-reference"]),
            "request",
            "key",
            "bootstrap",
            crate::db::unix_millis() + 270_000,
        )
        .await
        .unwrap();
    assert!(matches!(
        authority
            .register_empty(
                RegisterEmptyInventory {
                    inventory_id: "recovery".into()
                },
                "lock",
                "bootstrap"
            )
            .await,
        Err(AppError::Overloaded)
    ));
    assert_eq!(authority.status().await.unwrap().candidates.len(), 1);
}

#[cfg(unix)]
#[tokio::test]
async fn empty_inventory_rejects_symlink_and_nonempty_host_directory() {
    let (directory, state) = fixture().await;
    for (id, symlink) in [("link", true), ("nonempty", false)] {
        let digest = crate::plugin::plugin_configuration_schema_digest(&json!(id)).unwrap();
        let root = directory.path().join(format!(".mtc-empty-{digest}"));
        if symlink {
            std::os::unix::fs::symlink(directory.path(), &root).unwrap();
        } else {
            std::fs::create_dir(&root).unwrap();
            std::fs::write(root.join("unexpected"), b"synthetic").unwrap();
        }
        assert_eq!(
            register(
                &state,
                &state.config.service_token,
                json!({"inventory_id":id}),
                Some(id)
            )
            .await,
            StatusCode::FORBIDDEN
        );
    }
    assert!(
        state
            .application_plugins
            .as_ref()
            .unwrap()
            .status()
            .await
            .unwrap()
            .candidates
            .is_empty()
    );
    assert!(state.db.plugin_audit(None).await.unwrap().is_empty());
}
