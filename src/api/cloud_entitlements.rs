use super::*;
use serde::Serialize;
use sha2::{Digest, Sha256};

const MEMELOOP_CLOUD_PROVIDER: &str = "memeloop-cloud";
const WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS: i64 = 5 * 60;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CloudSubscriptionStatus {
    Active,
    Cancelled,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct CloudSubscriptionWebhook {
    tenant_external_id: String,
    principal_external_id: String,
    external_subscription_id: String,
    external_cycle_id: Option<String>,
    period_start: Option<i64>,
    period_end: Option<i64>,
    currency: String,
    desired: Option<String>,
    version: i64,
    status: CloudSubscriptionStatus,
    policy: KeyPolicy,
    /// Normalized route grants. Presence (including an empty array) opts in to
    /// the grants contract; only older senders that omit both fields use the
    /// one-time `allowed_models` compatibility snapshot.
    #[serde(default)]
    route_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    route_group_ids: Option<Vec<Uuid>>,
    #[serde(default)]
    proration: Option<Value>,
}

fn required_ascii_header<'a>(headers: &'a HeaderMap, name: &str) -> Result<&'a str, AppError> {
    headers
        .get(name)
        .ok_or(AppError::Unauthorized)?
        .to_str()
        .map_err(|_| AppError::Unauthorized)
}

fn authenticate_cloud_webhook(
    state: &AppState,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(), AppError> {
    let secret = state
        .config
        .memeloop_cloud_webhook_secret
        .as_deref()
        .ok_or(AppError::Unauthorized)?;
    let timestamp = required_ascii_header(headers, "x-mtc-webhook-timestamp")?;
    if timestamp.is_empty()
        || timestamp.len() > 20
        || !timestamp.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(AppError::Unauthorized);
    }
    let timestamp_seconds = timestamp
        .parse::<i64>()
        .map_err(|_| AppError::Unauthorized)?;
    let current_seconds = unix_millis() / 1_000;
    if current_seconds.abs_diff(timestamp_seconds) > WEBHOOK_TIMESTAMP_TOLERANCE_SECONDS as u64 {
        return Err(AppError::Unauthorized);
    }
    let signature = required_ascii_header(headers, "x-mtc-webhook-signature")?;
    if !crate::crypto::verify_webhook_payload(secret.as_bytes(), timestamp, body, signature) {
        return Err(AppError::Unauthorized);
    }
    Ok(())
}

fn required_event_id(headers: &HeaderMap) -> Result<&str, AppError> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.len() <= 200
                && value.bytes().all(|byte| byte.is_ascii_graphic())
        })
        .ok_or_else(|| {
            AppError::BadRequest(
                "Idempotency-Key must contain 1 to 200 visible ASCII characters".into(),
            )
        })
}

fn digest(parts: &[&[u8]]) -> String {
    let mut digest = Sha256::new();
    for part in parts {
        digest.update((*part).len().to_be_bytes());
        digest.update(part);
    }
    format!("{:x}", digest.finalize())
}

fn entitlement_operation(
    body: &CloudSubscriptionWebhook,
    account_id: Uuid,
    event_digest: &str,
) -> Result<EntitlementOperation, AppError> {
    let source = format!("memeloop-cloud:webhook:{}", &event_digest[..48]);
    match body.status {
        CloudSubscriptionStatus::Active => {
            let external_cycle_id = body.external_cycle_id.clone().ok_or_else(|| {
                AppError::BadRequest("active subscription requires external_cycle_id".into())
            })?;
            let period_start = body.period_start.ok_or_else(|| {
                AppError::BadRequest("active subscription requires period_start".into())
            })?;
            let period_end = body.period_end.ok_or_else(|| {
                AppError::BadRequest("active subscription requires period_end".into())
            })?;
            let desired = body.desired.as_deref().ok_or_else(|| {
                AppError::BadRequest("active subscription requires desired".into())
            })?;
            let desired_micros = parse_money_micros(desired, "desired")?;
            if desired_micros < 0 {
                return Err(AppError::BadRequest(
                    "desired entitlement cannot be negative".into(),
                ));
            }
            let proration_json = serde_json::to_string(&json!({
                "cloud_event_sha256": event_digest,
                "metadata": body.proration.clone(),
            }))
            .map_err(|_| AppError::Internal)?;
            Ok(EntitlementOperation::Reconcile(ReconcileEntitlementInput {
                tenant_external_id: body.tenant_external_id.clone(),
                account_id,
                provider: MEMELOOP_CLOUD_PROVIDER.into(),
                external_subscription_id: body.external_subscription_id.clone(),
                external_cycle_id,
                period_start,
                period_end,
                currency: body.currency.clone(),
                desired_micros,
                version: body.version,
                source,
                proration_json: Some(proration_json),
            }))
        }
        CloudSubscriptionStatus::Cancelled => {
            if body.period_start.is_some() || body.period_end.is_some() || body.desired.is_some() {
                return Err(AppError::BadRequest(
                    "cancelled subscription must omit period_start, period_end, and desired".into(),
                ));
            }
            if body.proration.is_some() {
                return Err(AppError::BadRequest(
                    "cancelled subscription must omit proration".into(),
                ));
            }
            Ok(EntitlementOperation::Cancel(CancelEntitlementInput {
                tenant_external_id: body.tenant_external_id.clone(),
                provider: MEMELOOP_CLOUD_PROVIDER.into(),
                external_subscription_id: body.external_subscription_id.clone(),
                external_cycle_id: body.external_cycle_id.clone(),
                version: body.version,
                source,
            }))
        }
    }
}

