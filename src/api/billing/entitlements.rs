use super::super::*;
use super::money::parse_decimal;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct EntitlementQuery {
    tenant_external_id: Option<String>,
    provider: Option<String>,
    external_subscription_id: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ReconcileEntitlementRequest {
    tenant_external_id: Option<String>,
    account_id: Uuid,
    provider: String,
    external_subscription_id: String,
    external_cycle_id: String,
    period_start: i64,
    period_end: i64,
    currency: String,
    desired: String,
    version: i64,
    source: String,
    proration: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct CancelEntitlementRequest {
    tenant_external_id: Option<String>,
    provider: String,
    external_subscription_id: String,
    external_cycle_id: Option<String>,
    version: i64,
    source: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::api) struct ReplaceEntitlementRequest {
    tenant_external_id: Option<String>,
    provider: String,
    external_subscription_id: String,
    version: i64,
    source: String,
    replacement: ReconcileEntitlementRequest,
}

fn required_idempotency_key(headers: &HeaderMap) -> Result<&str, AppError> {
    headers
        .get("idempotency-key")
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| AppError::BadRequest("Idempotency-Key is required".into()))
}

fn entitlement_reconcile_input(
    tenant_external_id: String,
    body: ReconcileEntitlementRequest,
) -> Result<ReconcileEntitlementInput, AppError> {
    let desired = parse_decimal(&body.desired, "desired")?;
    let desired_micros = desired
        .checked_mul(Decimal::from(crate::model::MONEY_SCALE))
        .filter(|value| value.fract().is_zero())
        .and_then(|value| value.to_string().parse::<i64>().ok())
        .ok_or_else(|| {
            AppError::BadRequest(
                "desired must have at most 6 decimal places and fit monetary range".into(),
            )
        })?;
    Ok(ReconcileEntitlementInput {
        tenant_external_id,
        account_id: body.account_id,
        provider: body.provider,
        external_subscription_id: body.external_subscription_id,
        external_cycle_id: body.external_cycle_id,
        period_start: body.period_start,
        period_end: body.period_end,
        currency: body.currency,
        desired_micros,
        version: body.version,
        source: body.source,
        proration_json: body
            .proration
            .map(|value| serde_json::to_string(&value).map_err(|_| AppError::Internal))
            .transpose()?,
    })
}

pub(in crate::api) async fn list_entitlements(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<EntitlementQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "entitlements:read").await?;
    let tenant = management_tenant(&service, query.tenant_external_id)?;
    Ok(Json(
        state
            .db
            .list_entitlements(
                tenant.as_deref(),
                query.provider.as_deref(),
                query.external_subscription_id.as_deref(),
            )
            .await?,
    ))
}

pub(in crate::api) async fn reconcile_entitlement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReconcileEntitlementRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "entitlements:write").await?;
    let tenant = management_tenant(&service, body.tenant_external_id.clone())?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    state
        .db
        .require_account_tenant(body.account_id, &tenant)
        .await?;
    let idempotency_key = required_idempotency_key(&headers)?;
    let result = state
        .db
        .reconcile_entitlement(
            EntitlementOperation::Reconcile(entitlement_reconcile_input(tenant, body)?),
            idempotency_key,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(result)))
}

pub(in crate::api) async fn cancel_entitlement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<CancelEntitlementRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "entitlements:write").await?;
    let tenant = management_tenant(&service, body.tenant_external_id)?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let result = state
        .db
        .reconcile_entitlement(
            EntitlementOperation::Cancel(CancelEntitlementInput {
                tenant_external_id: tenant,
                provider: body.provider,
                external_subscription_id: body.external_subscription_id,
                external_cycle_id: body.external_cycle_id,
                version: body.version,
                source: body.source,
            }),
            required_idempotency_key(&headers)?,
        )
        .await?;
    Ok(Json(result))
}

pub(in crate::api) async fn replace_entitlement(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<ReplaceEntitlementRequest>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "entitlements:write").await?;
    let tenant = management_tenant(&service, body.tenant_external_id.clone())?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let replacement_tenant =
        management_tenant(&service, body.replacement.tenant_external_id.clone())?
            .unwrap_or_else(|| tenant.clone());
    if replacement_tenant != tenant {
        return Err(AppError::Forbidden);
    }
    state
        .db
        .require_account_tenant(body.replacement.account_id, &tenant)
        .await?;
    let replacement = entitlement_reconcile_input(replacement_tenant, body.replacement)?;
    let result = state
        .db
        .reconcile_entitlement(
            EntitlementOperation::Replace(ReplaceEntitlementInput {
                tenant_external_id: tenant,
                provider: body.provider,
                external_subscription_id: body.external_subscription_id,
                version: body.version,
                source: body.source,
                replacement,
            }),
            required_idempotency_key(&headers)?,
        )
        .await?;
    Ok((StatusCode::CREATED, Json(result)))
}
