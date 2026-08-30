use super::super::*;
use crate::{db::UsageAnalysisFilter, model::SelfUsageAnalysisResponse};

/// Public analytics filters intentionally exclude every management identity
/// selector. Unknown fields are rejected instead of being silently ignored so
/// callers cannot mistake a tenant/key/upstream selector for an effective one.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct SelfUsageAnalysisQuery {
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
    granularity: Option<String>,
    model: Option<String>,
    protocol: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
}

impl SelfUsageAnalysisQuery {
    fn into_filter(self) -> UsageAnalysisFilter {
        UsageAnalysisFilter {
            from_created_at: self.from_created_at,
            to_created_at: self.to_created_at,
            granularity: self.granularity,
            model: self.model,
            protocol: self.protocol,
            status: self.status,
            error_code: self.error_code,
            ..UsageAnalysisFilter::default()
        }
    }
}

pub(in crate::api) async fn self_usage_analysis(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<SelfUsageAnalysisQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let analysis = state
        .db
        .self_usage_analysis(key.tenant_id, key.key_id, query.into_filter())
        .await?;
    Ok((
        [(header::CACHE_CONTROL, "private, no-store")],
        Json::<SelfUsageAnalysisResponse>(analysis),
    ))
}
