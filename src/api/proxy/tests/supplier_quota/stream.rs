use super::*;

#[tokio::test]
async fn two_candidates_deliver_first_event_before_release_and_keep_the_selected_stream() {
    use futures_util::StreamExt;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    for comment in [false, true] {
        for ending in ["complete", "quota", "cancel"] {
            let label = format!("first-event-{comment}-{ending}");
            let fixture = codex_route_fixture(&label).await;
            add_codex_standby_route(&fixture, &format!("codex-route-{label}"), "account-456").await;
            let prefix = if comment {
                ": upstream progress\n\n".to_owned()
            } else {
                "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp-codex\"}}\n\n"
                    .to_owned()
            };
            let suffix = if ending == "quota" {
                "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":null,\"type\":\"usage_limit_reached\"}}}\n\n".to_owned()
            } else {
                completed_codex_sse("selected complete")
            };
            let (release, gate) = tokio::sync::oneshot::channel::<()>();
            let (dropped, observed_drop) = tokio::sync::oneshot::channel::<()>();
            let gate = Arc::new(std::sync::Mutex::new(Some((gate, dropped))));
            let standby_posts = Arc::new(AtomicUsize::new(0));
            let handler_posts = standby_posts.clone();
            let app = axum::Router::new().fallback(move |headers: HeaderMap| {
                let gate = gate.clone();
                let prefix = prefix.clone();
                let suffix = suffix.clone();
                let posts = handler_posts.clone();
                async move {
                    if headers
                        .get("chatgpt-account-id")
                        .and_then(|value| value.to_str().ok())
                        == Some("account-456")
                    {
                        posts.fetch_add(1, Ordering::SeqCst);
                        return (
                            [(header::CONTENT_TYPE, "text/event-stream")],
                            Body::from(completed_codex_sse("wrong candidate")),
                        );
                    }
                    let (gate, dropped) = gate.lock().unwrap().take().unwrap();
                    let stream = futures_util::stream::once(async move {
                        Ok::<_, std::io::Error>(Bytes::from(prefix))
                    })
                    .chain(futures_util::stream::once(async move {
                        let _dropped = dropped;
                        gate.await.unwrap();
                        Ok::<_, std::io::Error>(Bytes::from(suffix))
                    }));
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        Body::from_stream(stream),
                    )
                }
            });
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let response = tokio::time::timeout(
                Duration::from_secs(3),
                send_codex_route_to_endpoint(
                    &fixture,
                    endpoint,
                    "/v1/responses",
                    json!({"model":fixture.model,"input":"slow reasoning","stream":true}),
                ),
            )
            .await
            .expect("headers must precede upstream release");
            assert_eq!(response.status(), StatusCode::OK);
            let mut body = response.into_body().into_data_stream();
            let first = tokio::time::timeout(Duration::from_secs(3), body.next())
                .await
                .expect("first event must precede release")
                .unwrap()
                .unwrap();
            let mut rendered = String::from_utf8(first.to_vec()).unwrap();
            assert!(
                rendered.contains(if comment {
                    ": heartbeat"
                } else {
                    "response.created"
                }),
                "{rendered}"
            );
            assert_eq!(standby_posts.load(Ordering::SeqCst), 0);
            if ending == "cancel" {
                drop(body);
                cancellation::assert_single_unknown_receipt(
                    &fixture,
                    499,
                    Some("client_cancelled"),
                )
                .await;
                tokio::time::timeout(Duration::from_secs(5), observed_drop)
                    .await
                    .expect("cancellation must release upstream body")
                    .unwrap_err();
                drop(release);
            } else {
                if !comment && ending == "complete" {
                    tokio::time::timeout(Duration::from_secs(25), async {
                        while !rendered.contains(": keep-alive") {
                            rendered.push_str(
                                std::str::from_utf8(&body.next().await.unwrap().unwrap()).unwrap(),
                            );
                        }
                    })
                    .await
                    .expect("existing progress heartbeat must run before release");
                }
                release.send(()).unwrap();
                while let Some(chunk) = body.next().await {
                    rendered.push_str(std::str::from_utf8(&chunk.unwrap()).unwrap());
                }
                if ending == "complete" {
                    assert!(rendered.contains("selected complete"));
                    assert_eq!(rendered.matches("event: response.completed").count(), 1);
                    wait_for_request_settlement(&fixture, 1).await;
                    let rows = fixture
                        .state
                        .db
                        .list_requests(fixture.key_id, 10)
                        .await
                        .unwrap();
                    assert_eq!(
                        rows[0].upstream_account_id,
                        Some(fixture.upstream_account_id)
                    );
                    assert_eq!((rows[0].input_tokens, rows[0].output_tokens), (3, 2));
                    assert_exactly_once_side_effects(
                        &fixture,
                        rows[0].request_id,
                        Some("resp-codex"),
                    )
                    .await;
                } else {
                    assert!(rendered.contains("response.failed"));
                    assert!(!rendered.contains("response.completed"));
                    cancellation::assert_single_unknown_receipt(
                        &fixture,
                        502,
                        Some("upstream_failed_response"),
                    )
                    .await;
                }
            }
            assert_eq!(standby_posts.load(Ordering::SeqCst), 0);
            server.abort();
        }
    }
}

