use super::*;
use serde_json::json;
use std::io::Write as _;

const PREFIX: usize = 4096;
const OBSERVATIONS: usize = 96;
const PEPPER: &[u8] = b"conversation prefix storage contract pepper";

#[test]
fn reference_encoding_is_explicit_and_malformed_paths_are_rejected() {
    let empty = serde_json::to_string(&Fingerprint::from_hashes(&[])).unwrap();
    assert_eq!(empty, r#"{"context_prefix_v1":null,"atom_count":0}"#);
    assert!(decode(&empty).is_ok());
    for encoded in [
        r#"{"context_prefix_v1":null,"atom_count":1}"#,
        r#"{"context_prefix_v1":"missing","atom_count":0}"#,
        r#"{"context_prefix_v1":"missing","atom_count":1025}"#,
        r#"{"context_prefix_v2":null,"atom_count":0}"#,
        r#"{"atom_count":0}"#,
        "[]",
    ] {
        assert!(decode(encoded).is_err());
    }
    assert!(
        PrefixPaths::default()
            .hashes(Some("missing"), Some(1))
            .is_err()
    );
}

pub(crate) async fn key(database: &Database, tenant: &str) -> AuthenticatedKey {
    let issued = database
        .create_key(
            CreateKeyInput {
                tenant_external_id: tenant.into(),
                principal_external_id: "prefix-member".into(),
                alias: "prefix contract".into(),
                currency: "USD".into(),
                policy: KeyPolicy::default(),
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .unwrap();
    database
        .authenticate_key(&issued.key, PEPPER)
        .await
        .unwrap()
}

pub(crate) async fn observe(
    database: &Database,
    key: &AuthenticatedKey,
    body: &serde_json::Value,
    hints: &ConversationHints,
    request_id: Uuid,
    observed_at: i64,
    commit: bool,
) -> Uuid {
    // This is also the archive-only import path: no native request record.
    let mut transaction = database.begin_write_transaction().await.unwrap();
    let cluster = database
        .record_conversation_observation_in_transaction(
            &mut transaction,
            ConversationObservationInput {
                key,
                request_id,
                request_json: body,
                hints,
                client_name: Some("Codex"),
                upstream_response_id: None,
                observed_at,
                attach_request_record: false,
                content_materialized: false,
            },
        )
        .await
        .unwrap();
    if commit {
        transaction.commit().await.unwrap();
    } else {
        transaction.rollback().await.unwrap();
    }
    cluster
}

pub(crate) async fn load(
    database: &Database,
    tenant: Uuid,
    request: Uuid,
) -> (String, Vec<String>, Vec<String>) {
    let row = sqlx::query("SELECT atom_hashes_json, leaf_node_hash FROM conversation_observations WHERE request_id = $1")
        .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
    let encoded: String = row.get("atom_hashes_json");
    let leaf: Option<String> = row.get("leaf_node_hash");
    let mut transaction = database.begin_write_transaction().await.unwrap();
    let paths = load_candidate_paths(
        &mut transaction,
        &tenant.to_string(),
        std::slice::from_ref(&row),
        leaf.as_deref(),
    )
    .await
    .unwrap();
    let fingerprint = candidate_hashes(&row, &paths).unwrap();
    let full = paths.hashes(leaf.as_deref(), None).unwrap();
    transaction.rollback().await.unwrap();
    (encoded, fingerprint, full)
}

async fn contract(url: &str) {
    let database = Database::connect_with_max(url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let first = key(&database, "prefix-tenant-a").await;
    let other_key = key(&database, "prefix-tenant-a").await;
    let other_tenant = key(&database, "prefix-tenant-b").await;
    let now = unix_millis();
    let base: Vec<_> = (0..PREFIX)
        .map(|index| json!({"role":"user", "content":format!("unique prefix atom {index}")}))
        .collect();
    let base_body = json!({"input": base});
    let default_hints = ConversationHints::default();
    let base_id = Uuid::now_v7();
    let cluster = observe(
        &database,
        &first,
        &base_body,
        &default_hints,
        base_id,
        now,
        true,
    )
    .await;
    let base_reference: String = sqlx::query_scalar(
        "SELECT atom_hashes_json FROM conversation_observations WHERE request_id = $1",
    )
    .bind(base_id.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert!(
        decode(&base_reference).is_ok(),
        "latest writers are always enabled"
    );
    let all_hashes: Vec<_> = extract_atoms(&base_body)
        .into_iter()
        .map(|atom| atom.content_hash)
        .collect();
    let repeated_array =
        serde_json::to_string(&all_hashes[..MAX_CONVERSATION_FINGERPRINT_ATOMS]).unwrap();
    let nodes_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_nodes WHERE tenant_id = $1")
            .bind(first.tenant_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(nodes_before, PREFIX as i64);
    let mut expected_nodes = PREFIX;
    let mut requests = Vec::new();
    for index in 0..OBSERVATIONS {
        let mut messages = base.clone();
        // Half are growing continuations, half are siblings with the same long prefix.
        for tail in 0..=index / 2 {
            messages.push(json!({"role":"assistant", "content":format!("tail {tail}")}));
        }
        if index % 2 == 1 {
            messages.push(json!({"role":"user", "content":format!("branch {index}")}));
            expected_nodes += 1;
        } else {
            expected_nodes += 1;
        }
        let body = json!({"input": messages});
        let request = Uuid::now_v7();
        assert_eq!(
            observe(
                &database,
                &first,
                &body,
                &default_hints,
                request,
                now + index as i64 + 1,
                true
            )
            .await,
            cluster
        );
        let (encoded, fingerprint, full) = load(&database, first.tenant_id, request).await;
        let expected: Vec<_> = extract_atoms(&body)
            .into_iter()
            .map(|atom| atom.content_hash)
            .collect();
        assert_eq!(
            full, expected,
            "lossless readback for long observation {index}"
        );
        assert_eq!(fingerprint, expected[..MAX_CONVERSATION_FINGERPRINT_ATOMS]);
        assert!(
            encoded.len() < 128,
            "reference storage must stay independent of prefix length"
        );
        requests.push(request);
    }
    let nodes_after: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM context_nodes WHERE tenant_id = $1")
            .bind(first.tenant_id.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(
        nodes_after, expected_nodes as i64,
        "no duplicate prefix tree"
    );
    let reference_bytes: i64 = sqlx::query_scalar("SELECT SUM(LENGTH(atom_hashes_json)) FROM conversation_observations WHERE key_id = $1 AND request_id <> $2")
        .bind(first.key_id.to_string()).bind(base_id.to_string()).fetch_one(&database.pool).await.unwrap();
    let repeated_array_bytes = repeated_array.len() * OBSERVATIONS;
    assert!(reference_bytes * 100 < repeated_array_bytes as i64);
    // Fifty observations share one actual prefix path, not fifty SQL lookups
    // or fifty copies of the common ancestors in the recursive result.
    let candidates = sqlx::query("SELECT atom_hashes_json, leaf_node_hash FROM conversation_observations WHERE key_id = $1 AND request_id <> $2 ORDER BY created_at DESC LIMIT 50")
        .bind(first.key_id.to_string()).bind(base_id.to_string()).fetch_all(&database.pool).await.unwrap();
    let mut transaction = database.begin_write_transaction().await.unwrap();
    let shared = load_candidate_paths(
        &mut transaction,
        &first.tenant_id.to_string(),
        &candidates,
        None,
    )
    .await
    .unwrap();
    assert_eq!(shared.nodes.len(), MAX_CONVERSATION_FINGERPRINT_ATOMS);
    transaction.rollback().await.unwrap();
    // Write directly so the existing all-targets GHA logs retain the measured
    // evidence even when libtest captures println! output from passing tests.
    writeln!(std::io::stdout(),
        "PREFIX_STORAGE prefix_atoms={PREFIX} observations={OBSERVATIONS} legacy_array_bytes={repeated_array_bytes} reference_bytes={reference_bytes} new_nodes={} shared_candidate_nodes={}",
        nodes_after - nodes_before, shared.nodes.len()
    ).unwrap();
    if matches!(database.backend, DatabaseBackend::PostgreSql) {
        let physical: i64 = sqlx::query_scalar(
            "SELECT pg_total_relation_size('conversation_observations')::BIGINT",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap();
        println!(
            "PREFIX_STORAGE postgres_observation_relation_bytes={physical} (includes heap/index/TOAST allocation; not a production reclaim estimate)"
        );
    }

    // A retry and a shorter partial prefix introduce no new nodes, and stay in the same cluster.
    let retry = Uuid::now_v7();
    assert_eq!(
        observe(
            &database,
            &first,
            &base_body,
            &default_hints,
            retry,
            now + 200,
            true
        )
        .await,
        cluster
    );
    let partial = json!({"input": base[..PREFIX / 2].to_vec()});
    let branch_parent = ConversationHints {
        turn_id: Some("long-parent".into()),
        branch_id: Some("main".into()),
        ..ConversationHints::default()
    };
    observe(
        &database,
        &first,
        &base_body,
        &branch_parent,
        Uuid::now_v7(),
        now + 300,
        true,
    )
    .await;
    let branch_child = ConversationHints {
        parent_turn_id: Some("long-parent".into()),
        branch_id: Some("alternate".into()),
        ..ConversationHints::default()
    };
    let branch_request = Uuid::now_v7();
    observe(
        &database,
        &first,
        &partial,
        &branch_child,
        branch_request,
        now + 301,
        true,
    )
    .await;
    let relation: String = sqlx::query_scalar("SELECT edge.relation_kind FROM conversation_edges edge JOIN conversation_observations observation ON observation.id = edge.to_observation_id WHERE observation.request_id = $1")
        .bind(branch_request.to_string()).fetch_one(&database.pool).await.unwrap();
    assert_eq!(relation, "branch");
    let partial_id = Uuid::now_v7();
    assert_eq!(
        observe(
            &database,
            &first,
            &partial,
            &default_hints,
            partial_id,
            now + 201,
            true
        )
        .await,
        cluster
    );
    assert_eq!(
        load(&database, first.tenant_id, partial_id).await.2.len(),
        PREFIX / 2
    );
    for scoped in [&other_key, &other_tenant] {
        assert_ne!(
            observe(
                &database,
                scoped,
                &base_body,
                &default_hints,
                Uuid::now_v7(),
                now + 202,
                true
            )
            .await,
            cluster
        );
    }
    // Identical hashes have separate ownership; an absent tenant cannot resolve a reference.
    let row = sqlx::query("SELECT atom_hashes_json, leaf_node_hash FROM conversation_observations WHERE request_id = $1")
        .bind(retry.to_string()).fetch_one(&database.pool).await.unwrap();
    let mut transaction = database.begin_write_transaction().await.unwrap();
    assert!(
        load_candidate_paths(&mut transaction, &Uuid::now_v7().to_string(), &[row], None)
            .await
            .is_err()
    );
    transaction.rollback().await.unwrap();

    let compacted = json!({"input":[{"role":"user","content":"summary"}, base[100].clone()]});
    let compaction = ConversationHints {
        compaction: true,
        ..ConversationHints::default()
    };
    assert_eq!(
        observe(
            &database,
            &first,
            &compacted,
            &compaction,
            Uuid::now_v7(),
            now + 203,
            true
        )
        .await,
        cluster
    );
    // A crash before commit leaves no observation; replaying the archive-only input is lossless.
    let rolled_back = Uuid::now_v7();
    observe(
        &database,
        &first,
        &base_body,
        &default_hints,
        rolled_back,
        now + 204,
        false,
    )
    .await;
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1")
            .bind(rolled_back.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    observe(
        &database,
        &first,
        &base_body,
        &default_hints,
        rolled_back,
        now + 204,
        true,
    )
    .await;
    assert_eq!(
        load(&database, first.tenant_id, rolled_back).await.2,
        all_hashes
    );

    // Projection crash/reclaim uses the unchanged durable snapshot/payload envelope.
    let projected = Uuid::now_v7();
    database
        .record_request_started(NewRequest {
            request_id: projected,
            key_id: first.key_id,
            tenant_id: first.tenant_id,
            protocol: "openai-responses".into(),
            model: "prefix-model".into(),
            request_object: "memory://prefix".into(),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    database
        .record_request_finished(FinishRequest {
            request_id: projected,
            first_output_ms: None,
            generation_duration_ms: None,
            status_code: 200,
            duration_ms: 1,
            input_tokens: 0,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            output_tokens: 0,
            service_tier: None,
            cost_micros: 0,
            error_code: None,
            response_object: "memory://prefix-response".into(),
        })
        .await
        .unwrap();
    let mut transaction = database.begin_write_transaction().await.unwrap();
    let input = ConversationProjectionEnqueueInput {
        request_id: projected,
        key: &first,
        request_json: &base_body,
        hints: &default_hints,
        client_name: Some("Codex"),
        upstream_response_id: None,
        observed_at: now + 205,
    };
    enqueue_conversation_projection_in_transaction(&mut transaction, input)
        .await
        .unwrap();
    transaction.commit().await.unwrap();
    let crashed = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(crashed, 1)
            .await
            .unwrap()[0]
            .request_id,
        projected
    );
    sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_expires_at = 0 WHERE request_id = $1")
        .bind(projected.to_string()).execute(&database.pool).await.unwrap();
    let owner = Uuid::now_v7();
    assert_eq!(
        database
            .claim_conversation_projection_tasks(owner, 1)
            .await
            .unwrap()[0]
            .request_id,
        projected
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(crashed, projected)
            .await
            .unwrap()
    );
    assert!(
        database
            .project_claimed_conversation_projection_task(owner, projected)
            .await
            .unwrap()
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(owner, projected)
            .await
            .unwrap()
    );
    assert_eq!(
        load(&database, first.tenant_id, projected).await.2,
        all_hashes
    );
    let empty_request = Uuid::now_v7();
    observe(
        &database,
        &other_key,
        &json!({}),
        &default_hints,
        empty_request,
        now + 207,
        true,
    )
    .await;
    let (empty_encoding, empty_fingerprint, empty_full) =
        load(&database, other_key.tenant_id, empty_request).await;
    assert_eq!(
        empty_encoding,
        r#"{"context_prefix_v1":null,"atom_count":0}"#
    );
    assert!(empty_fingerprint.is_empty() && empty_full.is_empty());
    // A false content_materialized assertion cannot publish a dangling path.
    let missing_body = json!({"input":"this sequence was never materialized"});
    let mut transaction = database.begin_write_transaction().await.unwrap();
    assert!(
        database
            .record_conversation_observation_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key: &other_key,
                    request_id: Uuid::now_v7(),
                    request_json: &missing_body,
                    hints: &default_hints,
                    client_name: Some("Codex"),
                    upstream_response_id: None,
                    observed_at: now + 208,
                    attach_request_record: false,
                    content_materialized: true,
                }
            )
            .await
            .is_err()
    );
    transaction.rollback().await.unwrap();
    // Same hash with inconsistent node data is rejected before a reference becomes visible.
    let leaf = build_prefix(&extract_atoms(&base_body))
        .pop()
        .unwrap()
        .node_hash;
    sqlx::query("UPDATE context_nodes SET atom_hash = 'inconsistent' WHERE tenant_id = $1 AND node_hash = $2")
        .bind(first.tenant_id.to_string()).bind(leaf).execute(&database.pool).await.unwrap();
    let mut transaction = database.begin_write_transaction().await.unwrap();
    assert!(
        database
            .record_conversation_observation_in_transaction(
                &mut transaction,
                ConversationObservationInput {
                    key: &first,
                    request_id: Uuid::now_v7(),
                    request_json: &base_body,
                    hints: &default_hints,
                    client_name: Some("Codex"),
                    upstream_response_id: None,
                    observed_at: now + 206,
                    attach_request_record: false,
                    content_materialized: true,
                }
            )
            .await
            .is_err()
    );
    transaction.rollback().await.unwrap();
    database.pool.close().await;
}

#[tokio::test]
async fn sqlite_long_prefix_storage_and_lossless_replay() {
    let directory = tempfile::tempdir().unwrap();
    contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("prefix.db").display()
    ))
    .await;
}

#[tokio::test]
async fn postgres_long_prefix_storage_and_lossless_replay() {
    // Existing all-targets GHA supplies this service; no new pipeline or ignored test.
    let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let admin = sqlx::PgPool::connect(&url).await.unwrap();
    let schema = format!("prefix_storage_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut isolated = url::Url::parse(&url).unwrap();
    isolated
        .query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    contract(isolated.as_str()).await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
