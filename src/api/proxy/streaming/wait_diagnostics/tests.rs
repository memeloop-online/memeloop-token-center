use super::*;
use crate::api::proxy::streaming::{StreamPoll, poll_upstream_downstream_or_progress_heartbeat};

#[test]
fn summary_contains_only_fixed_metadata_fields() {
    #[derive(Clone, Default)]
    struct Writer(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let writer = Writer::default();
    let sink = writer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(move || sink.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, || {
        let mut diagnostics = WaitDiagnostics::default();
        diagnostics.terminal("completed");
        diagnostics.emit(uuid::Uuid::nil(), "lifecycle_returned");
    });
    let bytes = writer.0.lock().unwrap();
    let event: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let fields = event["fields"].as_object().unwrap();
    let expected = [
        "message",
        "request_id",
        "stage",
        "observation_layer",
        "http2_data_silence_proven",
        "owner_outcome",
        "transport_terminal",
        "upstream_pending_ms",
        "max_upstream_pending_ms",
        "max_no_body_pending_lower_bound_ms",
        "downstream_capacity_pending_ms",
        "max_downstream_capacity_pending_ms",
        "nonempty_body_chunks",
        "progress_heartbeats_enqueued",
    ];
    assert_eq!(fields.len(), expected.len());
    assert!(fields.keys().all(|key| expected.contains(&key.as_str())));
    assert_eq!(fields["http2_data_silence_proven"], false);
    assert_eq!(fields["observation_layer"], "application_body_poll");
    assert_eq!(fields["request_id"], uuid::Uuid::nil().to_string());
    assert_eq!(fields["transport_terminal"], "completed");
}

#[tokio::test(start_paused = true)]
async fn quiet_body_wait_survives_heartbeat_cancellation_without_counting_send_time() {
    let mut diagnostics = WaitDiagnostics::default();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    for _ in 0..21 {
        let next = poll_upstream_downstream_or_progress_heartbeat(
            &sender,
            diagnostics.upstream(std::future::pending::<()>()),
            deadline,
            Some(tokio::time::Instant::now() + Duration::from_secs(15)),
        )
        .await;
        assert!(matches!(next, StreamPoll::ProgressHeartbeat));
        diagnostics
            .downstream(sender.send(Ok(bytes::Bytes::from_static(b"synthetic"))))
            .await
            .unwrap();
        diagnostics.heartbeat();
        receiver.recv().await.unwrap().unwrap();
    }
    assert_eq!(diagnostics.max_no_body_pending, Duration::from_secs(315));
    assert_eq!(diagnostics.max_upstream_pending, Duration::from_secs(15));
    assert_eq!(diagnostics.downstream_pending, Duration::ZERO);
    assert_eq!(diagnostics.heartbeats, 21);
    diagnostics.body_chunk();
    assert_eq!(diagnostics.no_body_pending, Duration::ZERO);
    assert_eq!(diagnostics.max_no_body_pending, Duration::from_secs(315));
}

#[tokio::test(start_paused = true)]
async fn slow_consumer_and_unpolled_processing_never_become_upstream_wait() {
    let mut diagnostics = WaitDiagnostics::default();
    let (sender, mut receiver) = tokio::sync::mpsc::channel(1);
    sender.send(()).await.unwrap();
    diagnostics.upstream(std::future::ready(())).await;
    diagnostics.body_chunk();
    let consumer = async move {
        tokio::time::sleep(Duration::from_secs(310)).await;
        receiver.recv().await.unwrap();
        receiver
    };
    let (result, _receiver) = tokio::join!(diagnostics.downstream(sender.send(())), consumer);
    result.unwrap();
    tokio::time::sleep(Duration::from_secs(310)).await;
    diagnostics.upstream(std::future::ready(())).await;
    diagnostics.body_chunk();
    assert_eq!(diagnostics.downstream_pending, Duration::from_secs(310));
    assert_eq!(diagnostics.max_downstream_pending, Duration::from_secs(310));
    assert_eq!(diagnostics.upstream_pending, Duration::ZERO);
    assert_eq!(diagnostics.max_no_body_pending, Duration::ZERO);
}

#[tokio::test(start_paused = true)]
async fn deadline_cancellation_accounts_pending_wait_and_ready_reads_add_nothing() {
    let mut diagnostics = WaitDiagnostics::default();
    let result = tokio::time::timeout(
        Duration::from_secs(7),
        diagnostics.upstream(std::future::pending::<()>()),
    )
    .await;
    assert!(result.is_err());
    assert_eq!(diagnostics.upstream_pending, Duration::from_secs(7));
    diagnostics.upstream(std::future::ready(())).await;
    assert_eq!(diagnostics.upstream_pending, Duration::from_secs(7));
}
