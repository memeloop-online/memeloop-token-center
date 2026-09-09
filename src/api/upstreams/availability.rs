use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
};
use serde::Deserialize;

use super::super::{require_service, require_service_tenant};
use crate::{AppState, db::UpstreamAccountAvailabilityFilter, error::AppError};

/// The upstream page always selects a tenant before reading operational facts.
/// Making that tenant explicit prevents a global service credential from
/// accidentally widening a provider card into cross-tenant traffic.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct UpstreamAvailabilityQuery {
    tenant_external_id: String,
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
}

impl UpstreamAvailabilityQuery {
    fn into_request(
        self,
        service: &crate::model::AuthenticatedService,
    ) -> Result<(String, UpstreamAccountAvailabilityFilter), AppError> {
        let tenant_external_id = self.tenant_external_id.trim().to_owned();
        if tenant_external_id.is_empty() || tenant_external_id.len() > 200 {
            return Err(AppError::BadRequest(
                "upstream availability requires tenant_external_id".into(),
            ));
        }
        require_service_tenant(service, &tenant_external_id)?;
        let from_created_at = self.from_created_at.ok_or_else(|| {
            AppError::BadRequest("upstream availability requires from_created_at".into())
        })?;
        let to_created_at = self.to_created_at.ok_or_else(|| {
            AppError::BadRequest("upstream availability requires to_created_at".into())
        })?;
        Ok((
            tenant_external_id,
            UpstreamAccountAvailabilityFilter {
                from_created_at,
                to_created_at,
            },
        ))
    }
}

pub(in crate::api) async fn upstream_account_availability(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<UpstreamAvailabilityQuery>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "providers:read").await?;
    // Recent terminal identifiers, outcomes, and request deep links are
    // request data. Do not grant them to a service that can only administer
    // provider configuration.
    if !service.allows("requests:read") {
        return Err(AppError::Forbidden);
    }
    let (tenant_external_id, filter) = query.into_request(&service)?;
    let availability = state
        .db
        .upstream_account_availability(&tenant_external_id, filter)
        .await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(availability)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn availability_query_requires_a_tenant_and_both_window_bounds() {
        let service = crate::model::AuthenticatedService::bootstrap();
        assert!(
            UpstreamAvailabilityQuery {
                tenant_external_id: "".to_owned(),
                from_created_at: Some(1),
                to_created_at: Some(2),
            }
            .into_request(&service)
            .is_err()
        );
        assert!(
            UpstreamAvailabilityQuery {
                tenant_external_id: "tenant-a".to_owned(),
                from_created_at: None,
                to_created_at: Some(2),
            }
            .into_request(&service)
            .is_err()
        );
        assert!(
            UpstreamAvailabilityQuery {
                tenant_external_id: "tenant-a".to_owned(),
                from_created_at: Some(1),
                to_created_at: None,
            }
            .into_request(&service)
            .is_err()
        );

        let tenant_scoped_service = crate::model::AuthenticatedService {
            service_id: Some(uuid::Uuid::now_v7()),
            scopes: vec!["providers:read".to_owned(), "requests:read".to_owned()],
            tenant_external_id: Some("tenant-b".to_owned()),
        };
        assert!(
            UpstreamAvailabilityQuery {
                tenant_external_id: "tenant-a".to_owned(),
                from_created_at: Some(1),
                to_created_at: Some(2),
            }
            .into_request(&tenant_scoped_service)
            .is_err()
        );
    }
}
