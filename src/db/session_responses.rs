use super::*;

pub(crate) struct SessionResponseTask {
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub request_id: Uuid,
    pub owner: Uuid,
    pub attempts: i64,
    pub created_at: i64,
}

pub(crate) struct SessionResponseSummary {
    pub state: String,
    pub text: Option<String>,
    pub atom: Option<String>,
    pub total_chars: i64,
    pub error_code: Option<String>,
}

fn encode_session_response(answer: &str) -> Result<String, serde_json::Error> {
    // Match the existing JSON Value's sorted key order and atom identity,
    // without constructing another owned copy of the answer in a Value.
    #[derive(serde::Serialize)]
    struct Summary<'a> {
        content: &'a str,
        role: &'static str,
        #[serde(rename = "type")]
        kind: &'static str,
    }
    // Leave room for the envelope so a large, mostly unescaped paragraph does
    // not double the output allocation just to append the JSON metadata.
    let mut encoded = Vec::with_capacity(answer.len() + 64);
    serde_json::to_writer(
        &mut encoded,
        &Summary {
            content: answer,
            role: "assistant",
            kind: "response_summary",
        },
    )?;
    Ok(String::from_utf8(encoded).expect("serialized JSON is UTF-8"))
}

impl Database {
    /// A derived reading job only. The HTTP path never fetches an archive.
    pub(crate) async fn session_response_summary(
        &self,
        scope: &SessionContextScope,
    ) -> Result<SessionResponseSummary, AppError> {
        let now = unix_millis();
        sqlx::query("INSERT INTO session_response_summaries (tenant_id, key_id, request_id, session_id, next_attempt_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $5, $5) ON CONFLICT(tenant_id, key_id, request_id) DO NOTHING")
            .bind(scope.tenant_id.to_string()).bind(scope.key_id.to_string()).bind(scope.request_id.to_string()).bind(scope.session_id.to_string()).bind(now).execute(&self.pool).await?;
        let row = sqlx::query("SELECT r.state, r.atom_hash, r.error_code FROM session_response_summaries r WHERE r.tenant_id = $1 AND r.key_id = $2 AND r.request_id = $3 AND r.session_id = $4")
            .bind(scope.tenant_id.to_string()).bind(scope.key_id.to_string()).bind(scope.request_id.to_string()).bind(scope.session_id.to_string())
            .fetch_one(&self.pool).await?;
        let atom: Option<String> = row.try_get("atom_hash")?;
        let (text, total_chars) = match atom.as_deref() {
            Some(atom) => {
                let (text, count) = self.session_response_text(scope, atom, 0).await?;
                (Some(text), count)
            }
            None => (None, 0),
        };
        Ok(SessionResponseSummary {
            state: row.try_get("state")?,
            text,
            atom,
            total_chars,
            error_code: row.try_get("error_code")?,
        })
    }

    pub(crate) async fn claim_session_response(
        &self,
        owner: Uuid,
    ) -> Result<Option<SessionResponseTask>, AppError> {
        let now = unix_millis();
        sqlx::query("UPDATE session_response_summaries SET state = 'unavailable', error_code = 'projection_interrupted', lease_owner = NULL, lease_until = NULL WHERE (tenant_id, key_id, request_id) IN (SELECT tenant_id, key_id, request_id FROM session_response_summaries WHERE state = 'running' AND attempts >= 3 AND lease_until <= $1 ORDER BY next_attempt_at LIMIT 100) AND state = 'running' AND lease_until <= $1")
            .bind(now).execute(&self.pool).await?;
        let lock = if matches!(self.backend, DatabaseBackend::PostgreSql) {
            " FOR UPDATE SKIP LOCKED"
        } else {
            ""
        };
        let sql = format!(
            "UPDATE session_response_summaries SET state = 'running', attempts = attempts + 1, lease_owner = $1, lease_until = $2 WHERE (tenant_id, key_id, request_id) IN (SELECT tenant_id, key_id, request_id FROM session_response_summaries WHERE state IN ('pending', 'running') AND attempts < 3 AND next_attempt_at <= $3 AND (lease_until IS NULL OR lease_until <= $3) ORDER BY next_attempt_at LIMIT 1{lock}) RETURNING tenant_id, key_id, request_id, attempts, created_at"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(owner.to_string())
            .bind(now + 120_000)
            .bind(now)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| {
            Ok(SessionResponseTask {
                tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                request_id: parse_uuid(row.try_get("request_id")?)?,
                owner,
                attempts: row.try_get("attempts")?,
                created_at: row.try_get("created_at")?,
            })
        })
        .transpose()
    }

