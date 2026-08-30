use super::*;
use crate::api::tests::test_state;

fn request_detail_refs(request_id: Uuid) -> crate::model::RequestArchiveRefs {
    crate::model::RequestArchiveRefs {
        view: crate::model::RequestView {
            request_id,
            created_at: 1,
            protocol: "openai".to_owned(),
            model: "request-detail-test".to_owned(),
            status_code: Some(200),
            duration_ms: Some(1),
            input_tokens: 1,
            output_tokens: 1,
            cost: "0".to_owned(),
            error_code: None,
            session_context: None,
        },
        request_object: "inline-json:{\"prompt\":\"detail body\"}".to_owned(),
        response_object: None,
        response_json: Some(json!({"output": "detail body"})),
        provenance: None,
    }
}

#[tokio::test]
async fn request_detail_response_has_exact_content_length_and_bounded_json_body() {
    let (state, _directory) = test_state().await;
    let response = request_detail_response(&state, request_detail_refs(Uuid::now_v7()))
        .await
        .expect("request detail response");
    let content_length = response.headers()[header::CONTENT_LENGTH]
        .to_str()
        .expect("ASCII Content-Length")
        .parse::<usize>()
        .expect("numeric Content-Length");
    let body = axum::body::to_bytes(response.into_body(), MAX_ARCHIVE_DETAIL_RESPONSE)
        .await
        .expect("bounded request detail body");
    assert_eq!(body.len(), content_length);
    let detail: Value = serde_json::from_slice(&body).expect("request detail JSON body");
    assert_eq!(detail["request_body"]["prompt"], "detail body");
    assert_eq!(detail["response_body"]["output"], "detail body");
}