#[tokio::test]
async fn preoutput_nonquota_sse_error_remains_failure_without_replay() {
    for (label, error, execution) in [
        ("auth", json!({"type":"authentication_error"}), json!({})),
        ("overload", json!({"type":"overloaded_error"}), json!({})),
        (
            "conflict",
            json!({"code":"invalid_api_key","type":"usage_limit_reached"}),
            json!({}),
        ),
        (
            "message-conflict",
            json!({"type":"rate_limit_error","message":"Invalid API key"}),
            json!({}),
        ),
        (
            "rate-usage",
            json!({"type":"rate_limit_error"}),
            json!({"usage":{"input_tokens":4}}),
        ),
        (
            "quota-usage",
            json!({"code":null,"type":"usage_limit_reached"}),
            json!({"usage":{"input_tokens":4}}),
        ),
        (
            "rate-tool",
            json!({"type":"rate_limit_error"}),
            json!({"output":[{"type":"function_call","id":"call-a"}]}),
        ),
        (
            "quota-tool",
            json!({"code":null,"type":"usage_limit_reached"}),
            json!({"output":[{"type":"function_call","id":"call-a"}]}),
        ),
        (
            "rate-content",
            json!({"type":"rate_limit_error"}),
            json!({"output":[{"type":"message","content":[{"type":"output_text","text":"generated"}]}]}),
        ),
        (
            "quota-content",
            json!({"code":null,"type":"usage_limit_reached"}),
            json!({"output":[{"type":"message","content":[{"type":"output_text","text":"generated"}]}]}),
        ),
    ] {
        let label = format!("sse-{label}-no-replay");
        let fixture = codex_route_fixture(&label).await;
        let upstream = MockServer::start().await;
        let mut rejected = execution;
        rejected["error"] = error;
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-123"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                format!(
                    "event: response.failed\ndata: {}\n\n",
                    json!({"type":"response.failed","response":rejected})
                ),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-456"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("must not replay"), "text/event-stream"),
            )
            .expect(0)
            .mount(&upstream)
            .await;
        add_codex_standby_route(&fixture, &format!("codex-route-{label}"), "account-456").await;
        let response = send_codex_route(
            &fixture,
            &upstream,
            "/v1/responses",
            json!({"model":fixture.model,"input":"denied","stream":true}),
        )
        .await;
        let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("response.failed"), "{text}");
        assert!(!text.contains("must not replay"), "{text}");
        assert!(!text.contains("response.completed"), "{text}");
        fixture.state.routing_persistence.drain_for_test().await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM upstream_supplier_availability")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(count, 0);
        upstream.verify().await;
        pool.close().await;
    }
}

#[tokio::test]
async fn chat_adapter_quota_is_classified_before_responses_translation() {
    let rejected = MockServer::start().await;
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&rejected)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(completed_codex_sse("native standby"), "text/event-stream"),
        )
        .expect(1)
        .mount(&standby)
        .await;
    let fixture = resilient_route_fixture(
        "chat-adapter-quota",
        &[(rejected.uri(), 0), (standby.uri(), 10)],
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("UPDATE upstream_accounts SET config_json=$2 WHERE id=$1")
        .bind(fixture.accounts[0].to_string())
        .bind(json!({"base_url":rejected.uri(),"network_scope":"public","responses_transport":"chat_completions"}).to_string())
        .execute(&pool).await.unwrap();
    let request = Request::post("/v1/responses")
        .header(header::CONTENT_TYPE, "application/json")
        .header(header::AUTHORIZATION, format!("Bearer {}", fixture.key))
        .body(Body::from(
            json!({"model":fixture.model,"input":"quota","stream":true}).to_string(),
        ))
        .unwrap();
    let response = router_for_role(fixture.state.clone(), RuntimeRole::Gateway)
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("native standby"), "{text}");
    assert!(!text.contains("response.failed"), "{text}");
    rejected.verify().await;
    standby.verify().await;
    pool.close().await;
}

#[tokio::test]
async fn delayed_sse_quota_body_after_headers_still_switches_to_complete_standby() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let upstream = axum::Router::new().fallback(|| async {
        let body = futures_util::stream::once(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            Ok::<_, std::io::Error>(Bytes::from_static(
                b"data: {\"error\":{\"code\":\"insufficient_quota\"}}\n\n",
            ))
        });
        (
            [(header::CONTENT_TYPE, "text/event-stream")],
            Body::from_stream(body),
        )
    });
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });
    let standby = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&standby)
        .await;
    let fixture =
        resilient_route_fixture("delayed-sse-quota", &[(endpoint, 0), (standby.uri(), 10)]).await;
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    standby.verify().await;
    server.abort();
}

