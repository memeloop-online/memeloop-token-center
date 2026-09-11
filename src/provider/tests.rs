use super::credential::{
    ENVELOPE_AAD, LEGACY_ENVELOPE_VERSION, MAX_ADAPTER_STATE_BYTES, MAX_ADAPTER_STATE_DEPTH,
    MAX_ADAPTER_STATE_NODES, authorization_header, bearer_prefix, current_encryption_key,
    legacy_encryption_key,
};
use super::*;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use chacha20poly1305::{
    ChaCha20Poly1305, KeyInit,
    aead::{Aead, Payload},
};
use serde_json::{Value, json};
use std::sync::Arc;

#[test]
fn credential_envelope_round_trips_without_plaintext() {
    let credential = UpstreamCredential::OAuth {
        access_token: "secret-access".to_owned(),
        refresh_token: Some("secret-refresh".to_owned()),
        expires_at: Some(42),
        header: authorization_header(),
        prefix: bearer_prefix(),
        adapter_state: None,
        proxy_url: None,
        proxy_network_scope: None,
    };
    let envelope = seal_credential(&credential, b"a key material with at least 32 bytes").unwrap();
    assert!(envelope.starts_with("v2."));
    assert!(!envelope.contains("secret"));
    let opened = open_credential(&envelope, b"a key material with at least 32 bytes").unwrap();
    assert_eq!(opened.auth_kind(), "oauth");
    assert_eq!(opened.expires_at(), Some(42));
}

#[test]
fn legacy_v1_envelopes_remain_readable_but_are_never_written() {
    let key_material = b"a key material with at least 32 bytes";
    let credential = UpstreamCredential::ApiKey {
        value: "legacy-secret".to_owned(),
        header: "authorization".to_owned(),
        prefix: "Bearer ".to_owned(),
    };
    let plaintext = serde_json::to_vec(&credential).unwrap();
    let cipher = ChaCha20Poly1305::new_from_slice(&legacy_encryption_key(key_material)).unwrap();
    let nonce = [7_u8; 12];
    let ciphertext = cipher
        .encrypt(
            (&nonce).into(),
            Payload {
                msg: &plaintext,
                aad: ENVELOPE_AAD,
            },
        )
        .unwrap();
    let envelope = format!(
        "{LEGACY_ENVELOPE_VERSION}.{}.{}",
        URL_SAFE_NO_PAD.encode(nonce),
        URL_SAFE_NO_PAD.encode(ciphertext)
    );

    let opened = open_credential(&envelope, key_material).unwrap();
    assert_eq!(opened.auth_kind(), "api_key");
    let rewritten = seal_credential(&opened, key_material).unwrap();
    assert!(rewritten.starts_with("v2."));
}

#[test]
fn hkdf_key_derivation_is_deterministic_and_separated_from_legacy() {
    let key_material = b"a key material with at least 32 bytes";
    let current = current_encryption_key(key_material).unwrap();
    assert_eq!(current, current_encryption_key(key_material).unwrap());
    assert_ne!(current, legacy_encryption_key(key_material));
    assert_ne!(
        current,
        current_encryption_key(b"a different key material with 32 bytes").unwrap()
    );
}

#[test]
fn envelope_aad_is_domain_separated_and_tamper_evident() {
    let key_material = b"a key material with at least 32 bytes";
    let envelope =
        seal_private_json(&json!({"secret": "value"}), key_material, b"domain-a").unwrap();
    assert!(
        open_private_json::<Value>(&envelope, key_material, b"domain-b").is_err(),
        "an envelope must not open under a different protocol domain"
    );
    let mut bytes = envelope.into_bytes();
    let index = bytes.len() - 1;
    bytes[index] = if bytes[index] == b'A' { b'B' } else { b'A' };
    let tampered = String::from_utf8(bytes).unwrap();
    assert!(
        open_private_json::<Value>(&tampered, key_material, b"domain-a").is_err(),
        "authenticated encryption must reject modified ciphertext"
    );
}

#[test]
fn unauthenticated_credential_is_valid() {
    let credential: UpstreamCredential = serde_json::from_value(json!({"type": "none"})).unwrap();
    assert_eq!(credential.auth_kind(), "none");
    credential.validate(42).unwrap();
}

