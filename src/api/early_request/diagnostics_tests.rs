use std::sync::{Arc, Mutex};

#[derive(Clone, Default)]
struct Writer(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Writer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[test]
fn failure_event_preserves_public_evidence_without_inventing_a_root_cause() {
    let writer = Writer::default();
    let sink = writer.clone();
    let subscriber = tracing_subscriber::fmt()
        .json()
        .without_time()
        .with_writer(move || sink.clone())
        .finish();
    let request_id = uuid::Uuid::now_v7();
    tracing::subscriber::with_default(subscriber, || {
        super::observe_failure(request_id, 503, "service_overloaded");
    });
    let event: serde_json::Value = serde_json::from_slice(&writer.0.lock().unwrap()).unwrap();
    assert_eq!(
        event["fields"],
        serde_json::json!({
            "message": "gateway pre-admission failure observed",
            "request_id": request_id.to_string(),
            "phase": "pre_admission",
            "terminal_cause_code": "gateway_pre_admission",
            "status": 503,
            "error_code": "service_overloaded",
        })
    );
}