    pub(crate) async fn finish_session_response(
        &self,
        task: &SessionResponseTask,
        answer: Option<String>,
        error: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        let waiting = error == Some("archive_pending") && now - task.created_at < 86_400_000;
        let state = if error.is_none() {
            "ready"
        } else if waiting || task.attempts < 3 {
            "pending"
        } else {
            "unavailable"
        };
        let mut tx = self.begin_write_transaction().await?;
        let valid: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM session_response_summaries WHERE tenant_id = $1 AND key_id = $2 AND request_id = $3 AND lease_owner = $4 AND lease_until > $5")
            .bind(task.tenant_id.to_string()).bind(task.key_id.to_string()).bind(task.request_id.to_string()).bind(task.owner.to_string()).bind(now).fetch_one(&mut *tx).await?;
        if valid != 1 {
            return Ok(());
        }
        let atom = if let Some(answer) = answer {
            let encoded = encode_session_response(&answer)
                .map_err(|_| AppError::internal("session_responses_finish_session_response_1"))?;
            // The worker transfers ownership so only the encoded text remains
            // while SQL creates its driver/native binding buffers.
            drop(answer);
            let digest = blake3::hash(encoded.as_bytes()).to_hex().to_string();
            sqlx::query("INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ($1, $2, $2, 'assistant', 'response_summary', $3, $4) ON CONFLICT(tenant_id, content_hash) DO NOTHING")
                .bind(task.tenant_id.to_string()).bind(&digest).bind(encoded).bind(now).execute(&mut *tx).await?;
            Some(digest)
        } else {
            None
        };
        let updated = sqlx::query("UPDATE session_response_summaries SET state = $1, atom_hash = $2, error_code = $3, lease_owner = NULL, lease_until = NULL, next_attempt_at = $4, updated_at = $5, attempts = CASE WHEN $6 = 1 THEN 0 ELSE attempts END WHERE tenant_id = $7 AND key_id = $8 AND request_id = $9 AND lease_owner = $10 AND lease_until > $5")
            .bind(state).bind(atom).bind(error).bind(now + 30_000).bind(now).bind(i64::from(waiting))
            .bind(task.tenant_id.to_string()).bind(task.key_id.to_string()).bind(task.request_id.to_string()).bind(task.owner.to_string()).execute(&mut *tx).await?;
        if updated.rows_affected() == 1 {
            tx.commit().await?;
        }
        Ok(())
    }

    pub(crate) async fn session_response_text(
        &self,
        scope: &SessionContextScope,
        atom: &str,
        offset: i64,
    ) -> Result<(String, i64), AppError> {
        let start = i32::try_from(offset.checked_add(1).ok_or(AppError::NotFound)?)
            .map_err(|_| AppError::NotFound)?;
        if start < 1 {
            return Err(AppError::NotFound);
        }
        let text = match self.backend {
            DatabaseBackend::PostgreSql => "a.content_json::jsonb ->> 'content'",
            DatabaseBackend::Sqlite => "json_extract(a.content_json, '$.content')",
        };
        let sql = format!(
            "SELECT SUBSTR({text}, $5, 16384) AS text, CAST(LENGTH({text}) AS BIGINT) AS total FROM session_response_summaries r JOIN semantic_atoms a ON a.tenant_id = r.tenant_id AND a.content_hash = r.atom_hash WHERE r.tenant_id = $1 AND r.key_id = $2 AND r.request_id = $3 AND r.atom_hash = $4 AND r.state = 'ready' AND r.session_id = $6"
        );
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(scope.tenant_id.to_string())
            .bind(scope.key_id.to_string())
            .bind(scope.request_id.to_string())
            .bind(atom)
            .bind(start)
            .bind(scope.session_id.to_string())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        Ok((row.try_get("text")?, row.try_get("total")?))
    }
}

#[cfg(test)]
mod tests;
