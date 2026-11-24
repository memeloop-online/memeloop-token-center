//! One capture summary. Input wall includes scheduling after a pending poll;
//! remaining time is deliberately not attributed to storage.
use futures_util::Stream;
use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
    time::{Duration, Instant},
};

#[derive(Default)]
struct Totals {
    input: Duration,
    scan: Duration,
    bytes: u64,
    chunks: u64,
    eof: bool,
    error: bool,
}
#[derive(Clone, Default)]
pub(super) struct Timing(Arc<Mutex<Totals>>);
pub(super) struct CaptureSummary {
    timing: Timing,
    start: Instant,
    request_id: Option<uuid::Uuid>,
}
impl CaptureSummary {
    pub(super) fn new(id: Option<&str>) -> Self {
        Self {
            timing: Timing::default(),
            start: Instant::now(),
            request_id: id.and_then(|s| uuid::Uuid::parse_str(s).ok()),
        }
    }
    pub(super) fn timing(&self) -> Timing {
        self.timing.clone()
    }
}
impl Drop for CaptureSummary {
    fn drop(&mut self) {
        let totals = self.timing.0.lock().unwrap();
        let total = self.start.elapsed();
        let remaining = total
            .saturating_sub(totals.input)
            .saturating_sub(totals.scan);
        tracing::info!(request_id=?self.request_id, phase="request_body_capture", input_wall_ms=totals.input.as_secs_f64()*1000., hash_scanner_elapsed_ms=totals.scan.as_secs_f64()*1000., remaining_elapsed_ms=remaining.as_secs_f64()*1000., total_elapsed_ms=total.as_secs_f64()*1000., bytes=totals.bytes, chunks=totals.chunks, outcome=if totals.error {"error"} else if totals.eof {"eof"} else {"drop"}, "request body capture summary");
    }
}
impl Timing {
    pub(super) fn scan(&self, started: Instant, bytes: usize) {
        let mut t = self.0.lock().unwrap();
        t.scan += started.elapsed();
        t.bytes += bytes as u64;
        t.chunks += 1;
    }
    pub(super) fn error(&self) {
        self.0.lock().unwrap().error = true;
    }
    pub(super) fn input<S: Stream>(&self, stream: S) -> TimedInput<S> {
        TimedInput {
            stream: Box::pin(stream),
            timing: self.clone(),
            waiting: None,
        }
    }
}
pub(super) struct TimedInput<S> {
    stream: Pin<Box<S>>,
    timing: Timing,
    waiting: Option<Instant>,
}
impl<S: Stream> Stream for TimedInput<S> {
    type Item = S::Item;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let start = *self.waiting.get_or_insert_with(Instant::now);
        match self.stream.as_mut().poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(item) => {
                self.waiting = None;
                let mut t = self.timing.0.lock().unwrap();
                t.input += start.elapsed();
                if item.is_none() {
                    t.eof = true;
                }
                Poll::Ready(item)
            }
        }
    }
}

impl<S> Drop for TimedInput<S> {
    fn drop(&mut self) {
        if let Some(start) = self.waiting.take() {
            self.timing.0.lock().unwrap().input += start.elapsed();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::StreamExt;

    #[tokio::test]
    async fn request_capture_timing_separates_input_and_preservation_gates() {
        let summary = CaptureSummary::new(None);
        let timing = summary.timing();
        let (send, receive) = tokio::sync::oneshot::channel::<bytes::Bytes>();
        let mut input = timing.input(futures_util::stream::once(async { receive.await.unwrap() }));
        let producer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            send.send(bytes::Bytes::from_static("多字节fixture".as_bytes()))
                .unwrap();
        });
        let bytes = input.next().await.unwrap();
        let scan = Instant::now();
        let hash = blake3::hash(&bytes);
        timing.scan(scan, bytes.len());
        let input_before = timing.0.lock().unwrap().input;
        // A separate consumer-side gate stands for carrier preservation. It
        // must never be counted as awaiting the next incoming frame.
        let (release, gate) = tokio::sync::oneshot::channel();
        let consumer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(30)).await;
            release.send(()).unwrap();
        });
        gate.await.unwrap();
        assert_eq!(timing.0.lock().unwrap().input, input_before);
        assert!(input_before >= Duration::from_millis(20));
        assert!(summary.start.elapsed().saturating_sub(input_before) >= Duration::from_millis(20));
        assert_eq!(hash, blake3::hash("多字节fixture".as_bytes()));
        assert!(input.next().await.is_none());
        assert!(timing.0.lock().unwrap().eof);
        producer.await.unwrap();
        consumer.await.unwrap();
    }

    #[tokio::test]
    async fn request_capture_timing_drop_keeps_pending_input_and_no_false_eof() {
        let summary = CaptureSummary::new(None);
        let timing = summary.timing();
        let mut input = timing.input(futures_util::stream::pending::<bytes::Bytes>());
        assert!(
            tokio::time::timeout(Duration::from_millis(30), input.next())
                .await
                .is_err()
        );
        drop(input);
        let t = timing.0.lock().unwrap();
        assert!(!t.eof);
        assert!(t.input >= Duration::from_millis(20));
        assert_eq!(t.bytes, 0);
    }
}
