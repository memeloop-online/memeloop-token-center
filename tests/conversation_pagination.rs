use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use memeloop_token_center::{
    AppState, api,
    config::Config,
    conversation::ConversationHints,
    db::{CreateKeyInput, NewRequest, SessionArchiveCommitInput, SessionArchiveTarget},
    model::{AuthenticatedKey, KeyPolicy},
};
use rust_decimal::Decimal;
use serde_json::{Value, json};
use sqlx::{AnyPool, Row};
use tempfile::TempDir;
use tower::ServiceExt;
use uuid::Uuid;

const PEPPER: &[u8] = b"conversation paging test pepper is long enough";

struct Fixture {
    _directory: TempDir,
    state: AppState,
    pool: AnyPool,
    issued_key: String,
    key: AuthenticatedKey,
}

impl Fixture {
    async fn new(name: &str) -> Self {
        let directory = tempfile::tempdir().expect("temporary database directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join(format!("{name}.sqlite")).display()
        );
        let mut config = Config::for_test(database_url.clone());
        config.key_pepper = String::from_utf8(PEPPER.to_vec()).expect("UTF-8 pepper");
        let state = AppState::initialize(config)
            .await
            .expect("initialize state");
        let issued = state
            .db
            .create_key(
                CreateKeyInput {
                    tenant_external_id: format!("conversation-{name}"),
                    principal_external_id: "linux-codex".into(),
                    alias: "Linux Codex".into(),
                    currency: "USD".into(),
                    policy: KeyPolicy {
                        allowed_models: vec!["*".into()],
                        ..KeyPolicy::default()
                    },
                    initial_balance: Decimal::TEN,
                    idempotency_key: None,
                },
                PEPPER,
            )
            .await
            .expect("create key");
        let key = state
            .db
            .authenticate_key(&issued.key, PEPPER)
            .await
            .expect("authenticate key");
        sqlx::any::install_default_drivers();
        let pool = AnyPool::connect(&database_url)
            .await
            .expect("connect fixture pool");
        Self {
            _directory: directory,
            state,
            pool,
            issued_key: issued.key,
            key,
        }
    }

