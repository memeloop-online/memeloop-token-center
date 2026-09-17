use sqlx::Row;
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid};
use crate::model::AuthenticatedKey;

impl Database {
    /// Returns the exact route/account pair used by the latest completed
    /// request for this model and protocol in an explicit session, but only
    /// when that terminal is a transport-class 502.
    ///
    /// The synchronous request terminal owns `explicit_session_id`; semantic
    /// conversation projection is deliberately outside this routing path.
    pub(crate) async fn latest_session_transport_route_to_avoid(
        &self,
        key: &AuthenticatedKey,
        explicit_session_id: &str,
        model: &str,
        protocol: &str,
    ) -> Result<Option<(Uuid, Uuid)>, AppError> {
        let row = sqlx::query(
            "SELECT r.status_code, r.error_code, r.model_route_id, r.upstream_account_id
             FROM request_records r
             JOIN key_records k
               ON k.id = r.key_id
              AND k.tenant_id = r.tenant_id
              AND k.principal_id = $3
             WHERE r.tenant_id = $1
               AND r.key_id = $2
               AND r.explicit_session_id = $4
               AND r.model = $5
               AND r.protocol = $6
               AND r.completed_at IS NOT NULL
             ORDER BY r.completed_at DESC, r.id DESC
             LIMIT 1",
        )
        .bind(key.tenant_id.to_string())
        .bind(key.key_id.to_string())
        .bind(key.principal_id.to_string())
        .bind(explicit_session_id)
        .bind(model)
        .bind(protocol)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<Option<i64>, _>("status_code")? != Some(502) {
            return Ok(None);
        }
        let error_code: Option<String> = row.try_get("error_code")?;
        if !error_code
            .as_deref()
            .is_some_and(is_session_avoid_transport_error_code)
        {
            return Ok(None);
        }
        let route_id = row
            .try_get::<Option<String>, _>("model_route_id")?
            .map(parse_uuid)
            .transpose()?;
        let account_id = row
            .try_get::<Option<String>, _>("upstream_account_id")?
            .map(parse_uuid)
            .transpose()?;
        Ok(route_id.zip(account_id))
    }
}

fn is_session_avoid_transport_error_code(error_code: &str) -> bool {
    error_code.starts_with("upstream_transport_")
        || matches!(
            error_code,
            "upstream_timeout"
                | "upstream_read_timeout"
                | "upstream_request_timeout"
                | "upstream_stream"
                | "upstream_stream_read_error"
        )
}

#[cfg(test)]
mod tests {
    use super::is_session_avoid_transport_error_code;

    #[test]
    fn only_transport_terminal_codes_are_session_avoid_evidence() {
        for error_code in [
            "upstream_transport_timeout",
            "upstream_transport_connection_reset",
            "upstream_transport_outer_deadline",
            "upstream_timeout",
            "upstream_read_timeout",
            "upstream_request_timeout",
            "upstream_stream",
            "upstream_stream_read_error",
        ] {
            assert!(is_session_avoid_transport_error_code(error_code));
        }
        for error_code in [
            "upstream_error",
            "upstream_incomplete_response",
            "upstream_invalid_response",
            "upstream_connection",
            "http_502",
            "proxy_memory_capacity",
        ] {
            assert!(!is_session_avoid_transport_error_code(error_code));
        }
    }
}
