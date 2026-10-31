use super::super::*;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct SelectedEgress {
    pub kind: String,
    pub group_id: Option<Uuid>,
    pub group_version: Option<i64>,
    pub group_name: Option<String>,
    pub member_id: Option<Uuid>,
    pub member_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proxy_version: Option<i64>,
    scheme: Option<String>,
    host: Option<String>,
    port: Option<u16>,
}

impl SelectedEgress {
    pub fn capture(selected_proxy_url: Option<&str>) -> Result<Self, AppError> {
        let address = selected_proxy_url
            .map(|value| {
                let parsed =
                    url::Url::parse(value).map_err(|_| AppError::internal("egress_capture_1"))?;
                let host = parsed
                    .host_str()
                    .ok_or(AppError::internal("egress_capture_1"))?
                    .to_owned();
                if !matches!(parsed.scheme(), "http" | "https" | "socks5" | "socks5h") {
                    return Err(AppError::internal("egress_capture_2"));
                }
                let port = parsed
                    .port_or_known_default()
                    .or_else(|| matches!(parsed.scheme(), "socks5" | "socks5h").then_some(1080));
                Ok((parsed.scheme().to_owned(), host, port))
            })
            .transpose()?;
        Ok(Self {
            kind: if address.is_some() {
                "proxy"
            } else {
                "unknown"
            }
            .into(),
            group_id: None,
            group_version: None,
            group_name: None,
            member_id: None,
            member_name: None,
            proxy_version: None,
            scheme: address.as_ref().map(|value| value.0.clone()),
            host: address.as_ref().map(|value| value.1.clone()),
            port: address.and_then(|value| value.2),
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestEgressAttempt {
    pub request_id: Uuid,
    pub attempt_id: Uuid,
    pub outbound_attempt: i64,
    pub connect_attempt: i64,
    pub route_id: Uuid,
    pub upstream_account_id: Uuid,
    pub credential_generation: i64,
    pub transport_revision: i64,
    pub observed_at: i64,
    pub selected: SelectedEgress,
    pub transport: Value,
    pub connection_binding: Option<Value>,
    pub request_payload_bytes: Option<i64>,
    pub response_payload_bytes: Option<i64>,
    pub completed: bool,
    pub final_response: bool,
    pub response_body_complete: bool,
    pub measurement_basis: &'static str,
}

#[derive(Debug, Serialize)]
pub struct RequestEgressSummary {
    pub selected: SelectedEgress,
    pub recorded_attempts: i64,
    pub completed_attempts: i64,
    pub final_response_attempts: i64,
    pub request_known_attempts: i64,
    pub response_known_attempts: i64,
    pub request_payload_bytes: Option<i64>,
    pub response_payload_bytes: Option<i64>,
    pub measurement_basis: &'static str,
    pub wire_bytes: Option<i64>,
    pub supplier_billed_bytes: Option<i64>,
}

impl Database {
    pub async fn record_request_egress_attempt(
        &self,
        attempt: &RequestEgressAttempt,
    ) -> Result<(), AppError> {
        let snapshot = serde_json::to_string(&attempt.selected)
            .map_err(|_| AppError::internal("egress_record_request_egress_attempt_1"))?;
        let transport = serde_json::to_string(&attempt.transport)
            .map_err(|_| AppError::internal("egress_record_request_egress_attempt_2"))?;
        let binding = attempt
            .connection_binding
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::internal("egress_record_request_egress_attempt_3"))?;
        if snapshot.len() > 8192
            || transport.len() > 8192
            || binding.as_ref().is_some_and(|value| value.len() > 2048)
        {
            return Err(AppError::internal("egress_record_request_egress_attempt_1"));
        }
        let result = sqlx::query(
            "INSERT INTO request_egress_attempts (
                request_id, outbound_attempt, connect_attempt, route_id, upstream_account_id,
                credential_generation, transport_revision, observed_at, snapshot_json,
                request_payload_bytes, response_payload_bytes, completed, final_response,
                attempt_id, transport_json, response_body_complete, connection_binding_json
             ) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17)
             ON CONFLICT (request_id, attempt_id) DO UPDATE SET
                request_payload_bytes = CASE
                    WHEN request_egress_attempts.request_payload_bytes IS NULL
                        OR excluded.request_payload_bytes > request_egress_attempts.request_payload_bytes
                    THEN excluded.request_payload_bytes ELSE request_egress_attempts.request_payload_bytes END,
                response_payload_bytes = CASE
                    WHEN request_egress_attempts.response_payload_bytes IS NULL
                        OR excluded.response_payload_bytes > request_egress_attempts.response_payload_bytes
                    THEN excluded.response_payload_bytes ELSE request_egress_attempts.response_payload_bytes END,
                completed = CASE WHEN excluded.completed > request_egress_attempts.completed
                    THEN excluded.completed ELSE request_egress_attempts.completed END,
                final_response = CASE WHEN excluded.final_response > request_egress_attempts.final_response
                    THEN excluded.final_response ELSE request_egress_attempts.final_response END,
                response_body_complete = CASE WHEN excluded.response_body_complete > request_egress_attempts.response_body_complete
                    THEN excluded.response_body_complete ELSE request_egress_attempts.response_body_complete END,
                connection_binding_json = COALESCE(request_egress_attempts.connection_binding_json, excluded.connection_binding_json)
             WHERE request_egress_attempts.route_id = excluded.route_id
                AND request_egress_attempts.outbound_attempt = excluded.outbound_attempt
                AND request_egress_attempts.connect_attempt = excluded.connect_attempt
                AND request_egress_attempts.upstream_account_id = excluded.upstream_account_id
                AND request_egress_attempts.credential_generation = excluded.credential_generation
                AND request_egress_attempts.transport_revision = excluded.transport_revision
                AND request_egress_attempts.observed_at = excluded.observed_at
                AND request_egress_attempts.snapshot_json = excluded.snapshot_json
                AND request_egress_attempts.transport_json = excluded.transport_json",
        )
        .bind(attempt.request_id.to_string())
        .bind(attempt.outbound_attempt)
        .bind(attempt.connect_attempt)
        .bind(attempt.route_id.to_string())
        .bind(attempt.upstream_account_id.to_string())
        .bind(attempt.credential_generation)
        .bind(attempt.transport_revision)
        .bind(attempt.observed_at)
        .bind(snapshot)
        .bind(attempt.request_payload_bytes)
        .bind(attempt.response_payload_bytes)
        .bind(i64::from(attempt.completed))
        .bind(i64::from(attempt.final_response))
        .bind(attempt.attempt_id.to_string())
        .bind(transport)
        .bind(i64::from(attempt.response_body_complete))
        .bind(binding)
        .execute(&self.pool)
        .await?;
        if result.rows_affected() != 1 {
            return Err(AppError::storage("egress_attempt_snapshot_conflict"));
        }
        crate::db::network_proxies::signals::stage_attempt(&self.pool, attempt).await?;
        Ok(())
    }

    pub async fn request_egress_attempts(
        &self,
        request_id: Uuid,
    ) -> Result<Vec<RequestEgressAttempt>, AppError> {
        let rows = sqlx::query(
            "SELECT * FROM request_egress_attempts WHERE request_id = $1
             ORDER BY observed_at, attempt_id",
        )
        .bind(request_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestEgressAttempt {
                    request_id,
                    attempt_id: parse_uuid(row.try_get("attempt_id")?)?,
                    outbound_attempt: row.try_get("outbound_attempt")?,
                    connect_attempt: row.try_get("connect_attempt")?,
                    route_id: parse_uuid(row.try_get("route_id")?)?,
                    upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
                    credential_generation: row.try_get("credential_generation")?,
                    transport_revision: row.try_get("transport_revision")?,
                    observed_at: row.try_get("observed_at")?,
                    selected: decode_selected(&row.try_get::<String, _>("snapshot_json")?)?,
                    transport: serde_json::from_str(&row.try_get::<String, _>("transport_json")?)
                        .map_err(|_| {
                        AppError::storage("egress_transport_snapshot_is_malformed")
                    })?,
                    connection_binding: row
                        .try_get::<Option<String>, _>("connection_binding_json")?
                        .map(|value| serde_json::from_str(&value))
                        .transpose()
                        .map_err(|_| AppError::storage("egress_connection_binding_is_malformed"))?,
                    request_payload_bytes: row.try_get("request_payload_bytes")?,
                    response_payload_bytes: row.try_get("response_payload_bytes")?,
                    completed: row.try_get::<i64, _>("completed")? != 0,
                    final_response: row.try_get::<i64, _>("final_response")? != 0,
                    response_body_complete: row.try_get::<i64, _>("response_body_complete")? != 0,
                    measurement_basis: "application_payload_estimate",
                })
            })
            .collect()
    }

    pub async fn request_egress_summary(
        &self,
        start: i64,
        end: i64,
    ) -> Result<Vec<RequestEgressSummary>, AppError> {
        self.request_egress_summary_for_tenant(start, end, None)
            .await
    }

    pub async fn request_egress_summary_for_tenant(
        &self,
        start: i64,
        end: i64,
        tenant_external_id: Option<&str>,
    ) -> Result<Vec<RequestEgressSummary>, AppError> {
        if start >= end {
            return Err(AppError::BadRequest(
                "invalid egress observation window".into(),
            ));
        }
        let rows = if let Some(external_id) = tenant_external_id {
            sqlx::query(
                "SELECT snapshot_json, COUNT(*) AS recorded_attempts,
                    CAST(SUM(completed) AS BIGINT) AS completed_attempts,
                    CAST(SUM(final_response) AS BIGINT) AS final_response_attempts,
                    COUNT(request_payload_bytes) AS request_known_attempts,
                    COUNT(response_payload_bytes) AS response_known_attempts,
                    CAST(SUM(request_payload_bytes) AS BIGINT) AS request_payload_bytes,
                    CAST(SUM(response_payload_bytes) AS BIGINT) AS response_payload_bytes
                 FROM request_egress_attempts e
                 JOIN request_record_locators locator ON locator.id = e.request_id
                 JOIN tenants tenant ON tenant.id = locator.tenant_id
                 WHERE e.observed_at >= $1 AND e.observed_at < $2 AND tenant.external_id = $3
                 GROUP BY snapshot_json ORDER BY snapshot_json",
            )
            .bind(start)
            .bind(end)
            .bind(external_id)
            .fetch_all(&self.pool)
            .await?
        } else {
            sqlx::query(
                "SELECT snapshot_json, COUNT(*) AS recorded_attempts,
                    CAST(SUM(completed) AS BIGINT) AS completed_attempts,
                    CAST(SUM(final_response) AS BIGINT) AS final_response_attempts,
                    COUNT(request_payload_bytes) AS request_known_attempts,
                    COUNT(response_payload_bytes) AS response_known_attempts,
                    CAST(SUM(request_payload_bytes) AS BIGINT) AS request_payload_bytes,
                    CAST(SUM(response_payload_bytes) AS BIGINT) AS response_payload_bytes
                 FROM request_egress_attempts e WHERE e.observed_at >= $1 AND e.observed_at < $2
                 GROUP BY snapshot_json ORDER BY snapshot_json",
            )
            .bind(start)
            .bind(end)
            .fetch_all(&self.pool)
            .await?
        };
        rows.into_iter()
            .map(|row| {
                Ok(RequestEgressSummary {
                    selected: decode_selected(&row.try_get::<String, _>("snapshot_json")?)?,
                    recorded_attempts: row.try_get("recorded_attempts")?,
                    completed_attempts: row.try_get("completed_attempts")?,
                    final_response_attempts: row.try_get("final_response_attempts")?,
                    request_known_attempts: row.try_get("request_known_attempts")?,
                    response_known_attempts: row.try_get("response_known_attempts")?,
                    request_payload_bytes: row.try_get("request_payload_bytes")?,
                    response_payload_bytes: row.try_get("response_payload_bytes")?,
                    measurement_basis: "application_payload_estimate",
                    wire_bytes: None,
                    supplier_billed_bytes: None,
                })
            })
            .collect()
    }
}

fn decode_selected(snapshot: &str) -> Result<SelectedEgress, AppError> {
    serde_json::from_str(snapshot).map_err(|_| AppError::storage("egress_snapshot_is_malformed"))
}

#[cfg(test)]
#[path = "egress_tests.rs"]
mod tests;