    async fn start_request(&self, request_object: &str) -> Uuid {
        let request_id = Uuid::now_v7();
        self.state
            .db
            .record_request_started(NewRequest {
                request_id,
                key_id: self.key.key_id,
                tenant_id: self.key.tenant_id,
                protocol: "openai-responses".into(),
                model: "gpt-conversation".into(),
                request_object: request_object.into(),
                reservation_id: Uuid::now_v7(),
                upstream_account_id: None,
                model_route_id: None,
            })
            .await
            .expect("start request");
        request_id
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        let response = api::router(self.state.clone())
            .oneshot(
                Request::builder()
                    .uri(path)
                    .header(header::AUTHORIZATION, format!("Bearer {}", self.issued_key))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("HTTP response");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded response body");
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        (status, body)
    }
}

async fn get_for_bearer(state: &AppState, path: &str, bearer: &str) -> (StatusCode, Value) {
    let response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header(header::AUTHORIZATION, format!("Bearer {bearer}"))
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("HTTP response");
    let status = response.status();
    let body = serde_json::from_slice(
        &to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded response body"),
    )
    .unwrap_or(Value::Null);
    (status, body)
}

async fn observe_request(
    state: &AppState,
    key: &AuthenticatedKey,
    request_json: &Value,
    hints: &ConversationHints,
    client_name: &str,
) -> (Uuid, Uuid) {
    let request_id = Uuid::now_v7();
    state
        .db
        .record_request_started(NewRequest {
            request_id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-responses".into(),
            model: "gpt-conversation".into(),
            request_object: format!("memory://{request_id}"),
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .expect("start request");
    let cluster_id = state
        .db
        .record_conversation_observation(key, request_id, request_json, hints, Some(client_name))
        .await
        .expect("record conversation observation");
    (request_id, cluster_id)
}

async fn execute_sql_script(pool: &AnyPool, sql: &str) {
    for statement in sql
        .split(';')
        .map(str::trim)
        .filter(|statement| !statement.is_empty())
    {
        sqlx::query(statement)
            .execute(pool)
            .await
            .expect("execute test migration statement");
    }
}

#[tokio::test]
async fn candidates_are_visible_but_do_not_merge_and_empty_context_never_links() {
    let fixture = Fixture::new("candidate").await;
    let first_request = fixture.start_request("memory://first").await;
    let first_cluster = fixture
        .state
        .db
        .record_conversation_observation(
            &fixture.key,
            first_request,
            &json!({"input": [{"role": "user", "content": "alpha"}]}),
            &ConversationHints::default(),
            Some("Codex"),
        )
        .await
        .expect("first observation");
    let second_request = fixture.start_request("memory://second").await;
    let second_cluster = fixture
        .state
        .db
        .record_conversation_observation(
            &fixture.key,
            second_request,
            &json!({"input": [{"role": "user", "content": "unrelated"}]}),
            &ConversationHints::default(),
            Some("Codex"),
        )
        .await
        .expect("candidate observation");
    assert_ne!(
        first_cluster, second_cluster,
        "a candidate must not force a merge"
    );

    let continuation = fixture.start_request("memory://continuation").await;
    let continued_cluster = fixture
        .state
        .db
        .record_conversation_observation(
            &fixture.key,
            continuation,
            &json!({"input": [
                {"role": "user", "content": "unrelated"},
                {"role": "assistant", "content": "answer"}
            ]}),
            &ConversationHints::default(),
            Some("Codex"),
        )
        .await
        .expect("strong prefix observation");
    assert_eq!(
        continued_cluster, second_cluster,
        "strong evidence upgrades the target cluster"
    );

    let empty_one = fixture.start_request("memory://empty-one").await;
    let empty_one_cluster = fixture
        .state
        .db
        .record_conversation_observation(
            &fixture.key,
            empty_one,
            &json!({}),
            &ConversationHints::default(),
            Some("Codex"),
        )
        .await
        .expect("first empty observation");
    let empty_two = fixture.start_request("memory://empty-two").await;
    let empty_two_cluster = fixture
        .state
        .db
        .record_conversation_observation(
            &fixture.key,
            empty_two,
            &json!({"input": []}),
            &ConversationHints::default(),
            Some("Codex"),
        )
        .await
        .expect("second empty observation");
    assert_ne!(empty_one_cluster, empty_two_cluster);
    assert_ne!(empty_one_cluster, second_cluster);
    assert_ne!(empty_two_cluster, second_cluster);

    let (status, clusters) = fixture.get("/self/v1/conversations?limit=50").await;
    assert_eq!(status, StatusCode::OK);
    let target = clusters
        .as_array()
        .expect("cluster array")
        .iter()
        .find(|cluster| cluster["cluster_id"] == second_cluster.to_string())
        .expect("candidate target cluster");
    assert_eq!(target["request_count"], 2);
    assert_eq!(target["candidate_edge_count"], 1);

    let (status, detail) = fixture
        .get(&format!(
            "/self/v1/conversations/{second_cluster}?limit=100"
        ))
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(detail["requests"].as_array().map(Vec::len), Some(2));
    let relations: Vec<_> = detail["edges"]
        .as_array()
        .expect("edges")
        .iter()
        .map(|edge| edge["relation"].as_str().expect("relation"))
        .collect();
    assert!(relations.contains(&"candidate"));
    assert!(relations.contains(&"continues"));
}

#[tokio::test]
async fn self_conversations_never_infer_or_expose_metadata_across_stable_keys() {
    let directory = tempfile::tempdir().expect("temporary database directory");
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("key-isolation.sqlite").display()
    );
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).expect("UTF-8 pepper");
    let state = AppState::initialize(config)
        .await
        .expect("initialize state");
    let create = |alias: &str| CreateKeyInput {
        tenant_external_id: "conversation-key-isolation".into(),
        principal_external_id: "shared-principal".into(),
        alias: alias.into(),
        currency: "USD".into(),
        policy: KeyPolicy {
            allowed_models: vec!["*".into()],
            ..KeyPolicy::default()
        },
        initial_balance: Decimal::TEN,
        idempotency_key: None,
    };
    let issued_a = state
        .db
        .create_key(create("Credential A"), PEPPER)
        .await
        .expect("create credential A");
    let issued_b = state
        .db
        .create_key(create("Credential B"), PEPPER)
        .await
        .expect("create credential B");
    let key_a = state
        .db
        .authenticate_key(&issued_a.key, PEPPER)
        .await
        .expect("authenticate credential A");
    let key_b = state
        .db
        .authenticate_key(&issued_b.key, PEPPER)
        .await
        .expect("authenticate credential B");
    assert_eq!(key_a.tenant_id, key_b.tenant_id);
    assert_eq!(key_a.principal_id, key_b.principal_id);
    assert_ne!(key_a.key_id, key_b.key_id);

    let (request_a_prefix, cluster_a_prefix) = observe_request(
        &state,
        &key_a,
        &json!({"input": [{"role": "user", "content": "shared prefix"}]}),
        &ConversationHints {
            session_id: Some("credential-a-private-session".into()),
            ..ConversationHints::default()
        },
        "PrefixClient",
    )
    .await;
    let (request_b_prefix, cluster_b_prefix) = observe_request(
        &state,
        &key_b,
        &json!({"input": [
            {"role": "user", "content": "shared prefix"},
            {"role": "assistant", "content": "credential B continuation"}
        ]}),
        &ConversationHints::default(),
        "PrefixClient",
    )
    .await;
    assert_ne!(
        cluster_a_prefix, cluster_b_prefix,
        "a semantic prefix on another stable key must not merge conversations"
    );

    let (request_a_candidate, _) = observe_request(
        &state,
        &key_a,
        &json!({"input": [{"role": "user", "content": "cross-key candidate source"}]}),
        &ConversationHints::default(),
        "SideChannelClient",
    )
    .await;
    let (request_b_candidate, cluster_b_candidate) = observe_request(
        &state,
        &key_b,
        &json!({"input": [{"role": "user", "content": "cross-key candidate target"}]}),
        &ConversationHints::default(),
        "SideChannelClient",
    )
    .await;

    let (status, list) =
        get_for_bearer(&state, "/self/v1/conversations?limit=50", &issued_b.key).await;
    assert_eq!(status, StatusCode::OK);
    let clusters = list.as_array().expect("conversation list");
    assert_eq!(clusters.len(), 2);
    for cluster in clusters {
        assert_eq!(cluster["explicit_session_id"], Value::Null);
        assert_eq!(cluster["request_count"], 1);
        assert_eq!(cluster["candidate_edge_count"], 0);
        let cluster_id = cluster["cluster_id"].as_str().expect("cluster id");
        let (status, detail) = get_for_bearer(
            &state,
            &format!("/self/v1/conversations/{cluster_id}"),
            &issued_b.key,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(detail["cluster"]["explicit_session_id"], Value::Null);
        let requests = detail["requests"].as_array().expect("request list");
        assert_eq!(requests.len(), 1);
        let visible = requests[0]["request_id"].as_str().expect("request id");
        assert!(
            visible == request_b_prefix.to_string() || visible == request_b_candidate.to_string()
        );
        assert_ne!(visible, request_a_prefix.to_string());
        assert_ne!(visible, request_a_candidate.to_string());
        assert!(detail["edges"].as_array().is_some_and(Vec::is_empty));
    }

    // Reproduce a legacy cross-key candidate edge and leaked projection values.
    // Both the fresh-install backfill and the upgrade repair must ignore/clear it.
    let pool = AnyPool::connect(&database_url)
        .await
        .expect("connect key-isolation fixture pool");
    let source_observation: String =
        sqlx::query_scalar("SELECT id FROM conversation_observations WHERE request_id = $1")
            .bind(request_a_candidate.to_string())
            .fetch_one(&pool)
            .await
            .expect("source observation");
    let target_observation: String =
        sqlx::query_scalar("SELECT id FROM conversation_observations WHERE request_id = $1")
            .bind(request_b_candidate.to_string())
            .fetch_one(&pool)
            .await
            .expect("target observation");
    sqlx::query(
        "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES ($1, $2, $3, $4, 'candidate', 100, '{}', 0, 1, $5)",
    )
    .bind(Uuid::now_v7().to_string())
    .bind(cluster_b_candidate.to_string())
    .bind(source_observation)
    .bind(target_observation)
    .bind(memeloop_token_center::db::unix_millis())
    .execute(&pool)
    .await
    .expect("insert legacy cross-key candidate edge");

    sqlx::query("DELETE FROM conversation_key_clusters")
        .execute(&pool)
        .await
        .expect("clear projections for fresh backfill");
    execute_sql_script(
        &pool,
        include_str!("../migrations/common/0025_conversation_key_clusters.sql"),
    )
    .await;
    let fresh_projection: (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(explicit_session_id), COALESCE(SUM(candidate_edge_count), 0) FROM conversation_key_clusters WHERE key_id = $1",
    )
    .bind(key_b.key_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("fresh key-local projection");
    assert_eq!(fresh_projection, (2, 0, 0));

    sqlx::query(
        "UPDATE conversation_key_clusters SET explicit_session_id = 'legacy-secret', candidate_edge_count = 99 WHERE key_id = $1",
    )
    .bind(key_b.key_id.to_string())
    .execute(&pool)
    .await
    .expect("corrupt legacy projection");
    execute_sql_script(
        &pool,
        include_str!("../migrations/common/0031_conversation_key_isolation.sql"),
    )
    .await;
    let repaired_projection: (i64, i64, i64) = sqlx::query_as(
        "SELECT COUNT(*), COUNT(explicit_session_id), COALESCE(SUM(candidate_edge_count), 0) FROM conversation_key_clusters WHERE key_id = $1",
    )
    .bind(key_b.key_id.to_string())
    .fetch_one(&pool)
    .await
    .expect("repaired key-local projection");
    assert_eq!(repaired_projection, (2, 0, 0));
}

#[tokio::test]
async fn postgres_conversations_are_key_scoped() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let mut config = Config::for_test(database_url);
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).expect("UTF-8 pepper");
    let state = AppState::initialize(config)
        .await
        .expect("initialize PostgreSQL state");
    let unique = Uuid::now_v7();
    let create = |alias: &str| CreateKeyInput {
        tenant_external_id: format!("conversation-key-isolation-pg-{unique}"),
        principal_external_id: "shared-principal".into(),
        alias: alias.into(),
        currency: "USD".into(),
        policy: KeyPolicy {
            allowed_models: vec!["*".into()],
            ..KeyPolicy::default()
        },
        initial_balance: Decimal::TEN,
        idempotency_key: None,
    };
    let issued_a = state
        .db
        .create_key(create("Credential A"), PEPPER)
        .await
        .expect("create PostgreSQL credential A");
    let issued_b = state
        .db
        .create_key(create("Credential B"), PEPPER)
        .await
        .expect("create PostgreSQL credential B");
    let key_a = state
        .db
        .authenticate_key(&issued_a.key, PEPPER)
        .await
        .expect("authenticate PostgreSQL credential A");
    let key_b = state
        .db
        .authenticate_key(&issued_b.key, PEPPER)
        .await
        .expect("authenticate PostgreSQL credential B");
    assert_eq!(key_a.principal_id, key_b.principal_id);
    assert_ne!(key_a.key_id, key_b.key_id);

    let (request_a_prefix, cluster_a_prefix) = observe_request(
        &state,
        &key_a,
        &json!({"input": [{"role": "user", "content": "shared PostgreSQL prefix"}]}),
        &ConversationHints {
            session_id: Some("credential-a-private-postgres-session".into()),
            ..ConversationHints::default()
        },
        "PrefixClient",
    )
    .await;
    let (request_b_prefix, cluster_b_prefix) = observe_request(
        &state,
        &key_b,
        &json!({"input": [
            {"role": "user", "content": "shared PostgreSQL prefix"},
            {"role": "assistant", "content": "credential B continuation"}
        ]}),
        &ConversationHints::default(),
        "PrefixClient",
    )
    .await;
    assert_ne!(cluster_a_prefix, cluster_b_prefix);
    let (request_a_candidate, _) = observe_request(
        &state,
        &key_a,
        &json!({"input": [{"role": "user", "content": "PostgreSQL candidate source"}]}),
        &ConversationHints::default(),
        "SideChannelClient",
    )
    .await;
    let (request_b_candidate, _) = observe_request(
        &state,
        &key_b,
        &json!({"input": [{"role": "user", "content": "PostgreSQL candidate target"}]}),
        &ConversationHints::default(),
        "SideChannelClient",
    )
    .await;

    let clusters = state
        .db
        .conversation_clusters(
            key_b.key_id,
            memeloop_token_center::db::ConversationListFilter {
                limit: 50,
                before_updated_at: None,
                before_cluster_id: None,
            },
        )
        .await
        .expect("PostgreSQL key-local conversation list");
    assert_eq!(clusters.len(), 2);
    for cluster in clusters {
        assert_eq!(cluster.explicit_session_id, None);
        assert_eq!(cluster.request_count, 1);
        assert_eq!(cluster.candidate_edge_count, 0);
        let detail = state
            .db
            .conversation_cluster_detail(
                key_b.key_id,
                cluster.cluster_id,
                memeloop_token_center::db::ConversationDetailFilter {
                    limit: 10,
                    before_created_at: None,
                    before_request_id: None,
                },
            )
            .await
            .expect("PostgreSQL key-local conversation detail");
        assert_eq!(detail.requests.len(), 1);
        let visible = detail.requests[0].request.request_id;
        assert!(visible == request_b_prefix || visible == request_b_candidate);
        assert_ne!(visible, request_a_prefix);
        assert_ne!(visible, request_a_candidate);
        assert!(detail.edges.is_empty());
    }
}

#[tokio::test]
async fn archive_conversation_and_import_metadata_are_one_atomic_idempotent_commit() {
    let fixture = Fixture::new("archive-atomic").await;
    let parent_request = fixture.start_request("memory://parent").await;
    fixture
        .state
        .db
        .record_conversation_observation(
            &fixture.key,
            parent_request,
            &json!({"input": [{"role": "user", "content": "parent"}]}),
            &ConversationHints {
                session_id: Some("archive-session".into()),
                turn_id: Some("parent-turn".into()),
                ..ConversationHints::default()
            },
            Some("Codex"),
        )
        .await
        .expect("parent observation");
    let child_request = fixture.start_request("gap://archive/request").await;
    let child_created_at: i64 =
        sqlx::query_scalar("SELECT created_at FROM request_record_locators WHERE id = $1")
            .bind(child_request.to_string())
            .fetch_one(&fixture.pool)
            .await
            .expect("child locator");

    let trigger = format!(
        "CREATE TRIGGER fail_archive_reference BEFORE UPDATE OF request_object ON request_records WHEN OLD.id = '{}' BEGIN SELECT RAISE(ABORT, 'injected archive reference failure'); END",
        child_request
    );
    sqlx::query(&trigger)
        .execute(&fixture.pool)
        .await
        .expect("install failure trigger");

    let baseline = archive_state(&fixture).await;
    let target = SessionArchiveTarget {
        tenant_id: fixture.key.tenant_id,
        target_request_id: child_request,
        request_created_at: child_created_at,
        key: fixture.key.clone(),
        external_event_hash: "e".repeat(64),
        source_created_at: child_created_at,
        source_model: "gpt-conversation".into(),
        replay: false,
    };
    let request_json = json!({
        "input": [
            {"role": "user", "content": "parent"},
            {"role": "assistant", "content": "child"}
        ]
    });
    let hints = ConversationHints {
        session_id: Some("archive-session".into()),
        turn_id: Some("child-turn".into()),
        parent_turn_id: Some("parent-turn".into()),
        ..ConversationHints::default()
    };
    let failed = fixture
        .state
        .db
        .commit_session_archive_request(archive_commit_input(&target, &request_json, &hints))
        .await;
    assert!(
        failed.is_err(),
        "the injected post-observation write must fail"
    );
    assert_eq!(archive_state(&fixture).await, baseline);
    let stored_ref: String = sqlx::query_scalar(
        "SELECT request_object FROM request_records WHERE id = $1 AND created_at = $2",
    )
    .bind(child_request.to_string())
    .bind(child_created_at)
    .fetch_one(&fixture.pool)
    .await
    .expect("request reference");
    assert_eq!(stored_ref, "gap://archive/request");

    sqlx::query("DROP TRIGGER fail_archive_reference")
        .execute(&fixture.pool)
        .await
        .expect("remove failure trigger");
    assert!(
        fixture
            .state
            .db
            .commit_session_archive_request(archive_commit_input(&target, &request_json, &hints,))
            .await
            .expect("successful archive commit")
    );
    let applied = archive_state(&fixture).await;
    assert_eq!(applied.observations, baseline.observations + 1);
    assert_eq!(applied.edges, baseline.edges + 1);
    assert_eq!(
        applied.projection_requests,
        baseline.projection_requests + 1
    );
    assert_eq!(applied.import_records, 1);
    assert_eq!(applied.checkpoint_records, 1);

    assert!(
        !fixture
            .state
            .db
            .commit_session_archive_request(archive_commit_input(&target, &request_json, &hints,))
            .await
            .expect("idempotent replay")
    );
    assert_eq!(archive_state(&fixture).await, applied);
}

fn archive_commit_input<'a>(
    target: &'a SessionArchiveTarget,
    request_json: &'a Value,
    hints: &'a ConversationHints,
) -> SessionArchiveCommitInput<'a> {
    SessionArchiveCommitInput {
        tenant_external_id: "conversation-archive-atomic",
        archive_source: "atomic-fixture",
        external_request_id: "archive-child",
        target,
        record_digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        request_digest: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"),
        response_digest: Some("cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"),
        request_object: Some("inline-json:{\"archived\":true}"),
        response_object: Some("inline-json:{\"ok\":true}"),
        request_json: Some(request_json),
        conversation_hints: hints,
        client_name: Some("Codex"),
        source_started_at: target.source_created_at,
        source_completed_at: None,
        identity_proof_kind: "test-exact-proof-v1",
        identity_proof_digest: "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
        correlation_proof_digest: "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ArchiveState {
    observations: i64,
    edges: i64,
    semantic_atoms: i64,
    context_nodes: i64,
    projection_requests: i64,
    import_records: i64,
    checkpoint_records: i64,
}

