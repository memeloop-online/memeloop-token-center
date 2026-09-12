#[path = "account_settlements/helpers.rs"]
mod helpers;
#[path = "account_settlements/support.rs"]
mod support;
use axum::http::{StatusCode, header};
use helpers::{assert_sanitized, get_json};
use support::Fixture;

#[tokio::test]
async fn sqlite_account_settlement_feed_is_scoped_paged_and_exact() {
    let fixture = Fixture::new().await;
    let target_path = format!(
        "/internal/v1/accounts/{}/settlements",
        fixture.target.account_id
    );
    let other_path = format!(
        "/internal/v1/accounts/{}/settlements",
        fixture.other.account_id
    );

    let (status, headers, first) = get_json(
        &fixture.state,
        &format!("{target_path}?limit=1"),
        &fixture.target_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(
        headers
            .get(header::CACHE_CONTROL)
            .and_then(|value| value.to_str().ok()),
        Some("no-store")
    );
    let first_item = &first["items"][0];
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    assert_eq!(first_item["settlement_sequence"], 1);
    assert_eq!(
        first_item["request_id"],
        fixture.text_request_id.to_string()
    );
    assert_eq!(first_item["kind"], "text");
    assert_eq!(
        first_item["account_id"],
        fixture.target.account_id.to_string()
    );
    assert_eq!(first_item["key_id"], fixture.target.key_id.to_string());
    assert_eq!(first_item["cost"], "3");
    assert_eq!(first_item["input_tokens"], 2);
    assert_eq!(first_item["cached_input_tokens"], 0);
    assert_eq!(first_item["cache_write_tokens"], 0);
    assert_eq!(first_item["output_tokens"], 1);
    assert!(first["next_cursor"].is_object());
    assert_sanitized(&first);

    let after_sequence = first["next_cursor"]["after_sequence"]
        .as_i64()
        .expect("sequence cursor");
    let after_id = first["next_cursor"]["after_id"]
        .as_str()
        .expect("settlement cursor id");
    assert_eq!(after_sequence, 1);
    assert_eq!(
        after_id,
        first_item["settlement_id"]
            .as_str()
            .expect("settlement id in item")
    );
    let (status, _, second) = get_json(
        &fixture.state,
        &format!("{target_path}?limit=1&after_sequence={after_sequence}&after_id={after_id}"),
        &fixture.target_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{second}");
    let second_item = &second["items"][0];
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second_item["settlement_sequence"], 2);
    assert_eq!(
        second_item["request_id"],
        fixture.generation_request_id.to_string()
    );
    assert_eq!(second_item["kind"], "generation");
    assert_eq!(second_item["cost"], "2");
    for field in [
        "input_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "output_tokens",
    ] {
        assert!(second_item[field].is_null(), "generation field {field}");
    }
    assert!(second["next_cursor"].is_null());
    assert_sanitized(&second);

    // Text requests and generation jobs use separate stores, so their UUIDs can
    // overlap. Exact lookups must retain the kind discriminator.
    assert_eq!(fixture.text_request_id, fixture.generation_request_id);
    for (request_kind, request_id) in [
        ("text", fixture.text_request_id),
        ("generation", fixture.generation_request_id),
    ] {
        let (status, headers, exact) = get_json(
            &fixture.state,
            &format!("{target_path}?request_kind={request_kind}&request_id={request_id}"),
            &fixture.target_token,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{exact}");
        assert_eq!(
            headers
                .get(header::CACHE_CONTROL)
                .and_then(|value| value.to_str().ok()),
            Some("no-store")
        );
        assert_eq!(exact["items"].as_array().unwrap().len(), 1);
        assert_eq!(exact["items"][0]["request_id"], request_id.to_string());
        assert_eq!(exact["items"][0]["kind"], request_kind);
        assert!(exact["next_cursor"].is_null());
        assert_sanitized(&exact);
    }

    let (status, _, cross_account_exact) = get_json(
        &fixture.state,
        &format!(
            "{target_path}?request_kind=text&request_id={}",
            fixture.other_text_request_id
        ),
        &fixture.target_token,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{cross_account_exact}");
    assert!(
        cross_account_exact["items"]
            .as_array()
            .expect("items array")
            .is_empty()
    );

    let (status, _, other_rows) = get_json(&fixture.state, &other_path, &fixture.other_token).await;
    assert_eq!(status, StatusCode::OK, "{other_rows}");
    assert_eq!(other_rows["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        other_rows["items"][0]["request_id"],
        fixture.other_text_request_id.to_string()
    );
    assert_sanitized(&other_rows);

    for token in [&fixture.credits_only_token, &fixture.requests_only_token] {
        let (status, _, body) = get_json(&fixture.state, &target_path, token).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }
    let (status, _, body) = get_json(&fixture.state, &other_path, &fixture.target_token).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

    let invalid_paths = [
        format!("{target_path}?limit=0"),
        format!("{target_path}?limit=501"),
        format!("{target_path}?after_sequence=1"),
        format!("{target_path}?after_id={after_id}"),
        format!("{target_path}?after_sequence=-1&after_id={after_id}"),
        format!(
            "{target_path}?request_kind=text&request_id={}&after_sequence=0&after_id={after_id}",
            fixture.text_request_id
        ),
        format!("{target_path}?request_id={}", fixture.text_request_id),
        format!("{target_path}?request_kind=text"),
    ];
    for path in invalid_paths {
        let (status, _, body) = get_json(&fixture.state, &path, &fixture.target_token).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}: {body}");
    }
}
