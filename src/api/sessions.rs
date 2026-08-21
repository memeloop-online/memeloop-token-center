use axum::{
    Json,
    extract::{Path, Query, State},
    http::HeaderMap,
};
use serde::Deserialize;
use uuid::Uuid;

use super::{authenticate_downstream, management_tenant, require_service};
use crate::{
    AppState,
    db::{ConversationDetailFilter, LogicalSessionListFilter},
    error::AppError,
    model::{LogicalSessionDetail, LogicalSessionListCursor, LogicalSessionListResponse},
};

#[derive(Debug, Deserialize)]
pub(super) struct RecentSessionsQuery {
    tenant_external_id: Option<String>,
    #[serde(default = "default_session_list_limit")]
    limit: i64,
    before_last_activity_at: Option<i64>,
    before_session_id: Option<String>,
    key_id: Option<Uuid>,
    state: Option<String>,
    model: Option<String>,
    q: Option<String>,
}

impl RecentSessionsQuery {
    fn cursor(&self) -> Result<Option<(i64, String)>, AppError> {
        match (&self.before_last_activity_at, &self.before_session_id) {
            (None, None) => Ok(None),
            (Some(last_activity_at), Some(session_id)) => {
                validate_session_id(session_id)?;
                Ok(Some((*last_activity_at, session_id.clone())))
            }
            _ => Err(AppError::BadRequest(
                "before_last_activity_at and before_session_id must be supplied together".into(),
            )),
        }
    }

    fn list_filter(&self) -> Result<LogicalSessionListFilter, AppError> {
        let state = self.state.as_deref().unwrap_or("all");
        if !matches!(state, "all" | "active" | "has_errors") {
            return Err(AppError::BadRequest(
                "state must be one of all, active, or has_errors".into(),
            ));
        }
        for (name, value, max_len) in [
            ("model", self.model.as_deref(), 512usize),
            ("q", self.q.as_deref(), 128usize),
        ] {
            if value.is_some_and(|value| {
                value.trim().is_empty()
                    || value.len() > max_len
                    || value.chars().any(char::is_control)
            }) {
                return Err(AppError::BadRequest(format!(
                    "{name} must contain 1 to {max_len} non-control characters"
                )));
            }
        }
        Ok(LogicalSessionListFilter {
            limit: self.limit,
            cursor: self.cursor()?,
            key_id: self.key_id,
            state: state.to_owned(),
            model: self.model.clone(),
            query: self.q.clone(),
        })
    }
}

#[derive(Debug, Deserialize)]
pub(super) struct SessionDetailQuery {
    tenant_external_id: Option<String>,
    key_id: Option<Uuid>,
    #[serde(default = "default_session_detail_limit")]
    limit: i64,
    before_created_at: Option<i64>,
    before_request_id: Option<Uuid>,
}

impl SessionDetailQuery {
    fn detail_filter(&self) -> Result<ConversationDetailFilter, AppError> {
        if self.before_created_at.is_some() != self.before_request_id.is_some() {
            return Err(AppError::BadRequest(
                "before_created_at and before_request_id must be supplied together".into(),
            ));
        }
        Ok(ConversationDetailFilter {
            limit: self.limit,
            before_created_at: self.before_created_at,
            before_request_id: self.before_request_id,
        })
    }
}

fn default_session_list_limit() -> i64 {
    50
}

fn default_session_detail_limit() -> i64 {
    100
}

fn validate_session_id(session_id: &str) -> Result<(), AppError> {
    if session_id.is_empty() || session_id.len() > 80 || session_id.chars().any(char::is_control) {
        return Err(AppError::NotFound);
    }
    Ok(())
}

pub(super) async fn internal_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RecentSessionsQuery>,
) -> Result<Json<LogicalSessionListResponse>, AppError> {
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let filter = query.list_filter()?;
    Ok(Json(session_list_response(
        state.db.operator_recent_sessions(&tenant, filter).await?,
        query.limit,
    )))
}

pub(super) async fn internal_session_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(query): Query<SessionDetailQuery>,
) -> Result<Json<LogicalSessionDetail>, AppError> {
    validate_session_id(&session_id)?;
    let service = require_service(&headers, &state, "requests:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id.clone())?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let key_id = query
        .key_id
        .ok_or_else(|| AppError::BadRequest("key_id is required".into()))?;
    Ok(Json(
        state
            .db
            .operator_logical_session_detail(&tenant, key_id, &session_id, query.detail_filter()?)
            .await?,
    ))
}

pub(super) async fn self_sessions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<RecentSessionsQuery>,
) -> Result<Json<LogicalSessionListResponse>, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    let mut filter = query.list_filter()?;
    filter.key_id = Some(key.key_id);
    Ok(Json(session_list_response(
        state.db.self_recent_sessions(key.tenant_id, filter).await?,
        query.limit,
    )))
}

fn session_list_response(
    mut sessions: Vec<crate::model::LogicalSessionSummary>,
    requested_limit: i64,
) -> LogicalSessionListResponse {
    let limit = requested_limit.clamp(1, 100) as usize;
    let has_more = sessions.len() > limit;
    if has_more {
        sessions.truncate(limit);
    }
    let next_cursor = has_more.then(|| {
        let oldest = sessions
            .last()
            .expect("a page with another session has at least one visible session");
        LogicalSessionListCursor {
            before_last_activity_at: oldest.last_activity_at,
            before_session_id: oldest.session_id.clone(),
        }
    });
    LogicalSessionListResponse {
        generated_at: crate::db::unix_millis(),
        sessions,
        next_cursor,
    }
}

pub(super) async fn self_session_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(query): Query<SessionDetailQuery>,
) -> Result<Json<LogicalSessionDetail>, AppError> {
    validate_session_id(&session_id)?;
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .logical_session_detail(
                key.tenant_id,
                key.key_id,
                &session_id,
                query.detail_filter()?,
            )
            .await?,
    ))
}
