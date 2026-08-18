use std::sync::Arc;

use memeloop_token_center::{
    db::{
        Database, ImportManagedOAuthAccountInput, ManagedOAuthImportStatus,
        UpdateUpstreamAccountInput, unix_millis,
    },
    error::AppError,
    provider::{
        MANAGED_OAUTH_ADAPTER_API_VERSION, ManagedOAuthAdapterContribution, ProviderCatalog,
        ProviderType, UpstreamCredential,
    },
};
use serde_json::json;
use sqlx::Row;
use uuid::Uuid;

const PEPPER: &[u8] = b"managed OAuth import test pepper longer than thirty-two bytes";

fn test_adapter() -> memeloop_token_center::provider::ResolvedManagedOAuthAdapter {
    let mut catalog = ProviderCatalog::builtins();
    catalog
        .extend([ProviderType {
            id: "managed-test".into(),
            display_name: "Managed test".into(),
            protocols: vec!["openai".into()],
            modalities: vec!["text".into()],
            config_schema: json!({"type": "object"}),
            credential_schema: json!({"type": "object"}),
            oauth_adapter: None,
            managed_oauth_adapter: Some(ManagedOAuthAdapterContribution {
                api_version: MANAGED_OAUTH_ADAPTER_API_VERSION.into(),
                source_types: vec!["codex-account".into()],
                normalize_url: "http://managed-oauth.default.svc/normalize".into(),
                refresh_url: "http://managed-oauth.default.svc/refresh".into(),
            }),
            component_adapter: None,
            source: "test".into(),
        }])
        .unwrap();
    catalog
        .managed_oauth_adapter_for_source("codex-account")
        .unwrap()
}

fn active_input(tenant: &str, source: char, digest: char) -> ImportManagedOAuthAccountInput {
    ImportManagedOAuthAccountInput {
        tenant_external_id: tenant.into(),
        source_key: source.to_string().repeat(64),
        payload_digest: digest.to_string().repeat(64),
        contract_version: 1,
        account_name: "Imported Codex".into(),
        config: json!({"base_url": "https://api.example.test"}),
        credential: UpstreamCredential::OAuth {
            access_token: "managed-access-secret".into(),
            refresh_token: Some("managed-refresh-secret".into()),
            expires_at: Some(unix_millis() + 3_600_000),
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: Some(json!({"family": "adapter-state-secret"})),
        },
        status: ManagedOAuthImportStatus::Active,
        adapter: test_adapter(),
    }
}

fn disabled_input(tenant: &str, source: char, digest: char) -> ImportManagedOAuthAccountInput {
    let mut input = active_input(tenant, source, digest);
    input.status = ManagedOAuthImportStatus::RefreshRequired;
    if let UpstreamCredential::OAuth { expires_at, .. } = &mut input.credential {
        *expires_at = Some(unix_millis() - 1);
    }
    input
}

fn administratively_disabled_input(
    tenant: &str,
    source: char,
    digest: char,
) -> ImportManagedOAuthAccountInput {
    let mut input = active_input(tenant, source, digest);
    input.status = ManagedOAuthImportStatus::Disabled;
    input
}

async fn sqlite_database(label: &str) -> (tempfile::TempDir, String, Database) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join(format!("{label}.db"));
    let url = format!("sqlite://{}?mode=rwc", path.display());
    let database = Database::connect_with_max(&url, 16).await.unwrap();
    database.migrate().await.unwrap();
    (directory, url, database)
}

