use super::super::*;

struct CompletedSessionRequest<'a> {
    key: &'a AuthenticatedKey,
    price: &'a ModelPrice,
    model: &'a str,
    protocol: &'a str,
    session_id: &'a str,
    model_route_id: Uuid,
    upstream_account_id: Uuid,
    status_code: i64,
    error_code: Option<&'a str>,
}

async fn insert_completed_session_request(database: &Database, input: CompletedSessionRequest<'_>) {
    let CompletedSessionRequest {
        key,
        price,
        model,
        protocol,
        session_id,
        model_route_id,
        upstream_account_id,
        status_code,
        error_code,
    } = input;
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key,
            price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol,
            model,
            request_object: "gap://session-transport-avoid/request",
            upstream_account_id: Some(upstream_account_id),
            model_route_id: Some(model_route_id),
        })
        .await
        .unwrap();
    database
        .finish_proxy_request(FinishProxyRequest {
            usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id,
            tenant_id: key.tenant_id,
            reservation: &reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            requested_service_tier: None,
            status_code,
            duration_ms: 1,
            usage: TokenUsage::default(),
            error_code,
            response_object: "gap://session-transport-avoid/response",
            routing_session_id: Some(session_id),
            // This deliberately models deferred/skipped semantic projection.
            // Routing evidence must commit without conversation content.
            conversation: None,
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn synchronous_session_terminal_evidence_is_immediate_and_route_scoped() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("session-transport-avoid.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"session transport avoid test pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "session-transport-avoid".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "session-transport-avoid".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    enforcement_mode: EnforcementMode::MeteredUnlimited,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
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
    let model_a = "session-transport-model-a";
    let model_b = "session-transport-model-b";
    let price_a = database
        .upsert_model_price(model_a, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let price_b = database
        .upsert_model_price(model_b, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let failed_account = Uuid::now_v7();
    let recovered_account = Uuid::now_v7();
    let failed_route = Uuid::now_v7();
    let recovered_route = Uuid::now_v7();
    let session_id = "explicit-session-transport-avoid";

    insert_completed_session_request(
        &database,
        CompletedSessionRequest {
            key: &key,
            price: &price_a,
            model: model_a,
            protocol: "openai-responses",
            session_id,
            model_route_id: failed_route,
            upstream_account_id: failed_account,
            status_code: 502,
            error_code: Some("upstream_transport_connection_reset"),
        },
    )
    .await;
    assert_eq!(
        database
            .latest_session_transport_route_to_avoid(&key, session_id, model_a, "openai-responses",)
            .await
            .unwrap(),
        Some((failed_route, failed_account))
    );

    // A terminal for another model or protocol cannot change this model's
    // next-request ordering.
    assert_eq!(
        database
            .latest_session_transport_route_to_avoid(&key, session_id, model_b, "openai-responses",)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        database
            .latest_session_transport_route_to_avoid(&key, session_id, model_a, "openai-chat",)
            .await
            .unwrap(),
        None
    );

    // A success in another session may heal global account health, but cannot
    // erase this session's terminal evidence.
    insert_completed_session_request(
        &database,
        CompletedSessionRequest {
            key: &key,
            price: &price_b,
            model: model_b,
            protocol: "openai-responses",
            session_id: "unrelated-successful-session",
            model_route_id: recovered_route,
            upstream_account_id: recovered_account,
            status_code: 200,
            error_code: None,
        },
    )
    .await;
    assert_eq!(
        database
            .latest_session_transport_route_to_avoid(&key, session_id, model_a, "openai-responses",)
            .await
            .unwrap(),
        Some((failed_route, failed_account))
    );

    // A newer terminal for the same session/model/protocol supersedes the
    // failed route. A non-transport 502 is intentionally not avoidance proof.
    insert_completed_session_request(
        &database,
        CompletedSessionRequest {
            key: &key,
            price: &price_a,
            model: model_a,
            protocol: "openai-responses",
            session_id,
            model_route_id: recovered_route,
            upstream_account_id: failed_account,
            status_code: 502,
            error_code: Some("upstream_invalid_response"),
        },
    )
    .await;
    assert_eq!(
        database
            .latest_session_transport_route_to_avoid(&key, session_id, model_a, "openai-responses",)
            .await
            .unwrap(),
        None
    );

    sqlx::query("UPDATE session_routing_terminals SET expires_at = 0")
        .execute(&database.pool)
        .await
        .unwrap();
    assert_eq!(
        database
            .delete_expired_session_routing_terminals(10)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn postgres_late_streaming_parent_atomically_reconciles_committed_child_cluster() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"late streaming parent reconciliation pepper";
    let model = format!("late-streaming-parent-{unique}");
    let price = database
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let tenant_external_id = format!("late-streaming-parent-{unique}");
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant_external_id.clone(),
                principal_external_id: "member".to_owned(),
                alias: "late-streaming-parent".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
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
    let legacy_issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id,
                principal_external_id: "member".to_owned(),
                alias: "late-streaming-parent-legacy".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let legacy_key = database
        .authenticate_key(&legacy_issued.key, pepper)
        .await
        .unwrap();
    let foreign_issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("late-streaming-parent-foreign-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "late-streaming-parent-foreign".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            pepper,
        )
        .await
        .unwrap();
    let foreign_key = database
        .authenticate_key(&foreign_issued.key, pepper)
        .await
        .unwrap();
    let parent_request_id = Uuid::now_v7();
    let child_request_id = Uuid::now_v7();
    let foreign_child_request_id = Uuid::now_v7();
    let _parent_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: parent_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai-responses",
            model: &model,
            request_object: "objects/late-parent",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let child_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: child_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai-responses",
            model: &model,
            request_object: "objects/early-child",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let _foreign_child_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: foreign_child_request_id,
            key: &foreign_key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai-responses",
            model: &model,
            request_object: "objects/foreign-early-child",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();

    let response_id = format!("resp-late-parent-{unique}");
    let parent_json = serde_json::json!({
        "input": [{"role": "user", "content": "parent request"}]
    });
    let parent_hints = ConversationHints::default();
    let mut parent_transaction = database.begin_write_transaction().await.unwrap();
    database
        .record_conversation_observation_in_transaction(
            &mut parent_transaction,
            ConversationObservationInput {
                key: &key,
                request_id: parent_request_id,
                request_json: &parent_json,
                hints: &parent_hints,
                client_name: Some("Codex"),
                // Exercise the durable repair path for an already in-flight
                // terminal writer that did not preclaim the response lock.
                upstream_response_id: None,
                // A delayed terminal writer may acquire its observation clock
                // after the already-issued follow-up. Explicit response
                // identity must remain authoritative over this inversion.
                observed_at: unix_millis().saturating_add(60_000),
                attach_request_record: true,
                content_materialized: false,
            },
        )
        .await
        .unwrap();
    let parent_cluster: String = sqlx::query_scalar(
        "SELECT cluster_id FROM conversation_observations WHERE request_id = $1",
    )
    .bind(parent_request_id.to_string())
    .fetch_one(&mut *parent_transaction)
    .await
    .unwrap();
    let legacy_observation_id = Uuid::now_v7();
    let legacy_request_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, atom_hashes_json, created_at, inference_version) VALUES ($1, $2, $3, $4, '[]', $5, 2)",
    )
    .bind(legacy_observation_id.to_string())
    .bind(&parent_cluster)
    .bind(legacy_request_id.to_string())
    .bind(legacy_key.key_id.to_string())
    .bind(unix_millis())
    .execute(&mut *parent_transaction)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, NULL, $3, 1, 0)",
    )
    .bind(legacy_key.key_id.to_string())
    .bind(&parent_cluster)
    .bind(unix_millis())
    .execute(&mut *parent_transaction)
    .await
    .unwrap();

    // Model the production ordering directly: the parent stream has already
    // yielded its response id, but its terminal observation is still hidden in
    // this transaction while the follow-up request commits independently.
    let child_hints = ConversationHints {
        parent_turn_id: Some(response_id.clone()),
        ..ConversationHints::default()
    };
    let child_cluster = database
        .record_conversation_observation(
            &key,
            child_request_id,
            &serde_json::json!({
                "input": [{"role": "user", "content": "child request"}]
            }),
            &child_hints,
            Some("Codex"),
        )
        .await
        .unwrap();
    database
        .finish_proxy_request(FinishProxyRequest {
            usage_basis: None,
            first_output_ms: None,
            generation_duration_ms: None,
            request_id: child_request_id,
            tenant_id: key.tenant_id,
            reservation: &child_reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 17,
            usage: TokenUsage {
                input_tokens: 3,
                output_tokens: 2,
                ..TokenUsage::default()
            },
            error_code: None,
            response_object: "objects/early-child-response",
            routing_session_id: None,
            conversation: None,
        })
        .await
        .unwrap();
    let archive_request_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO session_archive_unlinked_requests (tenant_id, source, external_request_id, archive_request_id, key_id, principal_id, conversation_cluster_id, source_started_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, imported_at) VALUES ($1, 'codex', $2, $3, $4, $5, $6, $7, 'openai-responses', $8, 200, 23, 5, 7, $9)",
    )
    .bind(key.tenant_id.to_string())
    .bind(format!("archive-child-{unique}"))
    .bind(archive_request_id.to_string())
    .bind(key.key_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(child_cluster.to_string())
    .bind(unix_millis())
    .bind(&model)
    .bind(unix_millis())
    .execute(&database.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO session_archive_totals (tenant_id, key_id, session_id, last_activity_at, requests, errors, input_tokens, output_tokens, duration_count, duration_sum_ms) VALUES ($1, $2, $3, $4, 1, 0, 5, 7, 1, 23)",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(child_cluster.to_string())
    .bind(unix_millis())
    .execute(&database.pool)
    .await
    .unwrap();
    let foreign_cluster = database
        .record_conversation_observation(
            &foreign_key,
            foreign_child_request_id,
            &serde_json::json!({
                "input": [{"role": "user", "content": "foreign child request"}]
            }),
            &child_hints,
            Some("Codex"),
        )
        .await
        .unwrap();

    let unresolved_before_parent_commit: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2",
    )
    .bind(key.key_id.to_string())
    .bind(&response_id)
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(unresolved_before_parent_commit, 1);
    let visible_parent_observations: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1")
            .bind(parent_request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(visible_parent_observations, 0);
    let visible_child_cluster: String =
        sqlx::query_scalar("SELECT conversation_cluster_id FROM request_records WHERE id = $1")
            .bind(child_request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(visible_child_cluster, child_cluster.to_string());

    attach_conversation_upstream_response_in_transaction(
        &mut parent_transaction,
        database.backend,
        parent_request_id,
        &response_id,
    )
    .await
    .unwrap();
    parent_transaction.commit().await.unwrap();

    let memberships: Vec<String> = sqlx::query_scalar(
        "SELECT conversation_cluster_id FROM request_records WHERE id = $1 OR id = $2 ORDER BY id",
    )
    .bind(parent_request_id.to_string())
    .bind(child_request_id.to_string())
    .fetch_all(&database.pool)
    .await
    .unwrap();
    assert_eq!(memberships.len(), 2);
    assert_eq!(memberships[0], memberships[1]);
    assert_ne!(memberships[0], child_cluster.to_string());
    let reconciled: (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2), (SELECT COUNT(*) FROM conversation_edges edge JOIN conversation_observations parent ON parent.id = edge.from_observation_id JOIN conversation_observations child ON child.id = edge.to_observation_id WHERE parent.request_id = $3 AND child.request_id = $4 AND edge.relation_kind = 'continues' AND edge.cluster_id = parent.cluster_id AND edge.cluster_id = child.cluster_id), (SELECT COUNT(*) FROM conversation_clusters WHERE id = $5)",
    )
    .bind(key.key_id.to_string())
    .bind(&response_id)
    .bind(parent_request_id.to_string())
    .bind(child_request_id.to_string())
    .bind(child_cluster.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(reconciled, (0, 1, 0));
    let canonical_cluster = &memberships[0];
    let usage_projection: (String, i64, i64, i64) = sqlx::query_as(
        "SELECT fact.session_id, (SELECT COUNT(*) FROM session_usage_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3), (SELECT COUNT(*) FROM session_usage_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $4), (SELECT requests FROM session_usage_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3)
         FROM request_stats_facts fact WHERE fact.request_id = $5",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(canonical_cluster)
    .bind(child_cluster.to_string())
    .bind(child_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(usage_projection, (canonical_cluster.clone(), 1, 0, 1));
    let archive_projection: (String, i64, i64, i64, i64) = sqlx::query_as(
        "SELECT conversation_cluster_id, (SELECT COUNT(*) FROM session_archive_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3), (SELECT COUNT(*) FROM session_archive_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $4), (SELECT input_tokens FROM session_archive_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3), (SELECT output_tokens FROM session_archive_totals WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3) FROM session_archive_unlinked_requests WHERE archive_request_id = $5",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(canonical_cluster)
    .bind(child_cluster.to_string())
    .bind(archive_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(archive_projection, (canonical_cluster.clone(), 1, 0, 5, 7));
    let legacy_projection: (String, i64) = sqlx::query_as(
        "SELECT cluster_id, (SELECT COUNT(*) FROM conversation_key_clusters WHERE key_id = $1 AND cluster_id = $2) FROM conversation_observations WHERE id = $3",
    )
    .bind(legacy_key.key_id.to_string())
    .bind(canonical_cluster)
    .bind(legacy_observation_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(legacy_projection, (canonical_cluster.clone(), 1));

    database
        .attach_conversation_upstream_response(parent_request_id, &response_id)
        .await
        .unwrap();
    let edge_count_after_replay: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_edges edge JOIN conversation_observations parent ON parent.id = edge.from_observation_id JOIN conversation_observations child ON child.id = edge.to_observation_id WHERE parent.request_id = $1 AND child.request_id = $2",
    )
    .bind(parent_request_id.to_string())
    .bind(child_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(edge_count_after_replay, 1);
    let foreign_state: (i64, String) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2), conversation_cluster_id FROM request_records WHERE id = $3",
    )
    .bind(foreign_key.key_id.to_string())
    .bind(&response_id)
    .bind(foreign_child_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(foreign_state, (1, foreign_cluster.to_string()));

    // If the parent commits before the child acquires the reference lock, its
    // exact identity remains authoritative even when the parent's observation
    // clock is later than the child's.
    let reverse_parent_request_id = Uuid::now_v7();
    let reverse_child_request_id = Uuid::now_v7();
    for (request_id, object) in [
        (reverse_parent_request_id, "objects/reverse-parent"),
        (reverse_child_request_id, "objects/reverse-child"),
    ] {
        database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai-responses",
                model: &model,
                request_object: object,
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
    }
    let reverse_response_id = format!("resp-reverse-parent-{unique}");
    let mut reverse_parent_transaction = database.begin_write_transaction().await.unwrap();
    let reverse_parent_cluster = database
        .record_conversation_observation_in_transaction(
            &mut reverse_parent_transaction,
            ConversationObservationInput {
                key: &key,
                request_id: reverse_parent_request_id,
                request_json: &serde_json::json!({"input": "reverse parent"}),
                hints: &ConversationHints::default(),
                client_name: Some("Codex"),
                upstream_response_id: Some(&reverse_response_id),
                observed_at: unix_millis().saturating_add(120_000),
                attach_request_record: true,
                content_materialized: false,
            },
        )
        .await
        .unwrap();
    attach_conversation_upstream_response_in_transaction(
        &mut reverse_parent_transaction,
        database.backend,
        reverse_parent_request_id,
        &reverse_response_id,
    )
    .await
    .unwrap();
    reverse_parent_transaction.commit().await.unwrap();
    let reverse_child_cluster = database
        .record_conversation_observation(
            &key,
            reverse_child_request_id,
            &serde_json::json!({"input": "reverse child"}),
            &ConversationHints {
                parent_turn_id: Some(reverse_response_id.clone()),
                ..ConversationHints::default()
            },
            Some("Codex"),
        )
        .await
        .unwrap();
    assert_eq!(reverse_child_cluster, reverse_parent_cluster);
    let reverse_state: (i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2), (SELECT COUNT(*) FROM conversation_edges edge JOIN conversation_observations parent ON parent.id = edge.from_observation_id JOIN conversation_observations child ON child.id = edge.to_observation_id WHERE parent.request_id = $3 AND child.request_id = $4 AND edge.cluster_id = parent.cluster_id AND edge.cluster_id = child.cluster_id)",
    )
    .bind(key.key_id.to_string())
    .bind(&reverse_response_id)
    .bind(reverse_parent_request_id.to_string())
    .bind(reverse_child_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(reverse_state, (0, 1));

    // Generic parent_turn_id references can name a declared turn rather than
    // a Responses response id. Publishing that turn must fire reconciliation.
    let turn_child_request_id = Uuid::now_v7();
    let turn_parent_request_id = Uuid::now_v7();
    for (request_id, object) in [
        (turn_child_request_id, "objects/turn-child"),
        (turn_parent_request_id, "objects/turn-parent"),
    ] {
        database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai-responses",
                model: &model,
                request_object: object,
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
    }
    let declared_turn_id = format!("turn-late-parent-{unique}");
    let turn_child_cluster = database
        .record_conversation_observation(
            &key,
            turn_child_request_id,
            &serde_json::json!({"input": "turn child"}),
            &ConversationHints {
                parent_turn_id: Some(declared_turn_id.clone()),
                ..ConversationHints::default()
            },
            Some("Codex"),
        )
        .await
        .unwrap();
    let turn_parent_cluster = database
        .record_conversation_observation(
            &key,
            turn_parent_request_id,
            &serde_json::json!({"input": "turn parent"}),
            &ConversationHints {
                turn_id: Some(declared_turn_id.clone()),
                ..ConversationHints::default()
            },
            Some("Codex"),
        )
        .await
        .unwrap();
    assert_ne!(turn_child_cluster, turn_parent_cluster);
    let turn_state: (String, String, i64, i64) = sqlx::query_as(
        "SELECT child.cluster_id, parent.cluster_id, (SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2), (SELECT COUNT(*) FROM conversation_edges edge WHERE edge.from_observation_id = parent.id AND edge.to_observation_id = child.id AND edge.cluster_id = parent.cluster_id AND edge.cluster_id = child.cluster_id) FROM conversation_observations child JOIN conversation_observations parent ON parent.request_id = $3 WHERE child.request_id = $4",
    )
    .bind(key.key_id.to_string())
    .bind(&declared_turn_id)
    .bind(turn_parent_request_id.to_string())
    .bind(turn_child_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(turn_state.0, turn_state.1);
    assert_eq!((turn_state.2, turn_state.3), (0, 1));

    // All references known to one terminal transaction must be locked in one
    // global order. Otherwise P(turn=Z,response=A) and C(parent=A,turn=Z)
    // acquire opposite locks across record and attach and deadlock.
    let ordered_parent_request_id = Uuid::now_v7();
    let ordered_child_request_id = Uuid::now_v7();
    for (request_id, object) in [
        (ordered_parent_request_id, "objects/ordered-parent"),
        (ordered_child_request_id, "objects/ordered-child"),
    ] {
        database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                protocol: "openai-responses",
                model: &model,
                request_object: object,
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .unwrap();
    }
    let ordered_response_id = format!("a-ordered-response-{unique}");
    let ordered_turn_id = format!("z-ordered-turn-{unique}");
    let ordered_parent_json = serde_json::json!({"input": "ordered parent"});
    let ordered_child_json = serde_json::json!({"input": "ordered child"});
    let ordered_parent_hints = ConversationHints {
        turn_id: Some(ordered_turn_id.clone()),
        ..ConversationHints::default()
    };
    let ordered_child_hints = ConversationHints {
        parent_turn_id: Some(ordered_response_id.clone()),
        turn_id: Some(ordered_turn_id.clone()),
        ..ConversationHints::default()
    };
    let mut ordered_parent_transaction = database.begin_write_transaction().await.unwrap();
    let ordered_parent_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *ordered_parent_transaction)
        .await
        .unwrap();
    let ordered_parent_cluster = database
        .record_conversation_observation_in_transaction(
            &mut ordered_parent_transaction,
            ConversationObservationInput {
                key: &key,
                request_id: ordered_parent_request_id,
                request_json: &ordered_parent_json,
                hints: &ordered_parent_hints,
                client_name: Some("Codex"),
                upstream_response_id: Some(&ordered_response_id),
                observed_at: unix_millis(),
                attach_request_record: true,
                content_materialized: false,
            },
        )
        .await
        .unwrap();
    let ordered_child_database = database.clone();
    let ordered_child_key = key.clone();
    let (ordered_child_pid_sender, ordered_child_pid_receiver) = tokio::sync::oneshot::channel();
    let ordered_child = tokio::spawn(async move {
        let mut transaction = ordered_child_database
            .begin_write_transaction()
            .await
            .unwrap();
        let backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *transaction)
            .await
            .unwrap();
        ordered_child_pid_sender.send(backend_pid).unwrap();
        let cluster = ordered_child_database
            .record_conversation_observation_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key: &ordered_child_key,
                    request_id: ordered_child_request_id,
                    request_json: &ordered_child_json,
                    hints: &ordered_child_hints,
                    client_name: Some("Codex"),
                    upstream_response_id: None,
                    observed_at: unix_millis(),
                    attach_request_record: true,
                    content_materialized: false,
                },
            )
            .await
            .unwrap();
        transaction.commit().await.unwrap();
        cluster
    });
    let ordered_child_pid = ordered_child_pid_receiver.await.unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let blocked_by_parent: bool = sqlx::query_scalar(
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity activity JOIN pg_locks advisory ON advisory.pid = activity.pid WHERE activity.pid = $1 AND activity.state = 'active' AND activity.wait_event_type = 'Lock' AND activity.wait_event = 'advisory' AND advisory.locktype = 'advisory' AND NOT advisory.granted AND $2 = ANY(pg_blocking_pids(activity.pid)))",
            )
            .bind(ordered_child_pid)
            .bind(ordered_parent_pid)
            .fetch_one(&database.pool)
            .await
            .unwrap();
            if blocked_by_parent {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("child did not wait on the parent's ordered conversation reference locks");
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        attach_conversation_upstream_response_in_transaction(
            &mut ordered_parent_transaction,
            database.backend,
            ordered_parent_request_id,
            &ordered_response_id,
        )
        .await
        .unwrap();
        ordered_parent_transaction.commit().await.unwrap();
    })
    .await
    .expect("parent attach deadlocked after record acquired conversation references");
    let ordered_child_cluster =
        tokio::time::timeout(std::time::Duration::from_secs(10), ordered_child)
            .await
            .expect("child did not resume after parent commit")
            .unwrap();
    assert_eq!(ordered_parent_cluster, ordered_child_cluster);
    let ordered_state: (String, String, i64) = sqlx::query_as(
        "SELECT parent.conversation_cluster_id, child.conversation_cluster_id, (SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2) FROM request_records parent JOIN request_records child ON child.id = $3 WHERE parent.id = $4",
    )
    .bind(key.key_id.to_string())
    .bind(&ordered_response_id)
    .bind(ordered_child_request_id.to_string())
    .bind(ordered_parent_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(ordered_state.0, ordered_state.1);
    assert_eq!(ordered_state.2, 0);
}

#[tokio::test]
async fn postgres_outbox_projection_reconciles_child_projected_before_parent() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"outbox late parent reconciliation pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("outbox-late-parent-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "outbox-late-parent".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    enforcement_mode: EnforcementMode::MeteredUnlimited,
                    max_concurrency: 4,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::ONE,
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
    let model = format!("outbox-late-parent-{unique}");
    let price = database
        .upsert_model_price(&model, "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let parent_request_id = Uuid::now_v7();
    let child_request_id = Uuid::now_v7();
    let parent_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: parent_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 2,
            output_token_ceiling: 2,
            protocol: "openai-responses",
            model: &model,
            request_object: "objects/outbox-parent",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let child_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: child_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 2,
            output_token_ceiling: 2,
            protocol: "openai-responses",
            model: &model,
            request_object: "objects/outbox-child",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let response_id = format!("resp-outbox-parent-{unique}");
    let parent_json = serde_json::json!({"input": "outbox parent"});
    let child_json = serde_json::json!({"input": "outbox child"});
    let parent_hints = ConversationHints::default();
    let child_hints = ConversationHints {
        parent_turn_id: Some(response_id.clone()),
        ..ConversationHints::default()
    };
    database
        .finish_proxy_request(FinishProxyRequest {
            usage_basis: None,
            first_output_ms: None,
            generation_duration_ms: None,
            request_id: parent_request_id,
            tenant_id: key.tenant_id,
            reservation: &parent_reservation,
            input_token_ceiling: 2,
            output_token_ceiling: 2,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 5,
            usage: TokenUsage::default(),
            error_code: None,
            response_object: "objects/outbox-parent-response",
            routing_session_id: None,
            conversation: Some(ProxyConversationInput {
                key: &key,
                request_json: &parent_json,
                hints: &parent_hints,
                client_name: Some("Codex"),
                upstream_response_id: Some(&response_id),
            }),
        })
        .await
        .unwrap();
    database
        .finish_proxy_request(FinishProxyRequest {
            usage_basis: None,
            first_output_ms: None,
            generation_duration_ms: None,
            request_id: child_request_id,
            tenant_id: key.tenant_id,
            reservation: &child_reservation,
            input_token_ceiling: 2,
            output_token_ceiling: 2,
            requested_service_tier: None,
            status_code: 200,
            duration_ms: 5,
            usage: TokenUsage::default(),
            error_code: None,
            response_object: "objects/outbox-child-response",
            routing_session_id: None,
            conversation: Some(ProxyConversationInput {
                key: &key,
                request_json: &child_json,
                hints: &child_hints,
                client_name: Some("Codex"),
                upstream_response_id: None,
            }),
        })
        .await
        .unwrap();

    let projector = Uuid::now_v7();
    let tasks = database
        .claim_conversation_projection_tasks(projector, 32)
        .await
        .unwrap();
    assert!(
        tasks
            .iter()
            .any(|task| task.request_id == parent_request_id)
    );
    assert!(tasks.iter().any(|task| task.request_id == child_request_id));
    assert!(
        database
            .project_claimed_conversation_projection_task(projector, child_request_id)
            .await
            .unwrap()
    );
    let child_cluster_before_parent: String =
        sqlx::query_scalar("SELECT conversation_cluster_id FROM request_records WHERE id = $1")
            .bind(child_request_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    let unresolved_before_parent: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2",
    )
    .bind(key.key_id.to_string())
    .bind(&response_id)
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(unresolved_before_parent, 1);
    assert!(
        database
            .project_claimed_conversation_projection_task(projector, parent_request_id)
            .await
            .unwrap()
    );
    let projected_state: (String, String, i64, i64) = sqlx::query_as(
        "SELECT parent.conversation_cluster_id, child.conversation_cluster_id, (SELECT COUNT(*) FROM conversation_unresolved_explicit_parents WHERE key_id = $1 AND parent_reference = $2), (SELECT COUNT(*) FROM conversation_clusters WHERE id = $3) FROM request_records parent JOIN request_records child ON child.id = $4 WHERE parent.id = $5",
    )
    .bind(key.key_id.to_string())
    .bind(&response_id)
    .bind(&child_cluster_before_parent)
    .bind(child_request_id.to_string())
    .bind(parent_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(projected_state.0, projected_state.1);
    assert_eq!((projected_state.2, projected_state.3), (0, 0));
}

#[tokio::test]
async fn buffered_conversation_content_wait_does_not_hold_archive_budget() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let unique = Uuid::now_v7();
    let pepper = b"buffered conversation budget lock pepper";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("buffered-conversation-{unique}"),
                principal_external_id: "member".to_owned(),
                alias: "buffered-conversation".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
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
    let model = format!("buffered-conversation-{unique}");
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
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: &model,
            request_object: "objects/blake3/buffered-conversation-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let request_json = serde_json::json!({"messages": [{
        "role": "user", "content": format!("buffered-content-{unique}")
    }]});
    let atom = extract_atoms(&request_json).remove(0);
    let mut blocker = database.begin_write_transaction().await.unwrap();
    let blocker_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut *blocker)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
    )
    .bind(key.tenant_id.to_string())
    .bind(&atom.content_hash)
    .bind(&atom.instance_hash)
    .bind(&atom.role)
    .bind(&atom.kind)
    .bind(serde_json::to_string(&atom.content).unwrap())
    .bind(unix_millis())
    .execute(&mut *blocker)
    .await
    .unwrap();
    let (response_presealed, release_response_preseal) =
        crate::response_archive_spool::pause_next_request_preseal_for_test(request_id);
    let finish_database = database.clone();
    let mut finish = tokio::spawn(async move {
        use crate::response_archive_spool::{BufferedArchive, BufferedArchivePurpose};
        let body = bytes::Bytes::from_static(b"buffered response");
        let archive = BufferedArchive::new(
            ArchiveSpoolIdentity {
                request_id,
                tenant_id: key.tenant_id,
                reservation_id: reservation.id,
            },
            BufferedArchivePurpose::Response,
            &body,
            pepper,
            false,
        )
        .unwrap();
        let hints = ConversationHints {
            session_id: Some(format!("buffered-session-{unique}")),
            ..ConversationHints::default()
        };
        finish_database
            .finish_proxy_request_with_buffered_archive_and_upstream_attribution(
                FinishProxyRequest {
                    usage_basis: None,
                    first_output_ms: None,
                    generation_duration_ms: None,
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 10,
                    output_token_ceiling: 10,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage::default(),
                    error_code: None,
                    response_object: "objects/blake3/buffered-conversation-response",
                    routing_session_id: None,
                    conversation: Some(ProxyConversationInput {
                        key: &key,
                        request_json: &request_json,
                        hints: &hints,
                        client_name: None,
                        upstream_response_id: None,
                    }),
                },
                &archive,
                ProxyRequestUpstreamAttribution::KeepSelected,
            )
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), response_presealed)
        .await
        .expect("response ciphertext must be prepared before the terminal transaction")
        .unwrap();
    let (preseal_budget, _) = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        database.spool_transaction(),
    )
    .await
    .expect("response pre-sealing must not hold the archive budget")
    .unwrap();
    preseal_budget.rollback().await.unwrap();
    release_response_preseal.send(()).unwrap();
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity WHERE datname = current_database() AND state = 'active' AND wait_event_type = 'Lock' AND query LIKE 'INSERT INTO semantic_atoms%' AND $1 = ANY(pg_blocking_pids(pid))",
            )
            .bind(blocker_pid)
            .fetch_one(&database.pool)
            .await
            .unwrap();
            if waiting > 0 {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await;
    // Keep the content blocker open while another archive operation takes the
    // global budget. Preparation must hold neither this row nor the session.
    let budget = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        database.spool_transaction(),
    )
    .await;
    let budget_available = match budget {
        Ok(Ok((transaction, _))) => {
            transaction.rollback().await.unwrap();
            true
        }
        _ => false,
    };
    blocker.rollback().await.unwrap();
    let terminal = tokio::time::timeout(std::time::Duration::from_secs(15), &mut finish).await;
    if terminal.is_err() {
        finish.abort();
        let _ = finish.await;
    }
    waiting.expect("buffered finish must reach the immutable-content lock barrier");
    assert!(
        budget_available,
        "content preparation must release the archive budget"
    );
    assert!(matches!(
        terminal.unwrap().unwrap().unwrap(),
        FinishProxyRequestResult::Finished { .. }
    ));
}

