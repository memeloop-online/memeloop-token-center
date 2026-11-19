//! Legacy bound locators use their original bytes without a canonical raw root.
use super::*;
use bytes::Bytes;
use memeloop_token_center::db::{FinishRequest, NewRequest};
const REQUEST: &[u8] = br#"{"input":"legacy question","stream":false}"#;
const RESPONSE: &[u8] = br#"{"object":"response","status":"incomplete","output":[],"incomplete_details":{"reason":"max_output_tokens"}}"#;

#[given("an authorized legacy bound request has a compressed response and no raw root")]
async fn legacy_bound_without_root(world: &mut TokenCenterWorld) {
    stop_test_worker(world).await;
    let state = world.state.as_ref().unwrap();
    let key = state
        .db
        .authenticate_key(&world.current_key, state.config.key_pepper.as_bytes())
        .await
        .unwrap();
    let id = Uuid::now_v7();
    let request_path = format!("staging/proxy/{id}/request/{id}/body.json");
    let response_base = format!("staging/proxy/{id}/response/{id}/body");
    state
        .archive
        .put(&request_path, Bytes::from_static(REQUEST))
        .await
        .unwrap();
    let mut writer = state
        .archive
        .start_compressed_writer(&response_base)
        .await
        .unwrap();
    writer.write(Bytes::from_static(RESPONSE)).await.unwrap();
    let response_path = writer.finish_staged().await.unwrap().object_locator;
    assert!(response_path.ends_with(".mtcz1"));
    state
        .db
        .record_request_started(NewRequest {
            request_id: id,
            key_id: key.key_id,
            tenant_id: key.tenant_id,
            protocol: "openai-responses".into(),
            model: "gpt-test".into(),
            request_object: request_path,
            reservation_id: Uuid::now_v7(),
            upstream_account_id: None,
            model_route_id: None,
        })
        .await
        .unwrap();
    state
        .db
        .record_request_finished(FinishRequest {
            request_id: id,
            status_code: 200,
            duration_ms: 40,
            input_tokens: 7,
            output_tokens: 3,
            cached_input_tokens: 0,
            cache_write_tokens: 0,
            first_output_ms: None,
            generation_duration_ms: None,
            service_tier: None,
            cost_micros: 0,
            error_code: None,
            response_object: response_path,
        })
        .await
        .unwrap();
    for side in ["request", "response"] {
        assert!(
            state
                .archive
                .raw_capture_reference(key.tenant_id, id, side)
                .await
                .unwrap()
                .is_none()
        );
    }
    world.synchronous_request_id = Some(id);
}

#[then("the customer archive API returns both legacy originals byte for byte")]
async fn read_legacy_originals(world: &mut TokenCenterWorld) {
    let id = world.synchronous_request_id.unwrap();
    for (side, expected) in [("request", REQUEST), ("response", RESPONSE)] {
        let response = world
            .client
            .get(format!(
                "{}/self/v1/requests/{id}/archive/{side}",
                world.service_url
            ))
            .bearer_auth(&world.current_key)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let actual = response.bytes().await.unwrap();
        assert_eq!(actual.len(), expected.len());
        assert_eq!(blake3::hash(&actual), blake3::hash(expected));
        assert_eq!(actual.as_ref(), expected);
        if side == "response" {
            let body: Value = serde_json::from_slice(&actual).unwrap();
            assert_eq!(body["status"], "incomplete");
            assert_eq!(body["incomplete_details"]["reason"], "max_output_tokens");
        }
    }
}
