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
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let budget = budget.clone();
        let archive = archive.clone();
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
                },
            )
            .await
            .unwrap();
            assert_eq!(actual.as_slice(), expected.as_ref());
            assert_eq!(polls.load(Ordering::SeqCst), 1);
            drop(actual);
        }));
    }
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
