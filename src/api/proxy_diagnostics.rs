//! Correlates pre-admission failures and response tails without database writes.
//! Never accept a client-provided correlation ID, path, body, or error text.
use std::time::Instant;
use uuid::Uuid;

mod downstream_body;

pub(super) fn observe_response(
    response: axum::response::Response,
    context: Context,
) -> axum::response::Response {
    downstream_body::observe_response(response, context)
}

#[derive(Clone, Copy)]
pub(super) struct Context {
    pub(super) request_id: Uuid,
    started: Instant,
}

tokio::task_local! {
    pub(super) static CONTEXT: Context;
}

impl Context {
    #[cfg(test)]
    pub(super) fn with_started_for_test(request_id: Uuid, started: Instant) -> Self {
        Self {
            request_id,
            started,
        }
    }
    pub(super) fn new() -> Self {
        Self {
            request_id: Uuid::now_v7(),
            started: Instant::now(),
        }
    }

    pub(super) fn current() -> Self {
        CONTEXT
            .try_with(|context| *context)
            .unwrap_or_else(|_| Self::new())
    }

    pub(super) fn for_request(request_id: Uuid) -> Self {
        let mut context = Self::current();
        context.request_id = request_id;
        context
    }

    /// Shares the ingress clock with persisted first-output observations;
    /// admission and body-reading time must not disappear from user latency.
    pub(super) fn elapsed_millis_at(self, now: Instant) -> i64 {
        now.saturating_duration_since(self.started)
            .as_millis()
            .min(i64::MAX as u128) as i64
    }
}

/// Only exact supported paths become labels. Query strings and arbitrary
/// path segments never enter logs.
pub(super) fn route_class(path: &str) -> Option<&'static str> {
    match path {
        "/v1/responses" => Some("responses"),
        "/v1/responses/compact" => Some("responses_compact"),
        "/v1/alpha/search" => Some("alpha_search"),
        "/v1/chat/completions" => Some("chat"),
        "/v1/messages" => Some("messages"),
        "/v1/messages/count_tokens" => Some("count_tokens"),
        "/v1/embeddings" => Some("embeddings"),
        _ => None,
    }
}

pub(super) fn ingress_request_id(value: Option<&str>) -> Option<Uuid> {
    value.and_then(|value| Uuid::parse_str(value).ok())
}

/// Match route templates, never concrete tenant/request identifiers or queries.
pub(super) fn control_route_class(template: &str) -> Option<&'static str> {
    match template {
        "/internal/v1/requests/query" => Some("request_list"),
        "/internal/v1/requests/{request_id}" => Some("request_detail"),
        "/internal/v1/upstreams" => Some("upstream_directory"),
        "/internal/v1/request-events" => Some("request_events"),
        _ => None,
    }
}

pub(super) struct Phase {
    context: Context,
    phase: &'static str,
    started: Instant,
    account: Option<Uuid>,
    generation: Option<i64>,
    finished: bool,
}

impl Phase {
    pub(super) fn new(context: Context, phase: &'static str) -> Self {
        Self::account(context, phase, None, None)
    }

    pub(super) fn account(
        context: Context,
        phase: &'static str,
        account: Option<Uuid>,
        generation: Option<i64>,
    ) -> Self {
        let timer = Self {
            context,
            phase,
            started: Instant::now(),
            account,
            generation,
            finished: false,
        };
        timer.emit("started", None, None);
        timer
    }

    pub(super) fn finish(
        mut self,
        outcome: &'static str,
        status: Option<u16>,
        bytes: Option<usize>,
    ) {
        self.emit(outcome, status, bytes);
        self.finished = true;
    }

