#[test]
fn dialogue_parts_preserve_tools_media_and_future_extensions() {
    for part in [
        json!({"kind":"tool_call","part_index":1,"tool_name":"echo","arguments":{"nested":[1,"完整"]},"arguments_encoding":"json","completeness":"complete","call_id":"call-1","future":{"preserved":true}}),
        json!({"kind":"tool_result","part_index":2,"result_content":{"text":"完整工具结果"},"result_status":"complete","tool_call_id":"call-1"}),
        json!({"kind":"media","part_index":3,"media_type":"audio/wav","byte_length":123,"locator":{"digest":"abc"}}),
    ] {
        let converted = serde_json::to_value(dialogue_part(part.clone())).unwrap();
        for (key, value) in part.as_object().unwrap() {
            assert_eq!(&converted[key], value, "part field {key}");
        }
    }
}

#[tokio::test]
async fn canonical_training_reads_complete_prefix_without_legacy_root_and_fails_closed() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("canonical.db").display()
    ));
    if let Ok(endpoint) = std::env::var("MTC_RUSTFS_TEST_ENDPOINT") {
        config.archive_backend = crate::config::ArchiveBackend::S3;
        config.s3_endpoint = Some(endpoint);
        config.s3_bucket = Some(std::env::var("MTC_RUSTFS_TEST_BUCKET").unwrap());
        config.s3_access_key = Some(std::env::var("MTC_RUSTFS_TEST_ACCESS_KEY").unwrap());
        config.s3_secret_key = Some(std::env::var("MTC_RUSTFS_TEST_SECRET_KEY").unwrap());
        config.archive_object_compression_enabled = true;
    }
    let state = AppState::initialize(config).await.unwrap();
    let owner = client(&state, "canonical-training").await;
    let key = state
        .db
        .authenticate_key(&owner.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let request = Uuid::now_v7();
    let mut transaction = state.db.begin_write_transaction().await.unwrap();
    let request_json = json!({"messages":[
        {"role":"user","content":"完整问题"},
        {"role":"assistant","tool_calls":[{"id":"call-1","type":"function","function":{"name":"echo","arguments":"{\"message\":\"完整\"}"}}]},
        {"role":"tool","tool_call_id":"call-1","content":"完整工具结果"},
        {"role":"assistant","content":"完整答案"}
    ]});
    let session = state
        .db
        .record_conversation_observation_in_transaction(
            &mut transaction,
            crate::db::ConversationObservationInput {
                key: &key,
                request_id: request,
                request_json: &request_json,
                hints: &crate::conversation::ConversationHints::default(),
                client_name: Some("canonical-training"),
                upstream_response_id: None,
                observed_at: crate::db::unix_millis(),
                attach_request_record: false,
                content_materialized: false,
            },
        )
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    // Persist the same canonical turns and immutable objects produced by the body worker.
    // The observation above supplies the real tenant/key/session authorization boundary.
    let body = crate::conversation::session_body_from_documents(&request_json, &json!({}));
    let (atoms, nodes) = body.into_prefix();
    let inspection = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    for atom in &atoms {
        let reference = state
            .db
            .store_body_reference(key.tenant_id, atom.content.to_string())
            .await
            .unwrap();
        sqlx::query("INSERT INTO semantic_atoms (tenant_id,content_hash,instance_hash,role,kind,content_json,created_at) VALUES ($1,$2,$3,$4,$5,$6,1) ON CONFLICT DO NOTHING")
            .bind(key.tenant_id.to_string()).bind(&atom.content_hash).bind(&atom.instance_hash).bind(&atom.role).bind(&atom.kind).bind(reference).execute(&inspection).await.unwrap();
    }
    for node in &nodes {
        sqlx::query("INSERT INTO context_nodes (tenant_id,node_hash,parent_hash,atom_hash,depth,created_at) VALUES ($1,$2,$3,$4,$5,1) ON CONFLICT DO NOTHING")
            .bind(key.tenant_id.to_string()).bind(&node.node_hash).bind(&node.parent_hash).bind(&node.atom_hash).bind(i64::try_from(node.depth).unwrap()).execute(&inspection).await.unwrap();
    }
    sqlx::query("UPDATE conversation_observations SET leaf_node_hash=$1 WHERE request_id=$2")
        .bind(&nodes.last().unwrap().node_hash)
        .bind(request.to_string())
        .execute(&inspection)
        .await
        .unwrap();
    inspection.close().await;

    assert!(
        state
            .archive
            .raw_capture_reference(key.tenant_id, request, "request")
            .await
            .unwrap()
            .is_none()
    );
    let service = state
        .db
        .create_service_token(
            crate::db::CreateServiceTokenInput {
                name: "canonical-training-reader".into(),
                scopes: vec!["requests:read".into()],
                tenant_external_id: Some("canonical-training".into()),
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let query = format!(
        "tenant_external_id=canonical-training&key_id={}",
        key.key_id
    );
    let route = format!("/internal/v1/sessions/{session}/requests/{request}/training?{query}");
    let response = http(&state, "GET", &route, &service.token).await;
    assert_eq!(response.status(), StatusCode::OK);
    use futures_util::StreamExt as _;
    let mut frames = response.into_body().into_data_stream();
    let mut bytes = Vec::new();
    let mut frame_count = 0;
    while let Some(frame) = frames.next().await {
        let frame = frame.unwrap();
        assert_eq!(frame.last(), Some(&b'\n'));
        frame_count += 1;
        bytes.extend_from_slice(&frame);
    }
    assert_eq!(
        frame_count, 6,
        "manifest, four turns, and end stream independently"
    );
    let records: Vec<Value> = std::str::from_utf8(&bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records[0]["contract_version"], "session_training_v2");
    assert_eq!(records[0]["turn_order"], "ascending");
    assert_eq!(records.last().unwrap()["turn_count"], 4);
    assert_eq!(records[1]["role"], "user");
    assert_eq!(records[2]["role"], "assistant");
    assert_eq!(records[3]["role"], "tool");
    assert_eq!(records[4]["role"], "assistant");
    assert_eq!(records[2]["parts"][0]["tool_name"], "echo");
    assert_eq!(
        records[2]["parts"][0]["arguments"],
        json!({"message":"完整"})
    );
    assert_eq!(records[3]["parts"][0]["result_content"], "完整工具结果");
    assert_eq!(
        records[2]["parts"][0]["call_id"],
        records[3]["parts"][0]["tool_call_id"]
    );
    assert_eq!(records[4]["parts"][0]["text"], "完整答案");

    let dialogue_path = format!("/self/v1/sessions/{session}/requests/{request}/dialogue");
    let response = http(&state, "GET", &dialogue_path, &owner.key).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    for (index, turn) in body["turns"].as_array().unwrap().iter().enumerate() {
        for (part, original) in turn["parts"]
            .as_array()
            .unwrap()
            .iter()
            .zip(records[index + 1]["parts"].as_array().unwrap())
        {
            for (key, value) in original.as_object().unwrap() {
                assert_eq!(&part[key], value);
            }
        }
    }
    let foreign = client(&state, "canonical-training-foreign").await;
    assert_eq!(
        http(&state, "GET", &dialogue_path, &foreign.key)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let inspection = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    let leaf: String = sqlx::query_scalar(
        "SELECT leaf_node_hash FROM conversation_observations WHERE request_id=$1",
    )
    .bind(request.to_string())
    .fetch_one(&inspection)
    .await
    .unwrap();
    sqlx::query("DELETE FROM semantic_atoms WHERE tenant_id=$1 AND content_hash=(SELECT atom_hash FROM context_nodes WHERE tenant_id=$1 AND node_hash=$2)")
        .bind(key.tenant_id.to_string()).bind(leaf).execute(&inspection).await.unwrap();
    let response = http(&state, "GET", &route, &service.token).await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
}