#[tokio::test]
async fn fresh_sqlite_migrates_to_v34_and_exact_replay_keeps_generation_one() {
    let (_directory, url, database) = sqlite_database("exact-replay").await;
    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    let migration_name: String =
        sqlx::query_scalar("SELECT name FROM schema_migrations WHERE version = 34")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(migration_name, "CPA managed OAuth account imports");

    let input = active_input("managed-exact", 'a', 'b');
    let created = database
        .import_cpa_managed_oauth_account(input.clone(), PEPPER)
        .await
        .unwrap();
    assert!(!created.replayed);
    let replay = database
        .import_cpa_managed_oauth_account(input, PEPPER)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.account.id, created.account.id);
    assert_eq!(replay.account.credential_generation, 1);
    assert_eq!(
        database
            .list_upstream_accounts("managed-exact")
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn changed_payload_digest_conflicts_without_disclosing_hashes() {
    let (_directory, _url, database) = sqlite_database("immutable-conflict").await;
    let original = active_input("managed-conflict", 'c', 'd');
    database
        .import_cpa_managed_oauth_account(original.clone(), PEPPER)
        .await
        .unwrap();

    let mut changed_digest = original.clone();
    changed_digest.payload_digest = "e".repeat(64);
    let error = database
        .import_cpa_managed_oauth_account(changed_digest, PEPPER)
        .await
        .unwrap_err();
    assert!(matches!(error, AppError::Conflict(_)));
    let message = error.to_string();
    assert!(!message.contains(&original.payload_digest));
    assert!(!message.contains(&original.source_key));
}

#[tokio::test]
async fn provenance_lookup_is_exact_conflict_static_and_tenant_isolated() {
    let (_directory, _url, database) = sqlite_database("provenance-lookup").await;
    let input = active_input("lookup-tenant-a", 'a', 'b');
    let created = database
        .import_cpa_managed_oauth_account(input.clone(), PEPPER)
        .await
        .unwrap();

    let exact = database
        .lookup_cpa_managed_oauth_import(
            "lookup-tenant-a",
            &input.source_key,
            &input.payload_digest,
        )
        .await
        .unwrap()
        .expect("exact provenance mapping");
    assert_eq!(exact.id, created.account.id);
    assert_eq!(exact.credential_generation, 1);

    let changed = database
        .lookup_cpa_managed_oauth_import("lookup-tenant-a", &input.source_key, &"c".repeat(64))
        .await
        .unwrap_err();
    assert!(matches!(changed, AppError::Conflict(_)));
    assert!(!changed.to_string().contains(&input.source_key));
    assert!(!changed.to_string().contains(&input.payload_digest));

    assert!(
        database
            .lookup_cpa_managed_oauth_import(
                "lookup-tenant-b",
                &input.source_key,
                &input.payload_digest,
            )
            .await
            .unwrap()
            .is_none()
    );
    let mut other_tenant = input;
    other_tenant.tenant_external_id = "lookup-tenant-b".into();
    let other = database
        .import_cpa_managed_oauth_account(other_tenant.clone(), PEPPER)
        .await
        .unwrap();
    assert_ne!(other.account.id, created.account.id);
    let isolated = database
        .lookup_cpa_managed_oauth_import(
            "lookup-tenant-b",
            &other_tenant.source_key,
            &other_tenant.payload_digest,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(isolated.id, other.account.id);
}

#[tokio::test]
async fn exact_replay_returns_current_account_after_operator_edits_and_refresh() {
    let (_directory, _url, database) = sqlite_database("mutable-current-view").await;
    let input = active_input("mutable-current-view", '3', '4');
    let created = database
        .import_cpa_managed_oauth_account(input.clone(), PEPPER)
        .await
        .unwrap();
    let updated = database
        .update_upstream_account(
            created.account.id,
            "mutable-current-view",
            UpdateUpstreamAccountInput {
                name: "Operator renamed".into(),
                config: json!({"base_url": "https://changed.example.test"}),
                expected_updated_at: created.account.updated_at,
            },
        )
        .await
        .unwrap();
    let disabled = database
        .set_upstream_account_status(
            updated.id,
            "mutable-current-view",
            "disabled",
            updated.updated_at,
        )
        .await
        .unwrap();
    database
        .begin_upstream_oauth_refresh(disabled.id, "mutable-view-refresh", PEPPER)
        .await
        .unwrap();
    let refreshed = database
        .finish_upstream_oauth_refresh(
            disabled.id,
            UpstreamCredential::OAuth {
                access_token: "refreshed-access".into(),
                refresh_token: Some("refreshed-token".into()),
                expires_at: Some(unix_millis() + 3_600_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: Some(json!({"generation": 2})),
            },
            "mutable-view-refresh",
            PEPPER,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.credential_generation, 2);
    assert_eq!(refreshed.status, "disabled");

    let replay = database
        .import_cpa_managed_oauth_account(input, PEPPER)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.account.id, created.account.id);
    assert_eq!(replay.account.name, "Operator renamed");
    assert_eq!(
        replay.account.config,
        json!({"base_url": "https://changed.example.test"})
    );
    assert_eq!(replay.account.status, "disabled");
    assert_eq!(replay.account.credential_generation, 2);
}

#[tokio::test]
async fn name_conflict_and_insert_faults_roll_back_mapping_and_credentials() {
    let (_directory, url, database) = sqlite_database("atomic-rollback").await;
    let first = active_input("managed-rollback", 'f', '1');
    database
        .import_cpa_managed_oauth_account(first, PEPPER)
        .await
        .unwrap();

    let mut name_conflict = active_input("managed-rollback", '2', '3');
    let source_key = name_conflict.source_key.clone();
    assert!(matches!(
        database
            .import_cpa_managed_oauth_account(name_conflict.clone(), PEPPER)
            .await,
        Err(AppError::Conflict(_))
    ));
    name_conflict.account_name = "Recovered name".into();
    database
        .import_cpa_managed_oauth_account(name_conflict, PEPPER)
        .await
        .expect("name conflict must not retain the mapping claim");

    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    sqlx::query(
        "CREATE TRIGGER fail_managed_account BEFORE INSERT ON upstream_accounts BEGIN SELECT RAISE(ABORT, 'account insert fault'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let account_fault = active_input("managed-account-fault", '4', '5');
    assert!(
        database
            .import_cpa_managed_oauth_account(account_fault.clone(), PEPPER)
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_managed_account")
        .execute(&pool)
        .await
        .unwrap();
    database
        .import_cpa_managed_oauth_account(account_fault, PEPPER)
        .await
        .expect("account fault must roll back the mapping claim");

    sqlx::query(
        "CREATE TRIGGER fail_managed_credential BEFORE INSERT ON upstream_credentials BEGIN SELECT RAISE(ABORT, 'credential insert fault'); END",
    )
    .execute(&pool)
    .await
    .unwrap();
    let credential_fault = active_input("managed-credential-fault", '6', '7');
    assert!(
        database
            .import_cpa_managed_oauth_account(credential_fault.clone(), PEPPER)
            .await
            .is_err()
    );
    sqlx::query("DROP TRIGGER fail_managed_credential")
        .execute(&pool)
        .await
        .unwrap();
    database
        .import_cpa_managed_oauth_account(credential_fault, PEPPER)
        .await
        .expect("credential fault must roll back account and mapping");

    let mapped: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM upstream_account_imports WHERE source_key = ?1")
            .bind(source_key)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(mapped, 1);
}

#[tokio::test]
async fn imported_disabled_account_cannot_be_deleted_and_refresh_preserves_disabled_status() {
    let (_directory, _url, database) = sqlite_database("disabled-refresh").await;
    let imported = database
        .import_cpa_managed_oauth_account(disabled_input("managed-disabled", '8', '9'), PEPPER)
        .await
        .unwrap();
    assert_eq!(imported.account.status, "disabled");
    assert!(imported.account.can_refresh);
    assert!(matches!(
        database
            .delete_upstream_account(
                imported.account.id,
                "managed-disabled",
                imported.account.updated_at
            )
            .await,
        Err(AppError::Conflict(_))
    ));

    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for index in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let account_id = imported.account.id;
        tasks.push(tokio::spawn(async move {
            let key = format!("disabled-refresh-{index}");
            barrier.wait().await;
            (
                key.clone(),
                database
                    .begin_upstream_oauth_refresh(account_id, &key, PEPPER)
                    .await,
            )
        }));
    }
    let mut winner = None;
    for task in tasks {
        let (key, result) = task.await.unwrap();
        match result {
            Ok(None) => winner = Some(key),
            Err(AppError::Conflict(_)) => {}
            other => panic!("unexpected disabled refresh claim: {other:?}"),
        }
    }
    let refreshed = database
        .finish_upstream_oauth_refresh(
            imported.account.id,
            UpstreamCredential::OAuth {
                access_token: "new-access".into(),
                refresh_token: Some("new-refresh".into()),
                expires_at: Some(unix_millis() + 3_600_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: Some(json!({"new": "state"})),
            },
            &winner.expect("one refresh winner"),
            PEPPER,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.status, "disabled");
    assert_eq!(refreshed.credential_generation, 2);
}

#[tokio::test]
async fn unexpired_disabled_replays_and_expired_unrefreshable_state_writes_nothing() {
    let (_directory, _url, database) = sqlite_database("admin-disabled").await;
    let input = administratively_disabled_input("managed-admin-disabled", 'b', 'd');
    let created = database
        .import_cpa_managed_oauth_account(input.clone(), PEPPER)
        .await
        .unwrap();
    assert_eq!(created.account.status, "disabled");
    let replay = database
        .import_cpa_managed_oauth_account(input, PEPPER)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.account.id, created.account.id);
    assert_eq!(replay.account.credential_generation, 1);

    let mut rejected = administratively_disabled_input("managed-no-refresh", 'e', '0');
    if let UpstreamCredential::OAuth {
        refresh_token,
        expires_at,
        adapter_state,
        ..
    } = &mut rejected.credential
    {
        *refresh_token = None;
        *expires_at = Some(unix_millis() - 1);
        *adapter_state = None;
    }
    assert!(matches!(
        database
            .import_cpa_managed_oauth_account(rejected, PEPPER)
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert!(
        database
            .list_upstream_accounts("managed-no-refresh")
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn refresh_capability_is_persisted_from_the_adapter_and_filters_worker_candidates() {
    let (_directory, _url, database) = sqlite_database("adapter-refresh-capability").await;
    let refresh_before = unix_millis() + 5 * 60 * 1_000;

    let mut codex = active_input("managed-capability", '1', '2');
    codex.account_name = "Refreshable Codex".into();
    if let UpstreamCredential::OAuth { expires_at, .. } = &mut codex.credential {
        *expires_at = Some(unix_millis() + 60_000);
    }
    let codex = database
        .import_cpa_managed_oauth_account(codex, PEPPER)
        .await
        .unwrap();
    assert!(codex.account.can_refresh);

    let mut legacy_gemini_input = active_input("managed-capability", '3', '4');
    legacy_gemini_input.account_name = "Non-refreshable legacy Gemini".into();
    legacy_gemini_input.adapter = ProviderCatalog::builtins()
        .managed_oauth_adapter_for_source("gemini-legacy")
        .unwrap();
    if let UpstreamCredential::OAuth { expires_at, .. } = &mut legacy_gemini_input.credential {
        *expires_at = Some(unix_millis() + 60_000);
    }
    let legacy_gemini = database
        .import_cpa_managed_oauth_account(legacy_gemini_input.clone(), PEPPER)
        .await
        .unwrap();
    assert!(!legacy_gemini.account.can_refresh);
    let replay = database
        .import_cpa_managed_oauth_account(legacy_gemini_input, PEPPER)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert!(!replay.account.can_refresh);

    let candidates = database
        .list_managed_oauth_refresh_candidates(refresh_before, 20)
        .await
        .unwrap();
    assert!(candidates.contains(&(codex.account.id, 1)));
    assert!(
        !candidates
            .iter()
            .any(|(id, _)| *id == legacy_gemini.account.id)
    );
}

#[tokio::test]
async fn adapter_state_is_encrypted_and_never_appears_in_debug_or_account_view() {
    let (_directory, url, database) = sqlite_database("adapter-state").await;
    let input = active_input("managed-adapter-state", 'a', 'c');
    assert!(!format!("{:?}", input.credential).contains("adapter-state-secret"));
    let imported = database
        .import_cpa_managed_oauth_account(input, PEPPER)
        .await
        .unwrap();
    assert!(
        !serde_json::to_string(&imported.account)
            .unwrap()
            .contains("adapter-state-secret")
    );

    let pool = sqlx::SqlitePool::connect(&url).await.unwrap();
    let row = sqlx::query(
        "SELECT credential_ciphertext FROM upstream_credentials WHERE upstream_account_id = ?1",
    )
    .bind(imported.account.id.to_string())
    .fetch_one(&pool)
    .await
    .unwrap();
    let ciphertext: String = row.try_get("credential_ciphertext").unwrap();
    assert!(!ciphertext.contains("adapter-state-secret"));
    assert!(!ciphertext.contains("managed-access-secret"));
    let (_, opened) = database
        .upstream_account_with_credential(imported.account.id, PEPPER)
        .await
        .unwrap();
    assert_eq!(
        opened.adapter_state().unwrap()["family"],
        "adapter-state-secret"
    );
}

async fn postgres_concurrent_import_case(database: Database, tenant: String, mixed: bool) {
    let barrier = Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for index in 0..16 {
        let database = database.clone();
        let barrier = barrier.clone();
        let mut input = active_input(&tenant, 'd', 'e');
        if mixed && index % 2 == 1 {
            input.payload_digest = "f".repeat(64);
        }
        tasks.push(tokio::spawn(async move {
            barrier.wait().await;
            database
                .import_cpa_managed_oauth_account(input, PEPPER)
                .await
        }));
    }
    let mut account_id = None;
    let mut successes = 0;
    let mut conflicts = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(result) => {
                successes += 1;
                assert_eq!(result.account.credential_generation, 1);
                assert!(account_id.is_none_or(|id| id == result.account.id));
                account_id = Some(result.account.id);
            }
            Err(AppError::Conflict(_)) if mixed => conflicts += 1,
            other => panic!("unexpected concurrent managed import result: {other:?}"),
        }
    }
    assert_eq!(
        database
            .list_upstream_accounts(&tenant)
            .await
            .unwrap()
            .len(),
        1
    );
    if mixed {
        assert_eq!(successes, 8);
        assert_eq!(conflicts, 8);
    } else {
        assert_eq!(successes, 16);
    }
}

#[tokio::test]
async fn sqlite_same_and_mixed_payload_imports_are_serialized() {
    let (_directory, _url, database) = sqlite_database("sqlite-concurrency").await;
    postgres_concurrent_import_case(database.clone(), "sqlite-same".into(), false).await;
    postgres_concurrent_import_case(database, "sqlite-mixed".into(), true).await;
}

#[tokio::test]
async fn postgres_same_and_mixed_payload_imports_are_serialized() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 24).await.unwrap();
    database.migrate().await.unwrap();
    let suffix = Uuid::now_v7();
    postgres_concurrent_import_case(database.clone(), format!("managed-pg-same-{suffix}"), false)
        .await;
    postgres_concurrent_import_case(database, format!("managed-pg-mixed-{suffix}"), true).await;

    let database = Database::connect_with_max(&database_url, 4).await.unwrap();
    let input = administratively_disabled_input(&format!("managed-pg-disabled-{suffix}"), '1', '2');
    let created = database
        .import_cpa_managed_oauth_account(input.clone(), PEPPER)
        .await
        .unwrap();
    let replay = database
        .import_cpa_managed_oauth_account(input, PEPPER)
        .await
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.account.id, created.account.id);
    assert_eq!(replay.account.status, "disabled");

    let barrier = Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = Vec::new();
    for index in 0..8 {
        let database = database.clone();
        let barrier = barrier.clone();
        let account_id = replay.account.id;
        tasks.push(tokio::spawn(async move {
            let key = format!("managed-pg-disabled-refresh-{suffix}-{index}");
            barrier.wait().await;
            (
                key.clone(),
                database
                    .begin_upstream_oauth_refresh(account_id, &key, PEPPER)
                    .await,
            )
        }));
    }
    let mut winner = None;
    let mut conflicts = 0;
    for task in tasks {
        let (key, result) = task.await.unwrap();
        match result {
            Ok(None) => winner = Some(key),
            Err(AppError::Conflict(_)) => conflicts += 1,
            other => panic!("unexpected PostgreSQL disabled refresh claim: {other:?}"),
        }
    }
    assert_eq!(conflicts, 7);
    let refreshed = database
        .finish_upstream_oauth_refresh(
            replay.account.id,
            UpstreamCredential::OAuth {
                access_token: "postgres-disabled-refreshed".into(),
                refresh_token: Some("postgres-disabled-refresh-state".into()),
                expires_at: Some(unix_millis() + 3_600_000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: Some(json!({"postgres": "state"})),
            },
            &winner.expect("one PostgreSQL disabled refresh winner"),
            PEPPER,
        )
        .await
        .unwrap();
    assert_eq!(refreshed.status, "disabled");
    assert_eq!(refreshed.credential_generation, 2);
}