#[test]
fn retired_credential_shapes_are_not_part_of_the_runtime_type() {
    assert!(
        serde_json::from_value::<UpstreamCredential>(json!({
            "type": "subscription_bridge",
            "handle": "historical"
        }))
        .is_err()
    );
}

#[test]
fn credential_debug_omits_custom_auth_metadata_and_secrets() {
    let api_key = UpstreamCredential::ApiKey {
        value: "MTC_CANARY_API_KEY_VALUE".to_owned(),
        header: "MTC-CANARY-API-HEADER".to_owned(),
        prefix: "MTC_CANARY_API_PREFIX ".to_owned(),
    };
    let oauth = UpstreamCredential::OAuth {
        access_token: "MTC_CANARY_OAUTH_ACCESS".to_owned(),
        refresh_token: Some("MTC_CANARY_OAUTH_REFRESH".to_owned()),
        expires_at: Some(42),
        header: "MTC-CANARY-OAUTH-HEADER".to_owned(),
        prefix: "MTC_CANARY_OAUTH_PREFIX ".to_owned(),
        adapter_state: Some(json!({"secret": "MTC_CANARY_ADAPTER_STATE"})),
        proxy_url: None,
        proxy_network_scope: None,
    };

    for (credential, kind, canaries) in [
        (
            api_key,
            "UpstreamCredential::ApiKey",
            &[
                "MTC_CANARY_API_KEY_VALUE",
                "MTC-CANARY-API-HEADER",
                "MTC_CANARY_API_PREFIX",
            ][..],
        ),
        (
            oauth,
            "UpstreamCredential::OAuth",
            &[
                "MTC_CANARY_OAUTH_ACCESS",
                "MTC_CANARY_OAUTH_REFRESH",
                "MTC-CANARY-OAUTH-HEADER",
                "MTC_CANARY_OAUTH_PREFIX",
                "MTC_CANARY_ADAPTER_STATE",
            ][..],
        ),
    ] {
        let rendered = format!("{credential:?}");
        assert!(rendered.contains(kind));
        for canary in canaries {
            assert!(!rendered.contains(canary), "Debug exposed {canary}");
        }
    }
}

#[test]
fn oauth_adapter_state_is_bounded_redacted_and_backward_compatible() {
    let legacy: UpstreamCredential = serde_json::from_value(json!({
        "type": "oauth",
        "access_token": "legacy-secret",
        "expires_at": 42
    }))
    .unwrap();
    assert!(legacy.adapter_state().is_none());
    assert!(!format!("{legacy:?}").contains("legacy-secret"));

    let valid: UpstreamCredential = serde_json::from_value(json!({
        "type": "oauth",
        "access_token": "access-secret",
        "adapter_state": {"refresh_family": ["state-secret"]}
    }))
    .unwrap();
    assert_eq!(
        valid.adapter_state().unwrap()["refresh_family"][0],
        "state-secret"
    );
    let rendered = format!("{valid:?}");
    assert!(!rendered.contains("access-secret"));
    assert!(!rendered.contains("state-secret"));

    let oversized = json!({
        "type": "oauth",
        "access_token": "access",
        "adapter_state": "x".repeat(MAX_ADAPTER_STATE_BYTES + 1)
    });
    assert!(serde_json::from_value::<UpstreamCredential>(oversized).is_err());

    let mut nested = json!(null);
    for _ in 0..=MAX_ADAPTER_STATE_DEPTH {
        nested = json!([nested]);
    }
    assert!(
        serde_json::from_value::<UpstreamCredential>(json!({
            "type": "oauth",
            "access_token": "access",
            "adapter_state": nested
        }))
        .is_err()
    );

    assert!(
        serde_json::from_value::<UpstreamCredential>(json!({
            "type": "oauth",
            "access_token": "access",
            "adapter_state": vec![0; MAX_ADAPTER_STATE_NODES + 1]
        }))
        .is_err()
    );
}

