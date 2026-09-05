use super::*;

use futures_util::StreamExt;

#[tokio::test]
async fn codex_terminal_id_conflict_never_archives_or_settles_as_success() {
    let fixture = codex_route_fixture("terminal-id-conflict").await;
    let upstream = MockServer::start().await;
    let sse = concat!(
        "event: response.created\n",
        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-a\"}}\n\n",
        "event: response.completed\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp-b\",\"output\":[],\"usage\":{\"input_tokens\":3,\"output_tokens\":2}}}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path(codex_transport::RESPONSES_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_raw(sse, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;

    let response = send_codex_route(
        &fixture,
        &upstream,
        "/v1/responses",
        json!({"model": fixture.model, "input": "reject terminal", "stream": true}),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut delivered = Vec::new();
    let mut saw_error = false;
    while let Some(next) = body.next().await {
        match next {
            Ok(bytes) => delivered.extend_from_slice(&bytes),
            Err(_) => saw_error = true,
        }
    }
    // The conflicting completed envelope shares this raw chunk with the
    // created event. It is rejected before either the terminal frame or a
    // prefix is placed on the client-facing body channel.
    assert!(saw_error);
    assert!(delivered.is_empty());

    wait_for_request_settlement(&fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows[0].status_code, Some(502));
    assert_eq!(
        rows[0].error_code.as_deref(),
        Some("upstream_invalid_response")
    );
    assert_exactly_once_side_effects(&fixture, rows[0].request_id, None).await;
    let refs = fixture
        .state
        .db
        .request_archive_refs(fixture.key_id, rows[0].request_id)
        .await
        .unwrap();
    assert_eq!(
        refs.response_object.as_deref(),
        Some(format!("gap://{}/response", rows[0].request_id).as_str())
    );
    upstream.verify().await;
}
