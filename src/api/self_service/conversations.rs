use super::super::*;

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ConversationListQuery {
    #[serde(default = "default_conversation_list_limit")]
    limit: i64,
    before_updated_at: Option<i64>,
    before_cluster_id: Option<Uuid>,
}

impl ConversationListQuery {
    fn to_filter(&self) -> Result<crate::db::ConversationListFilter, AppError> {
        if self.before_updated_at.is_some() != self.before_cluster_id.is_some() {
            return Err(AppError::BadRequest(
                "before_updated_at and before_cluster_id must be supplied together".into(),
            ));
        }
        Ok(crate::db::ConversationListFilter {
            limit: self.limit,
            before_updated_at: self.before_updated_at,
            before_cluster_id: self.before_cluster_id,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(in crate::api) struct ConversationDetailQuery {
    #[serde(default = "default_conversation_detail_limit")]
    limit: i64,
    before_created_at: Option<i64>,
    before_request_id: Option<Uuid>,
}

impl ConversationDetailQuery {
    fn to_filter(&self) -> Result<crate::db::ConversationDetailFilter, AppError> {
        if self.before_created_at.is_some() != self.before_request_id.is_some() {
            return Err(AppError::BadRequest(
                "before_created_at and before_request_id must be supplied together".into(),
            ));
        }
        Ok(crate::db::ConversationDetailFilter {
            limit: self.limit,
            before_created_at: self.before_created_at,
            before_request_id: self.before_request_id,
        })
    }
}

fn default_conversation_list_limit() -> i64 {
    50
}

fn default_conversation_detail_limit() -> i64 {
    100
}

pub(in crate::api) async fn self_conversations(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ConversationListQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .conversation_clusters(key.key_id, query.to_filter()?)
            .await?,
    ))
}

pub(in crate::api) async fn self_conversation_detail(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(cluster_id): Path<Uuid>,
    Query(query): Query<ConversationDetailQuery>,
) -> Result<impl IntoResponse, AppError> {
    let key = authenticate_downstream(&headers, &state).await?;
    Ok(Json(
        state
            .db
            .conversation_cluster_detail(key.key_id, cluster_id, query.to_filter()?)
            .await?,
    ))
}
