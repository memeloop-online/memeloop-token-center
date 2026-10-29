use super::*;
use crate::{
    config::Config,
    db::{ArchiveSpoolIdentity, Database},
    gateway_body::memory::ProxyMemoryBudget,
};
use std::time::Duration;

#[tokio::test]
async fn archive_source_survives_request_drop_without_pinning_upload_bytes_on_database() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("source.db").display()
    );
    let database = Database::connect_with_max(&database_url, 4).await.unwrap();
    database.migrate().await.unwrap();
    let blocker = sqlx::AnyPool::connect(&database_url).await.unwrap();
    let held = blocker.begin_with("BEGIN IMMEDIATE").await.unwrap();
    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = ProxyMemoryBudget::new(512 * 1024);
    let memory = budget.reservation();
    let request_id = uuid::Uuid::now_v7();
    let body = Bytes::from(vec![b'x'; 128 * 1024]);
    let source = Arc::new(
        admission
            .capture_with_memory(
                Body::from(body.clone()),
                body.len(),
                Some(body.len()),
                Some(&memory),
                0,
                Some(&request_id.to_string()),
            )
            .await
            .unwrap(),
    );
    admission.register_source(request_id, &source);
    let raw = source.read_all().await.unwrap();
    let shared = source.shared_bytes(raw.clone());
    assert_eq!(raw.as_ptr(), shared.as_ptr());
    assert!(Arc::ptr_eq(&source, &admission.source(request_id).unwrap()));
    let locator = source.object_locator().unwrap().to_owned();
    source.retain_for_archive(
        database.clone(),
        ArchiveSpoolIdentity {
            request_id,
            tenant_id: uuid::Uuid::now_v7(),
            reservation_id: uuid::Uuid::now_v7(),
        },
    );
    drop(shared);
    drop(raw);
    source.release_retained();
    drop(memory);
    assert!(budget.snapshot().0 >= body.len());
    release.send(()).unwrap();
    assert!(
        tokio::time::timeout(Duration::from_secs(1), source.upload_outcome())
            .await
            .unwrap()
    );
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), source.read_all())
            .await
            .unwrap()
            .unwrap(),
        body
    );
    assert_eq!(budget.snapshot().0, 0);
    drop(source);
    assert!(admission.source(request_id).is_none());
    assert_eq!(store.objects.lock().unwrap().get(&locator), Some(&body));
    held.rollback().await.unwrap();
    blocker.close().await;
    database.close().await;
}

#[tokio::test]
async fn pending_sources_share_one_budget_and_later_upload_spills_without_a_queue_limit() {
    let directory = tempfile::tempdir().unwrap();
    let store = MemoryObjectStore::new();
    let release = store.pause_next_write();
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = ProxyMemoryBudget::new(192 * 1024);
    let memory = budget.reservation();
    let first = Arc::new(
        admission
            .capture_with_memory(
                Body::from(vec![b'a'; 128 * 1024]),
                128 * 1024,
                None,
                Some(&memory),
                0,
                Some(&uuid::Uuid::now_v7().to_string()),
            )
            .await
            .unwrap(),
    );
    tokio::time::timeout(Duration::from_secs(1), async {
        while store.entered_writes.load(Ordering::Acquire) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let second = admission
        .capture_with_memory(
            Body::from(vec![b'b'; 256 * 1024]),
            256 * 1024,
            None,
            Some(&memory),
            0,
            Some(&uuid::Uuid::now_v7().to_string()),
        )
        .await
        .unwrap();
    assert_eq!(
        second.read_all().await.unwrap(),
        Bytes::from(vec![b'b'; 256 * 1024])
    );
    assert!(budget.snapshot().0 <= budget.snapshot().1);
    assert_eq!(admission.snapshot().used_bytes, 128 * 1024);
    release.send(()).unwrap();
    first.upload_outcome().await;
    drop(first);
    drop(second);
    drop(memory);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn failed_source_retries_the_original_bytes_without_releasing_their_budget() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("failure.db").display()
    );
    let database = Database::connect_with_max(&database_url, 4).await.unwrap();
    database.migrate().await.unwrap();
    let held = database.saturate_gateway_persistence_for_test().await;
    let store = MemoryObjectStore::new();
    store.fail_writes.store(true, Ordering::Release);
    let admission =
        RequestSpoolAdmission::new(directory.path().to_owned()).with_memory_store(store.clone());
    let budget = ProxyMemoryBudget::new(512 * 1024);
    let memory = budget.reservation();
    let request_id = uuid::Uuid::now_v7();
    let source = Arc::new(
        admission
            .capture_with_memory(
                Body::from(vec![b'x'; 128 * 1024]),
                128 * 1024,
                None,
                Some(&memory),
                0,
                Some(&request_id.to_string()),
            )
            .await
            .unwrap(),
    );
    let pepper = b"source-failure-real-admission-test-pepper";
    let issued = database
        .create_key(
            crate::db::CreateKeyInput {
                tenant_external_id: request_id.to_string(),
                principal_external_id: "member".into(),
                alias: "failed-source".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let price = database
        .upsert_model_price(
            "source",
            "USD",
            rust_decimal::Decimal::ONE,
            rust_decimal::Decimal::ONE,
        )
        .await
        .unwrap();
    database
        .start_proxy_request_with_source(
            crate::db::StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 1,
                output_token_ceiling: 1,
                protocol: "openai",
                model: "source",
                request_object: source.object_locator().unwrap(),
                upstream_account_id: None,
                model_route_id: None,
            },
            &source,
            None,
            Some(1),
        )
        .await
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(300), source.upload_outcome())
            .await
            .is_err()
    );
    source.release_retained();
    drop(source);
    drop(memory);
    assert!(budget.snapshot().0 >= 128 * 1024);
    drop(held);
    store.fail_writes.store(false, Ordering::Release);
    tokio::time::timeout(Duration::from_secs(3), async {
        while budget.snapshot().0 != 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    database.drain_gateway_persistence_for_test().await;
    database.close().await;
}

#[tokio::test]
async fn archive_source_keys_match_the_existing_typed_cleanup_prefix() {
    let request_id = uuid::Uuid::now_v7();
    let key = crate::archive_staging::ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(request_id),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        request_id,
    )
    .unwrap();
    let locator = request_body_object_key(&request_id.to_string()).unwrap();
    assert_eq!(locator, format!("{}/body.json", key.canonical_prefix()));
    assert_eq!(source_request_id(&locator), Some(request_id));
    assert!(source_request_id(&format!("{}/other.json", key.canonical_prefix())).is_none());
    let config = Config::for_test("sqlite::memory:".into());
    let archive = crate::archive::ArchiveStore::from_config(&config)
        .await
        .unwrap();
    archive
        .put(&locator, Bytes::from_static(b"{}"))
        .await
        .unwrap();
    assert_eq!(
        archive.get(&locator).await.unwrap(),
        Bytes::from_static(b"{}")
    );
}
