use super::*;
use futures_util::StreamExt;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

mod peer;
use peer::{Peer, Post};

const CREATED: &str =
    "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-usage-contract\"}}\n\n";
const TOOL: &str = "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"id\":\"fc-contract\",\"type\":\"function_call\",\"call_id\":\"call-contract\",\"name\":\"lookup\",\"arguments\":\"\"}}\n\ndata: {\"type\":\"response.function_call_arguments.delta\",\"item_id\":\"fc-contract\",\"output_index\":0,\"delta\":\"{}\"}\n\n";

async fn emit(post: &Post, text: &str) {
    post.output
        .send(Ok(bytes::Bytes::copy_from_slice(text.as_bytes())))
        .await
        .unwrap();
}

async fn complete(post: Post) {
    emit(
        &post,
        &format!(
            "data: {{\"type\":\"response.completed\",\"response\":{}}}\n\ndata: [DONE]\n\n",
            completed_response_with_usage(3, 7)
        ),
    )
    .await;
    drop(post);
}

async fn settled(fixture: &CodexRouteFixture, status: u16) {
    wait_for_request_settlement(fixture, 1).await;
    let rows = fixture
        .state
        .db
        .list_requests(fixture.key_id, 10)
        .await
        .unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].status_code, Some(status));
    if status == 200 {
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 7));
        assert_eq!(rows[0].cost, "0.00001");
    } else {
        assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (0, 0));
        assert_eq!(rows[0].cost, "0");
    }
    assert_exactly_once_side_effects(fixture, rows[0].request_id, Some("resp-usage-contract"))
        .await;
}

async fn streaming(version: http::Version, cancel: bool) {
    let fixture = codex_route_fixture("protocol-stream").await;
    let mut peer = Peer::start(version, crate::provider::CodexTransportPolicy::default()).await;
    fixture
        .state
        .codex_clients
        .install_test_client(peer.client.clone());
    let send = send_codex_route_to_endpoint(
        &fixture,
        peer.endpoint(),
        "/v1/responses",
        json!({"model":fixture.model,"input":"protocol contract","stream":true,
            "tools":[{"type":"function","name":"lookup","parameters":{"type":"object"}}]}),
    );
    tokio::pin!(send);
    let post = tokio::select! {
        post = peer.post() => post,
        _ = &mut send => panic!("must reach peer before response"),
    };
    assert_eq!(post.body["model"], fixture.upstream_model);
    assert_eq!(post.body["tools"][0]["name"], "lookup");
    emit(&post, CREATED).await;
    emit(&post, TOOL).await;
    let response = send.await;
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body().into_data_stream();
    let mut rendered = String::new();
    while !rendered.contains("response.function_call_arguments.delta") {
        let chunk = tokio::time::timeout(Duration::from_secs(3), body.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        rendered.push_str(std::str::from_utf8(&chunk).unwrap());
    }
    assert!(rendered.contains("call-contract"));
    assert!(!rendered.contains("response.completed"));
    if cancel {
        drop(body);
        tokio::time::timeout(Duration::from_secs(3), peer.cancelled.recv())
            .await
            .expect("body drop must cancel silent upstream immediately")
            .unwrap();
        settled(&fixture, 499).await;
        assert!(post.output.is_closed());
    } else {
        complete(post).await;
        while let Some(chunk) = body.next().await {
            rendered.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
        }
        assert!(rendered.contains("response.completed"));
        assert!(rendered.contains("[DONE]"));
        settled(&fixture, 200).await;
    }
    assert_eq!(
        peer.posts.load(Ordering::SeqCst),
        1,
        "tool output must never replay"
    );
}

async fn compact(version: http::Version) {
    let fixture = codex_route_fixture("protocol-compact").await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    let mut config: Value = serde_json::from_str(
        &sqlx::query_scalar::<_, String>("SELECT config_json FROM upstream_accounts WHERE id = $1")
            .bind(fixture.upstream_account_id.to_string())
            .fetch_one(&pool)
            .await
            .unwrap(),
    )
    .unwrap();
    config["transport_policy"] = json!({"failover_deadline_millis":1000,"connect_timeout_millis":1000,
        "read_timeout_millis":1000,"request_timeout_millis":2000});
    sqlx::query("UPDATE upstream_accounts SET config_json = $1 WHERE id = $2")
        .bind(config.to_string())
        .bind(fixture.upstream_account_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let policy = crate::provider::CodexTransportPolicy {
        failover_deadline_millis: 1000,
        connect_timeout_millis: 1000,
        read_timeout_millis: 1000,
        request_timeout_millis: 2000,
        ..Default::default()
    };
    let mut peer = Peer::start(version, policy).await;
    fixture
        .state
        .codex_clients
        .install_test_client(peer.client.clone());
    let send = send_codex_route_to_endpoint(
        &fixture,
        peer.endpoint(),
        "/v1/responses",
        json!({"model":fixture.model,"input":[{"role":"user","content":"summarize"},{"type":"compaction_trigger"}],"stream":false}),
    );
    tokio::pin!(send);
    let post = tokio::select! {
        post = peer.post() => post,
        _ = &mut send => panic!("must reach peer before response"),
    };
    tokio::select! {
        _ = tokio::time::sleep(Duration::from_secs(3)) => {},
        _ = &mut send => panic!("compact must outlive failover and transport deadlines"),
    }
    emit(&post, CREATED).await;
    complete(post).await;
    let response = send.await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    assert!(std::str::from_utf8(&body).unwrap().contains("cutover-ok"));
    settled(&fixture, 200).await;
    assert_eq!(peer.posts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn native_tls_socks_h1_h2_stream_tool_cancel_and_compact_contract() {
    for version in [http::Version::HTTP_11, http::Version::HTTP_2] {
        tokio::time::timeout(Duration::from_secs(60), async {
            streaming(version, false).await;
            streaming(version, true).await;
            compact(version).await;
        })
        .await
        .expect("bounded native protocol contract");
    }
}