/// Signed, idempotent full-state subscription webhook used by MemeLoop Cloud. It keeps
/// the durable credit account and key identity stable while applying quota and
/// policy under the same monotonically increasing subscription version.
pub(in crate::api) async fn sync_memeloop_cloud_subscription(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, AppError> {
    authenticate_cloud_webhook(&state, &headers, &body)?;
    let event_id = required_event_id(&headers)?;
    let payload: CloudSubscriptionWebhook = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("request body must match the webhook schema".into()))?;
    let canonical = serde_json::to_vec(&payload).map_err(|_| AppError::Internal)?;
    let event_digest = digest(&[canonical.as_slice()]);
    crate::db::validate_key_policy(&payload.policy)?;
    // Validate every entitlement field before creating durable identity rows.
    // The placeholder account is replaced only after the stable principal has
    // been resolved.
    let operation = entitlement_operation(&payload, Uuid::nil(), &event_digest)?;
    crate::db::validate_entitlement_operation(&operation)?;
    let provisioning_key = format!(
        "memeloop-cloud-principal:{}",
        digest(&[
            payload.tenant_external_id.as_bytes(),
            payload.principal_external_id.as_bytes(),
        ])
    );
    // A bounded digest is used as the durable event namespace; the raw event
    // identifier may contain provider punctuation and is never persisted or
    // reflected into logs.
    let event_key_hash = digest(&[payload.tenant_external_id.as_bytes(), event_id.as_bytes()]);
    let reconciliation_key = format!("memeloop-cloud-event:{event_key_hash}");
    let routing = match payload.status {
        CloudSubscriptionStatus::Cancelled => {
            if payload
                .route_ids
                .as_ref()
                .is_some_and(|ids| !ids.is_empty())
                || payload
                    .route_group_ids
                    .as_ref()
                    .is_some_and(|ids| !ids.is_empty())
            {
                return Err(AppError::BadRequest(
                    "cancelled subscription cannot grant model routes".into(),
                ));
            }
            CloudRoutingGrantSnapshot::DenyAll
        }
        CloudSubscriptionStatus::Active => {
            if payload.route_ids.is_some() || payload.route_group_ids.is_some() {
                CloudRoutingGrantSnapshot::Explicit {
                    route_ids: payload.route_ids.clone().unwrap_or_default(),
                    route_group_ids: payload.route_group_ids.clone().unwrap_or_default(),
                }
            } else {
                CloudRoutingGrantSnapshot::LegacyAllowedModels(
                    payload.policy.allowed_models.clone(),
                )
            }
        }
    };
    let result = state
        .db
        .apply_cloud_entitlement(
            ApplyCloudEntitlementInput {
                tenant_external_id: payload.tenant_external_id.clone(),
                principal_external_id: payload.principal_external_id.clone(),
                provider: MEMELOOP_CLOUD_PROVIDER.into(),
                external_subscription_id: payload.external_subscription_id.clone(),
                currency: payload.currency.clone(),
                provisioning_idempotency_key: provisioning_key,
                reconciliation_idempotency_key: reconciliation_key,
                operation,
                policy: payload.policy,
                routing,
                audit: CloudSubscriptionEventInput {
                    tenant_external_id: payload.tenant_external_id.clone(),
                    principal_external_id: payload.principal_external_id.clone(),
                    key_id: Uuid::nil(),
                    entitlement_id: Uuid::nil(),
                    event_key_hash,
                    request_hash: event_digest,
                    version: payload.version,
                    subscription_status: match payload.status {
                        CloudSubscriptionStatus::Active => "active".into(),
                        CloudSubscriptionStatus::Cancelled => "cancelled".into(),
                    },
                },
            },
            state.config.key_pepper.as_bytes(),
        )
        .await?;

    let status = match payload.status {
        CloudSubscriptionStatus::Active => StatusCode::CREATED,
        CloudSubscriptionStatus::Cancelled => StatusCode::OK,
    };
    Ok((
        status,
        Json(json!({
            "credential": result.credential,
            "entitlement": result.entitlement,
            "policy": result.policy,
        })),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_frames_fields_instead_of_concatenating_them() {
        assert_ne!(digest(&[b"ab", b"c"]), digest(&[b"a", b"bc"]));
    }
}
