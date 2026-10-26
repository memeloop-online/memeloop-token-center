use super::super::*;

#[tokio::test]
async fn sqlite_open_request_keeps_a_recorded_stage_without_rewriting_settlement() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("in-flight-stage.db").display()
    );
    assert_in_flight_stage_contract(&database_url).await;
}

#[tokio::test]
async fn postgres_open_request_keeps_a_recorded_stage_without_rewriting_settlement() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    assert_in_flight_stage_contract(&database_url).await;
}

async fn assert_in_flight_stage_contract(database_url: &str) {
    let database = Database::connect(database_url).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"in-flight stage contract pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("in-flight-stage-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "in-flight-stage".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::from(10),
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let key = database
        .authenticate_key(&issued.key, pepper)
        .await
        .unwrap();
    let model = format!("in-flight-stage-{unique}");
    let price = database
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            protocol: "openai-responses",
            model: &model,
            request_object: "gap://in-flight-stage/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();

    let stage_of = |rows: &[RequestView]| {
        rows.iter()
            .find(|row| row.request_id == request_id)
            .map(|row| {
                (
                    row.status_code,
                    row.error_code.clone(),
                    row.in_flight_stage.clone(),
                )
            })
    };
    let admitted = database.list_requests(key.key_id, 20).await.unwrap();
    assert_eq!(stage_of(&admitted), Some((None, None, None)));

    for stage in [
        "upstream_response_headers",
        "upstream_response_body",
        "response_delivery",
    ] {
        database
            .record_proxy_in_flight_stage(
                request_id,
                key.tenant_id,
                key.key_id,
                reservation.id,
                stage,
            )
            .await
            .unwrap();
        let listed = database.list_requests(key.key_id, 20).await.unwrap();
        assert_eq!(
            stage_of(&listed),
            Some((None, None, Some(stage.to_owned()))),
            "{stage}"
        );
        let detail = database
            .request_archive_refs(key.key_id, request_id)
            .await
            .unwrap();
        assert_eq!(detail.view.status_code, None, "{stage}");
        assert_eq!(detail.view.error_code, None, "{stage}");
        assert_eq!(detail.view.in_flight_stage.as_deref(), Some(stage), "{stage}");
        let error_code: Option<String> =
            sqlx::query_scalar("SELECT error_code FROM request_records WHERE id = $1")
                .bind(request_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(error_code, None, "{stage} must not become a settlement marker");
    }
    assert!(
        database
            .record_proxy_in_flight_stage(
                request_id,
                key.tenant_id,
                key.key_id,
                reservation.id,
                "delivery_prepared",
            )
            .await
            .is_err()
    );
    let unchanged: Option<String> =
        sqlx::query_scalar("SELECT in_flight_stage FROM request_records WHERE id = $1")
            .bind(request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(unchanged.as_deref(), Some("response_delivery"));

    database
        .finish_proxy_request(FinishProxyRequest {
            terminal_cause: Some(crate::model::RequestTerminalCause::UpstreamHttpStatus),
            usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id: key.tenant_id,
            reservation: &reservation,
            input_token_ceiling: 1,
            output_token_ceiling: 1,
            requested_service_tier: None,
            status_code: 502,
            duration_ms: 1,
            usage: TokenUsage::default(),
            error_code: Some("http_502"),
            response_object: "gap://in-flight-stage/response",
            routing_session_id: None,
            routing_terminal_observed_at: None,
            conversation: None,
        })
        .await
        .unwrap();
    database
        .record_proxy_in_flight_stage(
            request_id,
            key.tenant_id,
            key.key_id,
            reservation.id,
            "upstream_response_headers",
        )
        .await
        .unwrap();
    let bind = request_id.to_string();
    let error_code: Option<String> =
        sqlx::query_scalar("SELECT error_code FROM request_records WHERE id = $1")
            .bind(&bind)
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let stage: Option<String> =
        sqlx::query_scalar("SELECT in_flight_stage FROM request_records WHERE id = $1")
            .bind(&bind)
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let status_code: Option<i64> =
        sqlx::query_scalar("SELECT status_code FROM request_records WHERE id = $1")
            .bind(&bind)
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(error_code.as_deref(), Some("http_502"));
    assert_eq!(stage.as_deref(), Some("response_delivery"));
    assert_eq!(status_code, Some(502));
}