fn test_provider(id: &str) -> ProviderType {
    ProviderType {
        id: id.into(),
        display_name: id.into(),
        protocols: vec!["openai".into()],
        modalities: vec!["text".into()],
        config_schema: json!({"type": "object"}),
        credential_schema: json!({"type": "object"}),
        oauth_adapter: None,
        component_adapter: None,
        source: "test".into(),
    }
}

#[test]
fn cloned_provider_catalog_shares_frozen_schemas_and_extends_copy_on_write() {
    let mut catalog = ProviderCatalog::builtins();
    let cloned = catalog.clone();
    let builtin_count = catalog.list().len();
    assert!(Arc::ptr_eq(&catalog.types, &cloned.types));
    assert!(Arc::ptr_eq(
        &catalog.builtin_managed_oauth,
        &cloned.builtin_managed_oauth
    ));

    catalog
        .extend([test_provider("copy-on-write")])
        .expect("extend cloned catalog");

    assert!(!Arc::ptr_eq(&catalog.types, &cloned.types));
    assert_eq!(catalog.list().len(), builtin_count + 1);
    assert_eq!(cloned.list().len(), builtin_count);
    assert!(catalog.contains("copy-on-write"));
    assert!(!cloned.contains("copy-on-write"));
}

#[test]
fn http_json_provider_schema_accepts_exact_generation_result_origins() {
    let catalog = ProviderCatalog::builtins();
    let provider = catalog.get("http-json").expect("built-in provider");
    crate::schema::validate_instance(
        &provider.config_schema,
        &json!({
            "base_url": "https://provider.example/v1",
            "network_scope": "public",
            "result_origins": ["https://assets.provider.example"]
        }),
    )
    .expect("http-json image providers need an explicit asset-origin allowlist");
}

#[test]
fn builtin_cbcnx_exposes_only_verified_openai_text_embedding_and_image_contracts() {
    let catalog = ProviderCatalog::builtins();
    let cbcnx = catalog
        .get(CBCNX_PROVIDER_DRIVER)
        .expect("CBCNX is a built-in provider");
    assert_eq!(cbcnx.display_name, "广电（CBCNX）");
    assert_eq!(cbcnx.protocols, vec!["openai", "generation"]);
    assert_eq!(cbcnx.modalities, vec!["text", "embedding", "image"]);
    assert!(!cbcnx.modalities.iter().any(|modality| modality == "video"));
    assert!(catalog.supports_direct_credential(CBCNX_PROVIDER_DRIVER, "api_key"));
    crate::schema::validate_instance(
        &cbcnx.config_schema,
        &json!({
            "base_url": "https://cbcnx.example.test/v1",
            "stream_usage_contract": "openai-chat-usage-only",
            "input_token_overhead_ceiling": 512,
            "result_origins": ["https://assets.cbcnx.example.test"]
        }),
    )
    .expect("verified CBCNX text and image configuration");
    for invalid in [
        json!({"base_url": "https://cbcnx.example.test/v1"}),
        json!({
            "base_url": "https://cbcnx.example.test/v1",
            "stream_usage_contract": "none"
        }),
        json!({
            "base_url": "https://cbcnx.example.test/v1",
            "stream_usage_contract": "openai-chat-usage-only",
            "video_models": ["unverified-video-model"]
        }),
    ] {
        assert!(
            crate::schema::validate_instance(&cbcnx.config_schema, &invalid).is_err(),
            "CBCNX must reject unverified configuration: {invalid}"
        );
    }
}

