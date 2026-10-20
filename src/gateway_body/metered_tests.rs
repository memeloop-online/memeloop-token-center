use super::*;
use std::convert::Infallible;

const TEXT_ROUTES: [&str; 5] = [
    "/v1/chat/completions",
    "/v1/alpha/search",
    "/v1/embeddings",
    "/v1/messages",
    "/v1/messages/count_tokens",
];

#[tokio::test]
async fn responses_empty_uploads_account_before_first_chunk_and_cancel() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
    let budget = Arc::new(memory::ProxyMemoryBudget::new(64 * 1024 * 1024));
    let mut tasks = Vec::new();
    for index in 0..65 {
        let (started, polled) = tokio::sync::oneshot::channel();
        let body = Body::from_stream(futures_util::stream::once(async move {
            let _ = started.send(());
            std::future::pending::<Result<bytes::Bytes, Infallible>>().await
        }));
        let path = if index % 2 == 0 {
            "/v1/responses"
        } else {
            "/v1/responses/compact"
        };
        let request = Request::post(path).body(body).unwrap();
        let active_budget = budget.clone();
        let active_spool = spool.clone();
        tasks.push(tokio::spawn(async move {
            admit(request, Some(&active_budget), &active_spool).await
        }));
        polled.await.unwrap();
        assert_eq!(spool.snapshot().active_files, index + 1);
        assert!(budget.snapshot().0 >= (index + 1) * 64 * 1024);
        assert_eq!(spool.snapshot().used_bytes, 0);
    }
    for task in tasks {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    assert_eq!(budget.snapshot().0, 0);
    assert_eq!(spool.snapshot().active_files, 0);
}

#[tokio::test]
async fn responses_full_memory_rejects_before_poll_or_spool_open() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
    let budget = memory::ProxyMemoryBudget::new(1024 * 1024);
    let held = budget.reservation();
    assert!(held.try_grow(1024 * 1024, 1));
    for path in ["/v1/responses", "/v1/responses/compact"] {
        let polled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = polled.clone();
        let body = Body::from_stream(futures_util::stream::once(async move {
            observed.store(true, Ordering::Release);
            Ok::<_, Infallible>(bytes::Bytes::from_static(b"{}"))
        }));
        assert!(matches!(
            admit(
                Request::post(path).body(body).unwrap(),
                Some(&budget),
                &spool
            )
            .await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
        assert!(!polled.load(Ordering::Acquire));
        assert_eq!(spool.snapshot().active_files, 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    }
    drop(held);
    assert_eq!(budget.snapshot().0, 0);
}

#[tokio::test]
async fn responses_frame_budget_and_spool_owner_release_independently() {
    let directory = tempfile::tempdir().unwrap();
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
    for path in ["/v1/responses", "/v1/responses/compact"] {
        let budget = memory::ProxyMemoryBudget::new(128 * 1024);
        assert!(matches!(
            admit(
                Request::post(path)
                    .body(Body::from(vec![b'x'; 256 * 1024]))
                    .unwrap(),
                Some(&budget),
                &spool
            )
            .await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(spool.snapshot().used_bytes, 0);
        assert_eq!(spool.snapshot().active_files, 0);
        let budget = memory::ProxyMemoryBudget::new(1024 * 1024);
        let admitted = admit(
            Request::post(path).body(Body::from("{}")).unwrap(),
            Some(&budget),
            &spool,
        )
        .await
        .unwrap();
        let owner = admitted
            .extensions()
            .get::<Arc<request_spool::RequestSpool>>()
            .unwrap()
            .clone();
        let reservation = admitted
            .extensions()
            .get::<Arc<memory::ProxyMemoryReservation>>()
            .unwrap()
            .clone();
        let metadata_bytes = budget.snapshot().0;
        drop(admitted);
        reservation.release_rejected_request();
        assert_eq!(budget.snapshot().0, metadata_bytes);
        assert_eq!(owner.read_all().await.unwrap(), "{}");
        drop(owner);
        assert_eq!(budget.snapshot().0, 0);
        assert_eq!(spool.snapshot().active_files, 0);
    }
}

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
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
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
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
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
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
    let budget = memory::ProxyMemoryBudget::new(128 * 1024 * 1024);
    for path in [
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
    let spool = request_spool::RequestSpoolAdmission::new(directory.path().to_owned());
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
