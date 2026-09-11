use super::*;

fn field<'a>(value: &'a Value, snake: &str, camel: &str) -> &'a Value {
    value
        .get(snake)
        .or_else(|| value.get(camel))
        .unwrap_or(&Value::Null)
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.parse().ok())
        .filter(|value| value.is_finite() && *value >= 0.0)
}

fn integer(value: &Value) -> Option<i64> {
    number(value)
        .filter(|value| value.fract() == 0.0 && *value < i64::MAX as f64)
        .map(|value| value as i64)
}

fn text(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|text| {
            !text.is_empty()
                && text.len() <= 160
                && text.chars().all(|character| !character.is_control())
        })
        .map(str::to_owned)
}

pub(super) fn usage(
    snapshot: &mut QuotaSnapshot,
    usage: &Value,
    now: i64,
) -> Result<(), &'static str> {
    if !usage.is_object()
        || ![
            "rate_limit",
            "rateLimit",
            "code_review_rate_limit",
            "codeReviewRateLimit",
            "additional_rate_limits",
            "additionalRateLimits",
            "credits",
            "plan_type",
            "planType",
        ]
        .iter()
        .any(|key| usage.get(key).is_some())
    {
        return Err("quota_invalid_payload");
    }
    snapshot.plan_type = text(field(usage, "plan_type", "planType"));
    add_windows(
        snapshot,
        "code",
        field(usage, "rate_limit", "rateLimit"),
        now,
    )?;
    add_windows(
        snapshot,
        "code_review",
        field(usage, "code_review_rate_limit", "codeReviewRateLimit"),
        now,
    )?;
    if let Some(additional) =
        field(usage, "additional_rate_limits", "additionalRateLimits").as_array()
    {
        if additional.len() > 32 {
            return Err("quota_too_many_windows");
        }
        for (index, feature) in additional.iter().enumerate() {
            let name = text(field(feature, "metered_feature", "meteredFeature"))
                .or_else(|| text(field(feature, "limit_name", "limitName")))
                .unwrap_or_else(|| format!("additional_{index}"));
            add_windows(
                snapshot,
                &name,
                field(feature, "rate_limit", "rateLimit"),
                now,
            )?;
        }
    }
    let credits = &usage["credits"];
    // Preserve the supplier's decimal text without a floating-point roundtrip.
    snapshot.credits.balance = match &credits["balance"] {
        Value::String(value)
            if value.len() <= 80 && value.parse::<rust_decimal::Decimal>().is_ok() =>
        {
            Some(value.clone())
        }
        Value::Number(value) => Some(value.to_string()),
        _ => None,
    };
    snapshot.credits.unlimited = credits["unlimited"].as_bool();
    snapshot.credits.has_credits = field(credits, "has_credits", "hasCredits").as_bool();
    snapshot.reset_capability.available_credits = integer(field(
        field(usage, "rate_limit_reset_credits", "rateLimitResetCredits"),
        "available_count",
        "availableCount",
    ));
    Ok(())
}

fn add_windows(
    snapshot: &mut QuotaSnapshot,
    scope: &str,
    rate: &Value,
    now: i64,
) -> Result<(), &'static str> {
    for (snake, camel) in [
        ("primary_window", "primaryWindow"),
        ("secondary_window", "secondaryWindow"),
    ] {
        let window = field(rate, snake, camel);
        if window.is_null() {
            continue;
        }
        if !window.is_object() {
            return Err("quota_invalid_payload");
        }
        let absolute =
            integer(field(window, "reset_at", "resetAt")).and_then(|value| value.checked_mul(1000));
        let relative = integer(field(window, "reset_after_seconds", "resetAfterSeconds"))
            .and_then(|value| value.checked_mul(1000))
            .and_then(|value| now.checked_add(value));
        let id = format!("{scope}:{snake}");
        if snapshot.windows.iter().any(|window| window.id == id) {
            return Err("quota_duplicate_window");
        }
        snapshot.windows.push(QuotaWindow {
            id: id.clone(),
            label: id,
            used_percent: number(field(window, "used_percent", "usedPercent")),
            remaining: None,
            limit: None,
            reset_at: absolute.or(relative),
            period_seconds: integer(field(window, "limit_window_seconds", "limitWindowSeconds")),
            source: "codex_usage",
            reset_is_estimated: absolute.is_none() && relative.is_some(),
            allowed: rate["allowed"].as_bool(),
            limit_reached: field(rate, "limit_reached", "limitReached").as_bool(),
        });
    }
    Ok(())
}

