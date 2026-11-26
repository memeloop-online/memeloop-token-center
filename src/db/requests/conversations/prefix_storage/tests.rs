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
    let atoms = extract_atoms(&base_body);
    let nodes = build_prefix(&atoms);
    assert!(
        database
            .prepare_session_body_archive(owner, projected, atoms, &nodes, &[])
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

async fn prepared_paths_contract(url: &str) {
    let database = Database::connect_with_max(url, 8).await.unwrap();
    database.migrate().await.unwrap();
    let actor = key(&database, "prepared-prefix").await;
    let body = json!({"messages":[{"role":"user","content":"prepared prefix"}]});
    let now = unix_millis();
    let hints = ConversationHints {
        session_id: Some("prepared-session".into()),
        ..Default::default()
    };
    let original = Uuid::now_v7();
    observe(&database, &actor, &body, &hints, original, now, true).await;
    let (encoded, hashes, full) = load(&database, actor.tenant_id, original).await;
    let leaf: String = sqlx::query_scalar(
        "SELECT leaf_node_hash FROM conversation_observations WHERE request_id=$1",
    )
    .bind(original.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let mut read = database.pool.begin().await.unwrap();
    let candidates = fetch_conversation_candidates(
        &mut read,
        &actor.tenant_id.to_string(),
        &actor.principal_id.to_string(),
        &actor.key_id.to_string(),
        &hints,
        now + 1,
        &Uuid::now_v7().to_string(),
    )
    .await
    .unwrap();
    let prepared = PreparedCandidatePaths::load(
        &mut read,
        actor.tenant_id.to_string(),
        &candidates,
        Some(&leaf),
    )
    .await
    .unwrap();
    read.commit().await.unwrap();
    assert_eq!(
        prepared
            .for_fresh_candidates(&actor.tenant_id.to_string(), &candidates, Some(&leaf))
            .unwrap()
            .hashes(Some(&leaf), None)
            .unwrap(),
        full
    );
    let foreign = prepared
        .for_fresh_candidates(&Uuid::now_v7().to_string(), &candidates, Some(&leaf))
        .err()
        .expect("foreign tenant must reject prepared content");
    assert!(
        !foreign.retry_after_confirmed_rollback(),
        "tenant mismatch is not candidate churn"
    );
    assert!(
        prepared
            .for_fresh_candidates(
                &actor.tenant_id.to_string(),
                &candidates,
                Some("new-unprepared-root")
            )
            .is_err()
    );
    assert_eq!(
        candidate_hashes(&candidates[0], &prepared.paths).unwrap(),
        hashes
    );
    assert_eq!(
        serde_json::to_string(&Fingerprint::from_hashes(&hashes)).unwrap(),
        encoded
    );
    database.pool.close().await;
}

#[tokio::test]
async fn sqlite_prepared_prefix_reuses_only_verified_tenant_roots() {
    let directory = tempfile::tempdir().unwrap();
    prepared_paths_contract(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("prepared.db").display()
    ))
    .await;
}

#[tokio::test]
#[ignore = "requires CI PostgreSQL"]
async fn postgres_prefix_preload_releases_projection_scope_before_graph_receive() {
    prefix_preload_contract(false, false, false).await;
    prefix_preload_contract(true, false, false).await;
    prefix_preload_contract(false, true, false).await;
    prefix_preload_contract(true, true, false).await;
    prefix_preload_contract(false, false, true).await;
}

async fn prefix_preload_contract(change_roots: bool, identity_only: bool, pruned_reopened: bool) {
    let postgres = std::env::var("MTC_TEST_POSTGRES_URL").unwrap();
    let admin = sqlx::PgPool::connect(&postgres).await.unwrap();
    let schema = format!("prefix_preload_{}", Uuid::now_v7().simple());
    sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
        .execute(&admin)
        .await
        .unwrap();
    let mut url = url::Url::parse(&postgres).unwrap();
    url.query_pairs_mut()
        .append_pair("options", &format!("-csearch_path={schema}"));
    let database = Database::connect_with_max(url.as_str(), 8).await.unwrap();
    database.migrate().await.unwrap();
    let actor = key(&database, "preload-lock").await;
    let body = json!({"messages":[{"role":"user","content":"preload graph"}]});
    let now = unix_millis();
    let hints = ConversationHints {
        session_id: Some("preload-lock-session".into()),
        ..Default::default()
    };
    let candidate = Uuid::now_v7();
    let expected_cluster = observe(&database, &actor, &body, &hints, candidate, now, true).await;
    let changed_candidate = Uuid::now_v7();
    let changed_cluster = if change_roots {
        Some(
            observe(
                &database,
                &actor,
                &json!({"messages":[{"role":"user","content":"a newly committed candidate root"}]}),
                &ConversationHints {
                    session_id: Some("not-selected-yet".into()),
                    ..Default::default()
                },
                changed_candidate,
                now + 2,
                true,
            )
            .await,
        )
    } else {
        None
    };
    let (_, hashes, _) = load(&database, actor.tenant_id, candidate).await;
    let leaf: String = sqlx::query_scalar(
        "SELECT leaf_node_hash FROM conversation_observations WHERE request_id=$1",
    )
    .bind(candidate.to_string())
    .fetch_one(&database.pool)
    .await
    .unwrap();
    let request = Uuid::now_v7();
    let owner = Uuid::now_v7();
    sqlx::query("INSERT INTO request_records(id,tenant_id,key_id,created_at,completed_at,protocol,model,status_code,input_tokens,output_tokens,cost_micros,request_object,reservation_id) VALUES($1,$2,$3,$4,$4,'openai','preload',200,0,0,0,'gap://preload/request',$5)")
        .bind(request.to_string()).bind(actor.tenant_id.to_string()).bind(actor.key_id.to_string()).bind(now+1).bind(Uuid::now_v7().to_string()).execute(&database.pool).await.unwrap();
    sqlx::query(
        "INSERT INTO request_record_locators(id,created_at,tenant_id,key_id) VALUES($1,$2,$3,$4)",
    )
    .bind(request.to_string())
    .bind(now + 1)
    .bind(actor.tenant_id.to_string())
    .bind(actor.key_id.to_string())
    .execute(&database.pool)
    .await
    .unwrap();
    let snapshot = serde_json::to_string(&ConversationSemanticSnapshot {
        leaf: Some(leaf),
        atom_hashes: hashes,
        payload_digest: None,
    })
    .unwrap();
    sqlx::query("INSERT INTO conversation_projection_outbox(request_id,tenant_id,key_id,principal_id,request_json,hints_json,observed_at,semantic_snapshot_json,lease_owner,lease_expires_at) VALUES($1,$2,$3,$4,'{}',$5,$6,$7,$8,$9)")
        .bind(request.to_string()).bind(actor.tenant_id.to_string()).bind(actor.key_id.to_string()).bind(actor.principal_id.to_string()).bind(serde_json::to_string(&hints).unwrap()).bind(now+1).bind(&snapshot).bind(owner.to_string()).bind(now+60000).execute(&database.pool).await.unwrap();
    if identity_only {
        let key_snapshot = json!({"account_id":actor.account_id,"alias":actor.alias,
            "currency":actor.currency,"credential_generation":actor.credential_generation,
            "policy":actor.policy})
        .to_string();
        sqlx::query(
            "UPDATE conversation_projection_outbox SET key_snapshot_json=$2 WHERE request_id=$1",
        )
        .bind(request.to_string())
        .bind(key_snapshot)
        .execute(&database.pool)
        .await
        .unwrap();
    }
    if change_roots {
        sqlx::query("UPDATE conversation_projection_outbox SET attempts=1, lease_owner='terminal-v118', lease_expires_at=9223372036854775807, terminal_lease_owner=$2, terminal_lease_expires_at=$3 WHERE request_id=$1")
            .bind(request.to_string()).bind(owner.to_string()).bind(now+300000)
            .execute(&database.pool).await.unwrap();
    }
    if pruned_reopened {
        // Preserve a real non-empty historical observation and exercise the
        // pruned terminal branch, not the ordinary observation insertion.
        sqlx::query("UPDATE conversation_observations SET request_id=$1 WHERE request_id=$2")
            .bind(request.to_string())
            .bind(candidate.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        sqlx::query("UPDATE conversation_projection_outbox SET statistics_outcome='session_body' WHERE request_id=$1")
            .bind(request.to_string()).execute(&database.pool).await.unwrap();
        sqlx::query("INSERT INTO terminal_projection_outbox(request_id,reservation_id,tenant_id,key_id,account_id,created_at,completed_at,model,protocol,status_code,error_code,upstream_account_id,model_route_id,duration_ms,input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,generation_units,billing_unit,service_tier,currency,cost_micros,usage_basis,session_id,response_object,projected_at) VALUES($1,$2,$3,$4,$5,$6,$6,'pruned-preload','openai',200,'','','',0,0,0,0,0,0,'token','default','USD',0,'unknown','','',1)")
            .bind(request.to_string()).bind(Uuid::now_v7().to_string()).bind(actor.tenant_id.to_string())
            .bind(actor.key_id.to_string()).bind(actor.account_id.to_string()).bind(now+1)
            .execute(&database.pool).await.unwrap();
        sqlx::query("INSERT INTO observability_prune_boundaries(scope,before_day,recorded_at) VALUES('global',$1,$2)")
            .bind(now/86400000+1).bind(now).execute(&database.pool).await.unwrap();
    }
    // A real recursive SELECT is suspended before its result is received.
    sqlx::raw_sql("CREATE FUNCTION prefix_receive_gate() RETURNS BOOLEAN LANGUAGE plpgsql VOLATILE AS $$ BEGIN IF EXISTS (SELECT 1 FROM pg_locks WHERE pid=pg_backend_pid() AND relation='request_records'::regclass AND mode IN ('RowShareLock','RowExclusiveLock') AND granted) THEN RAISE EXCEPTION 'recursive prefix received under request scope lock'; END IF; PERFORM pg_advisory_xact_lock(91844,1); RETURN TRUE; END $$; ALTER TABLE context_nodes RENAME TO prefix_nodes; CREATE VIEW context_nodes AS SELECT * FROM prefix_nodes WHERE prefix_receive_gate()")
        .execute(&database.pool).await.unwrap();
    let mut blocker = database.pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(91844,1)")
        .execute(&mut *blocker)
        .await
        .unwrap();
    let worker = database.clone();
    let task = tokio::spawn(async move {
        if identity_only {
            worker
                .project_claimed_conversation_identity(owner, request)
                .await
                .map(|()| true)
        } else {
            worker
                .project_claimed_conversation_projection_task(owner, request)
                .await
        }
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_locks WHERE locktype='advisory' AND classid=91844 AND objid=1 AND NOT granted)").fetch_one(&database.pool).await.unwrap();
            if waiting {break;}
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }).await.unwrap();
    let mut probe = database.pool.begin().await.unwrap();
    sqlx::query("SET LOCAL statement_timeout='1s'")
        .execute(&mut *probe)
        .await
        .unwrap();
    lock_request_stats_projection_rebuild_in_transaction(&mut probe)
        .await
        .unwrap();
    sqlx::query("SELECT id FROM request_records WHERE id=$1 AND created_at=$2 FOR UPDATE NOWAIT")
        .bind(request.to_string())
        .bind(now + 1)
        .fetch_one(&mut *probe)
        .await
        .unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,$2))")
        .bind(format!(
            "{}:{}",
            actor.key_id,
            hints.session_id.as_deref().unwrap()
        ))
        .bind(EXPLICIT_SESSION_LOCK_SEED)
        .execute(&mut *probe)
        .await
        .unwrap();
    let replacement = Uuid::now_v7();
    if change_roots {
        // B commits a different root into A's candidate scope after A selected
        // its roots, while A is still receiving its immutable prefix graph.
        sqlx::query("UPDATE conversation_observations SET explicit_session_id=$2, created_at=$3 WHERE request_id=$1")
            .bind(changed_candidate.to_string()).bind(hints.session_id.as_deref().unwrap())
            .bind(now+1).execute(&mut *probe).await.unwrap();
    } else if !pruned_reopened {
        sqlx::query("UPDATE conversation_projection_outbox SET lease_owner=$2 WHERE request_id=$1")
            .bind(request.to_string())
            .bind(replacement.to_string())
            .execute(&mut *probe)
            .await
            .unwrap();
    }
    probe.commit().await.unwrap();
    blocker.rollback().await.unwrap();
    let result = task.await.unwrap();
    if pruned_reopened {
        assert!(
            result.unwrap(),
            "pruned reopened task must use the preloaded nonempty graph"
        );
        let row = sqlx::query("SELECT projected_at,statistics_outcome FROM conversation_projection_outbox WHERE request_id=$1")
            .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(row.get::<Option<i64>, _>("projected_at").is_some());
        assert_eq!(row.get::<String, _>("statistics_outcome"), "session_body");
        let actual_leaf: String = sqlx::query_scalar(
            "SELECT leaf_node_hash FROM conversation_observations WHERE request_id=$1",
        )
        .bind(request.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        let expected: ConversationSemanticSnapshot = serde_json::from_str(&snapshot).unwrap();
        assert_eq!(Some(actual_leaf), expected.leaf);
        let terminal: (i64,i64,i64)=sqlx::query_as("SELECT projected_at,cost_micros,attempts FROM terminal_projection_outbox WHERE request_id=$1")
            .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(
            terminal,
            (1, 0, 0),
            "canonical rebuild does not replay financial projection"
        );
        sqlx::raw_sql("DROP VIEW context_nodes; ALTER TABLE prefix_nodes RENAME TO context_nodes; DROP FUNCTION prefix_receive_gate()")
            .execute(&database.pool).await.unwrap();
        database.pool.close().await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
        return;
    }
    if change_roots {
        assert!(matches!(result, Err(AppError::Conflict(_))));
        let row = sqlx::query("SELECT terminal_lease_owner, terminal_lease_expires_at, projected_at FROM conversation_projection_outbox WHERE request_id=$1")
            .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(
            row.get::<Option<String>, _>("terminal_lease_owner"),
            Some(owner.to_string())
        );
        assert!(row.get::<Option<i64>, _>("projected_at").is_none());
        let expiry = row.get::<i64, _>("terminal_lease_expires_at");
        assert!(
            expiry <= unix_millis() + 1000,
            "changed roots must not retain the five-minute lease"
        );
        // Advance only this fixture's retry clock, then use the real claim
        // path to re-read fresh candidates under a new owner.
        sqlx::query("UPDATE conversation_projection_outbox SET terminal_lease_expires_at=$2 WHERE request_id=$1")
            .bind(request.to_string()).bind(unix_millis()-1).execute(&database.pool).await.unwrap();
        let claims = database
            .claim_conversation_projection_tasks(replacement, 1)
            .await
            .unwrap();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].request_id, request);
        let held: Option<i64> = sqlx::query_scalar("SELECT terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id=$1")
            .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
        assert!(
            !database
                .schedule_rolled_back_conversation_retry(request, owner)
                .await
                .unwrap()
        );
        let unchanged: Option<i64> = sqlx::query_scalar("SELECT terminal_lease_expires_at FROM conversation_projection_outbox WHERE request_id=$1")
            .bind(request.to_string()).fetch_one(&database.pool).await.unwrap();
        assert_eq!(held, unchanged, "old owner cannot shorten the new lease");
    } else if !identity_only {
        assert!(
            !result.unwrap(),
            "a replaced lease cannot publish the prepared graph"
        );
    } else {
        result.unwrap(); // Missing fresh owner grants no authority to publish.
    }
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM conversation_observations WHERE request_id=$1")
            .bind(request.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(count, 0);
    assert!(
        database
            .project_claimed_conversation_projection_task(replacement, request)
            .await
            .unwrap()
    );
    assert!(
        !database
            .project_claimed_conversation_projection_task(replacement, request)
            .await
            .unwrap()
    );
    let cluster: String =
        sqlx::query_scalar("SELECT cluster_id FROM conversation_observations WHERE request_id=$1")
            .bind(request.to_string())
            .fetch_one(&database.pool)
            .await
            .unwrap();
    assert_eq!(
        cluster,
        changed_cluster.unwrap_or(expected_cluster).to_string()
    );
    sqlx::raw_sql("DROP VIEW context_nodes; ALTER TABLE prefix_nodes RENAME TO context_nodes; DROP FUNCTION prefix_receive_gate()")
        .execute(&database.pool).await.unwrap();
    database.pool.close().await;
    sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}
