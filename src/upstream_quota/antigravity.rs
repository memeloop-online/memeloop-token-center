//! Read-only quota summary; no token refresh, onboarding, reset or model request.
//! Wire evidence: CPA's official management panel at c12997e1a544374336ea385d5e9a9bbabe1e4767,
//! src/utils/quota/{constants,builders}.ts: retrieveUserQuotaSummary groups/buckets.
use super::*;
use crate::provider::antigravity::Config;

const SUMMARY_PATH: &str = "/v1internal:retrieveUserQuotaSummary";
const MAX_GROUPS: usize = 32;
const MAX_WINDOWS: usize = 64;

pub(super) async fn read(
    state: &AppState,
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
    mut snapshot: QuotaSnapshot,
    trigger: QuotaReadTrigger,
    session: &retry::ReadSession<'_>,
) -> Result<QuotaSnapshot, &'static str> {
    if !matches!(credential, UpstreamCredential::OAuth { .. }) {
        return Err("credential_invalid");
    }
    credential
        .validate(unix_millis())
        .map_err(|_| "credential_invalid")?;
    let config = Config::from_account(&account.config).map_err(|_| "quota_destination_invalid")?;
    if config.project_id.trim().is_empty() {
        return Err("quota_project_required");
    }
    // Use the account's explicitly selected API origin and encrypted proxy.
    // Do not probe daily/sandbox/production alternatives or invent client headers.
    let endpoint = format!("{}{}", config.base_url.trim_end_matches('/'), SUMMARY_PATH);
    let http = crate::network::client_for_config_url_no_retry(
        &state.http,
        &endpoint,
        &json!({"network_scope":"public"}),
        credential.proxy(),
        state.config.allow_oauth_loopback,
    )
    .await
    .map_err(|_| "quota_destination_invalid")?;
    let payload = get_summary(
        &http,
        credential,
        &config,
        &endpoint,
        QuotaRequestContext::for_account(account, "antigravity_quota_summary", trigger),
        session,
    )
    .await?;
    snapshot.windows = session.validated("antigravity_quota_summary", windows(&payload))?;
    let now = unix_millis();
    snapshot.status = "ready";
    snapshot.freshness = "fresh";
    snapshot.observed_at = Some(now);
    snapshot.stale_after = Some(now + FRESH_MS);
    Ok(snapshot)
}

async fn get_summary(
    http: &reqwest::Client,
    credential: &UpstreamCredential,
    config: &Config,
    endpoint: &str,
    context: QuotaRequestContext,
    session: &retry::ReadSession<'_>,
) -> Result<Value, &'static str> {
    // retrieveUserQuotaSummary is a documented read RPC despite using POST;
    // no other Antigravity POST (onboarding, token or model) enters this scope.
    session
        .run(context, || async {
            let started = tokio::time::Instant::now();
            let request = http
                .post(endpoint)
                .header(reqwest::header::ACCEPT, "application/json")
                .timeout(session.budget.read)
                .json(&json!({"project": config.project_id}));
            let request = credential
                .apply(request, unix_millis())
                .map_err(|_| retry::Failure::terminal("credential_invalid", "credential"))?;
            let response = config
                .apply_headers(request)
                .map_err(|_| retry::Failure::terminal("quota_destination_invalid", "client"))?
                .send()
                .await
                .map_err(retry::Failure::reqwest)?;
            if let Some(failure) =
                retry::Failure::status(response.status().as_u16(), response.headers())
            {
                return Err(failure);
            }
            decode_response(response, context, started)
                .await
                .map_err(|code| retry::Failure::terminal(code, "body"))
        })
        .await
}

fn field<'a>(value: &'a Value, camel: &str, snake: &str) -> &'a Value {
    value
        .get(camel)
        .or_else(|| value.get(snake))
        .unwrap_or(&Value::Null)
}

fn label(value: &Value) -> Result<Option<String>, &'static str> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_str()
        .filter(|text| {
            !text.trim().is_empty() && text.len() <= 160 && !text.chars().any(char::is_control)
        })
        .map(|text| Some(text.to_owned()))
        .ok_or("quota_invalid_payload")
}

