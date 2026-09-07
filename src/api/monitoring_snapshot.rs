use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
};
use serde::Deserialize;

use super::{management_tenant, require_global_service, require_service};
use crate::{
    AppState,
    db::{MonitoringScope, MonitoringSnapshotFilter},
    error::AppError,
    model::OperatorMonitoringSnapshot,
};

/// This endpoint intentionally makes the read scope and both time bounds
/// explicit. A global service says `scope=global`; a tenant scope says
/// `scope=tenant&tenant_external_id=…`. There is no implicit rolling window.
#[derive(Debug, Deserialize)]
pub(super) struct MonitoringSnapshotQuery {
    scope: Option<String>,
    tenant_external_id: Option<String>,
    from_created_at: Option<i64>,
    to_created_at: Option<i64>,
}

impl MonitoringSnapshotQuery {
    fn into_request(
        self,
        service: &crate::model::AuthenticatedService,
    ) -> Result<(MonitoringScope, MonitoringSnapshotFilter), AppError> {
        let from_created_at = self.from_created_at.ok_or_else(|| {
            AppError::BadRequest("monitoring snapshot requires from_created_at".into())
        })?;
        let to_created_at = self.to_created_at.ok_or_else(|| {
            AppError::BadRequest("monitoring snapshot requires to_created_at".into())
        })?;
        let scope = match self.scope.as_deref() {
            Some("tenant") => {
                let tenant =
                    management_tenant(service, self.tenant_external_id)?.ok_or_else(|| {
                        AppError::BadRequest("tenant monitoring requires tenant_external_id".into())
                    })?;
                MonitoringScope::Tenant(tenant)
            }
            Some("global") => {
                if self
                    .tenant_external_id
                    .as_deref()
                    .is_some_and(|value| !value.trim().is_empty())
                {
                    return Err(AppError::BadRequest(
                        "global monitoring must not include tenant_external_id".into(),
                    ));
                }
                require_global_service(service)?;
                MonitoringScope::Global
            }
            Some(_) => {
                return Err(AppError::BadRequest(
                    "monitoring scope must be tenant or global".into(),
                ));
            }
            None => {
                return Err(AppError::BadRequest(
                    "monitoring snapshot requires scope=tenant or scope=global".into(),
                ));
            }
        };
        Ok((
            scope,
            MonitoringSnapshotFilter {
                from_created_at,
                to_created_at,
            },
        ))
    }
}

pub(super) async fn internal_monitoring_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<MonitoringSnapshotQuery>,
) -> Result<impl axum::response::IntoResponse, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let (scope, filter) = query.into_request(&service)?;
    let snapshot: OperatorMonitoringSnapshot =
        state.db.operator_monitoring_snapshot(scope, filter).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(snapshot)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_monitoring_requires_an_explicit_scope_and_window() {
        let service = crate::model::AuthenticatedService::bootstrap();
        let missing_scope = MonitoringSnapshotQuery {
            scope: None,
            tenant_external_id: None,
            from_created_at: Some(1),
            to_created_at: Some(2),
        };
        assert!(missing_scope.into_request(&service).is_err());
        let missing_window = MonitoringSnapshotQuery {
            scope: Some("global".to_owned()),
            tenant_external_id: None,
            from_created_at: None,
            to_created_at: Some(2),
        };
        assert!(missing_window.into_request(&service).is_err());
    }
}
