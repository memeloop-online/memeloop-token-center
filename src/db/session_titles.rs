use super::*;
use crate::model::LogicalSessionSummary;

const TITLE_LEASE_MILLIS: i64 = 120_000;

#[derive(Debug, Serialize)]
pub(crate) struct SessionTitleView {
    pub title: Option<String>,
    pub state: String,
    pub error_code: Option<String>,
    pub updated_at: Option<i64>,
}

pub(crate) struct SessionTitleTask {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub session_id: Uuid,
    pub revision: i64,
    pub attempts: i64,
    pub lease_owner: Uuid,
}

pub(crate) async fn enqueue_initial_session_title(
    tx: &mut Transaction<'_, Any>,
    tenant_id: Uuid,
    key_id: Uuid,
    session_id: Uuid,
) -> Result<(), AppError> {
    let now = unix_millis();
    sqlx::query("INSERT INTO session_generated_titles (tenant_id, key_id, session_id, next_attempt_at, updated_at) VALUES ($1, $2, $3, $4, $4) ON CONFLICT(tenant_id, key_id, session_id) DO NOTHING")
        .bind(tenant_id.to_string()).bind(key_id.to_string()).bind(session_id.to_string())
        .bind(now).execute(&mut **tx).await?;
    Ok(())
}

impl Database {
    pub(crate) async fn session_title_tenant(
        &self,
        tenant: &str,
        key_id: Uuid,
        session_id: Uuid,
    ) -> Result<Uuid, AppError> {
        let row = sqlx::query("SELECT c.tenant_id FROM conversation_key_clusters p JOIN conversation_clusters c ON c.id = p.cluster_id JOIN tenants t ON t.id = c.tenant_id WHERE p.key_id = $1 AND p.cluster_id = $2 AND t.external_id = $3")
            .bind(key_id.to_string()).bind(session_id.to_string()).bind(tenant)
            .fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        parse_uuid(row.try_get("tenant_id")?)
    }

    pub(crate) async fn session_title(
        &self,
        tenant_id: Uuid,
        key_id: Uuid,
        session_id: Uuid,
    ) -> Result<SessionTitleView, AppError> {
        let row = sqlx::query("SELECT title, state, error_code, updated_at FROM session_generated_titles WHERE tenant_id = $1 AND key_id = $2 AND session_id = $3")
            .bind(tenant_id.to_string()).bind(key_id.to_string()).bind(session_id.to_string())
            .fetch_optional(&self.pool).await?;
        match row {
            Some(row) => Ok(SessionTitleView {
                title: row.try_get("title")?,
                state: row.try_get("state")?,
                error_code: row.try_get("error_code")?,
                updated_at: Some(row.try_get("updated_at")?),
            }),
            None => Ok(SessionTitleView {
                title: None,
                state: "unavailable".into(),
                error_code: None,
                updated_at: None,
            }),
        }
    }

    /// Repeated clicks while work is queued/running do not create extra model
    /// calls. Keep the old title visible until its replacement commits.
    pub(crate) async fn request_session_title(
        &self,
        tenant_id: Uuid,
        key_id: Uuid,
        session_id: Uuid,
    ) -> Result<SessionTitleView, AppError> {
        let now = unix_millis();
        sqlx::query("INSERT INTO session_generated_titles (tenant_id, key_id, session_id, next_attempt_at, updated_at) VALUES ($1, $2, $3, $4, $4) ON CONFLICT(tenant_id, key_id, session_id) DO UPDATE SET state = 'pending', revision = session_generated_titles.revision + 1, attempts = 0, next_attempt_at = excluded.next_attempt_at, updated_at = excluded.updated_at, lease_owner = NULL, lease_until = NULL, error_code = NULL WHERE session_generated_titles.state NOT IN ('pending', 'running') OR (session_generated_titles.state = 'running' AND session_generated_titles.lease_until <= excluded.updated_at)")
            .bind(tenant_id.to_string()).bind(key_id.to_string()).bind(session_id.to_string())
            .bind(now).execute(&self.pool).await?;
        self.session_title(tenant_id, key_id, session_id).await
    }

