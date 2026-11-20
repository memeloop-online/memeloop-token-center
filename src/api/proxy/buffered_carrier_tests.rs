use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn response(body: Bytes, polls: Arc<AtomicUsize>) -> UpstreamResponse {
    UpstreamResponse::Prefetched {
        status: StatusCode::OK,
        headers: HeaderMap::new(),
        version: http::Version::HTTP_11,
        content_length: None,
        stream: Box::pin(
            futures_util::stream::iter(
                body.chunks(16 * 1024)
                    .map(Bytes::copy_from_slice)
                    .collect::<Vec<_>>(),
            )
            .map(move |chunk| {
                polls.fetch_add(1, Ordering::SeqCst);
                Ok(chunk)
            }),
        ),
    }
}

#[tokio::test]
async fn unknown_length_small_responses_share_actual_budget_and_preserve_bytes() {
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        4 * 1024 * 1024,
    ));
    let archive = crate::archive::ArchiveStore::memory_for_test();
    let ready = Arc::new(tokio::sync::Barrier::new(33));
    let release = Arc::new(tokio::sync::Barrier::new(33));
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let budget = budget.clone();
        let archive = archive.clone();
        let ready = ready.clone();
        let release = release.clone();
        tasks.push(tokio::spawn(async move {
            let memory = budget.reservation();
            let polls = Arc::new(AtomicUsize::new(0));
            let expected = Bytes::from_static(b"{\"result\":\"complete\"}");
            let actual = read_captured(
                response(expected.clone(), polls.clone()),
                64 * 1024 * 1024,
                ResponseCaptureContext {
                    memory: &memory,
                    budget,
                    archive,
                    request_id: Uuid::now_v7(),
                    conversation: None,
                    wait: Duration::from_secs(10),
                    conversion_ready: None,
                    adapter_envelope: 0,
                },
            )
            .await
            .unwrap();
            assert_eq!(actual.as_slice(), expected.as_ref());
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            ready.wait().await;
            release.wait().await;
            drop(actual);
        }));
    }
    ready.wait().await;
    assert!(budget.snapshot().0 > 0);
    release.wait().await;
    for task in tasks {
        task.await.unwrap();
    }
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn object_capture_replays_large_body_under_pressure_with_one_upstream_pass() {
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        2 * 1024 * 1024,
    ));
    let archive = crate::archive::ArchiveStore::memory_for_test();
    let memory = budget.reservation();
    let expected = Bytes::from(format!("{{\"output\":\"{}\"}}", "x".repeat(256 * 1024)));
    let polls = Arc::new(AtomicUsize::new(0));
    let actual = read_captured(
        response(expected.clone(), polls.clone()),
        64 * 1024 * 1024,
        ResponseCaptureContext {
            memory: &memory,
            budget: budget.clone(),
            archive,
            request_id: Uuid::now_v7(),
            conversation: None,
            wait: Duration::from_secs(10),
            conversion_ready: None,
            adapter_envelope: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(actual.as_slice(), expected.as_ref());
    assert_eq!(
        polls.load(Ordering::SeqCst),
        expected.len().div_ceil(16 * 1024)
    );
    drop(actual);
    drop(memory);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn cancelled_conversion_wait_releases_capture_without_repolling_upstream() {
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        2 * 1024 * 1024,
    ));
    let blocker = budget.reservation();
    assert!(blocker.try_grow(1536 * 1024, 1));
    let polls = Arc::new(AtomicUsize::new(0));
    let observed = polls.clone();
    let task_budget = budget.clone();
    let conversion_ready = Arc::new(tokio::sync::Notify::new());
    let observed_ready = conversion_ready.clone();
    let task = tokio::spawn(async move {
        let memory = task_budget.reservation();
        read_captured(
            response(
                Bytes::from(format!("{{\"output\":\"{}\"}}", "x".repeat(256 * 1024))),
                observed,
            ),
            64 * 1024 * 1024,
            ResponseCaptureContext {
                memory: &memory,
                budget: task_budget,
                archive: crate::archive::ArchiveStore::memory_for_test(),
                request_id: Uuid::now_v7(),
                conversation: None,
                wait: Duration::from_secs(60),
                conversion_ready: Some(observed_ready),
                adapter_envelope: 0,
            },
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(10), conversion_ready.notified())
        .await
        .unwrap();
    assert!(!task.is_finished());
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(polls.load(Ordering::SeqCst), 17);
    assert_eq!(budget.snapshot().0, 1536 * 1024);
    drop(blocker);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn conversion_projection_and_component_envelope_outlive_request_owner() {
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        4 * 1024 * 1024,
    ));
    let memory = budget.reservation();
    let projection = memory
        .reserve_buffered_shape_with_projection(
            128 * 1024,
            10,
            0,
            64 * 1024,
            2,
            tokio::time::Instant::now() + Duration::from_secs(1),
        )
        .await
        .unwrap();
    let charged = budget.snapshot().0;
    assert!(charged >= 512 * 1024);
    let input_memory = memory.take_response_envelope().unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let task = tokio::task::spawn_blocking(move || {
        started.send(()).unwrap();
        wait.recv().unwrap();
        drop(input_memory);
    });
    ready.await.unwrap();
    drop(memory);
    assert_eq!(budget.snapshot().0, charged);
    drop(projection);
    assert!(budget.snapshot().0 > 0);
    release.send(()).unwrap();
    task.await.unwrap();
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn response_and_conversation_projection_share_one_conversion_acquisition() {
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        2 * 1024 * 1024,
    ));
    let memory = budget.reservation();
    let conversation = ProxyConversation {
        key: crate::model::AuthenticatedKey {
            key_id: Uuid::nil(),
            tenant_id: Uuid::nil(),
            principal_id: Uuid::nil(),
            account_id: Uuid::nil(),
            alias: "test".to_owned(),
            currency: "USD".to_owned(),
            credential_generation: 0,
            policy: KeyPolicy::default(),
        },
        request_body: ConversationBody::InMemory(Bytes::from_static(
            b"{\"model\":\"test\",\"input\":\"hello\"}",
        )),
        hints: crate::conversation::ConversationHints::default(),
        client_name: None,
        projection_admission: std::sync::Mutex::new(ConversationProjectionAdmission::Deferred),
    };
    let expected = Bytes::from_static(b"{\"output\":\"complete\"}");
    let actual = read_captured(
        response(expected.clone(), Arc::new(AtomicUsize::new(0))),
        64 * 1024 * 1024,
        ResponseCaptureContext {
            memory: &memory,
            budget: budget.clone(),
            archive: crate::archive::ArchiveStore::memory_for_test(),
            request_id: Uuid::now_v7(),
            conversation: Some(&conversation),
            wait: Duration::from_secs(10),
            conversion_ready: None,
            adapter_envelope: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(actual.as_slice(), expected.as_ref());
    assert!(matches!(
        &*conversation.projection_admission.lock().unwrap(),
        ConversationProjectionAdmission::Reserved(_)
    ));
    drop(actual);
    drop(memory);
    assert!(budget.snapshot().0 > 0);
    drop(conversation);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
#[ignore = "dedicated GHA RustFS fixture"]
async fn rustfs_buffered_response_preserves_complete_original_and_releases_ram() {
    let mut config = crate::config::Config::for_test("sqlite::memory:".into());
    config.archive_backend = crate::config::ArchiveBackend::S3;
    config.s3_endpoint = Some(std::env::var("MTC_RUSTFS_TEST_ENDPOINT").unwrap());
    config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
    config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
    config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
    config.s3_region = "us-east-1".into();
    config.s3_allow_http = true;
    let archive = crate::archive::ArchiveStore::from_config(&config)
        .await
        .unwrap();
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        2 * 1024 * 1024,
    ));
    let memory = budget.reservation();
    let expected = Bytes::from(format!(
        "{{\"output\":\"{}\"}}",
        "real-object".repeat(20_000)
    ));
    let polls = Arc::new(AtomicUsize::new(0));
    let actual = read_captured(
        response(expected.clone(), polls.clone()),
        64 * 1024 * 1024,
        ResponseCaptureContext {
            memory: &memory,
            budget: budget.clone(),
            archive,
            request_id: Uuid::now_v7(),
            conversation: None,
            wait: Duration::from_secs(30),
            conversion_ready: None,
            adapter_envelope: 0,
        },
    )
    .await
    .unwrap();
    assert_eq!(actual.as_slice(), expected.as_ref());
    assert_eq!(
        polls.load(Ordering::SeqCst),
        expected.len().div_ceil(16 * 1024)
    );
    drop(actual);
    drop(memory);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn small_component_input_retains_declared_output_envelope() {
    let budget = Arc::new(crate::gateway_body::memory::ProxyMemoryBudget::new(
        2 * 1024 * 1024,
    ));
    let memory = budget.reservation();
    let body = read_captured(
        response(Bytes::from_static(b"{}"), Arc::new(AtomicUsize::new(0))),
        256 * 1024,
        ResponseCaptureContext {
            memory: &memory,
            budget: budget.clone(),
            archive: crate::archive::ArchiveStore::memory_for_test(),
            request_id: Uuid::now_v7(),
            conversation: None,
            wait: Duration::from_secs(10),
            conversion_ready: None,
            adapter_envelope: 256 * 1024,
        },
    )
    .await
    .unwrap();
    assert!(memory.buffered_response_reserved_bytes() >= 3 * 256 * 1024);
    let envelope = memory.take_response_envelope().unwrap();
    let (release, wait) = std::sync::mpsc::channel();
    let (started, ready) = tokio::sync::oneshot::channel();
    let guest = tokio::task::spawn_blocking(move || {
        let output = vec![b'x'; 256 * 1024];
        assert_eq!(body, b"{}");
        started.send(()).unwrap();
        wait.recv().unwrap();
        (output, envelope)
    });
    ready.await.unwrap();
    drop(memory);
    assert!(budget.snapshot().0 >= 3 * 256 * 1024);
    release.send(()).unwrap();
    let (output, envelope) = guest.await.unwrap();
    assert_eq!(output.len(), 256 * 1024);
    drop(output);
    drop(envelope);
    assert_eq!(budget.snapshot().0, 0);
}

#[test]
fn object_store_failure_is_local_health_evidence_with_distinct_code() {
    let code = BoundedUpstreamError::ObjectStore.code();
    assert_eq!(code, "upstream_response_object_store");
    assert!(super::super::buffered_upstream::is_local_response_boundary(
        code
    ));
    assert!(matches!(
        upstream_error_health_terminal(code),
        UpstreamAttemptTerminal::Inconclusive
    ));
    assert_eq!(
        BoundedUpstreamError::from_stream_error("upstream_read_timeout").code(),
        "upstream_read_timeout"
    );
}
