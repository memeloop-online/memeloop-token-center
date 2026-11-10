use bytes::Bytes;
use futures_util::StreamExt;
use uuid::Uuid;

use super::{ArchiveStore, CasArchiveObject};
use crate::config::{ArchiveBackend, Config};

#[tokio::test]
#[ignore = "requires an isolated RustFS TEST bucket; run only in the dedicated GHA fixture"]
async fn rustfs_multipart_cas_promotion_and_restart_reconstruction() {
    let mut config = Config::for_test("sqlite::memory:".into());
    config.archive_backend = ArchiveBackend::S3;
    config.s3_endpoint = Some(std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap());
    config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
    config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
    config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
    config.s3_region = "us-east-1".into();
    config.s3_allow_http = true;
    let store = ArchiveStore::from_config(&config).await.unwrap();
    let tenant = Uuid::parse_str("00000000-0000-4000-8000-000000000707").unwrap();
    let request = Uuid::parse_str("00000000-0000-4000-8000-000000000708").unwrap();
    let chunk = Bytes::from(vec![b'x'; 64 * 1024]);
    let chunk_count = 1280;
    let mut digest = blake3::Hasher::new();
    for _ in 0..chunk_count {
        digest.update(&chunk);
    }
    let digest = digest.finalize().to_hex().to_string();
    let expected = CasArchiveObject {
        object_locator: format!("tenants/{tenant}/cas/v1/blake3/{}/{digest}", &digest[..2]),
        blake3_digest: digest,
        size_bytes: chunk.len() as u64 * chunk_count,
    };
    if std::env::var("MTC_RUSTFS_RESTORE_ONLY").as_deref() != Ok("1") {
        let staging = format!("migration-staging/rustfs-contract/{}", Uuid::now_v7());
        let mut writer = store.start_writer(&staging).await.unwrap();
        for _ in 0..chunk_count {
            writer.write(chunk.clone()).await.unwrap();
        }
        let staged = writer.finish_staged().await.unwrap();
        let object = store
            .promote_staged_text_to_cas(tenant, &staged)
            .await
            .unwrap();
        assert_eq!(object, expected);
        assert_eq!(
            store
                .promote_staged_text_to_cas(tenant, &staged)
                .await
                .unwrap(),
            expected
        );
        store
            .publish_raw_capture(tenant, request, "response", &object)
            .await
            .unwrap();
        store
            .publish_raw_capture(tenant, request, "response", &object)
            .await
            .unwrap();
        store.delete(&staged.object_locator).await.unwrap();
    }
    assert_eq!(
        store
            .raw_capture_reference(tenant, request, "response")
            .await
            .unwrap(),
        Some(expected.clone())
    );
    store
        .verify_tenant_content(tenant, &expected)
        .await
        .unwrap();
    let mut restored = store
        .open_tenant_content(tenant, &expected)
        .await
        .unwrap()
        .stream;
    let mut bytes = 0_u64;
    while let Some(chunk) = restored.next().await {
        bytes += chunk.unwrap().len() as u64;
    }
    assert_eq!(bytes, expected.size_bytes);
    let mut range = store
        .open_stream(&expected.object_locator, Some(65530..65550))
        .await
        .unwrap()
        .stream;
    let mut ranged_bytes = Vec::new();
    while let Some(chunk) = range.next().await {
        ranged_bytes.extend_from_slice(&chunk.unwrap());
    }
    assert_eq!(ranged_bytes, vec![b'x'; 20]);
    assert!(store.delete(&expected.object_locator).await.is_err());
}
