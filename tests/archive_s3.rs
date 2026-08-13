use std::{env, time::Duration};

use axum::{body::to_bytes, response::IntoResponse};
use bytes::Bytes;
use memeloop_token_center::{
    archive::ArchiveStore,
    config::{ArchiveBackend, Config},
};
use serde_json::Value;
use uuid::Uuid;

fn s3_config() -> Option<Config> {
    let endpoint = match env::var("MTC_TEST_S3_ENDPOINT") {
        Ok(endpoint) => endpoint,
        Err(_) => {
            eprintln!("MTC_TEST_S3_ENDPOINT is unset; skipping MinIO integration assertion");
            return None;
        }
    };

    let mut config = Config::for_test("sqlite::memory:".to_owned());
    config.archive_backend = ArchiveBackend::S3;
    config.s3_bucket = Some(
        env::var("MTC_TEST_S3_BUCKET").unwrap_or_else(|_| "memeloop-token-center-test".to_owned()),
    );
    config.s3_endpoint = Some(endpoint);
    config.s3_region = "us-east-1".to_owned();
    config.s3_access_key =
        Some(env::var("MTC_TEST_S3_ACCESS_KEY").unwrap_or_else(|_| "token-center-test".to_owned()));
    config.s3_secret_key = Some(
        env::var("MTC_TEST_S3_SECRET_KEY")
            .unwrap_or_else(|_| "token-center-test-secret".to_owned()),
    );
    config.s3_allow_http = true;
    Some(config)
}

#[tokio::test]
async fn minio_put_get_list_missing_and_read_limit() {
    let Some(config) = s3_config() else {
        return;
    };
    let store = ArchiveStore::from_config(&config)
        .await
        .expect("construct MinIO archive store");

    // readiness_check exercises a signed ListObjectsV2 call, including bucket access.
    store.readiness_check().await.expect("list MinIO bucket");
    store
        .delete_prefix("tenants/test-tenant")
        .await
        .expect("remove fixtures left by an interrupted earlier test");

    let unique = Uuid::now_v7();
    let location = format!("tenants/test-tenant/objects/{unique}.bin");
    let body = Bytes::from(format!("archive integration payload {unique}"));
    store
        .put(&location, body.clone())
        .await
        .expect("put tenant object");
    assert_eq!(store.get(&location).await.expect("get tenant object"), body);

    let missing = format!("tenants/test-tenant/objects/missing-{unique}.bin");
    assert!(
        store.get(&missing).await.is_err(),
        "missing object must fail"
    );

    let oversized = Bytes::from(vec![0x5a; 32 * 1024]);
    let oversized_location = format!("tenants/test-tenant/objects/oversized-{unique}.bin");
    store
        .put(&oversized_location, oversized)
        .await
        .expect("put oversized fixture");
    let error = store
        .get_bounded(&oversized_location, 1024)
        .await
        .expect_err("bounded read must reject an oversized object");
    assert!(error.to_string().contains("read limit"));

    // Unsafe tenant/object input must be rejected locally, before it can become an S3 key.
    let unsafe_location = format!("tenants/{unique}/../another-tenant/object.bin");
    let error = store
        .put(&unsafe_location, Bytes::from_static(b"unsafe"))
        .await
        .expect_err("traversal-like object location must be rejected");
    assert!(!error.to_string().contains(&unsafe_location));

    store
        .delete_prefix("tenants/test-tenant")
        .await
        .expect("remove tenant integration fixtures");
}

#[tokio::test]
async fn minio_multipart_writer_publishes_only_the_content_address() {
    let Some(config) = s3_config() else {
        return;
    };
    let store = ArchiveStore::from_config(&config)
        .await
        .expect("construct MinIO archive store");
    let staging = format!("staging/{}/response.bin", Uuid::now_v7());
    let mut writer = store.start_writer(&staging).await.expect("start multipart");
    writer
        .write(Bytes::from_static(b"multipart "))
        .await
        .expect("write first part");
    writer
        .write(Bytes::from_static(b"archive"))
        .await
        .expect("write second part");

    let location = writer.finish().await.expect("publish content address");
    assert!(location.starts_with("objects/blake3/"));
    assert_eq!(
        store.get(&location).await.expect("read published object"),
        Bytes::from_static(b"multipart archive")
    );
    assert!(store.get(&staging).await.is_err(), "staging object removed");
    store
        .delete(&location)
        .await
        .expect("remove multipart test object");
}

#[tokio::test]
async fn minio_outage_is_bounded_and_http_error_does_not_leak_storage_details() {
    if env::var("MTC_TEST_S3_ENDPOINT").is_err() {
        eprintln!("MTC_TEST_S3_ENDPOINT is unset; skipping MinIO outage assertion");
        return;
    }

    let mut config = s3_config().expect("S3 test config");
    config.s3_endpoint = Some(
        env::var("MTC_TEST_S3_OUTAGE_ENDPOINT").unwrap_or_else(|_| "http://127.0.0.1:1".to_owned()),
    );
    config.s3_access_key = Some("must-not-leak-access-key".to_owned());
    config.s3_secret_key = Some("must-not-leak-secret-key".to_owned());
    let store = ArchiveStore::from_config(&config)
        .await
        .expect("construct unavailable archive store");

    let error = tokio::time::timeout(Duration::from_secs(20), store.readiness_check())
        .await
        .expect("S3 outage must be reported within the readiness deadline")
        .expect_err("unavailable S3 endpoint must fail");
    let response = error.into_response();
    assert_eq!(
        response.status(),
        axum::http::StatusCode::INTERNAL_SERVER_ERROR
    );
    let body = to_bytes(response.into_body(), 16 * 1024)
        .await
        .expect("read sanitized error body");
    let payload: Value = serde_json::from_slice(&body).expect("JSON error response");
    assert_eq!(payload["error"]["code"], "internal_error");
    assert_eq!(payload["error"]["message"], "internal error");
    let body = String::from_utf8_lossy(&body);
    assert!(!body.contains("127.0.0.1"));
    assert!(!body.contains("must-not-leak"));
}
