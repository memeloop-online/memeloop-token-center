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
        if let Some(terminal) = sqlx::query(
            "SELECT created_at, projected_at FROM terminal_projection_outbox WHERE request_id = $1",
        )
        .bind(request_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
            && (terminal
                .try_get::<Option<i64>, _>("projected_at")?
                .is_none()
                || super::super::terminal_projection::statistics_pruned_in_transaction(
                    &mut transaction,
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
        .fetch_one(&mut *transaction)
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
            &mut transaction,
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
        transaction.commit().await?;
        Ok(())
    }
}
