use std::{
    io::{self, Write},
    sync::{Arc, Mutex},
};

use super::*;

#[derive(Clone, Default)]
struct LogCapture(Arc<Mutex<Vec<u8>>>);

struct LogWriter(Arc<Mutex<Vec<u8>>>);

impl Write for LogWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'writer> tracing_subscriber::fmt::MakeWriter<'writer> for LogCapture {
    type Writer = LogWriter;

    fn make_writer(&'writer self) -> Self::Writer {
        LogWriter(self.0.clone())
    }
}

#[tokio::test(start_paused = true)]
async fn delivery_preserves_results_and_records_cancelled_capacity_waits() {
    use futures_util::FutureExt;
    use tracing::instrument::WithSubscriber;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let request_id = Uuid::now_v7();
    async {
        let mut waits = wait_diagnostics::WaitDiagnostics::default();
        let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
        let mut queued = false;
        let bytes = Bytes::from_static(b"DELIVERY_PAYLOAD_PRIVATE_CANARY");
        for billable in [false, true] {
            let delivered = send_frame(
                FrameDelivery {
                    waits: &mut waits,
                    sender: &sender,
                    queued: &mut queued,
                    probe: None,
                },
                SseDeliveryFrame {
                    bytes: bytes.clone(),
                    billable,
                    terminal: false,
                },
            )
            .await
            .unwrap();
            assert_eq!(delivered, billable);
            assert_eq!(queued, billable);
            assert_eq!(receiver.recv().await.unwrap().unwrap(), bytes);
        }
        sender.send(Ok(bytes.clone())).await.unwrap();
        let mut pending = Box::pin(send_frame(
            FrameDelivery {
                waits: &mut waits,
                sender: &sender,
                queued: &mut queued,
                probe: None,
            },
            SseDeliveryFrame {
                bytes,
                billable: true,
                terminal: true,
            },
        ));
        assert!(pending.as_mut().now_or_never().is_none());
        tokio::time::advance(Duration::from_secs(1)).await;
        drop(pending);
        assert!(queued);
        assert_eq!(
            receiver.recv().await.unwrap().unwrap(),
            Bytes::from_static(b"DELIVERY_PAYLOAD_PRIVATE_CANARY")
        );
        assert!(matches!(
            receiver.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        waits.emit(request_id, "lifecycle_interrupted");
    }
    .with_subscriber(subscriber)
    .await;
    let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let event: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    let fields = &event["fields"];
    assert_eq!(fields["request_id"], request_id.to_string());
    assert_eq!(fields["stage"], "stream_wait_summary");
    assert_eq!(fields["owner_outcome"], "lifecycle_interrupted");
    assert_eq!(fields["transport_terminal"], "not_observed");
    assert_eq!(fields["downstream_capacity_pending_ms"], 1000);
    assert_eq!(fields["max_downstream_capacity_pending_ms"], 1000);
    assert_eq!(fields["upstream_pending_ms"], 0);
    assert!(!rendered.contains("DELIVERY_PAYLOAD_PRIVATE_CANARY"));
    assert!(!rendered.contains("delivery_prepare"));
    assert!(!rendered.contains("delivery_confirm"));
}

#[tokio::test]
async fn owned_delivery_keeps_ingress_clock_without_task_local_inheritance() {
    use tracing::instrument::WithSubscriber;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let started = Instant::now() - Duration::from_secs(5);
    let context = proxy_diagnostics::Context::with_started_for_test(Uuid::now_v7(), started);
    let expected_id = context.request_id;
    // The owner is spawned without a task-local scope, just like the real
    // response stream. No sleeps or scheduler-dependent latency threshold.
    tokio::spawn(
        async move {
            assert!(proxy_diagnostics::CONTEXT.try_with(|_| ()).is_err());
            assert_eq!(
                context.elapsed_millis_at(started + Duration::from_secs(5)),
                5000
            );
            for name in ["terminal_delivery", "stream_terminal_settlement"] {
                proxy_diagnostics::Phase::new(context, name).finish("completed", None, None);
            }
        }
        .with_subscriber(subscriber),
    )
    .await
    .unwrap();
    let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    let events: Vec<serde_json::Value> = rendered
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(events.len(), 4);
    for event in events {
        let fields = &event["fields"];
        assert_eq!(fields["request_id"], expected_id.to_string());
        assert!(
            fields["request_elapsed_ms"].as_u64().unwrap() >= 5000,
            "delivery must retain the supplied five seconds of ingress history"
        );
    }
}

#[tokio::test]
async fn delivery_failure_log_uses_only_fixed_safe_fields() {
    use tracing::instrument::WithSubscriber;
    const CANARY: &str = "DELIVERY_PAYLOAD_SECRET_CANARY";
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let request_id = Uuid::now_v7();

    async {
        let mut waits = wait_diagnostics::WaitDiagnostics::default();
        let (sender, receiver) = tokio::sync::mpsc::channel(1);
        drop(receiver);
        let mut queued = false;
        let phase = proxy_diagnostics::Phase::new(
            proxy_diagnostics::Context::for_request(request_id),
            "terminal_delivery",
        );
        let failure = send_frame(
            FrameDelivery {
                waits: &mut waits,
                sender: &sender,
                queued: &mut queued,
                probe: None,
            },
            SseDeliveryFrame {
                bytes: Bytes::from_static(CANARY.as_bytes()),
                billable: true,
                terminal: true,
            },
        )
        .await;
        assert_eq!(failure, Err("downstream_disconnected"));
        assert!(!queued);
        let error_code = failure.unwrap_err();
        phase.finish(error_code, None, None);
        waits.terminal(error_code);
        waits.emit(request_id, "lifecycle_returned");
    }
    .with_subscriber(subscriber)
    .await;

    let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(rendered.contains(&request_id.to_string()));
    assert!(rendered.contains("terminal_delivery"));
    assert!(rendered.contains("stream_wait_summary"));
    assert!(rendered.contains("downstream_disconnected"));
    assert!(!rendered.contains("delivery_prepare"));
    assert!(!rendered.contains("delivery_confirm"));
    assert!(!rendered.contains(CANARY));
}
