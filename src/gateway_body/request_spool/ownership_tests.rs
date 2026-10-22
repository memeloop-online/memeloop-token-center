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

#[test]
fn pause_hooks_do_not_enlarge_cloned_admission_state() {
    assert_eq!(
        std::mem::size_of::<RequestSpoolAdmission>(),
        std::mem::size_of::<[Arc<()>; 3]>()
    );
}

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
    let spool = RequestSpoolAdmission::new(directory.path().to_owned());
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
                    let pause = Arc::new(BlockingPause::default());
                    let resume = ResumeOnDrop(pause.clone());
                    let mut spool = RequestSpoolAdmission::new(directory.path().to_owned());
                    Arc::get_mut(&mut spool.usage).unwrap().blocking_write = Some(pause.clone());
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
                        pause.wait_started().await;
                        assert!(!pause.is_finished());
                        assert_eq!(budget.snapshot().0, BUDGET_BYTES as usize);
                        assert_eq!(spool.snapshot().used_bytes, 0);
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
async fn cancelled_blocking_open_and_write_keep_memory_and_file_until_task_finishes() {
    for path in ROUTES {
        for frame_bytes in TRANSPORT_SIZED_FRAMES {
            let metadata = metadata_charge(path, Some(frame_bytes)).await;
            for pause_open in [true, false] {
                let directory = tempfile::tempdir().unwrap();
                let pause = Arc::new(BlockingPause::default());
                let resume = ResumeOnDrop(pause.clone());
                let mut spool = RequestSpoolAdmission::new(directory.path().to_owned());
                if pause_open {
                    Arc::get_mut(&mut spool.usage).unwrap().blocking_open = Some(pause.clone());
                } else {
                    Arc::get_mut(&mut spool.usage).unwrap().blocking_write = Some(pause.clone());
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
                assert_eq!(spool.snapshot().used_bytes, 0);
                assert_eq!(spool.snapshot().active_files, 1);
                capture.abort();
                assert!(capture.await.unwrap_err().is_cancelled());
                assert!(!pause.is_finished());
                assert_eq!(budget.snapshot().0, BUDGET_BYTES as usize);
                assert_eq!(spool.snapshot().used_bytes, 0);
                assert_eq!(spool.snapshot().active_files, 1);
                assert!(!held.try_grow(1, 1));
                drop(resume);
                pause.wait_finished().await;
                assert_eq!(budget.snapshot().0, held_bytes);
                assert_eq!(spool.snapshot().used_bytes, 0);
                assert_eq!(spool.snapshot().active_files, 0);
                drop(held);
                assert_eq!(budget.snapshot().0, 0);
                Arc::get_mut(&mut spool.usage).unwrap().blocking_open = None;
                Arc::get_mut(&mut spool.usage).unwrap().blocking_write = None;
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
async fn blocking_write_shares_transport_frame_and_releases_it_after_cancellation() {
    let directory = tempfile::tempdir().unwrap();
    let pause = Arc::new(BlockingPause::default());
    let resume = ResumeOnDrop(pause.clone());
    let mut spool = RequestSpoolAdmission::new(directory.path().to_owned());
    Arc::get_mut(&mut spool.usage).unwrap().blocking_write = Some(pause.clone());
    let dropped = Arc::new(AtomicUsize::new(0));
    let frame_bytes = REQUEST_SPOOL_CHUNK_BYTES * 2 + 17;
    let body = Body::from(Bytes::from_owner(OwnedFrame {
        bytes: vec![b'x'; frame_bytes],
        dropped: dropped.clone(),
    }));
    let budget = ProxyMemoryBudget::new(BUDGET_BYTES);
    let memory = budget.reservation();
    let active_spool = spool.clone();
    let capture = tokio::spawn(async move {
        active_spool
            .capture_with_memory(body, frame_bytes, Some(frame_bytes), Some(&memory), 0)
            .await
    });
    pause.wait_started().await;
    let charged = budget.snapshot().0;
    assert!(charged >= frame_bytes);
    capture.abort();
    assert!(matches!(capture.await, Err(error) if error.is_cancelled()));
    assert_eq!(dropped.load(Ordering::Acquire), 0);
    assert_eq!(budget.snapshot().0, charged);
    assert_eq!(spool.snapshot().active_files, 1);
    drop(resume);
    pause.wait_finished().await;
    assert_eq!(dropped.load(Ordering::Acquire), 1);
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
        let opening_pause = Arc::new(BlockingPause::default());
        let opening_resume = ResumeOnDrop(opening_pause.clone());
        let mut spool = RequestSpoolAdmission::new(directory.path().to_owned());
        Arc::get_mut(&mut spool.usage).unwrap().blocking_open = Some(opening_pause);
        let polls = Arc::new(AtomicUsize::new(0));
        let budget = Arc::new(ProxyMemoryBudget::new(32 * 1024 * 1024));
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
        while spool.snapshot().active_files < UPLOADS {
            assert!(
                !captures.iter().any(tokio::task::JoinHandle::is_finished),
                "all uploads must reach file creation before their first body poll"
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(polls.load(Ordering::Acquire), 0);
        assert_eq!(spool.snapshot().used_bytes, 0);
        drop(opening_resume);
        for _ in 0..UPLOADS {
            starts.recv().await.unwrap();
        }
        assert_eq!(spool.snapshot().used_bytes, 0);
        assert_eq!(spool.snapshot().active_files, UPLOADS);
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
