use super::*;

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
    sqlx::query("UPDATE session_response_summaries SET session_id = $1 WHERE tenant_id = $2 AND key_id = $3 AND session_id = $4")
        .bind(target).bind(tenant).bind(key).bind(source).execute(&mut **tx).await?;
    // Keep a destination title if one exists. A ready source title can replace
    // an unfinished destination job, whose old lease/revision is invalidated.
    sqlx::query("INSERT INTO session_generated_titles (tenant_id, key_id, session_id, title, state, next_attempt_at, updated_at) SELECT tenant_id, key_id, $1, title, CASE WHEN title IS NULL THEN 'pending' ELSE 'ready' END, $5, $5 FROM session_generated_titles WHERE tenant_id = $2 AND key_id = $3 AND session_id = $4 ON CONFLICT(tenant_id, key_id, session_id) DO UPDATE SET title = excluded.title, state = 'ready', revision = session_generated_titles.revision + 1, lease_owner = NULL, lease_until = NULL, attempts = 0, error_code = NULL, updated_at = excluded.updated_at WHERE session_generated_titles.title IS NULL AND excluded.title IS NOT NULL")
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
        db.finish_session_response(&response_task, Some("Final answer"), None)
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
}
