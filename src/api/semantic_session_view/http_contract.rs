//! The public cursor chain against the same canonical atoms used by training.
use super::*;
use crate::archive::{
    SemanticCoverage, SemanticHead, SemanticInstancePage, SemanticManifestV2, SemanticNode,
    SemanticProjectionState, SemanticTurnInstance,
};
use axum::{
    body::to_bytes,
    http::{Request, StatusCode},
};
use bytes::Bytes;
use serde_json::json;
use tower::ServiceExt;

async fn get(state: &AppState, path: &str, token: &str) -> Response {
    crate::api::router_for_role(state.clone(), crate::config::RuntimeRole::All)
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}
async fn value(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap()
}

#[tokio::test]
async fn canonical_http_fields_match_training_and_keep_occurrence_scope() {
    let directory = tempfile::tempdir().unwrap();
    let mut config = crate::config::Config::for_test(format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("reader.db").display()
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
    let issued = state
        .db
        .create_key(
            crate::db::CreateKeyInput {
                tenant_external_id: "reader-contract".into(),
                principal_external_id: "member".into(),
                alias: "reader".into(),
                currency: "USD".into(),
                policy: crate::model::KeyPolicy::default(),
                initial_balance: rust_decimal::Decimal::TEN,
                idempotency_key: None,
            },
            state.config.key_pepper.as_bytes(),
        )
        .await
        .unwrap();
    let key = state
        .db
        .authenticate_key(&issued.key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let long = "工具😀".repeat(20000);
    let mut messages = (0..35)
        .map(|i| json!({"role":"user","content":format!("message {i}")}))
        .collect::<Vec<_>>();
    messages[0] = json!({"role":"user","content":long});
    messages[34] = messages[0].clone();
    messages[1] = json!({"role":"assistant","tool_calls":[{"id":"call","type":"function","function":{"name":"echo","arguments":serde_json::to_string(&json!({"input":long})).unwrap()}}]});
    messages[2] = json!({"role":"tool","tool_call_id":"call","content":long});
    messages[33] = json!({"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,cmVhZGVyLW1lZGlh"}}]});
    let document = json!({"messages":messages});
    let request = Uuid::now_v7();
    let mut transaction = state.db.begin_write_transaction().await.unwrap();
    let session = state
        .db
        .record_conversation_observation_in_transaction(
            &mut transaction,
            crate::db::ConversationObservationInput {
                key: &key,
                request_id: request,
                request_json: &document,
                hints: &crate::conversation::ConversationHints::default(),
                client_name: None,
                upstream_response_id: None,
                observed_at: crate::db::unix_millis(),
                attach_request_record: false,
                content_materialized: false,
            },
        )
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let mut body = crate::conversation::session_body_from_documents(&document, &json!({}));
    let mut media_count = 0;
    for turn in &mut body.turns {
        for part in &mut turn.parts {
            if let Some(bytes) = part.media_bytes.take() {
                let object = state
                    .archive
                    .put_tenant_content(key.tenant_id, Bytes::from(bytes))
                    .await
                    .unwrap();
                part.byte_length = Some(object.size_bytes);
                part.content_blake3 = Some(object.blake3_digest);
                part.object_locator = Some(object.object_locator);
                media_count += 1;
            }
        }
    }
    assert_eq!(media_count, 1);
    let (atoms, nodes) = body.into_prefix();
    assert_eq!(atoms.len(), 35);
    assert_eq!(atoms[0].content_hash, atoms[34].content_hash);
    let mut manifest = SemanticManifestV2 {
        format_version: 2,
        projection_version: 1,
        tenant_id: key.tenant_id,
        session_id: session.to_string(),
        branch_id: "main".into(),
        parent_branch_id: None,
        parent_turn_id: None,
        request_id: request,
        revision: 1,
        backend_identity: None,
        semantic_head: Some(SemanticHead {
            node_hash: nodes.last().unwrap().node_hash.clone(),
            depth: nodes.len() as u64,
        }),
        instances_head: None,
        coverage: SemanticCoverage {
            observed_terminal: true,
            projection_state: SemanticProjectionState::Complete,
            omissions: vec![],
            gaps: vec![],
        },
        raw_sources: None,
    };
    let pool = sqlx::AnyPool::connect(&state.config.database_url)
        .await
        .unwrap();
    let mut instances = Vec::new();
    for (ordinal, (atom, node)) in atoms.iter().zip(&nodes).enumerate() {
        let object = state
            .archive
            .put_tenant_content(
                key.tenant_id,
                Bytes::from(serde_json::to_vec(&atom.content).unwrap()),
            )
            .await
            .unwrap();
        let reference = state
            .db
            .store_body_reference(key.tenant_id, atom.content.to_string())
            .await
            .unwrap();
        sqlx::query("INSERT INTO semantic_atoms (tenant_id,content_hash,instance_hash,role,kind,content_json,created_at) VALUES ($1,$2,$3,$4,$5,$6,1) ON CONFLICT DO NOTHING").bind(key.tenant_id.to_string()).bind(&atom.content_hash).bind(&atom.instance_hash).bind(&atom.role).bind(&atom.kind).bind(reference).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO context_nodes (tenant_id,node_hash,parent_hash,atom_hash,depth,created_at) VALUES ($1,$2,$3,$4,$5,1) ON CONFLICT DO NOTHING").bind(key.tenant_id.to_string()).bind(&node.node_hash).bind(&node.parent_hash).bind(&node.atom_hash).bind(node.depth as i64).execute(&pool).await.unwrap();
        state
            .archive
            .put_semantic_prefix_node(
                &manifest,
                &SemanticNode {
                    node_hash: node.node_hash.clone(),
                    parent_hash: node.parent_hash.clone(),
                    atom_hash: node.atom_hash.clone(),
                    depth: node.depth as u64,
                },
            )
            .await
            .unwrap();
        instances.push(SemanticTurnInstance {
            role: Some(atom.role.clone()),
            instance_id: format!("occurrence-{ordinal}"),
            request_id: request,
            attempt_id: None,
            message_id: None,
            turn_id: None,
            branch_id: "main".into(),
            parent_instance_id: None,
            origin: "request".into(),
            ordinal: ordinal as u64,
            atom_hash: atom.content_hash.clone(),
            node_hash: node.node_hash.clone(),
            terminal_state: "complete".into(),
            body: object,
        });
    }
    for (page_index, page) in instances.chunks(32).enumerate() {
        manifest.instances_head = Some(
            state
                .archive
                .put_semantic_instance_page(
                    &manifest,
                    &SemanticInstancePage {
                        format_version: 2,
                        tenant_id: key.tenant_id,
                        session_id: session.to_string(),
                        branch_id: "main".into(),
                        first_ordinal: (page_index * 32) as u64,
                        previous: manifest.instances_head.clone(),
                        instances: page.to_vec(),
                    },
                )
                .await
                .unwrap(),
        );
    }
    state
        .archive
        .publish_semantic_manifest(&manifest)
        .await
        .unwrap();
    sqlx::query("UPDATE conversation_observations SET leaf_node_hash=$1 WHERE request_id=$2")
        .bind(&nodes.last().unwrap().node_hash)
        .bind(request.to_string())
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let training = state
        .db
        .export_training_dialogue(key.tenant_id, &nodes.last().unwrap().node_hash)
        .await
        .unwrap();
    let path = format!("/self/v1/sessions/{session}/requests/{request}/semantic-view");
    let mut page = value(get(&state, &path, &issued.key).await).await;
    let mut observed = Vec::new();
    loop {
        observed.extend(page["instances"].as_array().unwrap().iter().cloned());
        let Some(cursor) = page["next_cursor"].as_str() else {
            break;
        };
        page = value(get(&state, &format!("{path}?cursor={cursor}"), &issued.key).await).await;
    }
    assert_eq!(observed.len(), 35);
    for instance in &observed {
        let id = instance["instance_id"].as_str().unwrap();
        let ordinal = id
            .strip_prefix("occurrence-")
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let parts = value(
            get(
                &state,
                &format!(
                    "{path}?parts_cursor={}",
                    instance["atom_cursor"].as_str().unwrap()
                ),
                &issued.key,
            )
            .await,
        )
        .await;
        assert_eq!(parts["instance_id"], id);
        for part in parts["parts"].as_array().unwrap() {
            if let Some(cursor) = part["media"]["read_cursor"].as_str() {
                let response = get(
                    &state,
                    &format!("{path}?media_cursor={cursor}"),
                    &issued.key,
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    "application/octet-stream"
                );
                assert_eq!(
                    response.headers()[header::CONTENT_DISPOSITION],
                    "attachment"
                );
                assert_eq!(
                    &to_bytes(response.into_body(), usize::MAX).await.unwrap()[..],
                    b"reader-media"
                );
            }
            for field in part["fields"].as_array().unwrap() {
                let response = get(
                    &state,
                    &format!(
                        "{path}?content_cursor={}",
                        field["content_cursor"].as_str().unwrap()
                    ),
                    &issued.key,
                )
                .await;
                assert_eq!(response.status(), StatusCode::OK);
                assert_eq!(
                    response.headers()[header::CONTENT_TYPE],
                    "application/x-ndjson"
                );
                let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
                let mut text = String::new();
                let mut complete = false;
                for line in bytes.split(|b| *b == b'\n').filter(|line| !line.is_empty()) {
                    let frame: Value = serde_json::from_slice(line).unwrap();
                    assert!(!complete);
                    assert_eq!(frame["instance_id"], id);
                    assert_eq!(frame["byte_offset"], text.len() as u64);
                    text.push_str(frame["text"].as_str().unwrap());
                    complete = frame["complete"].as_bool().unwrap();
                }
                assert!(complete);
                let expected = &training[ordinal].parts
                    [part["part_index"].as_u64().unwrap() as usize]
                    [field["name"].as_str().unwrap()];
                if field["format"] == "json" {
                    assert_eq!(&serde_json::from_str::<Value>(&text).unwrap(), expected);
                } else {
                    assert_eq!(Some(text.as_str()), expected.as_str());
                }
            }
        }
    }
    let token = observed[0]["atom_cursor"].as_str().unwrap();
    assert_eq!(
        get(
            &state,
            &format!(
                "/self/v1/sessions/{session}/requests/{}/semantic-view?parts_cursor={token}",
                Uuid::now_v7()
            ),
            &issued.key
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(
            &state,
            &format!("{path}?parts_cursor={token}&atom_cursor={token}"),
            &issued.key
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    manifest.revision = 2;
    state
        .archive
        .publish_semantic_manifest(&manifest)
        .await
        .unwrap();
    assert_eq!(
        get(&state, &format!("{path}?parts_cursor={token}"), &issued.key)
            .await
            .status(),
        StatusCode::CONFLICT
    );
}
