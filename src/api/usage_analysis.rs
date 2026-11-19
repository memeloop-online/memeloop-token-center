use axum::{
    Json,
    extract::{Query, State},
    http::HeaderMap,
};
use serde::Deserialize;
use uuid::Uuid;

use super::{management_tenant, require_service};
use crate::{
    AppState,
    db::{UsageAnalysisFilter, UsageAnalysisProjection, UsageAnalysisUpstreamFilter},
    error::AppError,
    model::{UsageAnalysisResponse, UsageAnalysisTrendsResponse},
};

#[derive(Debug, Default, Deserialize)]
pub(super) struct UsageAnalysisQuery {
    #[serde(default)]
    projection: UsageAnalysisProjection,
    tenant_external_id: Option<String>,
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
    granularity: Option<String>,
    key_id: Option<Uuid>,
    model: Option<String>,
    protocol: Option<String>,
    status: Option<String>,
    error_code: Option<String>,
    upstream_account_id: Option<String>,
    route_id: Option<Uuid>,
    key_alias: Option<String>,
    principal: Option<String>,
}

impl UsageAnalysisQuery {
    fn into_filter(self) -> Result<UsageAnalysisFilter, AppError> {
        let upstream_account_id = self
            .upstream_account_id
            .as_deref()
            .map(UsageAnalysisUpstreamFilter::parse)
            .transpose()?;
        Ok(UsageAnalysisFilter {
            from_created_at: self.from_created_at,
            to_created_at: self.to_created_at,
            granularity: self.granularity,
            key_id: self.key_id,
            model: self.model,
            protocol: self.protocol,
            status: self.status,
            error_code: self.error_code,
            upstream_account_id,
            route_id: self.route_id,
            key_alias: self.key_alias,
            principal: self.principal,
        })
    }
}

async fn authorized_usage_analysis_query(
    state: &AppState,
    headers: &HeaderMap,
    query: UsageAnalysisQuery,
) -> Result<(Option<String>, UsageAnalysisFilter), AppError> {
    let service = require_service(headers, state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?;
    Ok((tenant, query.into_filter()?))
}

pub(super) async fn internal_usage_analysis(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UsageAnalysisQuery>,
) -> Result<Json<UsageAnalysisResponse>, AppError> {
    let projection = query.projection;
    let (tenant, filter) = authorized_usage_analysis_query(&state, &headers, query).await?;
    let response = state
        .db
        .projected_usage_analysis(tenant.as_deref(), filter, projection)
        .await?;
    Ok(Json(response))
}

pub(super) async fn internal_usage_analysis_trends(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UsageAnalysisQuery>,
) -> Result<Json<UsageAnalysisTrendsResponse>, AppError> {
    let projection = query.projection;
    let (tenant, filter) = authorized_usage_analysis_query(&state, &headers, query).await?;
    if !matches!(
        projection,
        UsageAnalysisProjection::Full | UsageAnalysisProjection::Trends
    ) {
        return Err(AppError::BadRequest(
            "the trends endpoint supports only full or trends projection".into(),
        ));
    }
    if projection == UsageAnalysisProjection::Trends {
        let analysis = state
            .db
            .projected_usage_analysis(tenant.as_deref(), filter, projection)
            .await?;
        return Ok(Json(UsageAnalysisTrendsResponse {
            from_created_at: analysis.from_created_at,
            to_created_at: analysis.to_created_at,
            granularity: analysis.granularity,
            time_zone: analysis.time_zone,
            p95_is_approximate: analysis.p95_is_approximate,
            p95_method: analysis.p95_method,
            summary: analysis.summary,
            time_series: analysis.time_series,
        }));
    }
    let response = match tenant {
        Some(tenant) => {
            state
                .db
                .operator_usage_analysis_trends(&tenant, filter)
                .await?
        }
        None => state.db.global_usage_analysis_trends(filter).await?,
    };
    Ok(Json(response))
}