#[tokio::test]
async fn lifecycle_deadline_converges_pending_proxy_request_idempotently() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory
            .path()
            .join("proxy-lifecycle-deadline.db")
            .display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy lifecycle deadline test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-lifecycle-deadline".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-lifecycle-deadline".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
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
    let price = database
        .upsert_model_price(
            "proxy-lifecycle-deadline",
            "USD",
            Decimal::ONE,
            Decimal::ONE,
        )
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 7,
            output_token_ceiling: 11,
            protocol: "openai",
            model: "proxy-lifecycle-deadline",
            request_object: "gap://proxy-lifecycle-deadline/request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();

    assert!(matches!(
        database
            .expire_proxy_lifecycle_deadline(request_id, key.tenant_id, &reservation, 1_230)
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished {
            usage_invalid: false,
            ..
        }
    ));
    assert!(matches!(
        database
            .expire_proxy_lifecycle_deadline(request_id, key.tenant_id, &reservation, 1_240)
            .await
            .unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 504,
            ..
        }
    ));
    let terminal = sqlx::query(
        "SELECT status_code, error_code, input_tokens, output_tokens FROM request_records WHERE id = $1",
    )
    .bind(request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(terminal.get::<i64, _>("status_code"), 504);
    assert_eq!(
        terminal.get::<String, _>("error_code"),
        "request_lifecycle_timeout"
    );
    assert_eq!(terminal.get::<i64, _>("input_tokens"), 0);
    assert_eq!(terminal.get::<i64, _>("output_tokens"), 0);

    let delivered_request_id = Uuid::now_v7();
    let delivered_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: delivered_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 7,
            output_token_ceiling: 11,
            protocol: "openai",
            model: "proxy-lifecycle-deadline",
            request_object: "gap://proxy-lifecycle-deadline/delivered-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    database
        .prepare_proxy_delivery(
            delivered_request_id,
            key.tenant_id,
            &delivered_reservation,
            7,
            11,
            None,
        )
        .await
        .unwrap();
    database
        .mark_proxy_delivery_started(delivered_request_id, key.tenant_id, &delivered_reservation)
        .await
        .unwrap();
    assert!(matches!(
        database
            .expire_proxy_lifecycle_deadline(
                delivered_request_id,
                key.tenant_id,
                &delivered_reservation,
                1_230,
            )
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished {
            usage_invalid: false,
            ..
        }
    ));
    let delivered_terminal = sqlx::query(
        "SELECT status_code, error_code, input_tokens, output_tokens FROM request_records WHERE id = $1",
    )
    .bind(delivered_request_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(delivered_terminal.get::<i64, _>("status_code"), 504);
    assert_eq!(
        delivered_terminal.get::<String, _>("error_code"),
        "request_lifecycle_timeout"
    );
    assert_eq!(delivered_terminal.get::<i64, _>("input_tokens"), 0);
    assert_eq!(delivered_terminal.get::<i64, _>("output_tokens"), 0);
}

#[tokio::test]
async fn concurrent_proxy_starts_serialize_in_sqlite() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-start-race.db").display()
    );
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy start race test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-start-race".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-start-race".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    requests_per_minute: 100_000,
                    tokens_per_minute: 100_000_000,
                    max_concurrency: 32,
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::from(1_000),
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
    let price = database
        .upsert_model_price("start-race-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = Vec::new();
    for _ in 0..16 {
        let database = database.clone();
        let key = key.clone();
        let price = price.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let request_id = Uuid::now_v7();
            barrier.wait().await;
            database
                .start_proxy_request(StartProxyRequest {
                    request_id,
                    key: &key,
                    price: &price,
                    input_token_ceiling: 100,
                    output_token_ceiling: 100,
                    protocol: "openai",
                    model: "start-race-model",
                    request_object: "objects/blake3/start-race-request",
                    upstream_account_id: None,
                    model_route_id: None,
                })
                .await
        }));
    }
    for result in futures_util::future::join_all(tasks).await {
        result.unwrap().unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        16
    );
}

