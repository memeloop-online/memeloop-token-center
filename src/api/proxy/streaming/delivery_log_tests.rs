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

#[test]
fn delivery_phase_preserves_results_and_records_cancelled_waits() {
    use futures_util::FutureExt;
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let request_id = Uuid::now_v7();
    let context = proxy_diagnostics::Context::for_request(request_id);
    tracing::subscriber::with_default(subscriber, || {
        let success = observe_delivery_transition(context, "delivery_prepare", async {
            Ok::<_, AppError>(false)
        })
        .now_or_never()
        .unwrap()
        .unwrap();
        assert!(!success, "observing a database result must not change it");
        let failure = observe_delivery_transition(context, "delivery_confirm", async {
            Err::<(), _>(AppError::Conflict("SECRET_PHASE_CANARY".into()))
        })
        .now_or_never()
        .unwrap();
        assert!(matches!(failure, Err(AppError::Conflict(_))));
        assert!(
            observe_delivery_transition(
                context,
                "delivery_prepare",
                std::future::pending::<Result<(), AppError>>()
            )
            .now_or_never()
            .is_none()
        );
    });
    let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(rendered.contains(&request_id.to_string()));
    assert!(rendered.contains("completed"));
    assert!(rendered.contains("state_conflict"));
    assert!(rendered.contains("not_completed"));
    assert!(!rendered.contains("SECRET_PHASE_CANARY"));
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
            for name in ["delivery_prepare", "delivery_confirm"] {
                observe_delivery_transition(context, name, async { Ok::<_, AppError>(()) })
                    .await
                    .unwrap();
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

#[test]
fn delivery_failure_log_uses_only_fixed_safe_fields() {
    const CANARY: &str = "DELIVERY_DATABASE_SECRET_CANARY";
    let capture = LogCapture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(capture.clone())
        .finish();
    let request_id = Uuid::now_v7();

    tracing::subscriber::with_default(subscriber, || {
        log_delivery_state_failure(
            request_id,
            "delivery_prepare",
            &AppError::Conflict(CANARY.to_owned()),
        );
        log_delivery_state_failure(request_id, "delivery_confirm", &AppError::Internal);
    });

    let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(rendered.contains(&request_id.to_string()));
    assert!(rendered.contains("delivery_prepare"));
    assert!(rendered.contains("delivery_confirm"));
    assert!(rendered.contains("state_conflict"));
    assert!(rendered.contains("internal"));
    assert!(!rendered.contains(CANARY));
}