#[test]
fn builtin_codex_routes_openai_with_required_trusted_limits_only() {
    let catalog = ProviderCatalog::builtins();
    let codex = catalog.get("openai-codex").unwrap();
    assert_eq!(codex.protocols, vec!["openai"]);
    assert_eq!(
        codex.config_schema.pointer("/properties/base_url/const"),
        Some(&json!("https://chatgpt.com/backend-api/codex"))
    );
    assert_eq!(
        codex
            .config_schema
            .pointer("/properties/reservation_token_bounds/additionalProperties/minimum"),
        Some(&json!(1))
    );
    assert!(
        codex
            .config_schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.contains(&json!("reservation_token_bounds")))
    );
    assert!(
        codex
            .config_schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.contains(&json!("network_scope")))
    );
    assert_eq!(
        codex
            .config_schema
            .pointer("/properties/transport_policy/properties/connect_attempts/maximum"),
        Some(&json!(4))
    );
    assert_eq!(
        codex
            .config_schema
            .pointer("/properties/transport_policy/properties/shared_probe_attempts/minimum"),
        Some(&json!(0))
    );
    assert!(
        catalog
            .get(crate::oauth::managed::kimi::PROVIDER_DRIVER)
            .unwrap()
            .config_schema
            .pointer("/properties/transport_policy")
            .is_none()
    );

    let public_ids = catalog
        .list()
        .iter()
        .map(|provider| provider.id.as_str())
        .collect::<Vec<_>>();
    assert!(public_ids.contains(&"openai-codex"));
    assert!(!public_ids.iter().any(|driver| driver.starts_with("cpa-")));
    assert!(catalog.get("cpa-codex-oauth").is_none());
    assert!(catalog.get("cpa-subscription-bridge").is_none());
    assert!(catalog.get("cpa-gemini-oauth-legacy").is_none());
    assert!(!catalog.supports_direct_credential("openai-codex", "oauth"));
    // The schema remains authoritative and rejects unsupported API-key shapes
    // before the authorization-flow guard is consulted.
    assert!(catalog.supports_direct_credential("openai-codex", "api_key"));

    assert!(
        catalog
            .managed_oauth_adapter_for_driver("openai-codex")
            .is_ok()
    );
}

#[test]
fn builtin_http_json_optionally_bounds_trusted_input_token_overhead() {
    let catalog = ProviderCatalog::builtins();
    let http_json = catalog.get("http-json").unwrap();
    let overhead = http_json
        .config_schema
        .pointer("/properties/input_token_overhead_ceiling")
        .unwrap();
    assert_eq!(overhead.get("minimum"), Some(&json!(0)));
    assert_eq!(overhead.get("maximum"), Some(&json!(1_000_000)));
    assert_eq!(overhead.get("default"), Some(&json!(0)));
    assert!(
        http_json
            .config_schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| !required.contains(&json!("input_token_overhead_ceiling")))
    );
    crate::schema::validate_instance(
        &http_json.config_schema,
        &json!({"base_url": "https://example.com"}),
    )
    .unwrap();
    crate::schema::validate_instance(
        &http_json.config_schema,
        &json!({
            "base_url": "https://example.com",
            "input_token_overhead_ceiling": 256
        }),
    )
    .unwrap();
    assert!(
        crate::schema::validate_instance(
            &http_json.config_schema,
            &json!({
                "base_url": "https://example.com",
                "input_token_overhead_ceiling": 1_000_001
            }),
        )
        .is_err()
    );
}

#[test]
fn builtin_http_json_exposes_only_the_fixed_siliconflow_video_profile() {
    let catalog = ProviderCatalog::builtins();
    let http_json = catalog.get("http-json").unwrap();
    let video_api = http_json
        .config_schema
        .pointer("/properties/video_api")
        .expect("video API profile schema");
    assert_eq!(video_api["enum"], json!(["siliconflow-v1"]));
    crate::schema::validate_instance(
        &http_json.config_schema,
        &json!({
            "base_url": "https://api.siliconflow.cn/v1",
            "video_api": "siliconflow-v1",
            "video_models": ["Wan-AI/Wan2.2-T2V-A14B"],
            "result_origins": ["https://s3.siliconflow.cn"]
        }),
    )
    .unwrap();
    // RJSF materializes an optional array field as an empty array while an
    // operator edits an ordinary HTTP account.  The driver-level validator
    // below still requires a non-empty list whenever video_api is enabled.
    crate::schema::validate_instance(
        &http_json.config_schema,
        &json!({
            "base_url": "https://example.com/v1",
            "video_models": [],
            "result_origins": []
        }),
    )
    .unwrap();
    assert!(
        crate::schema::validate_instance(
            &http_json.config_schema,
            &json!({
                "base_url": "https://api.siliconflow.cn/v1",
                "video_api": "/operator-chosen/path"
            }),
        )
        .is_err()
    );
}
