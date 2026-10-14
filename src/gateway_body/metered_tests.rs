use super::*;
use std::convert::Infallible;

const TEXT_ROUTES: [&str; 5] = [
    "/v1/chat/completions",
    "/v1/alpha/search",
    "/v1/embeddings",
    "/v1/messages",
    "/v1/messages/count_tokens",
];

async fn admit(
    request: Request,
    budget: Option<&memory::ProxyMemoryBudget>,
    spool: &request_spool::RequestSpoolAdmission,
) -> Result<Request, GatewayBodyAdmissionError> {
    admit_gateway_request_body_with_memory(
        request,
        Arc::new(tokio::sync::Semaphore::new(0)),
        Arc::new(tokio::sync::Semaphore::new(0)),
        16 * 1024 * 1024,
        crate::config::DEFAULT_AUDIO_BODY_MAX_BYTES as usize,
        budget,
        spool,
    )
    .await
}

#[tokio::test]
async fn metered_text_reads_bypass_count_and_refund_unused_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned(), 1024 * 1024);
    let budget = memory::ProxyMemoryBudget::new(16 * 1024 * 1024);
    for path in TEXT_ROUTES {
        let request = Request::post(path)
            .header(header::CONTENT_LENGTH, "2")
            .body(Body::from("{}"))
            .unwrap();
        let admitted = admit(request, Some(&budget), &spool).await.unwrap();
        assert!(
            admitted
                .extensions()
                .get::<Arc<memory::ProxyMemoryReservation>>()
                .is_some()
        );
        let used = budget.snapshot().0;
        assert!(used >= 2 * memory::REQUEST_MEMORY_WEIGHT);
        assert!(used < MAX_DEFAULT_BODY * memory::REQUEST_MEMORY_WEIGHT);
        assert_eq!(
            to_bytes(admitted.into_body(), MAX_DEFAULT_BODY)
                .await
                .unwrap(),
            "{}"
        );
        assert_eq!(budget.snapshot().0, 0);
    }
    assert_eq!(spool.snapshot().active_files, 0);
}

#[tokio::test]
async fn metered_text_full_byte_budget_rejects_before_body_poll() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned(), 1024 * 1024);
    let budget = memory::ProxyMemoryBudget::new(16 * 1024 * 1024);
    let held = budget.reservation();
    assert!(held.try_grow(16 * 1024 * 1024, 1));
    for path in TEXT_ROUTES {
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = polled.clone();
        let body = Body::from_stream(futures_util::stream::once(async move {
            observed.store(true, Ordering::Release);
            Ok::<_, Infallible>(bytes::Bytes::from_static(b"{}"))
        }));
        let request = Request::post(path).body(body).unwrap();
        assert!(matches!(
            admit(request, Some(&budget), &spool).await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
        assert!(!polled.load(Ordering::Acquire));
        assert_eq!(budget.snapshot().0, 16 * 1024 * 1024);
    }
    drop(held);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn readers_without_complete_byte_admission_keep_the_count_gate() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned(), 1024 * 1024);
    let budget = memory::ProxyMemoryBudget::new(128 * 1024 * 1024);
    for path in [
        "/v1/responses",
        "/v1/responses/compact",
        "/v1/images/generations",
        "/v1/audio/transcriptions",
        "/v1/generations",
        "/v1/videos/generations",
        "/self/unclassified",
    ] {
        assert!(matches!(
            admit(
                Request::post(path).body(Body::empty()).unwrap(),
                Some(&budget),
                &spool
            )
            .await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(spool.snapshot().active_files, 0);
    }
    for path in TEXT_ROUTES {
        assert!(matches!(
            admit(
                Request::post(path).body(Body::empty()).unwrap(),
                None,
                &spool
            )
            .await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
    }
}

#[tokio::test]
async fn metered_text_length_and_chunk_rejections_release_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned(), 1024 * 1024);
    let budget = memory::ProxyMemoryBudget::new(16 * 1024 * 1024);
    for path in TEXT_ROUTES {
        for request in [
            Request::post(path)
                .header(header::CONTENT_LENGTH, (MAX_DEFAULT_BODY + 1).to_string())
                .body(Body::empty())
                .unwrap(),
            Request::post(path)
                .header(header::CONTENT_LENGTH, "1")
                .body(Body::from("{}"))
                .unwrap(),
            Request::post(path)
                .body(Body::from_stream(futures_util::stream::once(async {
                    Ok::<_, Infallible>(bytes::Bytes::from(vec![b'x'; MAX_DEFAULT_BODY + 1]))
                })))
                .unwrap(),
        ] {
            assert!(matches!(
                admit(request, Some(&budget), &spool).await,
                Err(GatewayBodyAdmissionError::Rejected(_))
            ));
            assert_eq!(budget.snapshot().0, 0);
        }
    }
}
