use crate::provider::{CodexTransportPolicy, ResolvedUpstream};

pub(super) fn record_response(
    request_id: uuid::Uuid,
    route: &ResolvedUpstream,
    candidate_rank: usize,
    outbound_attempt: usize,
    version: http::Version,
    status: http::StatusCode,
) {
    let configured_protocol = CodexTransportPolicy::parse(route.config.get("transport_policy"))
        .map(|policy| policy.http_protocol.as_str())
        .unwrap_or("unknown");
    let negotiated_http_version = match version {
        http::Version::HTTP_09 => "0.9",
        http::Version::HTTP_10 => "1.0",
        http::Version::HTTP_11 => "1.1",
        http::Version::HTTP_2 => "2",
        http::Version::HTTP_3 => "3",
        _ => "unknown",
    };
    tracing::info!(
        %request_id,
        upstream_account_id = %route.account_id,
        route_id = %route.route_id,
        transport_revision = route.transport_revision,
        candidate_rank,
        outbound_attempt,
        configured_protocol,
        negotiated_http_version,
        upstream_status = status.as_u16(),
        stage = "codex_upstream_http_protocol",
        observation_scope = "returned_candidate_response_headers_not_stream_completion",
        "Codex upstream HTTP protocol observed"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };

    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn protocol_log_uses_observed_version_and_never_serializes_account_material() {
        let capture = Capture::default();
        let sink = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .json()
            .without_time()
            .with_writer(move || sink.clone())
            .finish();
        let route = ResolvedUpstream {
            route_id: uuid::Uuid::now_v7(),
            account_id: uuid::Uuid::now_v7(),
            transport_revision: 17,
            credential_generation: 3,
            driver: "openai-codex".into(),
            base_url: "https://SECRET_ENDPOINT_CANARY.invalid".into(),
            config: serde_json::json!({"transport_policy":{"http_protocol":"http1_only"},"secret":"SECRET_CONFIG_CANARY"}),
            upstream_model: "SECRET_MODEL_CANARY".into(),
            credential: crate::provider::UpstreamCredential::ProxiedApiKey {
                value: "SECRET_TOKEN_CANARY".into(),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                proxy_url: "socks5h://SECRET_PROXY_CANARY:password@10.0.0.1:1080".into(),
                proxy_network_scope: crate::network::OutboundScope::Private,
            },
        };
        let request_id = uuid::Uuid::now_v7();
        tracing::subscriber::with_default(subscriber, || {
            record_response(
                request_id,
                &route,
                2,
                3,
                http::Version::HTTP_2,
                http::StatusCode::BAD_GATEWAY,
            );
            record_response(
                request_id,
                &route,
                2,
                4,
                http::Version::HTTP_11,
                http::StatusCode::OK,
            );
        });
        let rendered = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(!rendered.contains("SECRET_"));
        let rows: Vec<serde_json::Value> = rendered
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(rows.len(), 2);
        for row in &rows {
            assert_eq!(row["fields"]["configured_protocol"], "http1_only");
            assert_eq!(row["fields"]["request_id"], request_id.to_string());
            assert_eq!(
                row["fields"]["upstream_account_id"],
                route.account_id.to_string()
            );
            assert_eq!(
                row["fields"]["observation_scope"],
                "returned_candidate_response_headers_not_stream_completion"
            );
        }
        assert_eq!(rows[0]["fields"]["negotiated_http_version"], "2");
        assert_eq!(rows[0]["fields"]["upstream_status"], 502);
        assert_eq!(rows[1]["fields"]["negotiated_http_version"], "1.1");
        assert_eq!(rows[1]["fields"]["upstream_status"], 200);
    }
}
