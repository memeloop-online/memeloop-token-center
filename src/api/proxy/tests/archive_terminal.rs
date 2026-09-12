use super::*;

#[tokio::test]
async fn incomplete_or_failed_tail_revokes_held_success_terminal() {
    for (label, payload, expected_gap) in [
        (
            "incomplete",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
                "data: [DONE]\n\ndata: ",
            ),
            true,
        ),
        (
            "failed",
            concat!(
                "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
                "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"fixture\"}}\n\n",
                "data: {\"error\":{\"message\":\"private provider detail\"}}\n\n",
            ),
            false,
        ),
    ] {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(payload, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = resilient_route_fixture(label, &[(upstream.uri(), 0)]).await;
        let response = send_resilient_chat(&fixture, None, true).await;
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let text = std::str::from_utf8(&body).unwrap();
        assert!(text.contains("usable text"));
        assert!(text.contains("upstream stream did not complete"));
        assert!(!text.contains("[DONE]"));
        assert!(!text.contains("response.completed"));
        assert!(!text.contains("private provider detail"));
        if expected_gap {
            let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
            let state: String = sqlx::query_scalar("SELECT state FROM response_archive_spools")
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(state, "gap");
            pool.close().await;
        }
        upstream.verify().await;
    }
}

#[tokio::test]
async fn terminal_delivery_observes_sealed_spool_or_explicit_capture_gap() {
    for fail_append in [false, true] {
        let upstream = MockServer::start().await;
        let payload = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"usable text\"}}]}\n\n",
            "data: [DONE]\n\n",
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(payload, "text/event-stream"))
            .expect(1)
            .mount(&upstream)
            .await;
        let fixture = resilient_route_fixture(
            if fail_append {
                "terminal-gap"
            } else {
                "terminal-sealed"
            },
            &[(upstream.uri(), 0)],
        )
        .await;
        let pool = sqlx::AnyPool::connect(&fixture.database_url).await.unwrap();
        if fail_append {
            // A SQLite RAISE(ABORT) queues transaction rollback when SQLx drops
            // the failed append. Under executor load that rollback can delay
            // the gap write past its separate ACK budget, injecting two
            // failures instead of the one this contract covers. The
            // fixture-scoped, one-shot latch fails only the producer append and
            // leaves gap persistence healthy.
            crate::response_archive_spool::fail_next_append_for_test(&fixture.state);
        }
        let response = send_resilient_chat(&fixture, None, true).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        assert_eq!(body.as_ref(), payload.as_bytes());
        // No polling: receipt of terminal/EOF itself guarantees that capture
        // is no longer left as an unrecoverable "capturing" success.
        let (state, gap_reason): (String, Option<String>) =
            sqlx::query_as("SELECT state, last_error_code FROM response_archive_spools")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(state, if fail_append { "gap" } else { "pending" });
        assert_eq!(
            gap_reason.as_deref(),
            fail_append.then_some("capture_failed")
        );
        upstream.verify().await;
        pool.close().await;
    }
}