#[tokio::test]
async fn proxy_lifecycle_is_atomic_fault_safe_and_exactly_replayable() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-lifecycle-atomic.db").display()
    );
    let database = Database::connect_with_max(&database_url, 4).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy lifecycle atomic test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-atomic".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-atomic".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
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
    let price = database
        .upsert_model_price("atomic-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let request_digest = "a".repeat(64);
    let staging_request = format!("staging://blake3/{request_digest}");
    let archived_request = format!("staging/proxy/{request_id}/request.bin");
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: "atomic-model",
            request_object: &staging_request,
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    assert_eq!(
        database
            .attach_proxy_request_archive(
                request_id,
                key.tenant_id,
                reservation.id,
                &staging_request,
                &archived_request,
            )
            .await
            .unwrap(),
        AttachProxyArchiveResult::Attached
    );
    assert_eq!(
        database
            .attach_proxy_request_archive(
                request_id,
                key.tenant_id,
                reservation.id,
                &staging_request,
                &archived_request,
            )
            .await
            .unwrap(),
        AttachProxyArchiveResult::AlreadyAttached
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT status FROM usage_reservations WHERE id = $1")
            .bind(reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        "reserved"
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1"
        )
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap(),
        0,
        "lineage must not exist before the terminal transaction"
    );

    assert!(matches!(
        database
            .start_proxy_request(StartProxyRequest {
                request_id,
                key: &key,
                price: &price,
                input_token_ceiling: 100,
                output_token_ceiling: 100,
                protocol: "openai",
                model: "atomic-model",
                request_object: "objects/blake3/duplicate",
                upstream_account_id: None,
                model_route_id: None,
            })
            .await,
        Err(AppError::BadRequest(_))
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM usage_reservations")
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'"
        )
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap(),
        1
    );

    // Test-only SQL safety boundary: `request_id` is a typed UUID generated in this test and its
    // Display representation cannot contain SQL syntax. SQLite does not allow a bind parameter
    // in a persisted trigger definition.
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "CREATE TRIGGER proxy_terminal_fault BEFORE INSERT ON request_stats_facts WHEN NEW.request_id = '{}' BEGIN SELECT RAISE(ABORT, 'proxy terminal fault'); END",
        request_id
    )))
        .execute(&database.pool)
        .await
        .unwrap();
    let request_json = serde_json::json!({
        "model": "atomic-model",
        "input": [{"role": "user", "content": "atomic terminal"}]
    });
    let hints = ConversationHints {
        session_id: Some("atomic-session".to_owned()),
        ..ConversationHints::default()
    };
    let usage = TokenUsage {
        input_tokens: 10,
        output_tokens: 5,
        ..TokenUsage::default()
    };
    let finish = || FinishProxyRequest {
        usage_basis: None,
        first_output_ms: None,
        generation_duration_ms: None,
        request_id,
        tenant_id: key.tenant_id,
        reservation: &reservation,
        input_token_ceiling: 100,
        output_token_ceiling: 100,
        requested_service_tier: None,
        status_code: 200,
        duration_ms: 12,
        usage: usage.clone(),
        error_code: None,
        response_object: "objects/blake3/atomic-response",
        routing_session_id: None,
        conversation: Some(ProxyConversationInput {
            key: &key,
            request_json: &request_json,
            hints: &hints,
            client_name: Some("codex"),
            upstream_response_id: Some("resp-atomic"),
        }),
    };
    database
        .prepare_proxy_delivery(request_id, key.tenant_id, &reservation, 100, 100, None)
        .await
        .unwrap();
    // Preparation must be gated by all terminal owner checks, not merely a
    // caller-provided tenant/key. Invalid pending-owner attempts cannot leave
    // hidden immutable content behind even though the final transaction fails.
    for invalid_owner in 0..11 {
        let mut forged_reservation = reservation.clone();
        let mut forged_key = key.clone();
        let mut invalid = finish();
        match invalid_owner {
            0 => {
                forged_reservation.id = Uuid::now_v7();
                invalid.reservation = &forged_reservation;
            }
            1 => {
                forged_reservation.account_id = Uuid::now_v7();
                invalid.reservation = &forged_reservation;
            }
            2 => invalid.input_token_ceiling += 1,
            3 => invalid.tenant_id = Uuid::now_v7(),
            4 => {
                forged_key.key_id = Uuid::now_v7();
                invalid.conversation.as_mut().unwrap().key = &forged_key;
            }
            5 => {
                forged_key.tenant_id = Uuid::now_v7();
                invalid.conversation.as_mut().unwrap().key = &forged_key;
            }
            6 => invalid.request_id = Uuid::now_v7(),
            7 => {
                invalid.input_token_ceiling += 1;
                invalid.output_token_ceiling -= 1;
            }
            8 => invalid.requested_service_tier = Some("priority"),
            9 => {
                forged_key.principal_id = Uuid::now_v7();
                invalid.conversation.as_mut().unwrap().key = &forged_key;
            }
            10 => {
                forged_key.account_id = Uuid::now_v7();
                invalid.conversation.as_mut().unwrap().key = &forged_key;
            }
            _ => unreachable!(),
        }
        assert!(database.finish_proxy_request(invalid).await.is_err());
        let content: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM semantic_atoms), (SELECT COUNT(*) FROM context_nodes)",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(content, (0, 0), "invalid owner case {invalid_owner}");
    }
    assert!(database.finish_proxy_request(finish()).await.is_err());
    let rollback = sqlx::query(
            "SELECT r.status AS reservation_status, q.completed_at, q.status_code, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS fact_count, (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id) AS observation_count FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE r.id = $1",
        )
        .bind(reservation.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(rollback.get::<String, _>("reservation_status"), "reserved");
    assert_eq!(rollback.get::<Option<i64>, _>("completed_at"), None);
    assert_eq!(rollback.get::<Option<i64>, _>("status_code"), None);
    for field in ["ledger_count", "fact_count", "observation_count"] {
        assert_eq!(rollback.get::<i64, _>(field), 0, "{field}");
    }
    sqlx::query("DROP TRIGGER proxy_terminal_fault")
        .execute(&database.pool)
        .await
        .unwrap();

    assert!(matches!(
        database.finish_proxy_request(finish()).await.unwrap(),
        FinishProxyRequestResult::Finished {
            usage_invalid: false,
            ..
        }
    ));
    assert!(matches!(
        database.finish_proxy_request(finish()).await.unwrap(),
        FinishProxyRequestResult::AlreadyFinished {
            status_code: 200,
            ..
        }
    ));
    let committed = sqlx::query(
            "SELECT r.status AS reservation_status, q.status_code, q.input_tokens, q.output_tokens, (SELECT COUNT(*) FROM ledger_entries l WHERE l.source = r.id) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts f WHERE f.request_id = q.id) AS fact_count, (SELECT COUNT(*) FROM request_daily_aggregates) AS aggregate_count, (SELECT COUNT(*) FROM request_events e WHERE e.request_id = q.id AND e.event_kind = 'finished') AS finished_events, (SELECT COUNT(*) FROM conversation_observations o WHERE o.request_id = q.id AND o.upstream_response_id = 'resp-atomic') AS observation_count FROM usage_reservations r JOIN request_records q ON q.reservation_id = r.id WHERE r.id = $1",
        )
        .bind(reservation.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(committed.get::<String, _>("reservation_status"), "settled");
    assert_eq!(committed.get::<i64, _>("status_code"), 200);
    assert_eq!(committed.get::<i64, _>("input_tokens"), 10);
    assert_eq!(committed.get::<i64, _>("output_tokens"), 5);
    for field in [
        "ledger_count",
        "fact_count",
        "aggregate_count",
        "finished_events",
        "observation_count",
    ] {
        assert_eq!(committed.get::<i64, _>(field), 1, "{field}");
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT COUNT(*) FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'"
        )
        .bind(key.key_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap(),
        0,
        "a successful terminal replay must release active concurrency exactly once"
    );

    let invalid_request_id = Uuid::now_v7();
    let invalid_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: invalid_request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "atomic-model",
            request_object: "objects/blake3/invalid-usage-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        database
            .finish_proxy_request(FinishProxyRequest {
                usage_basis: None,
                first_output_ms: None,
                generation_duration_ms: None,
                request_id: invalid_request_id,
                tenant_id: key.tenant_id,
                reservation: &invalid_reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                requested_service_tier: None,
                status_code: 200,
                duration_ms: 1,
                usage: TokenUsage {
                    input_tokens: 11,
                    output_tokens: 1,
                    ..TokenUsage::default()
                },
                error_code: None,
                response_object: "objects/blake3/untrusted-invalid-usage-response",
                routing_session_id: None,
                conversation: None,
            })
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished {
            cost_micros: 0,
            usage_invalid: true
        }
    ));
    let invalid_terminal = sqlx::query(
            "SELECT status_code, error_code, input_tokens, output_tokens, cost_micros, response_object FROM request_records WHERE id = $1",
        )
        .bind(invalid_request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    assert_eq!(invalid_terminal.get::<i64, _>("status_code"), 502);
    assert_eq!(
        invalid_terminal.get::<String, _>("error_code"),
        "upstream_invalid_usage"
    );
    assert_eq!(invalid_terminal.get::<i64, _>("input_tokens"), 0);
    assert_eq!(invalid_terminal.get::<i64, _>("output_tokens"), 0);
    assert_eq!(invalid_terminal.get::<i64, _>("cost_micros"), 0);
    assert!(
        !invalid_terminal
            .get::<String, _>("response_object")
            .contains("untrusted-invalid-usage-response")
    );

    let expensive_price = database
        .upsert_model_price_tier(
            "full-contract-model",
            "USD",
            "default",
            Decimal::ONE,
            Decimal::ONE,
            Decimal::from(100),
            Decimal::ONE,
            false,
        )
        .await
        .unwrap();
    let delivered_request_id = Uuid::now_v7();
    let delivered_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: delivered_request_id,
            key: &key,
            price: &expensive_price,
            input_token_ceiling: 10,
            output_token_ceiling: 2,
            protocol: "openai",
            model: "full-contract-model",
            request_object: "objects/blake3/delivered-failure-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let delivered = database
        .finish_proxy_request(FinishProxyRequest {
            usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
            first_output_ms: None,
            generation_duration_ms: None,
            request_id: delivered_request_id,
            tenant_id: key.tenant_id,
            reservation: &delivered_reservation,
            input_token_ceiling: 10,
            output_token_ceiling: 2,
            requested_service_tier: None,
            status_code: 502,
            duration_ms: 1,
            usage: TokenUsage::default(),
            error_code: Some("upstream_incomplete_response"),
            response_object: "objects/blake3/delivered-failure-response",
            routing_session_id: None,
            conversation: None,
        })
        .await
        .unwrap();
    assert!(matches!(
        delivered,
        FinishProxyRequestResult::Finished {
            cost_micros,
            usage_invalid: false
        } if cost_micros == 0
    ));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT actual_micros FROM usage_reservations WHERE id = $1")
            .bind(delivered_reservation.id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap(),
        0,
        "an unobserved delivery failure must not promote its reservation to actual cost"
    );

    database
        .upsert_model_price_tier(
            "tier-contract-model",
            "USD",
            "default",
            Decimal::ONE,
            Decimal::ONE,
            Decimal::ONE,
            Decimal::ONE,
            false,
        )
        .await
        .unwrap();
    database
        .upsert_model_price_tier(
            "tier-contract-model",
            "USD",
            "flex",
            Decimal::from(2),
            Decimal::from(2),
            Decimal::from(2),
            Decimal::from(2),
            false,
        )
        .await
        .unwrap();
    let tiered_price = database
        .upsert_model_price_tier(
            "tier-contract-model",
            "USD",
            "priority",
            Decimal::from(100),
            Decimal::from(100),
            Decimal::from(100),
            Decimal::from(100),
            false,
        )
        .await
        .unwrap();
    let flex_request_id = Uuid::now_v7();
    let flex_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: flex_request_id,
            key: &key,
            price: &tiered_price,
            input_token_ceiling: 10,
            output_token_ceiling: 2,
            protocol: "openai",
            model: "tier-contract-model",
            request_object: "objects/blake3/flex-delivered-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    assert!(flex_reservation.reserved_micros > 24);
    assert!(matches!(
        database
            .finish_proxy_request(FinishProxyRequest {
                usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
                first_output_ms: None,
                generation_duration_ms: None,
                request_id: flex_request_id,
                tenant_id: key.tenant_id,
                reservation: &flex_reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 2,
                requested_service_tier: Some("flex"),
                status_code: 502,
                duration_ms: 1,
                usage: TokenUsage::default(),
                error_code: Some("upstream_incomplete_response"),
                response_object: "objects/blake3/flex-delivered-response",
                routing_session_id: None,
                conversation: None,
            })
            .await
            .unwrap(),
        FinishProxyRequestResult::Finished {
            cost_micros: 0,
            usage_invalid: false,
        }
    ));
}

