use bytes::Bytes;
use uuid::Uuid;

use super::tests::memory_store;

#[tokio::test]
async fn durable_raw_roots_are_idempotent_and_reject_changed_bytes() {
    let store = memory_store();
    let tenant = Uuid::now_v7();
    let request = Uuid::now_v7();
    let original = store
        .put_tenant_content(tenant, Bytes::from_static(b"data: complete\n\n"))
        .await
        .unwrap();
    assert!(
        store
            .raw_capture_reference(tenant, request, "response")
            .await
            .unwrap()
            .is_none()
    );
    store
        .publish_raw_capture_failure(tenant, request, "response")
        .await
        .unwrap();
    assert!(
        store
            .raw_capture_failed(tenant, request, "response")
            .await
            .unwrap()
    );
    assert!(
        !store
            .raw_capture_failed(Uuid::now_v7(), request, "response")
            .await
            .unwrap()
    );
    store
        .publish_raw_capture(tenant, request, "response", &original)
        .await
        .unwrap();
    store
        .publish_raw_capture(tenant, request, "response", &original)
        .await
        .unwrap();
    let changed = store
        .put_tenant_content(tenant, Bytes::from_static(b"data: changed\n\n"))
        .await
        .unwrap();
    assert!(
        store
            .publish_raw_capture(tenant, request, "response", &changed)
            .await
            .is_err()
    );
    assert!(
        store
            .publish_raw_capture(Uuid::now_v7(), request, "response", &original)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .raw_capture_reference(tenant, request, "response")
            .await
            .unwrap(),
        Some(original)
    );
    let root = format!("tenants/{tenant}/raw/v1/{request}/response.json");
    assert!(store.delete(&root).await.is_err());
    assert!(store.put(&root, Bytes::from_static(b"{}")).await.is_err());
    assert!(store.start_writer(&root).await.is_err());
    assert!(store.delete_prefix("tenants").await.is_err());
    assert!(
        store
            .delete_prefix(&format!("tenants/{tenant}/raw"))
            .await
            .is_err()
    );
}

#[tokio::test]
async fn relocated_raw_root_replays_and_binds_published_body_without_retired_copy() {
    use super::routing::TrainingStore;
    use object_store::{ObjectStoreExt, memory::InMemory, path::Path};
    use std::sync::Arc;

    let mut archive = memory_store();
    let tenant = Uuid::now_v7();
    let request = Uuid::now_v7();
    let body = Bytes::from_static(br#"{"original":"complete raw body"}"#);
    let legacy = archive
        .put_tenant_content(tenant, body.clone())
        .await
        .unwrap();
    let training = Arc::new(InMemory::new());
    archive.training = Some(TrainingStore {
        store: training.clone(),
        identity: "a".repeat(64),
    });
    archive.training_cas_writes = true;
    let published = archive.put_tenant_content(tenant, body).await.unwrap();
    archive
        .publish_raw_capture(tenant, request, "request", &published)
        .await
        .unwrap();
    archive
        .inner
        .delete(&Path::from(legacy.object_locator.as_str()))
        .await
        .unwrap();
    assert!(archive.open_tenant_content(tenant, &legacy).await.is_err());
    archive
        .publish_raw_capture(tenant, request, "request", &legacy)
        .await
        .unwrap();
    assert_eq!(
        archive
            .raw_capture_reference(tenant, request, "request")
            .await
            .unwrap(),
        Some(published.clone())
    );

    let directory = tempfile::tempdir().unwrap();
    let url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("raw-relocation.db").display()
    );
    let state = crate::AppState::initialize(crate::config::Config::for_test(url.clone()))
        .await
        .unwrap();
    let pool = sqlx::AnyPool::connect(&url).await.unwrap();
    sqlx::query("INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, response_object, reservation_id) VALUES ($1, $2, $3, 1, 'openai', 'test', 0, 0, 0, 'gap://' || $1 || '/request', 'gap://' || $1 || '/response', $4)")
        .bind(request.to_string()).bind(tenant.to_string()).bind(Uuid::now_v7().to_string()).bind(Uuid::now_v7().to_string()).execute(&pool).await.unwrap();
    let db = state.db.clone().with_archive(archive.clone());
    assert!(
        db.bind_raw_capture(tenant, request, "request", &legacy)
            .await
            .unwrap()
    );
    let actual: String =
        sqlx::query_scalar("SELECT request_object FROM request_records WHERE id=$1")
            .bind(request.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(actual, published.object_locator);

    let mut changed = legacy.clone();
    changed.size_bytes += 1;
    assert!(
        archive
            .publish_raw_capture(tenant, request, "request", &changed)
            .await
            .is_err()
    );
    assert!(
        db.bind_raw_capture(tenant, request, "request", &changed)
            .await
            .is_err()
    );
    assert!(
        archive
            .publish_raw_capture(Uuid::now_v7(), request, "request", &legacy)
            .await
            .is_err()
    );
    let mut wrong_backend = published.clone();
    wrong_backend.object_locator =
        wrong_backend
            .object_locator
            .replacen(&"a".repeat(64), &"b".repeat(64), 1);
    assert!(
        archive
            .publish_raw_capture(tenant, request, "request", &wrong_backend)
            .await
            .is_err()
    );
    let (_, key) = super::routing::split(&published.object_locator)
        .unwrap()
        .unwrap();
    training.delete(&Path::from(key)).await.unwrap();
    assert!(
        archive
            .publish_raw_capture(tenant, request, "request", &legacy)
            .await
            .is_err()
    );
    assert!(
        db.bind_raw_capture(tenant, request, "request", &legacy)
            .await
            .is_err()
    );
    assert_eq!(
        archive
            .raw_capture_reference(tenant, request, "request")
            .await
            .unwrap(),
        Some(published)
    );
}