    fn emit(&self, outcome: &'static str, status: Option<u16>, bytes: Option<usize>) {
        tracing::info!(
            request_id = %self.context.request_id,
            phase = self.phase,
            outcome,
            elapsed_ms = self.started.elapsed().as_millis() as u64,
            request_elapsed_ms = self.context.started.elapsed().as_millis() as u64,
            upstream_account_id = ?self.account,
            credential_generation = self.generation,
            status,
            bytes,
            "proxy phase observation"
        );
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        if !self.finished {
            // Drop also covers cancellation. Do not claim a transport failure
            // or infer delivery/replay permission from an interrupted future.
            self.emit("not_completed", None, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn control_labels_accept_only_the_reviewed_route_templates() {
        assert_eq!(
            control_route_class("/internal/v1/requests/{request_id}"),
            Some("request_detail")
        );
        assert_eq!(
            control_route_class("/internal/v1/request-events"),
            Some("request_events")
        );
        for path in [
            "/internal/v1/requests/secret-canary",
            "/internal/v1/upstreams?tenant=secret-canary",
            "/internal/v1/credentials",
            "unmatched",
        ] {
            assert_eq!(control_route_class(path), None);
        }
    }

    #[test]
    fn first_output_clock_includes_preparation_and_admission_wait() {
        let context = Context::new();
        let admitted_at = context.started + std::time::Duration::from_secs(3);
        let first_output_at = admitted_at + std::time::Duration::from_secs(2);
        assert_eq!(context.elapsed_millis_at(first_output_at), 5000);
        assert_eq!(context.elapsed_millis_at(context.started), 0);
    }

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
    fn phases_emit_only_safe_fields_and_interruption_is_not_success() {
        let writer = Writer::default();
        let sink = writer.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        let context = Context::new();
        tracing::subscriber::with_default(subscriber, || {
            let mut phase = Phase::account(
                context,
                "codex_transport_attempt",
                Some(Uuid::nil()),
                Some(3),
            );
            // Deterministic elapsed evidence without a scheduler-dependent sleep.
            phase.started = Instant::now() - std::time::Duration::from_secs(2);
            phase.finish("response_headers", Some(200), Some(16));
            drop(Phase::new(context, "request_archive_admission"));
        });
        let bytes = writer.0.lock().unwrap();
        let events: Vec<serde_json::Value> = std::str::from_utf8(&bytes)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events.len(), 4);
        for event in &events {
            let fields = event["fields"].as_object().unwrap();
            assert_eq!(fields["request_id"], context.request_id.to_string());
            for field in fields.keys() {
                assert!(
                    [
                        "message",
                        "request_id",
                        "phase",
                        "outcome",
                        "elapsed_ms",
                        "request_elapsed_ms",
                        "upstream_account_id",
                        "credential_generation",
                        "status",
                        "bytes"
                    ]
                    .contains(&field.as_str())
                );
            }
        }
        assert!(events[1]["fields"]["elapsed_ms"].as_u64().unwrap() >= 2_000);
        assert_eq!(events[1]["fields"]["status"], 200);
        assert_eq!(events[1]["fields"]["credential_generation"], 3);
        assert_eq!(events[3]["fields"]["outcome"], "not_completed");
    }

    #[tokio::test]
    async fn concurrent_contexts_cannot_exchange_request_ids() {
        let first = Context::new();
        let second = Context::new();
        let (first_seen, second_seen) = tokio::join!(
            CONTEXT.scope(first, async {
                tokio::task::yield_now().await;
                Context::current().request_id
            }),
            CONTEXT.scope(second, async {
                tokio::task::yield_now().await;
                Context::current().request_id
            }),
        );
        assert_eq!(first_seen, first.request_id);
        assert_eq!(second_seen, second.request_id);
        assert_ne!(first_seen, second_seen);
        assert!(CONTEXT.try_with(|_| ()).is_err());
    }

    #[test]
    fn compact_is_distinct_without_recording_raw_paths() {
        assert_eq!(route_class("/v1/responses"), Some("responses"));
        assert_eq!(
            route_class("/v1/responses/compact"),
            Some("responses_compact")
        );
        assert_eq!(route_class("/v1/alpha/search"), Some("alpha_search"));
        assert_eq!(route_class("/v1/responses/SECRET_CANARY"), None);
        assert_eq!(route_class("/v1/responses?api_key=SECRET_CANARY"), None);
        assert_eq!(ingress_request_id(Some("SECRET_CANARY")), None);
        let id = Uuid::new_v4();
        assert_eq!(ingress_request_id(Some(&id.to_string())), Some(id));
    }
}
