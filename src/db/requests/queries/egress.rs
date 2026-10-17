use super::*;
use crate::model::RequestEgressObservation;
use std::collections::HashMap;

pub(super) async fn request_list_egress(
    database: &Database,
    request_ids: &[String],
) -> Result<HashMap<Uuid, RequestEgressObservation>, AppError> {
    if request_ids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut query = PortableRequestListQuery::new(
        "SELECT request_id, snapshot_json, observed_at FROM (
            SELECT request_id, snapshot_json, observed_at,
                ROW_NUMBER() OVER (PARTITION BY request_id
                    ORDER BY final_response DESC, observed_at DESC, attempt_id DESC) AS attempt_rank
            FROM request_egress_attempts WHERE request_id IN (",
    );
    for (index, request_id) in request_ids.iter().enumerate() {
        if index > 0 {
            query.push(", ");
        }
        query.bind_text(request_id);
    }
    query.push(")) ranked WHERE attempt_rank = 1");
    let mut statement = sqlx::query(sqlx::AssertSqlSafe(query.statement));
    for request_id in request_ids {
        statement = statement.bind(request_id);
    }
    let rows = statement.fetch_all(&database.pool).await?;
    let mut observations = HashMap::with_capacity(rows.len());
    for row in rows {
        let snapshot: String = row.try_get("snapshot_json")?;
        let Ok(selected) = serde_json::from_str(&snapshot) else {
            continue;
        };
        observations.insert(
            parse_uuid(row.try_get("request_id")?)?,
            RequestEgressObservation {
                selected,
                observed_at: row.try_get("observed_at")?,
            },
        );
    }
    Ok(observations)
}

#[cfg(test)]
#[path = "egress_tests.rs"]
mod tests;
