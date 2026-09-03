use super::super::*;

pub(in crate::api) fn gateway_router(state: AppState) -> Router<AppState> {
    let authenticated = Router::new()
        .route("/self/v1/key", get(self_key))
        .route("/self/v1/key/limits", get(self_key_limits))
        .route(
            "/self/v1/entitlements",
            get(self_memeloop_cloud_entitlements),
        )
        .route("/self/v1/requests", get(self_requests))
        .route("/self/v1/requests/{request_id}", get(self_request_detail))
        .route(
            "/self/v1/requests/{request_id}/assets/{asset_id}",
            get(self_request_asset),
        )
        .route("/self/v1/stats", get(self_stats))
        .route("/self/v1/usage-analysis", get(self_usage_analysis))
        .route("/self/v1/generations", get(self_generations))
        .route(
            "/self/v1/generations/{job_id}",
            get(self_generation).delete(cancel_self_generation),
        )
        .route(
            "/self/v1/generations/{job_id}/assets/{asset_id}",
            get(self_generation_asset),
        )
        .route("/self/v1/conversations", get(self_conversations))
        .route(
            "/self/v1/conversations/{cluster_id}",
            get(self_conversation_detail),
        )
        .route("/self/v1/sessions", get(self_sessions))
        .route("/self/v1/sessions/{session_id}", get(self_session_detail))
        .route(
            "/v1/responses",
            post(proxy_openai_responses).layer(DefaultBodyLimit::max(MAX_RESPONSES_REQUEST_BODY)),
        )
        .route("/v1/models", get(list_models))
        .route("/v1/chat/completions", post(proxy_openai_chat))
        .route("/v1/embeddings", post(proxy_openai_embeddings))
        .route("/v1/generations", post(create_generation))
        .route("/v1/videos/generations", post(create_video_generation))
        .route(
            "/v1/images/generations",
            post(create_image_generation).layer(DefaultBodyLimit::max(MAX_IMAGE_REQUEST_BODY)),
        )
        .route("/v1/messages", post(proxy_anthropic))
        .route(
            "/v1/messages/count_tokens",
            post(proxy_anthropic_count_tokens),
        )
        .route_layer(middleware::from_fn_with_state(
            state,
            authenticate_gateway_before_body,
        ));
    Router::new()
        .route("/portal", get(portal_index))
        .merge(authenticated)
}