    pub(crate) async fn claim_session_title(
        &self,
        owner: Uuid,
    ) -> Result<Option<SessionTitleTask>, AppError> {
        let now = unix_millis();
        // A process can die after dispatching the model call. Expired claims
        // count as attempts too; otherwise repeated worker restarts could
        // silently cause unbounded billable calls.
        sqlx::query("UPDATE session_generated_titles SET state = 'failed', error_code = 'generation_interrupted', lease_owner = NULL, lease_until = NULL, updated_at = $1 WHERE (tenant_id, key_id, session_id) IN (SELECT tenant_id, key_id, session_id FROM session_generated_titles WHERE state = 'running' AND attempts >= 3 AND lease_until <= $1 ORDER BY next_attempt_at LIMIT 100) AND state = 'running' AND lease_until <= $1")
            .bind(now).execute(&self.pool).await?;
        let lock = if matches!(self.backend, DatabaseBackend::PostgreSql) {
            " FOR UPDATE SKIP LOCKED"
        } else {
            ""
        };
        let statement = format!(
            "UPDATE session_generated_titles SET state = 'running', lease_owner = $1, lease_until = $2, attempts = attempts + 1 \
             WHERE (tenant_id, key_id, session_id) IN (SELECT tenant_id, key_id, session_id FROM session_generated_titles \
             WHERE state IN ('pending', 'running', 'waiting_config') AND attempts < 3 AND next_attempt_at <= $3 \
             AND (lease_until IS NULL OR lease_until <= $3) ORDER BY next_attempt_at, tenant_id, key_id, session_id LIMIT 1{lock}) \
             RETURNING tenant_id, key_id, session_id, revision, attempts"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(statement))
            .bind(owner.to_string())
            .bind(now + TITLE_LEASE_MILLIS)
            .bind(now)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok(SessionTitleTask {
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                session_id: parse_uuid(row.try_get("session_id")?)?,
                revision: row.try_get("revision")?,
                attempts: row.try_get("attempts")?,
                lease_owner: owner,
            })
        })
        .transpose()
    }

    pub(crate) async fn finish_session_title(
        &self,
        task: &SessionTitleTask,
        title: Option<&str>,
        error_code: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let waiting = error_code == Some("assistant_not_configured");
        let state = if title.is_some() {
            "ready"
        } else if waiting {
            "waiting_config"
        } else if task.attempts >= 3 {
            "failed"
        } else {
            "pending"
        };
        let retry = if waiting {
            60 * 60 * 1_000
        } else {
            60_000 * task.attempts.clamp(1, 3)
        };
        sqlx::query("UPDATE session_generated_titles SET title = COALESCE($1, title), state = $2, error_code = $3, next_attempt_at = $4, updated_at = $5, lease_owner = NULL, lease_until = NULL, attempts = CASE WHEN $6 = 1 THEN 0 ELSE attempts END WHERE tenant_id = $7 AND key_id = $8 AND session_id = $9 AND revision = $10 AND lease_owner = $11 AND lease_until > $5")
            .bind(title).bind(state).bind(error_code).bind(now + retry).bind(now).bind(i64::from(waiting))
            .bind(task.tenant_id.to_string()).bind(task.key_id.to_string()).bind(task.session_id.to_string())
            .bind(task.revision).bind(task.lease_owner.to_string()).execute(&self.pool).await?;
        Ok(())
    }

    pub(crate) async fn session_title_external_tenant(
        &self,
        task: &SessionTitleTask,
    ) -> Result<String, AppError> {
        sqlx::query_scalar("SELECT external_id FROM tenants WHERE id = $1 AND status = 'active'")
            .bind(task.tenant_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)
    }

    /// Read only two small JSON prefixes from existing user-message atoms.
    /// Prefixes may end mid-JSON: they are untrusted model input, never parsed
    /// as a protocol request. Neither tool bodies nor archive objects are read.
    pub(crate) async fn session_title_sample(
        &self,
        task: &SessionTitleTask,
        newest: bool,
    ) -> Result<String, AppError> {
        let order = if newest { "DESC" } else { "ASC" };
        let statement = format!(
            "WITH RECURSIVE path(node_hash, parent_hash, atom_hash, depth, step) AS (\
             SELECT n.node_hash, n.parent_hash, n.atom_hash, n.depth, CAST(1 AS BIGINT) \
             FROM context_nodes n WHERE n.tenant_id = $1 AND n.node_hash = (\
                 SELECT o.leaf_node_hash FROM conversation_observations o \
                 JOIN conversation_clusters c ON c.id = o.cluster_id \
                 WHERE c.tenant_id = $1 AND o.key_id = $2 AND o.cluster_id = $3 \
                 ORDER BY o.created_at {order}, o.id {order} LIMIT 1) \
             UNION ALL SELECT n.node_hash, n.parent_hash, n.atom_hash, n.depth, p.step + 1 \
             FROM context_nodes n JOIN path p ON n.node_hash = p.parent_hash AND n.depth = p.depth - 1 \
             WHERE n.tenant_id = $1 AND p.step < 4096), \
             selected AS (SELECT p.atom_hash FROM path p JOIN semantic_atoms a \
                 ON a.tenant_id = $1 AND a.content_hash = p.atom_hash \
                 WHERE a.role = 'user' AND a.kind = 'message' ORDER BY p.depth {order} LIMIT 1) \
             SELECT a.content_json FROM selected \
             JOIN semantic_atoms a ON a.tenant_id = $1 AND a.content_hash = selected.atom_hash"
        );
        let reference: Option<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(statement))
            .bind(task.tenant_id.to_string())
            .bind(task.key_id.to_string())
            .bind(task.session_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
        match reference {
            Some(reference) => {
                let body = self.read_body_reference(task.tenant_id, &reference).await?;
                Ok(body.chars().take(1024).collect())
            }
            None => Ok(String::new()),
        }
    }

    /// One indexed batch for the already authorized list page; never one
    /// title query per session and never scan all title jobs.
    pub(crate) async fn apply_generated_session_titles(
        &self,
        tenant_id: &str,
        sessions: &mut [LogicalSessionSummary],
    ) -> Result<(), AppError> {
        if sessions.is_empty() {
            return Ok(());
        }
        let predicates = (0..sessions.len())
            .map(|index| {
                format!(
                    "(key_id = ${} AND session_id = ${})",
                    2 + index * 2,
                    3 + index * 2
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ");
        let statement = format!(
            "SELECT key_id, session_id, title FROM session_generated_titles WHERE tenant_id = $1 AND title IS NOT NULL AND ({predicates})"
        );
        let mut query = sqlx::query(sqlx::AssertSqlSafe(statement)).bind(tenant_id);
        for session in sessions.iter() {
            query = query
                .bind(session.key_id.to_string())
                .bind(&session.session_id);
        }
        let titles = query
            .fetch_all(&self.pool)
            .await?
            .into_iter()
            .map(|row| {
                Ok((
                    (
                        row.try_get::<String, _>("key_id")?,
                        row.try_get::<String, _>("session_id")?,
                    ),
                    row.try_get::<String, _>("title")?,
                ))
            })
            .collect::<Result<std::collections::HashMap<_, _>, AppError>>()?;
        for session in sessions {
            if let Some(title) =
                titles.get(&(session.key_id.to_string(), session.session_id.clone()))
            {
                session.session_name = Some(title.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conversation::ConversationHints;
    use crate::db::requests::conversations::prefix_storage::tests::{key, observe};
    use serde_json::json;

    #[tokio::test]
    async fn crashed_workers_cannot_dispatch_unbounded_attempts() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("crashed-titles.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let tenant = Uuid::now_v7();
        let key = Uuid::now_v7();
        let session = Uuid::now_v7();
        db.request_session_title(tenant, key, session)
            .await
            .unwrap();
        for attempt in 1..=3 {
            let task = db
                .claim_session_title(Uuid::now_v7())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task.attempts, attempt);
            sqlx::query("UPDATE session_generated_titles SET lease_until = 0")
                .execute(&db.pool)
                .await
                .unwrap();
        }
        assert!(
            db.claim_session_title(Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
        let title = db.session_title(tenant, key, session).await.unwrap();
        assert_eq!(title.state, "failed");
        assert_eq!(title.error_code.as_deref(), Some("generation_interrupted"));
    }

    #[tokio::test]
    async fn title_queue_coalesces_clicks_and_fences_stale_completions() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("titles.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        let key = key(&db, "titles").await;
        let session = observe(
            &db,
            &key,
            &json!({"input":[{"role":"user","content":"Repair session history loading"}]}),
            &ConversationHints::default(),
            Uuid::now_v7(),
            1,
            true,
        )
        .await;
        let tenant = key.tenant_id;
        assert_eq!(
            db.session_title(tenant, key.key_id, session)
                .await
                .unwrap()
                .state,
            "pending"
        );
        db.request_session_title(tenant, key.key_id, session)
            .await
            .unwrap();
        let first = db
            .claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        db.request_session_title(tenant, key.key_id, session)
            .await
            .unwrap();
        assert!(
            db.claim_session_title(Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            db.session_title_sample(&first, false)
                .await
                .unwrap()
                .contains("Repair session")
        );
        assert!(
            db.session_title_tenant("other", key.key_id, session)
                .await
                .is_err()
        );

        // Simulate a worker losing its lease; only its successor may commit.
        sqlx::query("UPDATE session_generated_titles SET lease_until = 0")
            .execute(&db.pool)
            .await
            .unwrap();
        let second = db
            .claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        db.finish_session_title(&first, Some("Stale"), None)
            .await
            .unwrap();
        assert!(
            db.session_title(tenant, key.key_id, session)
                .await
                .unwrap()
                .title
                .is_none()
        );
        db.finish_session_title(&second, Some("Session history"), None)
            .await
            .unwrap();
        let ready = db.session_title(tenant, key.key_id, session).await.unwrap();
        assert_eq!(ready.title.as_deref(), Some("Session history"));
        assert_eq!(ready.state, "ready");
        let pending = db
            .request_session_title(tenant, key.key_id, session)
            .await
            .unwrap();
        assert_eq!(pending.title.as_deref(), Some("Session history"));
        let third = db
            .claim_session_title(Uuid::now_v7())
            .await
            .unwrap()
            .unwrap();
        assert!(third.revision > first.revision);
        db.finish_session_title(&third, None, Some("assistant_not_configured"))
            .await
            .unwrap();
        assert_eq!(
            db.session_title(tenant, key.key_id, session)
                .await
                .unwrap()
                .state,
            "waiting_config"
        );
        assert!(
            db.claim_session_title(Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
        db.request_session_title(tenant, key.key_id, session)
            .await
            .unwrap();
        for attempt in 1..=3 {
            let task = db
                .claim_session_title(Uuid::now_v7())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task.attempts, attempt);
            db.finish_session_title(&task, None, Some("generation_failed"))
                .await
                .unwrap();
            sqlx::query("UPDATE session_generated_titles SET next_attempt_at = 0")
                .execute(&db.pool)
                .await
                .unwrap();
        }
        assert_eq!(
            db.session_title(tenant, key.key_id, session)
                .await
                .unwrap()
                .state,
            "failed"
        );
        assert!(
            db.claim_session_title(Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
    }
}
