use super::{routing::TrainingStore, tests::memory_store};
use bytes::Bytes;
use object_store::{ObjectStoreExt, PutPayload, memory::InMemory, path::Path};
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
    assert!(
        archive
            .source_object_exists(&legacy.object_locator)
            .await
            .unwrap()
    );
    assert!(
        archive
            .source_object_exists(&object.object_locator)
            .await
            .unwrap()
    );
    // Bare keys retain legacy meaning even when that key exists in training.
    assert!(!archive.source_object_exists(key).await.unwrap());
    let wrong_identity = object
        .object_locator
        .replacen(&"a".repeat(64), &"b".repeat(64), 1);
    assert!(archive.source_object_exists(&wrong_identity).await.is_err());
    assert!(
        archive
            .source_object_exists("stores/training/")
            .await
            .is_err()
    );
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
        !archive
            .source_object_exists(&object.object_locator)
            .await
            .unwrap()
    );
    assert!(archive.source_object_exists(key).await.unwrap());
    assert!(
        archive
            .get_tenant_content(tenant, &object, 100)
            .await
            .is_err()
    );
    archive.training = None;
    assert!(
        archive
            .source_object_exists(&object.object_locator)
            .await
            .is_err()
    );
    assert!(archive.source_object_exists(key).await.unwrap());
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

#[tokio::test]
#[ignore = "requires isolated RustFS fixture"]
async fn training_cas_real_s3_configuration_and_reader() {
    let mut config = crate::config::Config::for_test("sqlite::memory:".to_owned());
    let required = |name| std::env::var(name).expect("RustFS fixture configuration");
    config.training_s3 = Some(crate::config::TrainingS3Config {
        endpoint: required("MTC_RUSTFS_TEST_ENDPOINT"),
        bucket: required("MTC_RUSTFS_TEST_BUCKET"),
        region: "us-east-1".to_owned(),
        access_key: required("MTC_RUSTFS_TEST_ACCESS_KEY"),
        secret_key: required("MTC_RUSTFS_TEST_SECRET_KEY"),
        allow_http: true,
    });
    config.training_cas_writes = true;
    let archive = super::ArchiveStore::from_config(&config).await.unwrap();
    let tenant = Uuid::now_v7();
    let bytes = Bytes::from_static(b"training fixture plaintext");
    let reference = archive
        .put_tenant_content(tenant, bytes.clone())
        .await
        .unwrap();
    assert!(reference.object_locator.starts_with("stores/training/"));
    let reader = super::ArchiveStore::from_config(&config).await.unwrap();
    assert_eq!(
        reader
            .get_tenant_content(tenant, &reference, 100)
            .await
            .unwrap(),
        bytes
    );
    // Exercise actual S3 multipart and copy-if-not-exists on the configured
    // training backend; staged ownership remains fixed if defaults change.
    let mut staged_config = config.clone();
    staged_config.training_staging_writes = true;
    let staged_archive = super::ArchiveStore::from_config(&staged_config)
        .await
        .unwrap();
    let key = crate::archive_staging::ArchiveStagingKey::new(
        crate::archive_staging::ArchiveStagingOwner::ProxyRequest(Uuid::now_v7()),
        crate::archive_staging::ArchiveStagingPurpose::Request,
        Uuid::now_v7(),
    )
    .unwrap();
    let key = staged_archive.select_staging_key(key).unwrap();
    let mut writer = staged_archive
        .start_compressed_writer(&format!("{}/body", key.canonical_prefix()))
        .await
        .unwrap();
    writer.write(bytes.clone()).await.unwrap();
    let staged = writer.finish_staged().await.unwrap();
    let cas = reader
        .promote_staged_text_to_cas(tenant, &staged)
        .await
        .unwrap();
    assert_eq!(
        reader.get_tenant_content(tenant, &cas, 100).await.unwrap(),
        bytes
    );
    reader.delete_archive_staging_segment(key).await.unwrap();
    assert!(reader.archive_staging_segment_is_empty(key).await.unwrap());
    assert_eq!(
        reader.get_tenant_content(tenant, &cas, 100).await.unwrap(),
        bytes
    );
    let mut changed = config.clone();
    changed
        .training_s3
        .as_mut()
        .unwrap()
        .bucket
        .push_str("-changed");
    let wrong = super::ArchiveStore::from_config(&changed).await.unwrap();
    assert!(wrong.head_size(&reference.object_locator).await.is_err());
}

