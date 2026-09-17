use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid, unix_millis};
use crate::model::AuthenticatedKey;

const SESSION_ROUTING_TERMINAL_TTL_MS: i64 = 24 * 60 * 60 * 1_000;

#[derive(Clone, Copy)]
pub(crate) struct SessionRoutingTerminalInput<'a> {
    pub key: &'a AuthenticatedKey,
    pub request_id: Uuid,
    pub explicit_session_id: &'a str,
    pub model: &'a str,
    pub protocol: &'a str,
    pub status_code: i64,
    pub error_code: Option<&'a str>,
    pub model_route_id: Option<Uuid>,
    pub upstream_account_id: Option<Uuid>,
    pub observed_at: i64,
}

impl Database {
    /// Makes a streaming terminal visible to every gateway replica before the
    /// downstream body is closed. This records routing evidence only; it never
    /// settles, replays, or otherwise changes the current request.
    pub(crate) async fn record_session_routing_terminal(
        &self,
        input: SessionRoutingTerminalInput<'_>,
    ) -> Result<(), AppError> {
        let mut transaction = self.begin_write_transaction().await?;
        upsert_session_routing_terminal_in_transaction(&mut transaction, input).await?;
        transaction.commit().await?;
        Ok(())
    }

    /// Returns the exact route/account pair used by the latest terminal for
    /// this model and protocol in an explicit session, but only when that
    /// terminal is a transport-class 502.
    pub(crate) async fn latest_session_transport_route_to_avoid(
        &self,
        key: &AuthenticatedKey,
        explicit_session_id: &str,
        model: &str,
        protocol: &str,
    ) -> Result<Option<(Uuid, Uuid)>, AppError> {
        let row = sqlx::query(
            "SELECT status_code, error_code, model_route_id, upstream_account_id
             FROM session_routing_terminals
             WHERE tenant_id = $1
               AND principal_id = $2
               AND key_id = $3
               AND explicit_session_id = $4
               AND model = $5
               AND protocol = $6
               AND expires_at > $7
             ORDER BY observed_at DESC, request_id DESC
             LIMIT 1",
        )
        .bind(key.tenant_id.to_string())
        .bind(key.principal_id.to_string())
        .bind(key.key_id.to_string())
        .bind(explicit_session_id)
        .bind(model)
        .bind(protocol)
        .bind(unix_millis())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<i64, _>("status_code")? != 502 {
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

    pub async fn delete_expired_session_routing_terminals(
        &self,
        limit: i64,
    ) -> Result<u64, AppError> {
        let rows = sqlx::query(
            "DELETE FROM session_routing_terminals
             WHERE request_id IN (
                 SELECT request_id
                 FROM session_routing_terminals
                 WHERE expires_at <= $1
                 ORDER BY expires_at, request_id
                 LIMIT $2
             )",
        )
        .bind(unix_millis())
        .bind(limit.clamp(1, 100_000))
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(rows)
    }
}

pub(super) async fn upsert_session_routing_terminal_from_request_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    request_id: &str,
    created_at: i64,
    explicit_session_id: &str,
    status_code: i64,
    error_code: Option<&str>,
    observed_at: i64,
) -> Result<(), AppError> {
    let expires_at = observed_at.saturating_add(SESSION_ROUTING_TERMINAL_TTL_MS);
    sqlx::query(
        "INSERT INTO session_routing_terminals (
             tenant_id, principal_id, key_id, explicit_session_id, model, protocol,
             request_id, observed_at, status_code, error_code, model_route_id,
             upstream_account_id, expires_at
         )
         SELECT r.tenant_id, k.principal_id, r.key_id, $1, r.model, r.protocol,
                r.id, $2, $3, $4, r.model_route_id, r.upstream_account_id, $5
         FROM request_records r
         JOIN key_records k ON k.id = r.key_id AND k.tenant_id = r.tenant_id
         WHERE r.id = $6 AND r.created_at = $7 AND r.completed_at IS NULL
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(explicit_session_id)
    .bind(observed_at)
    .bind(status_code)
    .bind(error_code)
    .bind(expires_at)
    .bind(request_id)
    .bind(created_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

async fn upsert_session_routing_terminal_in_transaction(
    transaction: &mut Transaction<'_, Any>,
    input: SessionRoutingTerminalInput<'_>,
) -> Result<(), AppError> {
    let expires_at = input
        .observed_at
        .saturating_add(SESSION_ROUTING_TERMINAL_TTL_MS);
    sqlx::query(
        "INSERT INTO session_routing_terminals (
             tenant_id, principal_id, key_id, explicit_session_id, model, protocol,
             request_id, observed_at, status_code, error_code, model_route_id,
             upstream_account_id, expires_at
         ) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
         ON CONFLICT (request_id) DO NOTHING",
    )
    .bind(input.key.tenant_id.to_string())
    .bind(input.key.principal_id.to_string())
    .bind(input.key.key_id.to_string())
    .bind(input.explicit_session_id)
    .bind(input.model)
    .bind(input.protocol)
    .bind(input.request_id.to_string())
    .bind(input.observed_at)
    .bind(input.status_code)
    .bind(input.error_code)
    .bind(input.model_route_id.map(|id| id.to_string()))
    .bind(input.upstream_account_id.map(|id| id.to_string()))
    .bind(expires_at)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}

pub(crate) fn is_session_avoid_terminal(status_code: i64, error_code: Option<&str>) -> bool {
    status_code == 502 && error_code.is_some_and(is_session_avoid_transport_error_code)
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
    use super::{is_session_avoid_terminal, is_session_avoid_transport_error_code};

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
        assert!(is_session_avoid_terminal(
            502,
            Some("upstream_stream_read_error")
        ));
        assert!(!is_session_avoid_terminal(502, Some("http_502")));
        assert!(!is_session_avoid_terminal(
            503,
            Some("upstream_stream_read_error")
        ));
    }
}
