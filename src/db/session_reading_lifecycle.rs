use super::*;
use std::collections::BTreeSet;

/// Follow the same credential-scoped merge as observations and usage. Cached
/// response text remains content-addressed; no archived body is copied.
pub(crate) async fn merge_session_reading_in_transaction(
    tx: &mut Transaction<'_, Any>,
    tenant: &str,
    key: &str,
    source: &str,
    target: &str,
) -> Result<(), AppError> {
    if source == target {
        return Ok(());
    }
    sqlx::query("UPDATE tenants SET external_id=external_id WHERE id=$1")
        .bind(tenant)
        .execute(&mut **tx)
        .await?;
    let relationships = sqlx::query("SELECT key_id,session_id,target_key_id,target_session_id,kind,evidence,updated_at FROM session_relationships WHERE tenant_id=$1 AND ((key_id=$2 AND session_id=$3) OR (target_key_id=$2 AND target_session_id=$3))")
        .bind(tenant).bind(key).bind(source).fetch_all(&mut **tx).await?;
    // Collapsing two children must not combine incompatible parent evidence.
    // Compare the surviving targets, allowing multiple exact identities of the
    // same explicit parent thread rather than treating each credential as a parent.
    let mut parent_sets = Vec::new();
    for session in [source, target] {
        let rows = sqlx::query("SELECT target_key_id,target_session_id FROM session_relationships WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3 AND kind='parent'")
            .bind(tenant).bind(key).bind(session).fetch_all(&mut **tx).await?;
        let mut parents = BTreeSet::new();
        for row in rows {
            let parent_key: String = row.try_get("target_key_id")?;
            let mut parent_session: String = row.try_get("target_session_id")?;
            if parent_key == key && parent_session == source {
                parent_session = target.to_owned();
            }
            if parent_key != key || parent_session != target {
                parents.insert((parent_key, parent_session));
            }
        }
        parent_sets.push(parents);
    }
    if !parent_sets[0].is_empty() && !parent_sets[1].is_empty() && parent_sets[0] != parent_sets[1]
    {
        let mut common_threads: Option<BTreeSet<String>> = None;
        for (parent_key, parent_session) in parent_sets[0].union(&parent_sets[1]) {
            let threads: BTreeSet<String> = sqlx::query_scalar::<_, String>("SELECT DISTINCT o.explicit_session_id FROM conversation_observations o JOIN conversation_clusters c ON c.id=o.cluster_id WHERE c.tenant_id=$1 AND o.key_id=$2 AND o.cluster_id=$3 AND o.explicit_session_id IS NOT NULL")
                .bind(tenant).bind(parent_key).bind(parent_session).fetch_all(&mut **tx).await?.into_iter().collect();
            common_threads = Some(match common_threads {
                None => threads,
                Some(common) => common.intersection(&threads).cloned().collect(),
            });
        }
        if common_threads.is_none_or(|threads| threads.is_empty()) {
            return Err(AppError::Conflict(
                "contradictory parent evidence after session reconciliation".into(),
            ));
        }
    }
    sqlx::query("DELETE FROM session_relationships WHERE tenant_id=$1 AND ((key_id=$2 AND session_id=$3) OR (target_key_id=$2 AND target_session_id=$3))")
        .bind(tenant).bind(key).bind(source).execute(&mut **tx).await?;
    let mut remapped_parents = BTreeSet::new();
    for row in relationships {
        let mut from = (
            row.try_get::<String, _>("key_id")?,
            row.try_get::<String, _>("session_id")?,
        );
        let mut to = (
            row.try_get::<String, _>("target_key_id")?,
            row.try_get::<String, _>("target_session_id")?,
        );
        for endpoint in [&mut from, &mut to] {
            if endpoint.0 == key && endpoint.1 == source {
                endpoint.1 = target.to_owned();
            }
        }
        if from == to {
            continue;
        }
        let kind: String = row.try_get("kind")?;
        if kind == "related" && to < from {
            std::mem::swap(&mut from, &mut to);
        }
        if kind == "parent" {
            remapped_parents.insert((from.clone(), to.clone()));
        }
        sqlx::query("INSERT INTO session_relationships (tenant_id,key_id,session_id,target_key_id,target_session_id,kind,evidence,updated_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
            .bind(tenant).bind(from.0).bind(from.1).bind(to.0).bind(to.1).bind(kind)
            .bind(row.try_get::<Option<String>, _>("evidence")?).bind(row.try_get::<i64, _>("updated_at")?)
            .execute(&mut **tx).await?;
    }
    // Validate after every edge has moved: unchanged destination edges also
    // participate in reachability (S -> X -> T must not become T -> X -> T).
    // Any new cycle includes a remapped edge. Errors roll back the enclosing
    // observation/fact transaction, retaining the original authoritative graph.
    for (from, to) in remapped_parents {
        let cycle: i64 = sqlx::query_scalar("WITH RECURSIVE ancestors(key_id,session_id) AS (SELECT $2,$3 UNION SELECT r.target_key_id,r.target_session_id FROM session_relationships r JOIN ancestors a ON r.key_id=a.key_id AND r.session_id=a.session_id WHERE r.tenant_id=$1 AND r.kind='parent') SELECT COUNT(*) FROM ancestors WHERE key_id=$4 AND session_id=$5")
            .bind(tenant).bind(to.0).bind(to.1).bind(from.0).bind(from.1).fetch_one(&mut **tx).await?;
        if cycle > 0 {
            return Err(AppError::Conflict(
                "parent cycle after session reconciliation".into(),
            ));
        }
    }
    sqlx::query("UPDATE session_response_summaries SET session_id = $1 WHERE tenant_id = $2 AND key_id = $3 AND session_id = $4")
        .bind(target).bind(tenant).bind(key).bind(source).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO session_generated_titles (tenant_id, key_id, session_id, title, title_source, name_sync_id, state, next_attempt_at, updated_at) SELECT tenant_id, key_id, $1, title, title_source, name_sync_id, CASE WHEN title IS NULL THEN 'pending' ELSE 'ready' END, $5, $5 FROM session_generated_titles WHERE tenant_id = $2 AND key_id = $3 AND session_id = $4 ON CONFLICT(tenant_id, key_id, session_id) DO UPDATE SET title = excluded.title, title_source = excluded.title_source, name_sync_id = excluded.name_sync_id, state = 'ready', revision = session_generated_titles.revision + 1, lease_owner = NULL, lease_until = NULL, attempts = 0, error_code = NULL, updated_at = excluded.updated_at WHERE excluded.title IS NOT NULL AND (session_generated_titles.title IS NULL OR (excluded.title_source='manual' AND session_generated_titles.title_source<>'manual'))")
        .bind(target).bind(tenant).bind(key).bind(source).bind(unix_millis()).execute(&mut **tx).await?;
    sqlx::query("DELETE FROM session_generated_titles WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3")
        .bind(tenant).bind(key).bind(source).execute(&mut **tx).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merging_reading_state_moves_only_the_owned_key_and_invalidates_old_title_jobs() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("merge-reading.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let tenant = Uuid::now_v7();
        let key = Uuid::now_v7();
        let other = Uuid::now_v7();
        let source = Uuid::now_v7();
        let target = Uuid::now_v7();
        db.request_session_title(tenant, key, source).await.unwrap();
        let task = db
            .claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        db.finish_session_title(&task, Some("Preserved title"), None)
            .await
            .unwrap();
        sqlx::query("UPDATE session_generated_titles SET title_source='manual' WHERE tenant_id=$1 AND key_id=$2 AND session_id=$3")
            .bind(tenant.to_string()).bind(key.to_string()).bind(source.to_string()).execute(&db.pool).await.unwrap();
        db.request_session_title(tenant, key, target).await.unwrap();
        let generated_task = db
            .claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        db.finish_session_title(&generated_task, Some("Generated destination"), None)
            .await
            .unwrap();
        db.request_session_title(tenant, key, target).await.unwrap();
        let destination_task = db
            .claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        db.request_session_title(tenant, other, source)
            .await
            .unwrap();
        let scope = SessionContextScope {
            tenant_id: tenant,
            key_id: key,
            session_id: source,
            request_id: Uuid::now_v7(),
            leaf: None,
        };
        db.session_response_summary(&scope).await.unwrap();
        let response_task = db
            .claim_session_response(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        db.finish_session_response(&response_task, Some("Final answer".into()), None)
            .await
            .unwrap();
        let mut tx = db.begin_write_transaction().await.unwrap();
        merge_session_reading_in_transaction(
            &mut tx,
            &tenant.to_string(),
            &key.to_string(),
            &source.to_string(),
            &target.to_string(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        assert_eq!(
            db.session_title(tenant, key, source).await.unwrap().state,
            "unavailable"
        );
        assert_eq!(
            db.session_title(tenant, other, source).await.unwrap().state,
            "pending"
        );
        db.finish_session_title(&destination_task, Some("Stale generated title"), None)
            .await
            .unwrap();
        assert_eq!(
            db.session_title(tenant, key, target)
                .await
                .unwrap()
                .title
                .as_deref(),
            Some("Preserved title")
        );
        assert_eq!(
            db.session_title(tenant, key, target).await.unwrap().source,
            "manual"
        );
        let moved = SessionContextScope {
            session_id: target,
            ..scope
        };
        assert_eq!(
            db.session_response_summary(&moved)
                .await
                .unwrap()
                .text
                .as_deref(),
            Some("Final answer")
        );
    }

    #[tokio::test]
    async fn merge_preserves_manual_destination_and_remaps_only_owned_relationships() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("merge-metadata.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let tenant = Uuid::now_v7().to_string();
        let foreign = Uuid::now_v7().to_string();
        let key = Uuid::from_u128(1).to_string();
        let other_key = Uuid::from_u128(2).to_string();
        let target = Uuid::from_u128(10).to_string();
        let peer = Uuid::from_u128(20).to_string();
        let source = Uuid::from_u128(30).to_string();
        for (session, title) in [(&source, "Source manual"), (&target, "Destination manual")] {
            sqlx::query("INSERT INTO session_generated_titles (tenant_id,key_id,session_id,title,title_source,state,revision,next_attempt_at,updated_at) VALUES ($1,$2,$3,$4,'manual','ready',7,1,1)")
                .bind(&tenant).bind(&key).bind(session).bind(title).execute(&db.pool).await.unwrap();
        }
        for (scope, from_key, from, to_key, to, kind) in [
            (&tenant, &key, &peer, &key, &source, "related"),
            (&tenant, &key, &target, &key, &peer, "related"),
            (&tenant, &key, &target, &key, &source, "related"),
            (&tenant, &key, &source, &key, &peer, "parent"),
            (&tenant, &key, &source, &key, &target, "parent"),
            (&tenant, &other_key, &source, &other_key, &peer, "parent"),
            (&foreign, &key, &source, &key, &peer, "parent"),
        ] {
            sqlx::query("INSERT INTO session_relationships (tenant_id,key_id,session_id,target_key_id,target_session_id,kind,evidence,updated_at) VALUES ($1,$2,$3,$4,$5,$6,'thread_spawn_edges',1)")
                .bind(scope).bind(from_key).bind(from).bind(to_key).bind(to).bind(kind)
                .execute(&db.pool).await.unwrap();
        }
        let mut tx = db.begin_write_transaction().await.unwrap();
        merge_session_reading_in_transaction(&mut tx, &tenant, &key, &source, &target)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let title = db
            .session_title(
                parse_uuid(tenant.clone()).unwrap(),
                parse_uuid(key.clone()).unwrap(),
                parse_uuid(target.clone()).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(title.title.as_deref(), Some("Destination manual"));
        assert_eq!(title.revision, 7);
        let rows: Vec<(String, String, String, String, String, String)> = sqlx::query_as(
            "SELECT tenant_id,key_id,session_id,target_key_id,target_session_id,kind FROM session_relationships"
        ).fetch_all(&db.pool).await.unwrap();
        let expected = [
            (
                tenant.clone(),
                key.clone(),
                target.clone(),
                key.clone(),
                peer.clone(),
                "related".to_owned(),
            ),
            (
                tenant.clone(),
                key.clone(),
                target.clone(),
                key.clone(),
                peer.clone(),
                "parent".to_owned(),
            ),
            (
                tenant,
                other_key.clone(),
                source.clone(),
                other_key,
                peer.clone(),
                "parent".to_owned(),
            ),
            (foreign, key.clone(), source, key, peer, "parent".to_owned()),
        ];
        assert_eq!(
            rows.into_iter().collect::<std::collections::BTreeSet<_>>(),
            expected.into_iter().collect()
        );
    }
}
