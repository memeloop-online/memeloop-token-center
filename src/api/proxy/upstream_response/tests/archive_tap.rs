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
                // Queue credit is archival; carrier memory remains separately accounted.
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
                let input =
                    stream::once(async move { Ok(first) }).chain(stream::once(async move {
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
                release_fence.send(()).unwrap();
            }
        }
    }
}