pub(super) fn reset_credits(
    snapshot: &mut QuotaSnapshot,
    reset: &Value,
    now: i64,
) -> Result<(), &'static str> {
    let count = integer(field(reset, "available_count", "availableCount"));
    let credits = reset["credits"].as_array();
    if count.is_none() && credits.is_none() {
        return Err("quota_invalid_credit_payload");
    }
    if let Some(count) = count {
        snapshot.reset_capability.available_credits = Some(count);
    }
    if let Some(credits) = credits {
        if credits.len() > 1024 {
            return Err("quota_too_many_credits");
        }
        let mut applicable = 0;
        let mut incomplete = false;
        for credit in credits {
            if field(credit, "reset_type", "resetType").as_str() != Some("codex_rate_limits")
                || credit["status"].as_str() != Some("available")
            {
                continue;
            }
            let expires = field(credit, "expires_at", "expiresAt")
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis());
            let granted = field(credit, "granted_at", "grantedAt")
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
                .map(|value| value.timestamp_millis());
            if expires.is_none() || granted.is_none() {
                incomplete = true;
            }
            if expires.is_some_and(|expires| expires > now)
                && granted.is_some_and(|granted| granted <= now)
            {
                applicable += 1;
            }
        }
        snapshot.reset_capability.applicable_credits = (!incomplete).then_some(applicable);
        if incomplete {
            return Err("quota_incomplete_credit_payload");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> QuotaSnapshot {
        QuotaSnapshot {
            contract_version: "upstream_quota_v1",
            upstream_account_id: Uuid::nil(),
            tenant_external_id: "test".into(),
            provider: "openai-codex".into(),
            status: "error",
            observed_at: None,
            stale_after: None,
            stale: false,
            plan_type: None,
            windows: Vec::new(),
            credits: Credits::default(),
            reset_capability: ResetCapability {
                provider_supported: Some(true),
                implementation_available: false,
                prepare_available: false,
                confirmation_required: false,
                retryable: false,
                available_credits: None,
                applicable_credits: None,
                reason: "reset_workflow_not_implemented",
                credit_error_code: None,
            },
            error_code: None,
        }
    }

    #[test]
    fn normalized_windows_keep_scope_unknown_zero_and_exact_vs_estimated_time() {
        let mut result = snapshot();
        usage(&mut result, &json!({
            "email":"must-not-escape", "access_token":"must-not-escape",
            "plan_type":"pro",
            "rate_limit":{"allowed":false,"primary_window":{"used_percent":0,"reset_at":123,"limit_window_seconds":18000},
                "secondary_window":{"reset_after_seconds":7}},
            "code_review_rate_limit":{"primary_window":{"used_percent":12}},
            "additional_rate_limits":[{"metered_feature":"feature","rate_limit":{"secondary_window":{"used_percent":52}}}],
            "credits":{"balance":"0","unlimited":false},
            "rate_limit_reset_credits":{"available_count":0}
        }), 1000).unwrap();
        assert_eq!(result.windows.len(), 4);
        assert_eq!(result.windows[0].used_percent, Some(0.0));
        assert_eq!(result.windows[0].reset_at, Some(123000));
        assert!(!result.windows[0].reset_is_estimated);
        assert_eq!(result.windows[1].used_percent, None);
        assert_eq!(result.windows[1].reset_at, Some(8000));
        assert!(result.windows[1].reset_is_estimated);
        assert!(
            result
                .windows
                .iter()
                .all(|window| window.remaining.is_none())
        );
        assert_eq!(result.reset_capability.available_credits, Some(0));
        assert!(
            !serde_json::to_string(&result)
                .unwrap()
                .contains("must-not-escape")
        );
    }

    #[test]
    fn credit_counts_require_correct_type_available_status_and_valid_lifetime() {
        let mut result = snapshot();
        reset_credits(&mut result, &json!({"available_count":4,"credits":[
            {"reset_type":"codex_rate_limits","status":"available","granted_at":"2026-01-01T00:00:00Z","expires_at":"2027-01-01T00:00:00Z"},
            {"reset_type":"other","status":"available","granted_at":"2026-01-01T00:00:00Z","expires_at":"2027-01-01T00:00:00Z"},
            {"reset_type":"codex_rate_limits","status":"used","granted_at":"2026-01-01T00:00:00Z","expires_at":"2027-01-01T00:00:00Z"},
            {"reset_type":"codex_rate_limits","status":"available","expires_at":"bad"}
        ]}), 1_780_000_000_000).unwrap_err();
        assert_eq!(result.reset_capability.available_credits, Some(4));
        assert_eq!(result.reset_capability.applicable_credits, None);
        assert_eq!(result.reset_capability.provider_supported, Some(true));
        assert!(!result.reset_capability.implementation_available);
    }

    #[test]
    fn stale_projection_expires_and_never_claims_fresh_success() {
        let mut old = snapshot();
        old.observed_at = Some(1000);
        let mut error = snapshot();
        error.error_code = Some("quota_timeout");
        let stale = stale_or_error(Some(old.clone()), error.clone(), 2000);
        assert!(stale.stale);
        assert_eq!(stale.error_code, Some("quota_timeout"));
        assert!(
            stale_or_error(Some(old), error, STALE_MS + 2000)
                .observed_at
                .is_none()
        );
    }

    #[test]
    fn only_explicit_allowed_codex_window_is_recovery_evidence() {
        let mut result = snapshot();
        result.status = "ready";
        usage(
            &mut result,
            &json!({
                "rate_limit": {
                    "allowed": true,
                    "limit_reached": false,
                    "primary_window": {"used_percent": 42}
                }
            }),
            1_000,
        )
        .unwrap();
        assert!(result.conclusively_allows_codex());
        result.windows[0].allowed = None;
        assert!(
            !result.conclusively_allows_codex(),
            "unknown availability must not clear an exhausted cooldown"
        );
        result.windows[0].allowed = Some(true);
        result.windows[0].limit_reached = Some(true);
        assert!(
            !result.conclusively_allows_codex(),
            "explicit exhaustion wins over the allowed flag"
        );
        result.windows[0].limit_reached = None;
        assert!(
            result.conclusively_allows_codex(),
            "explicit allowed evidence is conclusive when exhaustion is not explicitly reported"
        );
        result.stale = true;
        assert!(
            !result.conclusively_allows_codex(),
            "cached observations cannot mutate routing health"
        );
    }
}
