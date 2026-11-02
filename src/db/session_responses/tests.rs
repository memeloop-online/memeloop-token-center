use super::*;

fn scope() -> SessionContextScope {
    SessionContextScope {
        tenant_id: Uuid::now_v7(),
        key_id: Uuid::now_v7(),
        session_id: Uuid::now_v7(),
        request_id: Uuid::now_v7(),
        leaf: None,
    }
}

#[test]
fn response_encoding_preserves_exact_json_bytes_and_atom_hash() {
    for answer in [
        String::new(),
        "完整回答😀\r\n\n  last\t\"\\\0\u{0008}\u{000c}".into(),
        "x".repeat(1024 * 1024),
    ] {
        let expected = serde_json::to_string(&serde_json::json!({
            "role": "assistant", "type": "response_summary", "content": &answer
        }))
        .unwrap();
        let encoded = encode_session_response(&answer).unwrap();
        assert_eq!(encoded, expected);
        assert_eq!(
            blake3::hash(encoded.as_bytes()),
            blake3::hash(expected.as_bytes())
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).unwrap()["content"],
            answer
        );
    }
}

#[tokio::test]
async fn response_cache_is_scoped_chunked_shared_and_lease_fenced() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("responses.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let scope = scope();
    assert_eq!(
        db.session_response_summary(&scope).await.unwrap().state,
        "pending"
    );
    let old = db
        .claim_session_response(Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    assert!(
        db.claim_session_response(Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );
    sqlx::query("UPDATE session_response_summaries SET lease_until = 0")
        .execute(&db.pool)
        .await
        .unwrap();
    let current = db
        .claim_session_response(Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    db.finish_session_response(&old, Some("must not persist".into()), None)
        .await
        .unwrap();
    let atoms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(
        atoms, 0,
        "stale completions must not create orphan text atoms"
    );
    let text = "完整回答😀\n".repeat(8_000);
    db.finish_session_response(&current, Some(text.clone()), None)
        .await
        .unwrap();
    let summary = db.session_response_summary(&scope).await.unwrap();
    assert_eq!(summary.state, "ready");
    assert_eq!(summary.total_chars, text.chars().count() as i64);
    assert_eq!(summary.text.as_ref().unwrap().chars().count(), 16384);
    let atom = summary.atom.unwrap();
    let mut offset = 0;
    let mut joined = String::new();
    while offset < summary.total_chars {
        let (chunk, total) = db
            .session_response_text(&scope, &atom, offset)
            .await
            .unwrap();
        assert_eq!(total, summary.total_chars);
        offset += chunk.chars().count() as i64;
        joined.push_str(&chunk);
    }
    assert_eq!(joined, text);
    for foreign in [
        SessionContextScope {
            tenant_id: Uuid::now_v7(),
            ..scope.clone()
        },
        SessionContextScope {
            key_id: Uuid::now_v7(),
            ..scope.clone()
        },
        SessionContextScope {
            session_id: Uuid::now_v7(),
            ..scope.clone()
        },
        SessionContextScope {
            request_id: Uuid::now_v7(),
            ..scope.clone()
        },
    ] {
        assert!(db.session_response_text(&foreign, &atom, 0).await.is_err());
    }
    let second = SessionContextScope {
        request_id: Uuid::now_v7(),
        ..scope
    };
    db.session_response_summary(&second).await.unwrap();
    let task = db
        .claim_session_response(Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    db.finish_session_response(&task, Some(text.clone()), None)
        .await
        .unwrap();
    assert_eq!(
        db.session_response_summary(&second)
            .await
            .unwrap()
            .atom
            .as_deref(),
        Some(atom.as_str())
    );
    let atoms: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM semantic_atoms")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(atoms, 1, "identical final text is stored once per tenant");
}

#[tokio::test]
async fn pending_archive_waits_and_failures_stop_after_three_attempts() {
    let directory = tempfile::tempdir().unwrap();
    let db = Database::connect(&format!(
        "sqlite://{}?mode=rwc",
        directory.path().join("retry.db").display()
    ))
    .await
    .unwrap();
    db.migrate().await.unwrap();
    let scope = scope();
    db.session_response_summary(&scope).await.unwrap();
    let first = db
        .claim_session_response(Uuid::now_v7())
        .await
        .unwrap()
        .unwrap();
    db.finish_session_response(&first, None, Some("archive_pending"))
        .await
        .unwrap();
    assert!(
        db.claim_session_response(Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );
    for attempt in 1..=3 {
        sqlx::query("UPDATE session_response_summaries SET next_attempt_at = 0")
            .execute(&db.pool)
            .await
            .unwrap();
        let task = db
            .claim_session_response(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(task.attempts, attempt);
        db.finish_session_response(&task, None, Some("archive_invalid_json"))
            .await
            .unwrap();
    }
    let summary = db.session_response_summary(&scope).await.unwrap();
    assert_eq!(summary.state, "unavailable");
    assert!(summary.text.is_none());
    assert!(
        db.claim_session_response(Uuid::now_v7())
            .await
            .unwrap()
            .is_none()
    );
}