fn fraction(value: &Value) -> Result<Option<f64>, &'static str> {
    if value.is_null() {
        return Ok(None);
    }
    value
        .as_f64()
        .or_else(|| {
            // CPA builders passes remainingFraction through normalizeQuotaFraction.
            // Support its explicit percent notation, retaining our finite/range bounds.
            let text = value.as_str()?.trim();
            if text.len() > 160 {
                return None;
            }
            match text.strip_suffix('%') {
                Some(percent) => percent.trim().parse::<f64>().ok().map(|n| n / 100.0),
                None => text.parse().ok(),
            }
        })
        .filter(|number| number.is_finite() && (0.0..=1.0).contains(number))
        .map(Some)
        .ok_or("quota_invalid_payload")
}

fn windows(payload: &Value) -> Result<Vec<QuotaWindow>, &'static str> {
    let groups = payload
        .get("groups")
        .and_then(Value::as_array)
        .ok_or("quota_invalid_payload")?;
    if groups.len() > MAX_GROUPS {
        return Err("quota_too_many_windows");
    }
    let mut result = Vec::new();
    for (group_index, group) in groups.iter().enumerate() {
        let group_label = label(field(group, "displayName", "display_name"))?
            .unwrap_or_else(|| format!("Group {}", group_index + 1));
        let buckets = group
            .get("buckets")
            .and_then(Value::as_array)
            .ok_or("quota_invalid_payload")?;
        if result.len().saturating_add(buckets.len()) > MAX_WINDOWS {
            return Err("quota_too_many_windows");
        }
        for (bucket_index, bucket) in buckets.iter().enumerate() {
            if !bucket.is_object() {
                return Err("quota_invalid_payload");
            }
            let window = label(&bucket["window"])?;
            let bucket_id = label(field(bucket, "bucketId", "bucket_id"))?
                .or_else(|| window.clone())
                .unwrap_or_else(|| format!("bucket-{}", bucket_index + 1));
            // Length-prefix the group so identical bucket IDs in different groups
            // stay distinct without lossy slug normalization of supplier labels.
            let id = format!(
                "antigravity:{}:{group_label}:{bucket_id}",
                group_label.len()
            );
            if result.iter().any(|row: &QuotaWindow| row.id == id) {
                return Err("quota_duplicate_window");
            }
            let bucket_label = label(field(bucket, "displayName", "display_name"))?
                .unwrap_or_else(|| bucket_id.clone());
            let remaining = fraction(field(bucket, "remainingFraction", "remaining_fraction"))?;
            let period_seconds = match window
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref()
            {
                Some("5h" | "five-hour" | "five_hour") => Some(18_000),
                Some("weekly" | "week") => Some(604_800),
                _ => None,
            };
            result.push(QuotaWindow {
                id,
                label: format!("{group_label} · {bucket_label}"),
                used_percent: remaining.map(|remaining| (1.0 - remaining) * 100.0),
                used: None,
                remaining: None,
                limit: None,
                unit: None,
                reset_at: normalize::timestamp(field(bucket, "resetTime", "reset_time")),
                period_seconds,
                source: "antigravity_quota_summary",
                reset_is_estimated: false,
                allowed: None,
                limit_reached: None,
            });
        }
    }
    if result.is_empty() {
        return Err("quota_invalid_payload");
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn summary_preserves_explicit_windows_and_exact_reset_instants() {
        let rows = windows(&json!({"groups":[{"displayName":"Models", "buckets":[
            {"bucketId":"short", "window":"5h", "remainingFraction":0.25,
             "resetTime":"1970-01-01T00:00:01.123Z"},
            {"bucket_id":"long", "window":"weekly", "remaining_fraction":"0.5",
             "reset_time":"1970-01-01T01:00:01.123+01:00"},
            {"bucketId":"unknown", "window":"future-window", "resetTime":"invalid"}
        ]}]}))
        .unwrap();
        assert_eq!(rows[0].used_percent, Some(75.0));
        assert_eq!(rows[1].used_percent, Some(50.0));
        assert_eq!(rows[0].period_seconds, Some(18_000));
        assert_eq!(rows[1].period_seconds, Some(604_800));
        assert_eq!(rows[0].reset_at, Some(1123));
        assert_eq!(rows[1].reset_at, Some(1123));
        assert!(rows[2].used_percent.is_none());
        assert!(rows[2].period_seconds.is_none() && rows[2].reset_at.is_none());
        assert!(rows.iter().all(|row| row.used.is_none()
            && row.remaining.is_none()
            && row.limit.is_none()
            && row.unit.is_none()
            && row.allowed.is_none()
            && row.limit_reached.is_none()
            && !row.reset_is_estimated));
        let capabilities = QuotaCapabilities::for_provider("google-antigravity");
        assert!(
            capabilities.read && capabilities.window_percent && capabilities.supplier_read_only
        );
        assert!(
            !capabilities.window_amounts
                && !capabilities.reset_credit_expiry
                && !capabilities.refreshes_credentials
                && !capabilities.consumes_reset_credit
        );
    }

    #[test]
    fn group_ids_are_distinct_and_stable_without_inferring_cadence_from_reset() {
        let group = |name| {
            json!({"display_name":name, "buckets":[{
                "bucketId":"shared", "resetTime":"2026-09-14T00:00:00Z"
            }]})
        };
        let a = windows(&json!({"groups":[group("A"),group("B")]})).unwrap();
        let b = windows(&json!({"groups":[group("B"),group("A")]})).unwrap();
        assert_ne!(a[0].id, a[1].id);
        assert_eq!(a[0].id, b[1].id);
        assert!(a.iter().all(|row| row.period_seconds.is_none()));
        assert!(matches!(
            windows(&json!({"groups":[group("A"),group("A")]})),
            Err("quota_duplicate_window")
        ));
    }

    #[test]
    fn supplier_percentage_strings_are_bounded_and_normalized() {
        for (input, used) in [
            ("0%", 100.0),
            ("25%", 75.0),
            (" 50 % ", 50.0),
            ("100%", 0.0),
        ] {
            let rows = windows(&json!({"groups":[{"buckets":[{
                "remainingFraction":input
            }]}]}))
            .unwrap();
            assert_eq!(rows[0].used_percent, Some(used));
        }
        for input in ["-1%", "101%", "NaN%", "Infinity%", "%", "25%%", "bad%"] {
            assert_eq!(
                fraction(&json!(input)).unwrap_err(),
                "quota_invalid_payload"
            );
        }
        assert!(fraction(&json!(format!("{}%", "0".repeat(160)))).is_err());
    }

    #[test]
    fn malformed_and_unbounded_summaries_fail_closed() {
        for fraction in [
            json!(-0.1),
            json!(1.1),
            json!("NaN"),
            json!("Infinity"),
            json!(true),
        ] {
            assert!(
                windows(&json!({"groups":[{"buckets":[{"remainingFraction":fraction}]}]})).is_err()
            );
        }
        for payload in [
            json!({}),
            json!({"groups":[]}),
            json!({"groups":[{"buckets":[]}]}),
            json!({"groups":[{"buckets":[null]}]}),
            json!({"groups":[{"displayName":"bad\nlabel", "buckets":[{}]}]}),
            json!({"groups":vec![json!({"buckets":[{}]});33]}),
            json!({"groups":[{"buckets":vec![json!({});65]}]}),
        ] {
            assert!(windows(&payload).is_err());
        }
    }

    #[test]
    fn stale_quota_never_enables_reset_or_conflates_token_expiry() {
        let account: UpstreamAccountView = serde_json::from_value(json!({
            "id":Uuid::from_u128(1), "tenant_id":Uuid::from_u128(2), "name":"fixture",
            "driver":"google-antigravity", "auth_kind":"oauth", "connection_method":"native_oauth",
            "credential_generation":1, "status":"active", "config":{}, "can_refresh":true,
            "can_rotate":false, "can_reauthorize":false, "route_count":0, "created_at":0, "updated_at":10
        })).unwrap();
        let empty = QuotaSnapshot::empty(&account, "fixture-tenant", Some("quota_not_authorized"));
        let mut previous = empty.clone();
        previous.observed_at = Some(1000);
        previous.status = "ready";
        previous.windows = windows(&json!({"groups":[{"buckets":[{
            "remainingFraction":0.5,"resetTime":"1970-01-01T00:01:00Z"
        }]}]}))
        .unwrap();
        let stale = stale_or_error(Some(previous), empty, 1001);
        assert_eq!(stale.freshness, "stale");
        assert_eq!(stale.windows[0].reset_at, Some(60_000));
        assert_eq!(stale.reset_capability.provider_supported, None);
        assert!(!stale.reset_capability.implementation_available);
        assert!(!stale.reset_capability.prepare_available);
        assert_eq!(stale.reset_capability.reason, "quota_reset_not_supported");
        assert!(stale.subscription_active_until.is_none() && stale.reset_credits.is_empty());
    }

    #[tokio::test]
    async fn summary_transport_retries_only_the_read_rpc_with_explicit_retry_after() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::{
            Mock, MockServer, ResponseTemplate,
            matchers::{body_json, header, method, path},
        };
        let credential = UpstreamCredential::OAuth {
            access_token: "fixture-token".into(),
            refresh_token: None,
            expires_at: None,
            header: "authorization".into(),
            prefix: "Bearer ".into(),
            adapter_state: None,
            proxy_url: None,
            proxy_network_scope: None,
        };
        let http = crate::build_no_retry_http_client(None, &[]).unwrap();
        for (status, expected) in [
            (200, None),
            (401, Some("quota_not_authorized")),
            (403, Some("quota_not_authorized")),
            (429, Some("quota_rate_limited")),
            (500, Some("quota_upstream_error")),
            (503, None),
        ] {
            let server = MockServer::start().await;
            let payload = json!({"groups":[{"buckets":[{"remainingFraction":1.0}]}]});
            let response_payload = payload.clone();
            let count = AtomicUsize::new(0);
            let expected_count = if status == 503 { 3 } else { 1 };
            Mock::given(method("POST"))
                .and(path(SUMMARY_PATH))
                .and(header("authorization", "Bearer fixture-token"))
                .and(body_json(json!({"project":"fixture-project"})))
                .respond_with(move |_: &wiremock::Request| {
                    let attempt = count.fetch_add(1, Ordering::SeqCst);
                    if status == 200 || (status == 503 && attempt == 2) {
                        ResponseTemplate::new(200).set_body_json(&response_payload)
                    } else if status == 503 {
                        ResponseTemplate::new(503).insert_header("Retry-After", "0")
                    } else {
                        ResponseTemplate::new(status).set_body_string("supplier-secret")
                    }
                })
                .expect(expected_count)
                .mount(&server)
                .await;
            let config = Config {
                base_url: server.uri(),
                project_id: "fixture-project".into(),
                ..Config::default()
            };
            let result = get_summary(
                &http,
                &credential,
                &config,
                &format!("{}{}", server.uri(), SUMMARY_PATH),
                QuotaRequestContext {
                    account_id: Uuid::from_u128(1),
                    credential_generation: 2,
                    endpoint_kind: "antigravity_quota_summary",
                    trigger: QuotaReadTrigger::Manual,
                },
                &retry::ReadSession::testing(codex_quota_budget(&json!({})).unwrap()),
            )
            .await;
            if let Some(expected) = expected {
                assert_eq!(result.unwrap_err(), expected);
            } else {
                assert_eq!(result.unwrap(), payload);
            }
            let requests = server.received_requests().await.unwrap();
            assert_eq!(requests.len(), expected_count as usize);
            assert!(
                requests
                    .iter()
                    .all(|request| request.url.path() == SUMMARY_PATH)
            );
            assert!(requests[0].headers.get("x-goog-api-client").is_none());
        }
    }
}
