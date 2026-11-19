use super::*;
use prefix_storage::tests::{key, observe};
use serde_json::json;

#[tokio::test]
async fn future_session_prefix_gets_forward_edge_without_inventing_other_ancestry() {
    let directory = tempfile::tempdir().unwrap();
    let database_url = format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("semantic-order.db").display()
    );
    let database = Database::connect(&database_url).await.unwrap();
    database.migrate().await.unwrap();
    let principal = key(&database, "semantic-order").await;
    let other_key = key(&database, "semantic-order").await;
    let now = unix_millis();
    let prefix = json!({"input":[{"role":"user","content":"one"}]});
    let continuation = json!({"input":[
        {"role":"user","content":"one"},
        {"role":"assistant","content":"two"},
        {"role":"user","content":"three"}
    ]});
    for (label, body, parent_reference, compaction, expected_edges) in [
        ("prefix", continuation.clone(), None, false, 1),
        ("unrelated", json!({"input":"unrelated"}), None, false, 0),
        ("compaction", continuation.clone(), None, true, 0),
        (
            "declared-parent",
            continuation.clone(),
            Some("another-turn"),
            false,
            0,
        ),
        ("identical", prefix.clone(), None, false, 0),
    ] {
        let hints = ConversationHints {
            session_id: Some(label.into()),
            ..ConversationHints::default()
        };
        let later_request = Uuid::now_v7();
        let cluster = observe(
            &database,
            &principal,
            &body,
            &ConversationHints {
                parent_turn_id: parent_reference.map(str::to_owned),
                compaction,
                ..hints.clone()
            },
            later_request,
            now + 100,
            true,
        )
        .await;
        let earlier_request = Uuid::now_v7();
        assert_eq!(
            observe(
                &database,
                &principal,
                &prefix,
                &hints,
                earlier_request,
                now,
                true,
            )
            .await,
            cluster
        );
        let edges = sqlx::query(
            "SELECT source.request_id AS source_request, target.request_id AS target_request, edge.relation_kind, edge.confidence_millis, edge.evidence_json FROM conversation_edges edge JOIN conversation_observations source ON source.id=edge.from_observation_id JOIN conversation_observations target ON target.id=edge.to_observation_id WHERE edge.cluster_id=$1",
        )
        .bind(cluster.to_string())
        .fetch_all(&database.pool)
        .await
        .unwrap();
        assert_eq!(edges.len(), expected_edges, "{label}");
        if expected_edges == 1 {
            assert_eq!(
                edges[0].get::<String, _>("source_request"),
                earlier_request.to_string()
            );
            assert_eq!(
                edges[0].get::<String, _>("target_request"),
                later_request.to_string()
            );
            assert_eq!(edges[0].get::<String, _>("relation_kind"), "continues");
            assert!(edges[0].get::<i64, _>("confidence_millis") >= 950);
            let evidence: serde_json::Value =
                serde_json::from_str(&edges[0].get::<String, _>("evidence_json")).unwrap();
            assert_eq!(evidence["semantic_prefix"], true);
            assert_eq!(evidence["explicit_parent"], false);
        }
        assert_ne!(
            observe(
                &database,
                &other_key,
                &prefix,
                &hints,
                Uuid::now_v7(),
                now,
                true,
            )
            .await,
            cluster
        );
    }
    let backwards: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM conversation_edges edge JOIN conversation_observations source ON source.id=edge.from_observation_id JOIN conversation_observations target ON target.id=edge.to_observation_id WHERE source.created_at>target.created_at",
    )
    .fetch_one(&database.pool)
    .await
    .unwrap();
    assert_eq!(backwards, 0);
}