#[tokio::test]
async fn concurrent_proxy_terminal_owners_settle_and_link_once() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-terminal-race.db").display()
    );
    let database = Database::connect_with_max(&database_url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy terminal race test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-race".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-race".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy {
                    tokens_per_minute: 100_000,
                    max_concurrency: 8,
                    ..KeyPolicy::default()
                },
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
    let price = database
        .upsert_model_price("race-model", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();
    let request_id = Uuid::now_v7();
    let reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id,
            key: &key,
            price: &price,
            input_token_ceiling: 100,
            output_token_ceiling: 100,
            protocol: "openai",
            model: "race-model",
            request_object: "objects/blake3/race-request",
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(6));
    let mut tasks = Vec::new();
    for _ in 0..6 {
        let database = database.clone();
        let key = key.clone();
        let reservation = reservation.clone();
        let barrier = barrier.clone();
        tasks.push(tokio::spawn(async move {
            let request_json = serde_json::json!({
                "model": "race-model",
                "messages": [{"role": "user", "content": "race"}]
            });
            let hints = ConversationHints::default();
            barrier.wait().await;
            database
                .finish_proxy_request(FinishProxyRequest {
                    usage_basis: None,
                    first_output_ms: None,
                    generation_duration_ms: None,
                    request_id,
                    tenant_id: key.tenant_id,
                    reservation: &reservation,
                    input_token_ceiling: 100,
                    output_token_ceiling: 100,
                    requested_service_tier: None,
                    status_code: 200,
                    duration_ms: 1,
                    usage: TokenUsage {
                        input_tokens: 7,
                        output_tokens: 3,
                        ..TokenUsage::default()
                    },
                    error_code: None,
                    response_object: "objects/blake3/race-response",
                    routing_session_id: None,
                    conversation: Some(ProxyConversationInput {
                        key: &key,
                        request_json: &request_json,
                        hints: &hints,
                        client_name: None,
                        upstream_response_id: Some("resp-race"),
                    }),
                })
                .await
        }));
    }
    let mut winners = 0;
    let mut replays = 0;
    for task in tasks {
        match task.await.unwrap().unwrap() {
            FinishProxyRequestResult::Finished { .. } => winners += 1,
            FinishProxyRequestResult::AlreadyFinished { .. } => replays += 1,
        }
    }
    assert_eq!(winners, 1);
    assert_eq!(replays, 5);
    let counts = sqlx::query(
            "SELECT (SELECT COUNT(*) FROM ledger_entries WHERE source = $1) AS ledger_count, (SELECT COUNT(*) FROM request_stats_facts WHERE request_id = $2) AS fact_count, (SELECT COUNT(*) FROM conversation_observations WHERE request_id = $3) AS observation_count, (SELECT COUNT(*) FROM request_events WHERE request_id = $4 AND event_kind = 'finished') AS event_count",
        )
        .bind(reservation.id.to_string())
        .bind(request_id.to_string())
        .bind(request_id.to_string())
        .bind(request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
    for field in [
        "ledger_count",
        "fact_count",
        "observation_count",
        "event_count",
    ] {
        assert_eq!(counts.get::<i64, _>(field), 1, "{field}");
    }
}

#[tokio::test]
async fn terminal_upstream_attribution_uses_only_dispatched_candidates() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("proxy-attribution.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let pepper = b"proxy attribution test pepper value";
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: "proxy-attribution".to_owned(),
                principal_external_id: "member".to_owned(),
                alias: "proxy-attribution".to_owned(),
                currency: "USD".to_owned(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::ONE,
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
    let price = database
        .upsert_model_price("proxy-attribution", "USD", Decimal::ONE, Decimal::ONE)
        .await
        .unwrap();

    let selected_account = Uuid::now_v7();
    let selected_route = Uuid::now_v7();
    let no_dispatch_request = Uuid::now_v7();
    let no_dispatch_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: no_dispatch_request,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "proxy-attribution",
            request_object: "gap://proxy-attribution/no-dispatch-request",
            upstream_account_id: Some(selected_account),
            model_route_id: Some(selected_route),
        })
        .await
        .unwrap();
    database
        .finish_proxy_request_with_archive_staging_and_upstream_attribution(
            FinishProxyRequest {
                usage_basis: None,
                first_output_ms: None,
                generation_duration_ms: None,
                request_id: no_dispatch_request,
                tenant_id: key.tenant_id,
                reservation: &no_dispatch_reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                requested_service_tier: None,
                status_code: 503,
                duration_ms: 1,
                usage: TokenUsage::default(),
                error_code: Some("upstream_unavailable"),
                response_object: "gap://proxy-attribution/no-dispatch-response",
                routing_session_id: None,
                conversation: None,
            },
            None,
            ProxyRequestUpstreamAttribution::LastDispatched(None),
        )
        .await
        .unwrap();
    let no_dispatch = sqlx::query(
        "SELECT upstream_account_id, model_route_id FROM request_records WHERE id = $1",
    )
    .bind(no_dispatch_request.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        no_dispatch.get::<Option<String>, _>("upstream_account_id"),
        None
    );
    assert_eq!(no_dispatch.get::<Option<String>, _>("model_route_id"), None);

    let dispatched_account = Uuid::now_v7();
    let dispatched_route = Uuid::now_v7();
    let failover_request = Uuid::now_v7();
    let failover_reservation = database
        .start_proxy_request(StartProxyRequest {
            request_id: failover_request,
            key: &key,
            price: &price,
            input_token_ceiling: 10,
            output_token_ceiling: 10,
            protocol: "openai",
            model: "proxy-attribution",
            request_object: "gap://proxy-attribution/failover-request",
            upstream_account_id: Some(selected_account),
            model_route_id: Some(selected_route),
        })
        .await
        .unwrap();
    database
        .finish_proxy_request_with_archive_staging_and_upstream_attribution(
            FinishProxyRequest {
                usage_basis: None,
                first_output_ms: None,
                generation_duration_ms: None,
                request_id: failover_request,
                tenant_id: key.tenant_id,
                reservation: &failover_reservation,
                input_token_ceiling: 10,
                output_token_ceiling: 10,
                requested_service_tier: None,
                status_code: 503,
                duration_ms: 1,
                usage: TokenUsage::default(),
                error_code: Some("upstream_connection"),
                response_object: "gap://proxy-attribution/failover-response",
                routing_session_id: None,
                conversation: None,
            },
            None,
            ProxyRequestUpstreamAttribution::LastDispatched(Some((
                dispatched_account,
                dispatched_route,
            ))),
        )
        .await
        .unwrap();
    let failover = sqlx::query(
        "SELECT upstream_account_id, model_route_id FROM request_records WHERE id = $1",
    )
    .bind(failover_request.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(
        failover.get::<Option<String>, _>("upstream_account_id"),
        Some(dispatched_account.to_string())
    );
    assert_eq!(
        failover.get::<Option<String>, _>("model_route_id"),
        Some(dispatched_route.to_string())
    );
}