async fn archive_state(fixture: &Fixture) -> ArchiveState {
    ArchiveState {
        observations: count(&fixture.pool, "conversation_observations").await,
        edges: count(&fixture.pool, "conversation_edges").await,
        semantic_atoms: count(&fixture.pool, "semantic_atoms").await,
        context_nodes: count(&fixture.pool, "context_nodes").await,
        projection_requests: sqlx::query_scalar(
            "SELECT COALESCE(SUM(request_count), 0) FROM conversation_key_clusters WHERE key_id = $1",
        )
        .bind(fixture.key.key_id.to_string())
        .fetch_one(&fixture.pool)
        .await
        .expect("projection count"),
        import_records: count(&fixture.pool, "session_archive_import_records").await,
        checkpoint_records: count(&fixture.pool, "session_archive_import_checkpoints").await,
    }
}

async fn count(pool: &AnyPool, table: &str) -> i64 {
    let query = format!("SELECT COUNT(*) FROM {table}");
    sqlx::query_scalar(&query)
        .fetch_one(pool)
        .await
        .expect("table count")
}

#[tokio::test]
async fn sqlite_110k_history_is_keyset_paginated_and_uses_covering_indexes() {
    let fixture = Fixture::new("scale").await;
    let cluster_id = Uuid::now_v7();
    let base = 1_700_000_000_000_i64;
    sqlx::query(
        "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, 'large-session', $4, $5)",
    )
    .bind(cluster_id.to_string())
    .bind(fixture.key.tenant_id.to_string())
    .bind(fixture.key.principal_id.to_string())
    .bind(base)
    .bind(base + 110_001)
    .execute(&fixture.pool)
    .await
    .expect("large cluster");
    sqlx::query(
        "WITH RECURSIVE sequence(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 110001) INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, request_object, response_object, reservation_id, conversation_cluster_id) SELECT printf('00000000-0000-7000-8000-%012x', value), $1, $2, $3 + value, 'openai-responses', 'gpt-scale', 200, 1, 1, 1, 1, 'memory://request', 'memory://response', printf('10000000-0000-7000-8000-%012x', value), $4 FROM sequence",
    )
    .bind(fixture.key.tenant_id.to_string())
    .bind(fixture.key.key_id.to_string())
    .bind(base)
    .bind(cluster_id.to_string())
    .execute(&fixture.pool)
    .await
    .expect("110k request history");
    sqlx::query(
        "WITH RECURSIVE sequence(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 110001) INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, atom_hashes_json, client_name, created_at, inference_version, compaction) SELECT printf('20000000-0000-7000-8000-%012x', value), $1, printf('00000000-0000-7000-8000-%012x', value), $2, '[]', 'Codex', $3 + value, 2, 0 FROM sequence",
    )
    .bind(cluster_id.to_string())
    .bind(fixture.key.key_id.to_string())
    .bind(base)
    .execute(&fixture.pool)
    .await
    .expect("110k conversation members");
    sqlx::query(
        "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, 'large-session', $3, 110001, 0)",
    )
    .bind(fixture.key.key_id.to_string())
    .bind(cluster_id.to_string())
    .bind(base + 110_001)
    .execute(&fixture.pool)
    .await
    .expect("large projection");
    sqlx::query(
        "WITH RECURSIVE sequence(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 121) INSERT INTO conversation_clusters (id, tenant_id, principal_id, created_at, updated_at) SELECT printf('30000000-0000-7000-8000-%012x', value), $1, $2, $3 + value, $3 + value FROM sequence",
    )
    .bind(fixture.key.tenant_id.to_string())
    .bind(fixture.key.principal_id.to_string())
    .bind(base + 200_000)
    .execute(&fixture.pool)
    .await
    .expect("paged clusters");
    sqlx::query(
        "WITH RECURSIVE sequence(value) AS (VALUES(1) UNION ALL SELECT value + 1 FROM sequence WHERE value < 121) INSERT INTO conversation_key_clusters (key_id, cluster_id, updated_at, request_count, candidate_edge_count) SELECT $1, printf('30000000-0000-7000-8000-%012x', value), $2 + value, 1, 0 FROM sequence",
    )
    .bind(fixture.key.key_id.to_string())
    .bind(base + 200_000)
    .execute(&fixture.pool)
    .await
    .expect("paged projections");

    let (status, first_clusters) = fixture.get("/self/v1/conversations?limit=999").await;
    assert_eq!(status, StatusCode::OK);
    let first_clusters = first_clusters.as_array().expect("first cluster page");
    assert_eq!(first_clusters.len(), 100, "list limit must be capped");
    let last_cluster = first_clusters.last().expect("list cursor row");
    let second_cluster_path = format!(
        "/self/v1/conversations?limit=100&before_updated_at={}&before_cluster_id={}",
        last_cluster["updated_at"].as_i64().expect("updated_at"),
        last_cluster["cluster_id"].as_str().expect("cluster id")
    );
    let (status, second_clusters) = fixture.get(&second_cluster_path).await;
    assert_eq!(status, StatusCode::OK);
    let second_clusters = second_clusters.as_array().expect("second cluster page");
    assert_eq!(second_clusters.len(), 22);
    let first_ids: std::collections::HashSet<_> = first_clusters
        .iter()
        .map(|cluster| cluster["cluster_id"].as_str().expect("cluster id"))
        .collect();
    assert!(second_clusters.iter().all(|cluster| !first_ids.contains(cluster["cluster_id"].as_str().expect("cluster id"))));

    let detail_path = format!("/self/v1/conversations/{cluster_id}?limit=999");
    let (status, first_detail) = fixture.get(&detail_path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first_detail["cluster"]["request_count"], 110_001);
    assert_eq!(first_detail["requests"].as_array().map(Vec::len), Some(200));
    assert_eq!(first_detail["has_more"], true);
    assert_eq!(first_detail["edges_truncated"], false);
    let cursor = &first_detail["next_cursor"];
    let older_path = format!(
        "/self/v1/conversations/{cluster_id}?limit=200&before_created_at={}&before_request_id={}",
        cursor["before_created_at"]
            .as_i64()
            .expect("request cursor time"),
        cursor["before_request_id"]
            .as_str()
            .expect("request cursor id")
    );
    let (status, older_detail) = fixture.get(&older_path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(older_detail["requests"].as_array().map(Vec::len), Some(200));
    let first_request_ids: std::collections::HashSet<_> = first_detail["requests"]
        .as_array()
        .expect("requests")
        .iter()
        .map(|request| request["request_id"].as_str().expect("request id"))
        .collect();
    assert!(
        older_detail["requests"]
            .as_array()
            .expect("older requests")
            .iter()
            .all(|request| !first_request_ids
                .contains(request["request_id"].as_str().expect("request id")))
    );

    let (status, _) = fixture
        .get("/self/v1/conversations?before_updated_at=1")
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = fixture
        .get(&format!(
            "/self/v1/conversations/{cluster_id}?before_created_at=1"
        ))
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let list_plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT cluster_id, updated_at, request_count FROM conversation_key_clusters WHERE key_id = $1 ORDER BY updated_at DESC, cluster_id DESC LIMIT 100",
    )
    .bind(fixture.key.key_id.to_string())
    .fetch_all(&fixture.pool)
    .await
    .expect("list query plan");
    let list_plan = list_plan
        .iter()
        .map(|row| row.try_get::<String, _>("detail").expect("plan detail"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        list_plan.contains("conversation_key_clusters_page_idx"),
        "{list_plan}"
    );
    let detail_plan = sqlx::query(
        "EXPLAIN QUERY PLAN SELECT id, created_at FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY created_at DESC, id DESC LIMIT 201",
    )
    .bind(fixture.key.key_id.to_string())
    .bind(cluster_id.to_string())
    .fetch_all(&fixture.pool)
    .await
    .expect("detail query plan");
    let detail_plan = detail_plan
        .iter()
        .map(|row| row.try_get::<String, _>("detail").expect("plan detail"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        detail_plan.contains("request_records_conversation_time_idx"),
        "{detail_plan}"
    );
}

#[tokio::test]
async fn postgres_110k_conversation_pages_are_indexed_and_bounded() {
    let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
        return;
    };
    let mut config = Config::for_test(database_url.clone());
    config.key_pepper = String::from_utf8(PEPPER.to_vec()).expect("UTF-8 pepper");
    let state = AppState::initialize(config)
        .await
        .expect("initialize PostgreSQL state");
    let unique = Uuid::now_v7();
    let issued = state
        .db
        .create_key(
            CreateKeyInput {
                tenant_external_id: format!("conversation-pg-{unique}"),
                principal_external_id: "postgres-scale".into(),
                alias: "PostgreSQL scale".into(),
                currency: "USD".into(),
                policy: KeyPolicy {
                    allowed_models: vec!["*".into()],
                    ..KeyPolicy::default()
                },
                initial_balance: Decimal::TEN,
                idempotency_key: None,
            },
            PEPPER,
        )
        .await
        .expect("create PostgreSQL key");
    let key = state
        .db
        .authenticate_key(&issued.key, PEPPER)
        .await
        .expect("authenticate PostgreSQL key");
    sqlx::any::install_default_drivers();
    let pool = AnyPool::connect(&database_url)
        .await
        .expect("connect PostgreSQL pool");
    let cluster_id = Uuid::now_v7();
    let base = memeloop_token_center::db::unix_millis();
    sqlx::query(
        "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, 'postgres-large-session', $4, $5)",
    )
    .bind(cluster_id.to_string())
    .bind(key.tenant_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(base)
    .bind(base + 110_001)
    .execute(&pool)
    .await
    .expect("PostgreSQL large cluster");
    let seed = unique.to_string();
    sqlx::query(
        "WITH source AS (SELECT value, md5($5 || value::TEXT) AS hash FROM generate_series(1, 110001) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, request_object, response_object, reservation_id, conversation_cluster_id) SELECT id, $1, $2, $3 + value, 'openai-responses', 'gpt-scale', 200, 1, 1, 1, 1, 'memory://request', 'memory://response', id, $4 FROM rows",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.key_id.to_string())
    .bind(base)
    .bind(cluster_id.to_string())
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL 110k request history");
    sqlx::query(
        "WITH source AS (SELECT value, md5($4 || value::TEXT) AS request_hash, md5('observation-' || $4 || value::TEXT) AS observation_hash FROM generate_series(1, 110001) value), rows AS (SELECT value, substring(request_hash FROM 1 FOR 8) || '-' || substring(request_hash FROM 9 FOR 4) || '-7' || substring(request_hash FROM 14 FOR 3) || '-8' || substring(request_hash FROM 18 FOR 3) || '-' || substring(request_hash FROM 21 FOR 12) AS request_id, substring(observation_hash FROM 1 FOR 8) || '-' || substring(observation_hash FROM 9 FOR 4) || '-7' || substring(observation_hash FROM 14 FOR 3) || '-8' || substring(observation_hash FROM 18 FOR 3) || '-' || substring(observation_hash FROM 21 FOR 12) AS observation_id FROM source) INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, atom_hashes_json, client_name, created_at, inference_version, compaction) SELECT observation_id, $1, request_id, $2, '[]', 'Codex', $3 + value, 2, 0 FROM rows",
    )
    .bind(cluster_id.to_string())
    .bind(key.key_id.to_string())
    .bind(base)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL 110k conversation members");
    sqlx::query(
        "INSERT INTO conversation_key_clusters (key_id, cluster_id, explicit_session_id, updated_at, request_count, candidate_edge_count) VALUES ($1, $2, 'postgres-large-session', $3, 110001, 0)",
    )
    .bind(key.key_id.to_string())
    .bind(cluster_id.to_string())
    .bind(base + 110_001)
    .execute(&pool)
    .await
    .expect("PostgreSQL conversation projection");
    sqlx::query(
        "WITH source AS (SELECT value, md5('cluster-' || $4 || value::TEXT) AS hash FROM generate_series(1, 10000) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO conversation_clusters (id, tenant_id, principal_id, created_at, updated_at) SELECT id, $1, $2, $3 - value, $3 - value FROM rows",
    )
    .bind(key.tenant_id.to_string())
    .bind(key.principal_id.to_string())
    .bind(base)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL paged clusters");
    sqlx::query(
        "WITH source AS (SELECT value, md5('cluster-' || $3 || value::TEXT) AS hash FROM generate_series(1, 10000) value), rows AS (SELECT value, substring(hash FROM 1 FOR 8) || '-' || substring(hash FROM 9 FOR 4) || '-7' || substring(hash FROM 14 FOR 3) || '-8' || substring(hash FROM 18 FOR 3) || '-' || substring(hash FROM 21 FOR 12) AS id FROM source) INSERT INTO conversation_key_clusters (key_id, cluster_id, updated_at, request_count, candidate_edge_count) SELECT $1, id, $2 - value, 1, 0 FROM rows",
    )
    .bind(key.key_id.to_string())
    .bind(base)
    .bind(&seed)
    .execute(&pool)
    .await
    .expect("PostgreSQL paged projections");
    sqlx::query("ANALYZE conversation_key_clusters")
        .execute(&pool)
        .await
        .expect("analyze PostgreSQL projections");
    sqlx::query("ANALYZE request_records")
        .execute(&pool)
        .await
        .expect("analyze PostgreSQL request history");

    let list_response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri("/self/v1/conversations?limit=999")
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::empty())
                .expect("PostgreSQL list request"),
        )
        .await
        .expect("PostgreSQL list response");
    assert_eq!(list_response.status(), StatusCode::OK);
    let list_body: Value = serde_json::from_slice(
        &to_bytes(list_response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded PostgreSQL list response"),
    )
    .expect("PostgreSQL list JSON");
    assert_eq!(
        list_body.as_array().map(Vec::len),
        Some(100),
        "PostgreSQL list limit must be capped"
    );

    let response = api::router(state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/self/v1/conversations/{cluster_id}?limit=999"))
                .header(header::AUTHORIZATION, format!("Bearer {}", issued.key))
                .body(Body::empty())
                .expect("PostgreSQL HTTP request"),
        )
        .await
        .expect("PostgreSQL HTTP response");
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), 4 * 1024 * 1024)
            .await
            .expect("bounded PostgreSQL response"),
    )
    .expect("PostgreSQL detail JSON");
    assert_eq!(body["cluster"]["request_count"], 110_001);
    assert_eq!(body["requests"].as_array().map(Vec::len), Some(200));
    assert_eq!(body["has_more"], true);

    let list_plan = postgres_plan(
        &pool,
        "EXPLAIN (ANALYZE, FORMAT TEXT, TIMING OFF) SELECT cluster_id, updated_at, request_count FROM conversation_key_clusters WHERE key_id = $1 ORDER BY updated_at DESC, cluster_id DESC LIMIT 100",
        &[key.key_id.to_string()],
    )
    .await;
    assert!(list_plan.contains("Index"), "{list_plan}");
    assert!(
        !list_plan.contains("Seq Scan on conversation_key_clusters"),
        "{list_plan}"
    );
    assert!(execution_time_ms(&list_plan) <= 250.0, "{list_plan}");
    let detail_plan = postgres_plan(
        &pool,
        "EXPLAIN (ANALYZE, FORMAT TEXT, TIMING OFF) SELECT id, created_at FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY created_at DESC, id DESC LIMIT 201",
        &[key.key_id.to_string(), cluster_id.to_string()],
    )
    .await;
    assert!(detail_plan.contains("Index"), "{detail_plan}");
    assert!(
        !detail_plan.contains("Seq Scan on request_records_default"),
        "{detail_plan}"
    );
    assert!(execution_time_ms(&detail_plan) <= 250.0, "{detail_plan}");
}

async fn postgres_plan(pool: &AnyPool, sql: &str, binds: &[String]) -> String {
    let mut query = sqlx::query(sql);
    for value in binds {
        query = query.bind(value);
    }
    query
        .fetch_all(pool)
        .await
        .expect("PostgreSQL EXPLAIN")
        .into_iter()
        .map(|row| row.get::<String, _>(0))
        .collect::<Vec<_>>()
        .join("\n")
}

fn execution_time_ms(plan: &str) -> f64 {
    plan.lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix("Execution Time: ")
                .and_then(|value| value.strip_suffix(" ms"))
                .and_then(|value| value.parse().ok())
        })
        .expect("EXPLAIN ANALYZE execution time")
}
