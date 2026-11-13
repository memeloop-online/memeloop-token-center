use super::*;

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

    /// Independent metadata-only consumer: row locks serialize identity, while
    /// the body's owner, lease, attempts and completion remain untouched.
    pub(crate) async fn project_declared_conversation_identities(
        &self,
        after: Option<&str>,
        limit: i64,
    ) -> Result<Option<String>, AppError> {
        // Parse declared identity per row below: one corrupt legacy hints value
        // must not poison the entire page before its cursor can advance.
        let eligible = "c.projected_at IS NULL AND c.key_snapshot_json IS NOT NULL
            AND t.projected_at IS NOT NULL AND t.tenant_id = c.tenant_id AND t.key_id = c.key_id
            AND NOT EXISTS (SELECT 1 FROM conversation_observations o WHERE o.request_id = c.request_id)
            AND NOT EXISTS (SELECT 1 FROM observability_prune_boundaries b WHERE b.scope = 'global' AND b.before_day > t.created_at / 86400000)";
        let ids: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT c.request_id FROM conversation_projection_outbox c JOIN terminal_projection_outbox t ON t.request_id = c.request_id WHERE c.request_id > $1 AND {eligible} ORDER BY c.request_id LIMIT $2"
        )))
        .bind(after.unwrap_or(""))
        .bind(limit.clamp(1, CONVERSATION_PROJECTION_BATCH_LIMIT))
        .fetch_all(&self.pool).await?;
        let next = ids.last().cloned();
        for id in ids {
            let result = async {
                let request_id = parse_uuid(id.clone())?;
                let mut transaction = self.begin_write_transaction().await?;
                // Keep the original stats/prune lock -> outbox row lock order.
                lock_request_stats_projection_writer_in_transaction(&mut transaction).await?;
                let lock = match self.backend {
                    DatabaseBackend::PostgreSql => " FOR UPDATE OF c SKIP LOCKED",
                    DatabaseBackend::Sqlite => "",
                };
                let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "SELECT c.tenant_id, c.key_id, c.principal_id, c.hints_json, c.key_snapshot_json, c.client_name, c.upstream_response_id, c.observed_at FROM conversation_projection_outbox c JOIN terminal_projection_outbox t ON t.request_id = c.request_id WHERE c.request_id = $1 AND {eligible}{lock}"
                )))
                .bind(&id).fetch_optional(&mut *transaction).await?;
                if let Some(row) = row {
                    self.project_conversation_identity_row_in_transaction(&mut transaction, request_id, row).await?;
                }
                transaction.commit().await?;
                Ok::<(), AppError>(())
            }.await;
            if let Err(error) = result {
                // Keep evidence/task for a later cursor pass; one bad snapshot
                // must not prevent other declared chats becoming visible.
                tracing::error!(%error, request_id = %id, "declared conversation identity remains pending");
            }
        }
        Ok(next)
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
