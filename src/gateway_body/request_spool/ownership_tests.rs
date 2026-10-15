use std::{
    convert::Infallible,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use axum::{body::Body, extract::Request, http::header};
use bytes::Bytes;
use futures_util::stream;
use tokio::{
    sync::Notify,
    task::{AbortHandle, JoinHandle},
};

use super::{REQUEST_SPOOL_CHUNK_BYTES, RequestSpoolAdmission};
use crate::gateway_body::{
    GatewayBodyAdmissionError, GatewayBodyRejectionReason, GatewayBodyRouteClass,
    admit_gateway_request_body_with_memory, memory::ProxyMemoryBudget,
};

const ROUTES: [&str; 2] = ["/v1/responses", "/v1/responses/compact"];
const TRANSPORT_SIZED_FRAMES: [usize; 2] = [16 * 1024, 64 * 1024];
const BUDGET_BYTES: u32 = 1024 * 1024;

#[derive(Default)]
pub(super) struct BlockingPause {
    entered: Notify,
    registered: Notify,
    released: Mutex<bool>,
    resume: Condvar,
    task: Mutex<Option<AbortHandle>>,
}

impl BlockingPause {
    pub(super) fn block(&self) {
        self.entered.notify_one();
        let mut released = self.released.lock().unwrap();
        while !*released {
            released = self.resume.wait(released).unwrap();
        }
    }

    pub(super) fn record<T>(&self, task: &JoinHandle<T>) {
        *self.task.lock().unwrap() = Some(task.abort_handle());
        self.registered.notify_one();
    }

    async fn wait_started(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            self.entered.notified().await;
            self.registered.notified().await;
        })
        .await
        .expect("blocking task enters its deterministic pause");
    }

    fn is_finished(&self) -> bool {
        self.task
            .lock()
            .unwrap()
            .as_ref()
            .expect("registered blocking task")
            .is_finished()
    }

    async fn wait_finished(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            while !self.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("released blocking task actually finishes");
    }
}

struct ResumeOnDrop(Arc<BlockingPause>);

impl Drop for ResumeOnDrop {
    fn drop(&mut self) {
        *self.0.released.lock().unwrap() = true;
        self.0.resume.notify_all();
    }
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
    let spool = RequestSpoolAdmission::new(directory.path().to_owned(), BUDGET_BYTES as usize);
    let budget = ProxyMemoryBudget::new(BUDGET_BYTES);
    let body = Body::from(vec![b'x'; declared.unwrap_or(0)]);
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
        let spool = RequestSpoolAdmission::new(spool_path.clone(), BUDGET_BYTES as usize);
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
                    let pause = Arc::new(BlockingPause::default());
                    let resume = ResumeOnDrop(pause.clone());
                    let mut spool = RequestSpoolAdmission::new(
                        directory.path().to_owned(),
                        BUDGET_BYTES as usize,
                    );
                    spool.blocking_write = Some(pause.clone());
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
                                    && rejection.reason == GatewayBodyRejectionReason::BodyReadRejected
                        ));
                    } else if frame_allowance == 0 {
                        assert!(matches!(
                            capture.await.unwrap(),
                            Err(GatewayBodyAdmissionError::CapacityExhausted)
                        ));
                    } else {
                        pause.wait_started().await;
                        assert!(!pause.is_finished());
                        assert_eq!(budget.snapshot().0, BUDGET_BYTES as usize);
                        assert_eq!(spool.snapshot().used_bytes, frame_bytes);
                        drop(resume);
                        let admitted = capture.await.unwrap().unwrap();
                        assert_eq!(budget.snapshot().0, held_bytes + metadata);
                        drop(admitted);
                    }
                    assert_eq!(budget.snapshot().0, held_bytes);
                    assert_eq!(spool.snapshot().used_bytes, 0);
                    assert_eq!(spool.snapshot().active_files, 0);
                    drop(held);
                    assert_eq!(budget.snapshot().0, 0);
                }
            }
        }
    }
}

#[tokio::test]
async fn cancelled_blocking_open_and_write_keep_both_budgets_until_task_finishes() {
    for path in ROUTES {
        for frame_bytes in TRANSPORT_SIZED_FRAMES {
            let metadata = metadata_charge(path, Some(frame_bytes)).await;
            for pause_open in [true, false] {
                let directory = tempfile::tempdir().unwrap();
                let pause = Arc::new(BlockingPause::default());
                let resume = ResumeOnDrop(pause.clone());
                let mut spool =
                    RequestSpoolAdmission::new(directory.path().to_owned(), BUDGET_BYTES as usize);
                if pause_open {
                    spool.blocking_open = Some(pause.clone());
                } else {
                    spool.blocking_write = Some(pause.clone());
                }
                let budget = Arc::new(ProxyMemoryBudget::new(BUDGET_BYTES));
                let held = budget.reservation();
                let frame_allowance = if pause_open {
                    0
                } else {
                    REQUEST_SPOOL_CHUNK_BYTES
                };
                let held_bytes = BUDGET_BYTES as usize - metadata - frame_allowance;
                assert!(held.try_grow(held_bytes, 1));
                let polls = Arc::new(AtomicUsize::new(0));
                let observed = polls.clone();
                let body = Body::from_stream(stream::once(async move {
                    observed.fetch_add(1, Ordering::AcqRel);
                    Ok::<_, Infallible>(Bytes::from(vec![b'x'; frame_bytes]))
                }));
                let active_budget = budget.clone();
                let active_spool = spool.clone();
                let capture = tokio::spawn(async move {
                    admit(path, body, Some(frame_bytes), &active_budget, &active_spool).await
                });
                pause.wait_started().await;
                assert_eq!(polls.load(Ordering::Acquire), usize::from(!pause_open));
                assert_eq!(budget.snapshot().0, BUDGET_BYTES as usize);
                assert_eq!(spool.snapshot().used_bytes, frame_bytes);
                assert_eq!(spool.snapshot().active_files, 1);
                capture.abort();
                assert!(capture.await.unwrap_err().is_cancelled());
                assert!(!pause.is_finished());
                assert_eq!(budget.snapshot().0, BUDGET_BYTES as usize);
                assert_eq!(spool.snapshot().used_bytes, frame_bytes);
                assert_eq!(spool.snapshot().active_files, 1);
                assert!(!held.try_grow(1, 1));
                drop(resume);
                pause.wait_finished().await;
                assert_eq!(budget.snapshot().0, held_bytes);
                assert_eq!(spool.snapshot().used_bytes, 0);
                assert_eq!(spool.snapshot().active_files, 0);
                drop(held);
                assert_eq!(budget.snapshot().0, 0);
                spool.blocking_open = None;
                spool.blocking_write = None;
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
}
