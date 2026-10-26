use super::*;

#[derive(Debug)]
pub(crate) enum ConversationContentError {
    Application(AppError),
    Database(sqlx::Error),
}

impl ConversationContentError {
    pub(super) fn retryable_sqlstate(&self) -> Option<&'static str> {
        let Self::Database(sqlx::Error::Database(error)) = self else {
            return None;
        };
        match error.code().as_deref() {
            Some("55P03") => Some("55P03"),
            Some("57014") => Some("57014"),
            Some("40001") => Some("40001"),
            Some("40P01") => Some("40P01"),
            _ => None,
        }
    }
}

impl From<AppError> for ConversationContentError {
    fn from(error: AppError) -> Self {
        Self::Application(error)
    }
}

impl From<sqlx::Error> for ConversationContentError {
    fn from(error: sqlx::Error) -> Self {
        Self::Database(error)
    }
}

impl From<ConversationContentError> for AppError {
    fn from(error: ConversationContentError) -> Self {
        match error {
            ConversationContentError::Application(error) => error,
            ConversationContentError::Database(error) => error.into(),
        }
    }
}

impl Database {
    pub(super) async fn try_schedule_rolled_back_conversation_retry(
        &self,
        request_id: Uuid,
        lease_owner: Uuid,
    ) {
        if let Err(error) = self
            .schedule_rolled_back_conversation_retry(request_id, lease_owner)
            .await
        {
            tracing::warn!(
                %request_id,
                error_category = error.diagnostic_category(),
                "conversation rollback retry scheduling failed"
            );
        }
    }

    pub(crate) async fn schedule_rolled_back_conversation_retry(
        &self,
        request_id: Uuid,
        lease_owner: Uuid,
    ) -> Result<bool, AppError> {
        let now = unix_millis();
        let updated = sqlx::query(
            "UPDATE conversation_projection_outbox SET terminal_lease_expires_at = $1 + CASE attempts WHEN 1 THEN 1000 WHEN 2 THEN 2000 WHEN 3 THEN 4000 WHEN 4 THEN 8000 WHEN 5 THEN 16000 WHEN 6 THEN 32000 ELSE 60000 END WHERE request_id = $2 AND projected_at IS NULL AND terminal_lease_owner = $3 AND terminal_lease_expires_at > $1 AND lease_owner = 'terminal-v118' AND lease_expires_at = 9223372036854775807 AND semantic_payload_snapshot_json IS NOT NULL",
        )
        .bind(now)
        .bind(request_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&self.pool)
        .await?;
        Ok(updated.rows_affected() == 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_and_application_errors_do_not_authorize_statement_retry() {
        for error in [
            ConversationContentError::Database(sqlx::Error::Io(std::io::Error::from(
                std::io::ErrorKind::ConnectionReset,
            ))),
            ConversationContentError::Database(sqlx::Error::PoolTimedOut),
            ConversationContentError::Application(AppError::internal(
                "projection_retry_transport_and_application_errors_do_not_authori",
            )),
            ConversationContentError::Application(AppError::Conflict("invalid snapshot".into())),
        ] {
            assert_eq!(error.retryable_sqlstate(), None);
        }
    }
}
