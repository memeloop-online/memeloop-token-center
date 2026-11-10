use super::{routing::TrainingStore, tests::memory_store};
use bytes::Bytes;
use object_store::{ObjectStore, ObjectStoreExt, PutPayload, memory::InMemory, path::Path};
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn qualified_cas_reads_preserve_backend_tenant_and_stream_integrity() {
    let mut archive = memory_store();
    let tenant = Uuid::now_v7();
    let legacy = archive
        .put_tenant_content(tenant, Bytes::from_static(b"legacy"))
        .await
        .unwrap();
    let training = Arc::new(InMemory::new());
    archive.training = Some(TrainingStore {
        store: training.clone(),
        identity: "a".repeat(64),
    });
    archive.training_cas_writes = true;
    let object = archive
        .put_tenant_content(tenant, Bytes::from_static(b"training body"))
        .await
        .unwrap();
    assert!(object.object_locator.starts_with("stores/training/"));
    assert_eq!(
        archive
            .get_tenant_content(tenant, &legacy, 100)
            .await
            .unwrap(),
        "legacy"
    );
    assert_eq!(
        archive
            .get_tenant_content(tenant, &object, 100)
            .await
            .unwrap(),
        "training body"
    );
    assert!(
        archive
            .get_tenant_content(Uuid::now_v7(), &object, 100)
            .await
            .is_err()
    );
    assert_eq!(archive.head_size(&object.object_locator).await.unwrap(), 13);
    assert!(archive.delete(&object.object_locator).await.is_err());
    assert!(archive.delete_prefix(&object.object_locator).await.is_err());
    assert!(archive.start_writer(&object.object_locator).await.is_err());
    let (_, key) = super::routing::split(&object.object_locator)
        .unwrap()
        .unwrap();
    // A same-key legacy object never substitutes for a missing training object.
    archive
        .inner
        .put(
            &Path::from(key),
            PutPayload::from_bytes(Bytes::from_static(b"training body")),
        )
        .await
        .unwrap();
    training.delete(&Path::from(key)).await.unwrap();
    assert!(
        archive
            .get_tenant_content(tenant, &object, 100)
            .await
            .is_err()
    );
    archive.training = None;
    assert!(archive.head_size(&object.object_locator).await.is_err());
    assert_eq!(
        archive
            .get_tenant_content(tenant, &legacy, 100)
            .await
            .unwrap(),
        "legacy"
    );
}

#[tokio::test]
async fn staging_remains_legacy_after_direct_cas_write_switch() {
    let mut archive = memory_store();
    archive.training = Some(TrainingStore {
        store: Arc::new(InMemory::new()),
        identity: "a".repeat(64),
    });
    archive.training_cas_writes = true;
    let mut writer = archive.start_writer("staging/legacy-body").await.unwrap();
    writer.write(Bytes::from_static(b"staged")).await.unwrap();
    let staged = writer.finish_staged().await.unwrap();
    let cas = archive
        .promote_staged_text_to_cas(Uuid::now_v7(), &staged)
        .await
        .unwrap();
    assert!(!cas.object_locator.starts_with("stores/"));
    assert_eq!(archive.get(&staged.object_locator).await.unwrap(), "staged");
}

#[test]
fn malformed_qualified_locations_are_rejected() {
    for key in [
        "stores/training/",
        "stores/training/no-hash/tenants/x",
        "stores/training/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/",
        "stores/training/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/stores/training/x",
    ] {
        assert!(super::routing::split(key).is_err(), "{key}");
    }
}

#[tokio::test]
async fn qualified_compressed_reads_and_cache_keys_keep_training_identity() {
    use futures_util::TryStreamExt;
    let mut archive = memory_store();
    let training = Arc::new(InMemory::new());
    let identity = "a".repeat(64);
    archive.training = Some(TrainingStore {
        store: training.clone(),
        identity: identity.clone(),
    });
    let tenant = Uuid::now_v7();
    let mut writer = archive
        .start_compressed_writer("staging/compressed-old")
        .await
        .unwrap();
    writer
        .write(Bytes::from_static(b"compressed training content"))
        .await
        .unwrap();
    let staged = writer.finish_staged().await.unwrap();
    let key = super::path::tenant_cas_location(tenant, &staged.blake3_digest, true).unwrap();
    let wire = archive
        .inner
        .get(&Path::from(staged.object_locator.as_str()))
        .await
        .unwrap()
        .bytes()
        .await
        .unwrap();
    training
        .put(&Path::from(key.as_str()), PutPayload::from_bytes(wire))
        .await
        .unwrap();
    let object = super::CasArchiveObject {
        object_locator: format!("stores/training/{identity}/{key}"),
        blake3_digest: staged.blake3_digest,
        size_bytes: staged.size_bytes,
    };
    assert_eq!(
        archive
            .get_tenant_content(tenant, &object, 100)
            .await
            .unwrap(),
        "compressed training content"
    );
    let range = archive
        .open_stream(&object.object_locator, Some(0..10))
        .await
        .unwrap();
    let chunks: Vec<Bytes> = range.stream.try_collect().await.unwrap();
    assert_eq!(chunks.concat(), b"compressed");
    let cache = memory_store();
    let key = cache
        .cache_original(&archive, tenant, &object)
        .await
        .unwrap();
    assert!(key.ends_with(&format!("/training-{identity}")));
    archive.training.as_mut().unwrap().identity = "b".repeat(64);
    assert!(archive.head_size(&object.object_locator).await.is_err());
}
