use super::*;

const IDENTITY_FORWARD_PAGE: &str = "SELECT request_id FROM conversation_projection_outbox WHERE request_id > $1 ORDER BY request_id ASC LIMIT $2";
const IDENTITY_HISTORY_PAGE: &str = "SELECT request_id FROM conversation_projection_outbox WHERE request_id < $1 ORDER BY request_id DESC LIMIT $2";
const IDENTITY_LATEST_PAGE: &str =
    "SELECT request_id FROM conversation_projection_outbox ORDER BY request_id DESC LIMIT $1";

impl Database {
    /// Publish only the bounded identity already captured in the durable
    /// receipt. Missing/slow body archives must not erase a declared chat.
    /// The body worker enriches this observation later under the same lease.
    pub(crate) async fn project_claimed_conversation_identity(
        &self,
        lease_owner: Uuid,
        request_id: Uuid,
    ) -> Result<(), AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
        let sql = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT tenant_id, key_id, principal_id, hints_json, key_snapshot_json, client_name, upstream_response_id, observed_at FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3 FOR UPDATE"
            }
            DatabaseBackend::Sqlite => {
                "SELECT tenant_id, key_id, principal_id, hints_json, key_snapshot_json, client_name, upstream_response_id, observed_at FROM conversation_projection_outbox WHERE request_id = $1 AND projected_at IS NULL AND COALESCE(terminal_lease_owner, lease_owner) = $2 AND COALESCE(terminal_lease_expires_at, lease_expires_at) >= $3"
            }
        };
        let row = sqlx::query(sql)
            .bind(request_id.to_string())
            .bind(lease_owner.to_string())
            .bind(unix_millis())
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(row) = row else {
            return Ok(());
        };
        self.project_conversation_identity_row_in_transaction(&mut transaction, request_id, row)
            .await?;
        transaction.commit().await?;
        Ok(())
    }

    /// The page boundary precedes all eligibility work. LIMIT on the old
    /// join/anti-join bounded results but still scanned the entire backlog.
    pub(crate) async fn project_declared_conversation_identity_pages(
        &self,
        live_after: Option<&str>,
        history_before: Option<&str>,
        limit: i64,
    ) -> Result<(Option<String>, Option<String>), AppError> {
        let budget = limit.clamp(2, CONVERSATION_PROJECTION_BATCH_LIMIT);
        let live_budget = (budget + 1) / 2;
        let live: Vec<String> = if let Some(after) = live_after {
            sqlx::query_scalar(IDENTITY_FORWARD_PAGE)
                .bind(after)
                .bind(live_budget)
                .fetch_all(&self.pool)
                .await?
        } else {
            sqlx::query_scalar(IDENTITY_LATEST_PAGE)
                .bind(live_budget)
                .fetch_all(&self.pool)
                .await?
        };
        let next_live = if live_after.is_some() {
            live.last()
        } else {
            live.first()
        }
        .cloned()
        .or_else(|| live_after.map(str::to_owned));
        let before = history_before
            .map(str::to_owned)
            .or_else(|| live.last().cloned());
        let history: Vec<String> = if let Some(before) = before {
            sqlx::query_scalar(IDENTITY_HISTORY_PAGE)
                .bind(before)
                .bind(budget / 2)
                .fetch_all(&self.pool)
                .await?
        } else {
            Vec::new()
        };
        let next_history = history.last().cloned().or_else(|| next_live.clone());
        let mut ids = live;
        ids.extend(history);
        let mut seen = std::collections::HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));
        self.project_conversation_identity_ids(ids).await;
        Ok((next_live, next_history))
    }

    #[cfg(test)]
    pub(crate) async fn project_declared_conversation_identities(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Option<String>, AppError> {
        let ids: Vec<String> = sqlx::query_scalar(IDENTITY_FORWARD_PAGE)
            .bind(after.unwrap_or(""))
            .bind(limit.clamp(1, CONVERSATION_PROJECTION_BATCH_LIMIT))
            .fetch_all(&self.pool)
            .await?;
        let next = ids.last().cloned();
        self.project_conversation_identity_ids(ids).await;
        Ok(next)
    }

    async fn project_conversation_identity_ids(&self, ids: Vec<String>) {
        let eligible = "c.projected_at IS NULL AND c.key_snapshot_json IS NOT NULL
            AND t.projected_at IS NOT NULL AND t.tenant_id = c.tenant_id AND t.key_id = c.key_id
            AND NOT EXISTS (SELECT 1 FROM conversation_observations o WHERE o.request_id = c.request_id)
            AND NOT EXISTS (SELECT 1 FROM observability_prune_boundaries b WHERE b.scope = 'global' AND b.before_day > t.created_at / 86400000)";
        let row_sql = format!(
            "SELECT c.tenant_id, c.key_id, c.principal_id, c.hints_json, c.key_snapshot_json, c.client_name, c.upstream_response_id, c.observed_at FROM conversation_projection_outbox c JOIN terminal_projection_outbox t ON t.request_id = c.request_id WHERE c.request_id = $1 AND {eligible}"
        );
        for id in ids {
            let result = async {
                let request_id = parse_uuid(id.clone())?;
                // Read/parse metadata before taking the shared stats writer.
                // Invalid/unrelated historical rows must not queue on that lock.
                let Some(row) = sqlx::query(sqlx::AssertSqlSafe(row_sql.clone()))
                    .bind(&id)
                    .fetch_optional(&self.pool)
                    .await?
                else {
                    return Ok(());
                };
                let hints: crate::conversation::ConversationHints =
                    serde_json::from_str(&row.try_get::<String, _>("hints_json")?)
                        .map_err(|_| AppError::internal("conversation_identity_hints"))?;
                if hints
                    .session_id
                    .as_deref()
                    .is_none_or(|session| session.trim().is_empty())
                {
                    return Ok(());
                }
                let mut transaction = self.begin_write_transaction().await?;
                lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
                let lock = match self.backend {
                    DatabaseBackend::PostgreSql => " FOR UPDATE OF c SKIP LOCKED",
                    DatabaseBackend::Sqlite => "",
                };
                // Preflight grants no authority: recheck eligibility under the
                // original stats/prune -> outbox locking order before publishing.
                let row = sqlx::query(sqlx::AssertSqlSafe(format!("{row_sql}{lock}")))
                    .bind(&id)
                    .fetch_optional(&mut *transaction)
                    .await?;
                if let Some(row) = row {
                    self.project_conversation_identity_row_in_transaction(
                        &mut transaction,
                        request_id,
                        row,
                    )
                    .await?;
                }
                transaction.commit().await?;
                Ok::<(), AppError>(())
            }
            .await;
            if let Err(error) = result {
                tracing::error!(%error, request_id = %id, "declared conversation identity remains pending");
            }
        }
    }

    async fn project_conversation_identity_row_in_transaction(
        &self,
        transaction: &mut Transaction<'_, Any>,
        request_id: Uuid,
        row: sqlx::any::AnyRow,
    ) -> Result<(), AppError> {
        if let Some(terminal) = sqlx::query(
            "SELECT created_at, projected_at FROM terminal_projection_outbox WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_optional(&mut **transaction)
        .await?
            && (terminal
                .try_get::<Option<i64>, _>("projected_at")?
                .is_none()
                || super::super::terminal_projection::statistics_pruned_in_transaction(
                    transaction,
                    terminal.try_get("created_at")?,
                )
                .await?)
        {
            return Ok(());
        }
        let hints: crate::conversation::ConversationHints =
            serde_json::from_str(&row.try_get::<String, _>("hints_json")?)
                .map_err(|_| AppError::internal("conversation_identity_hints"))?;
        if hints.session_id.is_none() {
            // Never turn key ownership or a missing identity into a chat.
            return Ok(());
        }
        let existing: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM conversation_observations WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_one(&mut **transaction)
        .await?;
        if existing != 0 {
            return Ok(());
        }
        let Some(snapshot) = row.try_get::<Option<String>, _>("key_snapshot_json")? else {
            // Older receipts retain their existing body projection path.
            return Ok(());
        };
        let snapshot: serde_json::Value = serde_json::from_str(&snapshot)
            .map_err(|_| AppError::internal("conversation_identity_key"))?;
        let key = AuthenticatedKey {
            key_id: parse_uuid(row.try_get("key_id")?)?,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            principal_id: parse_uuid(row.try_get("principal_id")?)?,
            account_id: serde_json::from_value(snapshot["account_id"].clone())
                .map_err(|_| AppError::internal("conversation_identity_account"))?,
            alias: serde_json::from_value(snapshot["alias"].clone())
                .map_err(|_| AppError::internal("conversation_identity_alias"))?,
            currency: serde_json::from_value(snapshot["currency"].clone())
                .map_err(|_| AppError::internal("conversation_identity_currency"))?,
            credential_generation: serde_json::from_value(
                snapshot["credential_generation"].clone(),
            )
            .map_err(|_| AppError::internal("conversation_identity_generation"))?,
            policy: serde_json::from_value(snapshot["policy"].clone())
                .map_err(|_| AppError::internal("conversation_identity_policy"))?,
        };
        let client_name: Option<String> = row.try_get("client_name")?;
        let response_id: Option<String> = row.try_get("upstream_response_id")?;
        self.record_conversation_observation_with_snapshot_in_transaction(
            transaction,
            ConversationObservationInput {
                key: &key,
                request_id,
                request_json: &serde_json::json!({}),
                hints: &hints,
                client_name: client_name.as_deref(),
                upstream_response_id: response_id.as_deref(),
                observed_at: row.try_get("observed_at")?,
                attach_request_record: true,
                content_materialized: true,
            },
            None,
        )
        .await
        .map_err(AppError::from)?;
        Ok(())
    }
}