#[tokio::test]
async fn preoutput_sse_quota_and_rate_rejection_fail_over_with_one_complete_response() {
    for (label, rejection) in [
        (
            "failed",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"id\":\"resp-rejected\",\"status\":\"failed\",\"error\":{\"code\":\"insufficient_quota\"}}}\n\n",
        ),
        (
            "error",
            "event: error\ndata: {\"type\":\"error\",\"code\":\"rate_limit_exceeded\"}\n\n",
        ),
        (
            "nested",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"code\":\"insufficient_user_quota\"}}\n\n",
        ),
        (
            "provider-type",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"rate_limit_error\",\"message\":\"retry after plan window\"}}\n\n",
        ),
        (
            "null-code",
            "event: response.failed\ndata: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":null,\"type\":\"usage_limit_reached\"}}}\n\n",
        ),
    ] {
        let name = format!("stream-quota-{label}");
        let fixture = codex_route_fixture(&name).await;
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-123"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("retry-after", "120")
                    .set_body_raw(rejection, "text/event-stream"),
            )
            .expect(1)
            .mount(&upstream)
            .await;
        Mock::given(method("POST"))
            .and(header_matcher("chatgpt-account-id", "account-456"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(completed_codex_sse("standby complete"), "text/event-stream"),
            )
            .expect(2)
            .mount(&upstream)
            .await;
        let standby =
            add_codex_standby_route(&fixture, &format!("codex-route-{name}"), "account-456").await;
        for count in 1..=2 {
            let response = send_codex_route(
                &fixture,
                &upstream,
                "/v1/responses",
                json!({"model":fixture.model,"input":"quota failover","stream":true}),
            )
            .await;
            assert_eq!(response.status(), StatusCode::OK);
            let body = to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
                .await
                .unwrap();
            let text = String::from_utf8_lossy(&body);
            assert!(text.contains("standby complete"), "{text}");
            assert!(!text.contains("resp-rejected"), "{text}");
            assert!(!text.contains("response.failed"), "{text}");
            assert_eq!(text.matches("event: response.completed").count(), 1);
            wait_for_request_settlement(&fixture, count).await;
            fixture.state.routing_persistence.drain_for_test().await;
        }
        let rows = fixture
            .state
            .db
            .list_requests(fixture.key_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 2);
        for row in rows {
            assert_eq!(row.upstream_account_id, Some(standby));
            assert_eq!((row.input_tokens, row.output_tokens), (3, 2));
            assert_eq!(row.error_code, None);
        }
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM ledger_entries l JOIN request_records r ON l.source=r.reservation_id WHERE r.key_id=$1",
        ).bind(fixture.key_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(count, 2);
        let failed_model: String = sqlx::query_scalar(
            "SELECT upstream_model FROM upstream_supplier_availability WHERE upstream_account_id=$1",
        ).bind(fixture.upstream_account_id.to_string()).fetch_one(&pool).await.unwrap();
        assert_eq!(failed_model, fixture.upstream_model);
        upstream.verify().await;
        pool.close().await;
    }
}

#[tokio::test]
async fn definitive_rejections_reach_fourth_authorized_model_alias_candidate() {
    let rejected = MockServer::start().await;
    let healthy = MockServer::start().await;
    let unauthorized = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "120")
                .set_body_json(json!({"error":{"code":"insufficient_quota"}})),
        )
        .expect(3)
        .mount(&rejected)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(1)
        .mount(&healthy)
        .await;
    Mock::given(method("POST"))
        .respond_with(successful_chat_response())
        .expect(0)
        .mount(&unauthorized)
        .await;
    let fixture = resilient_route_fixture(
        "quota-fourth-alias",
        &[
            (rejected.uri(), 0),
            (rejected.uri(), 10),
            (rejected.uri(), 20),
            (healthy.uri(), 30),
            (unauthorized.uri(), -10),
        ],
    )
    .await;
    let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
    sqlx::query("DELETE FROM routing_grants WHERE key_id=$1 AND model_route_id IN (SELECT id FROM model_routes WHERE upstream_account_id=$2)")
        .bind(fixture.key_id.to_string()).bind(fixture.accounts[4].to_string())
        .execute(&pool).await.unwrap();
    let duplicate = fixture
        .state
        .db
        .create_model_route(CreateModelRouteInput {
            tenant_external_id: "resilient-quota-fourth-alias".into(),
            public_model: fixture.model.clone(),
            upstream_account_id: fixture.accounts[0],
            upstream_model: "upstream-0".into(),
            protocol: "openai".into(),
            priority: 5,
        })
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO routing_grants (tenant_id,key_id,model_route_id,route_group_id,created_at)
         SELECT tenant_id,id,$2,NULL,$3 FROM key_records WHERE id=$1",
    )
    .bind(fixture.key_id.to_string())
    .bind(duplicate.id.to_string())
    .bind(unix_millis())
    .execute(&pool)
    .await
    .unwrap();
    let response = send_resilient_chat(&fixture, None, false).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), MAX_PROXY_RESPONSE_BODY)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["choices"][0]["message"]["content"], "ok");
    let requests = healthy.received_requests().await.unwrap();
    let forwarded: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(forwarded["model"], "upstream-3");
    rejected.verify().await;
    healthy.verify().await;
    unauthorized.verify().await;
    pool.close().await;
}
