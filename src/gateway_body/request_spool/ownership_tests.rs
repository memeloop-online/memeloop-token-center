use std::{
    convert::Infallible,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{body::Body, extract::Request, http::header};
use bytes::Bytes;
use futures_util::stream;

use super::{REQUEST_SPOOL_CHUNK_BYTES, RequestSpoolAdmission};
use crate::gateway_body::{
    GatewayBodyAdmissionError, GatewayBodyRejectionReason, GatewayBodyRouteClass,
    admit_gateway_request_body_with_memory, memory::ProxyMemoryBudget,
};

const ROUTES: [&str; 2] = ["/v1/responses", "/v1/responses/compact"];
const TRANSPORT_SIZED_FRAMES: [usize; 2] = [16 * 1024, 64 * 1024];
const BUDGET_BYTES: u32 = 1024 * 1024;

#[test]
fn pause_hooks_do_not_enlarge_cloned_admission_state() {
    assert_eq!(
        std::mem::size_of::<RequestSpoolAdmission>(),
        std::mem::size_of::<[Arc<()>; 3]>()
    );
}

async fn admit(
    path: &str,
    body: Body,
    declared: Option<usize>,
    budget: &ProxyMemoryBudget,
    spool: &RequestSpoolAdmission,
) -> Result<Request, GatewayBodyAdmissionError> {
    let mut request = Request::post(path);
    if let Some(length) = declared {
        request = request.header(header::CONTENT_LENGTH, length);
    }
    admit_gateway_request_body_with_memory(
        request.body(body).unwrap(),
        Arc::new(tokio::sync::Semaphore::new(0)),
        Arc::new(tokio::sync::Semaphore::new(0)),
        BUDGET_BYTES as usize,
        crate::config::DEFAULT_AUDIO_BODY_MAX_BYTES as usize,
        Some(budget),
        spool,
    )
    .await
}

async fn metadata_charge(path: &str, declared: Option<usize>) -> usize {
    let directory = tempfile::tempdir().unwrap();
    let spool = RequestSpoolAdmission::new(directory.path().to_owned());
    let budget = ProxyMemoryBudget::new(BUDGET_BYTES);
    // Empty on purpose: this measures the reservation taken before any body
    // byte, not the bytes retained for the upload itself.
    let body = Body::from(Vec::<u8>::new());
    let admitted = admit(path, body, declared, &budget, &spool).await.unwrap();
    let charged = budget.snapshot().0;
    assert!(charged > REQUEST_SPOOL_CHUNK_BYTES);
    assert_eq!(charged % REQUEST_SPOOL_CHUNK_BYTES, 0);
    drop(admitted);
    assert_eq!(budget.snapshot().0, 0);
    assert_eq!(spool.snapshot().active_files, 0);
    charged
}

#[tokio::test]
async fn responses_metadata_one_unit_short_rejects_before_directory_or_body_poll() {
    for path in ROUTES {
        let metadata = metadata_charge(path, Some(1)).await;
        let directory = tempfile::tempdir().unwrap();
        let spool_path = directory.path().join("not-opened");
        let spool = RequestSpoolAdmission::new(spool_path.clone());
        let budget = ProxyMemoryBudget::new(BUDGET_BYTES);
        let held = budget.reservation();
        let held_bytes = BUDGET_BYTES as usize - metadata + REQUEST_SPOOL_CHUNK_BYTES;
        assert!(held.try_grow(held_bytes, 1));
        let polls = Arc::new(AtomicUsize::new(0));
        let observed = polls.clone();
        let body = Body::from_stream(stream::once(async move {
            observed.fetch_add(1, Ordering::AcqRel);
            Ok::<_, Infallible>(Bytes::from_static(b"x"))
        }));
        assert!(matches!(
            admit(path, body, Some(1), &budget, &spool).await,
            Err(GatewayBodyAdmissionError::CapacityExhausted)
        ));
        assert_eq!(polls.load(Ordering::Acquire), 0);
        assert!(!spool_path.exists());
        assert_eq!(spool.snapshot().active_files, 0);
        assert_eq!(spool.snapshot().used_bytes, 0);
        assert_eq!(budget.snapshot().0, held_bytes);
        drop(held);
        assert_eq!(budget.snapshot().0, 0);
    }
}

#[tokio::test]
async fn responses_transport_sized_first_frame_and_false_length_at_byte_boundary() {
    for path in ROUTES {
        for frame_bytes in TRANSPORT_SIZED_FRAMES {
            assert!(frame_bytes <= REQUEST_SPOOL_CHUNK_BYTES);
            for declared in [None, Some(1), Some(frame_bytes)] {
                let metadata = metadata_charge(path, declared).await;
                for frame_allowance in [0, REQUEST_SPOOL_CHUNK_BYTES] {
                    let directory = tempfile::tempdir().unwrap();
                    let spool = RequestSpoolAdmission::new(directory.path().to_owned());
                    let budget = Arc::new(ProxyMemoryBudget::new(BUDGET_BYTES));
                    let held = budget.reservation();
                    let held_bytes = BUDGET_BYTES as usize - metadata - frame_allowance;
                    assert!(held.try_grow(held_bytes, 1));
                    let observed = budget.clone();
                    let (polled, first_poll) = tokio::sync::oneshot::channel();
                    let body = Body::from_stream(stream::once(async move {
                        let _ = polled.send(observed.snapshot().0);
                        Ok::<_, Infallible>(Bytes::from(vec![b'x'; frame_bytes]))
                    }));
                    let active_budget = budget.clone();
                    let active_spool = spool.clone();
                    let capture = tokio::spawn(async move {
                        admit(path, body, declared, &active_budget, &active_spool).await
                    });
                    assert_eq!(first_poll.await.unwrap(), held_bytes + metadata);
                    if declared == Some(1) {
                        assert!(matches!(
                            capture.await.unwrap(),
                            Err(GatewayBodyAdmissionError::Rejected(rejection))
                                if rejection.route_class == GatewayBodyRouteClass::Responses
                                    && rejection.reason == GatewayBodyRejectionReason::BodyLimitExceeded
                        ));
                    } else if frame_allowance == 0 {
                        assert!(matches!(
                            capture.await.unwrap(),
                            Err(GatewayBodyAdmissionError::CapacityExhausted)
                        ));
                    } else {
                        let admitted = capture.await.unwrap().unwrap();
                        assert!(budget.snapshot().0 >= held_bytes + metadata + frame_bytes);
                        drop(admitted);
                    }
                    assert_eq!(budget.snapshot().0, held_bytes);
                    assert_eq!(spool.snapshot().used_bytes, 0);
                    assert_eq!(spool.snapshot().active_files, 0);
                    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
                    drop(held);
                    assert_eq!(budget.snapshot().0, 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn cancelled_capture_releases_memory_without_a_detached_write() {
    for path in ROUTES {
        for frame_bytes in TRANSPORT_SIZED_FRAMES {
            let metadata = metadata_charge(path, Some(frame_bytes)).await;
            let directory = tempfile::tempdir().unwrap();
            let spool = RequestSpoolAdmission::new(directory.path().to_owned());
            let budget = Arc::new(ProxyMemoryBudget::new(BUDGET_BYTES));
            let held = budget.reservation();
            let held_bytes = BUDGET_BYTES as usize - metadata - REQUEST_SPOOL_CHUNK_BYTES;
            assert!(held.try_grow(held_bytes, 1));
            let (polled, first_poll) = tokio::sync::oneshot::channel();
            let body = Body::from_stream(stream::once(async move {
                let _ = polled.send(());
                std::future::pending::<Result<Bytes, Infallible>>().await
            }));
            let active_budget = budget.clone();
            let active_spool = spool.clone();
            let capture = tokio::spawn(async move {
                admit(path, body, Some(frame_bytes), &active_budget, &active_spool).await
            });
            first_poll.await.unwrap();
            assert_eq!(spool.snapshot().active_files, 1);
            capture.abort();
            assert!(capture.await.unwrap_err().is_cancelled());
            assert_eq!(budget.snapshot().0, held_bytes);
            assert_eq!(spool.snapshot().used_bytes, 0);
            assert_eq!(spool.snapshot().active_files, 0);
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
            drop(held);
            assert_eq!(budget.snapshot().0, 0);
            let recovered = admit(path, Body::from("x"), Some(1), &budget, &spool)
                .await
                .unwrap();
            drop(recovered);
            assert_eq!(budget.snapshot().0, 0);
            assert_eq!(spool.snapshot().used_bytes, 0);
            assert_eq!(spool.snapshot().active_files, 0);
        }
    }
}

struct OwnedFrame {
    bytes: Vec<u8>,
    dropped: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for OwnedFrame {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for OwnedFrame {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::AcqRel);
    }
}

#[tokio::test]
async fn retained_body_copies_the_caller_frame_and_leaves_no_file() {
    let directory = tempfile::tempdir().unwrap();
    let spool = RequestSpoolAdmission::new(directory.path().to_owned());
    let dropped = Arc::new(AtomicUsize::new(0));
    let frame_bytes = REQUEST_SPOOL_CHUNK_BYTES * 2 + 17;
    let body = Body::from(Bytes::from_owner(OwnedFrame {
        bytes: vec![b'x'; frame_bytes],
        dropped: dropped.clone(),
    }));
    let budget = ProxyMemoryBudget::new(BUDGET_BYTES);
    let memory = budget.reservation();
    let captured = spool
        .capture_with_memory(body, frame_bytes, Some(frame_bytes), Some(&memory), 0, None)
        .await
        .unwrap();
    assert_eq!(captured.len(), frame_bytes);
    assert_eq!(dropped.load(Ordering::Acquire), 1);
    assert!(budget.snapshot().0 >= frame_bytes);
    assert_eq!(spool.snapshot().used_bytes, frame_bytes);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
    drop(captured);
    assert_eq!(budget.snapshot().0, 0);
    assert_eq!(spool.snapshot().used_bytes, 0);
    assert_eq!(spool.snapshot().active_files, 0);
}

#[tokio::test(start_paused = true)]
async fn slow_uploads_and_retained_spools_above_former_aggregate_limit_remain_independent() {
    const UPLOAD_BYTES: usize = 12 * 1024 * 1024;
    const UPLOADS: usize = 6;
    const FORMER_AGGREGATE_LIMIT: usize = 64 * 1024 * 1024;

    for declared in [true, false] {
        let directory = tempfile::tempdir().unwrap();
        let spool = RequestSpoolAdmission::new(directory.path().to_owned());
        let polls = Arc::new(AtomicUsize::new(0));
        let budget = Arc::new(ProxyMemoryBudget::new(128 * 1024 * 1024));
        let (started, mut starts) = tokio::sync::mpsc::channel(UPLOADS);
        let (resume, waiting) = tokio::sync::watch::channel(false);
        let mut captures = Vec::new();
        for index in 0..UPLOADS {
            let started = started.clone();
            let waiting = waiting.clone();
            let polls = polls.clone();
            let body =
                Body::from_stream(stream::unfold((0, waiting), move |(piece, mut waiting)| {
                    let started = started.clone();
                    let polls = polls.clone();
                    async move {
                        if piece == UPLOAD_BYTES / REQUEST_SPOOL_CHUNK_BYTES {
                            return None;
                        }
                        if piece == 0 {
                            polls.fetch_add(1, Ordering::AcqRel);
                            started.send(()).await.unwrap();
                            waiting.wait_for(|ready| *ready).await.unwrap();
                            tokio::time::sleep(Duration::from_secs(120)).await;
                        }
                        Some((
                            Ok::<_, Infallible>(Bytes::from(vec![b'x'; REQUEST_SPOOL_CHUNK_BYTES])),
                            (piece + 1, waiting),
                        ))
                    }
                }));
            let mut request = Request::post(ROUTES[index % ROUTES.len()]);
            if declared {
                request = request.header(header::CONTENT_LENGTH, UPLOAD_BYTES);
            }
            let active_spool = spool.clone();
            let active_budget = budget.clone();
            captures.push(tokio::spawn(async move {
                admit_gateway_request_body_with_memory(
                    request.body(body).unwrap(),
                    Arc::new(tokio::sync::Semaphore::new(0)),
                    Arc::new(tokio::sync::Semaphore::new(0)),
                    16 * 1024 * 1024,
                    crate::config::DEFAULT_AUDIO_BODY_MAX_BYTES as usize,
                    Some(&active_budget),
                    &active_spool,
                )
                .await
                .unwrap()
            }));
        }
        for _ in 0..UPLOADS {
            starts.recv().await.unwrap();
        }
        assert_eq!(polls.load(Ordering::Acquire), UPLOADS);
        assert_eq!(spool.snapshot().used_bytes, 0);
        assert_eq!(spool.snapshot().active_files, UPLOADS);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let small = admit(ROUTES[0], Body::from("{}"), Some(2), &budget, &spool)
            .await
            .unwrap();
        drop(small);
        resume.send(true).unwrap();
        let mut retained = Vec::new();
        for capture in captures {
            let admitted = capture.await.unwrap();
            let owner = admitted
                .extensions()
                .get::<Arc<super::RequestSpool>>()
                .unwrap()
                .clone();
            assert_eq!(owner.len(), UPLOAD_BYTES);
            retained.push(owner);
            drop(admitted);
        }
        assert_eq!(spool.snapshot().used_bytes, UPLOADS * UPLOAD_BYTES);
        assert!(spool.snapshot().used_bytes > FORMER_AGGREGATE_LIMIT);
        assert_eq!(spool.snapshot().active_files, UPLOADS);
        assert_eq!(spool.read_count_for_test(), 0);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
        let small = admit(ROUTES[1], Body::from("{}"), None, &budget, &spool)
            .await
            .unwrap();
        drop(small);
        assert_eq!(spool.snapshot().used_bytes, UPLOADS * UPLOAD_BYTES);
        drop(retained);
        assert_eq!(spool.snapshot().used_bytes, 0);
        assert_eq!(spool.snapshot().active_files, 0);
        assert_eq!(budget.snapshot().0, 0);
    }
}