#[tokio::test]
async fn typed_staging_keeps_durable_backend_through_writer_promotion_and_cleanup() {
    use crate::archive_staging::{ArchiveStagingKey, ArchiveStagingOwner, ArchiveStagingPurpose};
    let mut archive = super::ArchiveStore::training_staging_memory_for_test();
    let bare = ArchiveStagingKey::new(
        ArchiveStagingOwner::ProxyRequest(Uuid::now_v7()),
        ArchiveStagingPurpose::Request,
        Uuid::now_v7(),
    )
    .unwrap();
    let key = archive.select_staging_key(bare).unwrap();
    let location = format!("{}/body", key.canonical_prefix());
    let legacy = format!("{}/body", bare.canonical_prefix());
    archive
        .put(&legacy, Bytes::from_static(b"legacy twin"))
        .await
        .unwrap();
    let mut writer = archive.start_compressed_writer(&location).await.unwrap();
    // Switching defaults after multipart creation cannot move its object.
    archive.training_staging_writes = false;
    archive.training_cas_writes = false;
    writer
        .write(Bytes::from_static(b"training staged plaintext"))
        .await
        .unwrap();
    let staged = writer.finish_staged().await.unwrap();
    assert_eq!(staged.object_locator, format!("{location}.mtcz1"));
    assert_eq!(
        archive.get(&staged.object_locator).await.unwrap(),
        "training staged plaintext"
    );
    let cas = archive
        .promote_staged_text_to_cas(Uuid::now_v7(), &staged)
        .await
        .unwrap();
    assert!(cas.object_locator.starts_with("stores/training/"));
    assert!(
        archive
            .delete_prefix(&key.canonical_prefix())
            .await
            .is_err()
    );
    assert!(archive.delete(&staged.object_locator).await.is_err());
    archive.delete_archive_staging_segment(key).await.unwrap();
    assert!(archive.archive_staging_segment_is_empty(key).await.unwrap());
    assert_eq!(archive.get(&legacy).await.unwrap(), "legacy twin");
    assert_eq!(
        archive.get(&cas.object_locator).await.unwrap(),
        "training staged plaintext"
    );

    // The inverse cleanup must not delete a training twin either.
    archive
        .put(&location, Bytes::from_static(b"training twin"))
        .await
        .unwrap();
    archive.delete_archive_staging_segment(bare).await.unwrap();
    assert_eq!(archive.get(&location).await.unwrap(), "training twin");
    archive.training = None;
    assert!(archive.get(&location).await.is_err());
    assert!(archive.delete_archive_staging_segment(key).await.is_err());
}

#[tokio::test]
async fn staging_identity_and_typed_segments_are_fail_closed() {
    use crate::archive_staging::{ArchiveStagingKey, ArchiveStagingOwner, ArchiveStagingPurpose};
    let archive = super::ArchiveStore::training_staging_memory_for_test();
    let key = ArchiveStagingKey::new(
        ArchiveStagingOwner::GenerationJob(Uuid::now_v7()),
        ArchiveStagingPurpose::Assets,
        Uuid::now_v7(),
    )
    .unwrap();
    let training_key = archive.select_staging_key(key).unwrap();
    let locator = format!("{}/asset", training_key.canonical_prefix());
    let wrong = locator.replacen(&"a".repeat(64), &"b".repeat(64), 1);
    assert!(archive.start_writer(&wrong).await.is_err());
    assert!(archive.put(&wrong, Bytes::new()).await.is_err());
    for malformed in [
        "stores/staging/",
        "stores/staging/no-hash/staging/proxy/x/request/y",
        "stores/staging/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/tenants/x",
    ] {
        assert!(archive.start_writer(malformed).await.is_err());
    }
    let mut writer = archive.start_writer(&locator).await.unwrap();
    writer
        .write(Bytes::from_static(b"must keep qualified locator"))
        .await
        .unwrap();
    // Generic finish would publish a bare location on the wrong backend.
    assert!(writer.finish().await.is_err());
}
