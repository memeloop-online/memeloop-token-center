use super::*;
use crate::{
    config::{ArchiveBackend, TrainingS3Config},
    gateway_body::memory::ProxyMemoryBudget,
};
use std::{sync::Arc, time::Duration};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

// Both configured S3 endpoints are real HTTP mocks, never a memory store.
// A failed capture is distinct from delivery: no raw root may be published.
#[tokio::test]
async fn slow_or_failed_dual_object_stores_do_not_own_customer_eof() {
    for slow_primary in [true, false] {
        for training in [false, true] {
            for sse in [false, true] {
                let primary = Arc::new(MockServer::start().await);
                let secondary = Arc::new(MockServer::start().await);
                let response = if slow_primary {
                    ResponseTemplate::new(503).set_delay(Duration::from_secs(60))
                } else {
                    ResponseTemplate::new(503)
                };
                Mock::given(any())
                    .respond_with(response)
                    .mount(&primary)
                    .await;
                Mock::given(any())
                    .respond_with(ResponseTemplate::new(503))
                    .mount(&secondary)
                    .await;
                let (_dir, mut state, _pool, identity) =
                    crate::response_archive_spool::tests::fixture().await;
                let mut config = (*state.config).clone();
                config.archive_backend = ArchiveBackend::S3;
                config.s3_endpoint = Some(primary.uri());
                config.s3_bucket = Some("tap-contract".into());
                config.s3_access_key = Some("synthetic".into());
                config.s3_secret_key = Some("synthetic".into());
                config.s3_allow_http = true;
                config.training_s3 = Some(TrainingS3Config {
                    endpoint: secondary.uri(),
                    bucket: "tap-contract".into(),
                    region: "us-east-1".into(),
                    access_key: "synthetic".into(),
                    secret_key: "synthetic".into(),
                    allow_http: true,
                });
                config.training_staging_writes = training;
                config.training_cas_writes = training;
                state.archive = crate::archive::ArchiveStore::from_config(&config)
                    .await
                    .unwrap();
                state.db = state.db.clone().with_archive(state.archive.clone());
                state.persistence_db = state
                    .persistence_db
                    .clone()
                    .with_archive(state.archive.clone());
                // Deliberately small byte budgets exercise spill and truthful overflow.
                // Queue and carrier transfer the same archival credit; delivery is separate.
                state.proxy_memory_budget = Arc::new(ProxyMemoryBudget::new(64 * 1024));
                let mut persistence = crate::api::proxy::persistence::Persistence::default();
                persistence.stream_memory = ProxyMemoryBudget::new(256 * 1024);
                state.persistence = Arc::new(persistence);
                let (probe, fenced, release_fence) =
                    crate::response_archive_spool::fence_probe::install(&state);
                let selected = if training {
                    secondary.clone()
                } else {
                    primary.clone()
                };
                let producer =
                    crate::response_archive_spool::ResponseArchiveProducer::begin_original(
                        &state, identity,
                    )
                    .await
                    .unwrap();
                let first = Bytes::from(vec![b'x'; 128 * 1024]);
                let rest = if sse {
                    Bytes::from(format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        "y".repeat(512 * 1024)
                    ))
                } else {
                    Bytes::from(vec![b'y'; 512 * 1024])
                };
                let expected = [first.clone(), rest.clone()].concat();
                let input = stream::iter(vec![
                    Ok(first.slice(..64 * 1024)),
                    Ok(first.slice(64 * 1024..)),
                ])
                .chain(stream::once(async move {
                    tokio::time::timeout(Duration::from_secs(5), async {
                        while selected.received_requests().await.unwrap().is_empty() {
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .expect("actual selected S3 endpoint must receive a spill operation");
                    Ok(rest)
                }));
                let mut customer = CapturedUpstream {
                    upstream: Some(Box::pin(input)),
                    capture: Some(producer),
                    admission: None,
                    pending: None,
                };
                let actual = tokio::time::timeout(Duration::from_secs(2), async {
                    let mut bytes = Vec::new();
                    while let Some(chunk) = customer.next().await {
                        bytes.extend_from_slice(&chunk.unwrap());
                    }
                    bytes
                })
                .await
                .expect("archival HTTP must not delay customer EOF");
                assert_eq!(actual, expected);
                tokio::time::timeout(Duration::from_secs(5), fenced)
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(
                    probe.calls(),
                    1,
                    "overflow must invoke the real failure fence"
                );
                let root_suffix = format!("/raw/v1/{}/response.json", identity.request_id);
                for server in [&primary, &secondary] {
                    assert!(
                        !server.received_requests().await.unwrap().iter().any(|r| r
                            .method
                            .as_str()
                            == "PUT"
                            && r.url.path().ends_with(&root_suffix)),
                        "an incomplete prefix cannot publish a raw root"
                    );
                }
                assert_eq!(
                    state.persistence.stream_memory.snapshot().0,
                    0,
                    "abandoned input/current receipt must return all archival credit"
                );
                assert_eq!(
                    state.proxy_memory_budget.snapshot().0,
                    0,
                    "archive tap must not charge delivery"
                );
                release_fence.send(()).unwrap();
            }
        }
    }
}

#[tokio::test]
async fn exhausted_upload_workspace_and_failed_s3_do_not_block_small_original_eof() {
    let primary = MockServer::start().await;
    let secondary = MockServer::start().await;
    for server in [&primary, &secondary] {
        Mock::given(any())
            .respond_with(ResponseTemplate::new(503))
            .mount(server)
            .await;
    }
    let (_dir, mut state, _pool, identity) = crate::response_archive_spool::tests::fixture().await;
    let mut config = (*state.config).clone();
    config.archive_backend = ArchiveBackend::S3;
    config.s3_endpoint = Some(primary.uri());
    config.s3_bucket = Some("workspace-contract".into());
    config.s3_access_key = Some("synthetic".into());
    config.s3_secret_key = Some("synthetic".into());
    config.s3_allow_http = true;
    config.training_s3 = Some(TrainingS3Config {
        endpoint: secondary.uri(),
        bucket: "workspace-contract".into(),
        region: "us-east-1".into(),
        access_key: "synthetic".into(),
        secret_key: "synthetic".into(),
        allow_http: true,
    });
    config.training_cas_writes = true;
    config.training_staging_writes = true;
    state.archive = crate::archive::ArchiveStore::from_config(&config)
        .await
        .unwrap();
    let occupied = state
        .persistence
        .upload_workspace
        .temporary(state.persistence.upload_workspace.snapshot().1)
        .unwrap();
    let producer =
        crate::response_archive_spool::ResponseArchiveProducer::begin_original(&state, identity)
            .await
            .unwrap();
    let chunks = vec![
        Bytes::from_static(b"data: first\n\n"),
        Bytes::from(vec![b'x'; 128 * 1024]),
        Bytes::from_static(b"\n\ndata: [DONE]\n\n"),
    ];
    let expected = chunks.concat();
    let mut customer = CapturedUpstream {
        upstream: Some(Box::pin(stream::iter(chunks.into_iter().map(Ok)))),
        capture: Some(producer),
        admission: None,
        pending: None,
    };
    let actual = tokio::time::timeout(Duration::from_secs(1), async {
        let mut out = Vec::new();
        while let Some(chunk) = customer.next().await {
            out.extend_from_slice(&chunk.unwrap());
        }
        out
    })
    .await
    .expect("workspace exhaustion cannot own customer EOF");
    assert_eq!(actual, expected);
    assert_eq!(state.proxy_memory_budget.snapshot().0, 0);
    assert!(state.persistence.stream_memory.snapshot().0 > 0);
    assert_eq!(
        state.persistence.upload_workspace.snapshot().0,
        crate::response_archive_spool::RAW_UPLOAD_WORKSPACE_BYTES
    );
    // Keep source ownership explicit while the original background path retries.
    drop(occupied);
}
