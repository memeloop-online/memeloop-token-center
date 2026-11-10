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
