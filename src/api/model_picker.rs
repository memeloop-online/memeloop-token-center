use std::collections::BTreeMap;

use axum::{
    Json,
    extract::{Query, State},
    http::{HeaderMap, header},
    response::IntoResponse,
};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::{management_tenant, require_service};
use crate::{
    AppState,
    db::{
        MODEL_PICKER_ITEM_LIMIT, ModelPickerItem, ModelPickerProjectionFilter,
        ModelPickerSelectionKind, unix_millis,
    },
    error::AppError,
};

const DEFAULT_MODEL_PICKER_LIMIT: i64 = 50;
const MAX_MODEL_PICKER_SEARCH_CHARS: usize = 500;
const MAX_MODEL_PICKER_CURSOR_BYTES: usize = 2_048;
const MAX_MODEL_PICKER_FILTER_IDS: usize = 100;
const MAX_MODEL_PICKER_FILTER_BYTES: usize =
    MAX_MODEL_PICKER_FILTER_IDS * 36 + (MAX_MODEL_PICKER_FILTER_IDS - 1);

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ModelPickerQuery {
    tenant_external_id: String,
    selection_kind: ModelPickerSelectionKind,
    #[serde(default)]
    q: String,
    cursor: Option<String>,
    #[serde(default = "default_model_picker_limit")]
    limit: i64,
    account_ids: Option<String>,
    include_provider_group_ids: Option<String>,
    exclude_provider_group_ids: Option<String>,
}

const fn default_model_picker_limit() -> i64 {
    DEFAULT_MODEL_PICKER_LIMIT
}

#[derive(Debug, Serialize)]
struct ModelPickerPage {
    contract_version: &'static str,
    generated_at: i64,
    data: Vec<ModelPickerItem>,
    next_cursor: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ModelPickerCursor {
    version: u8,
    selection_kind: ModelPickerSelectionKind,
    scope_sha256: String,
    sort_label: String,
    identity: String,
}

#[derive(Serialize)]
struct ModelPickerCursorScope<'a> {
    tenant_external_id: &'a str,
    selection_kind: ModelPickerSelectionKind,
    query: &'a str,
    explicit_account_ids: &'a [Uuid],
    included_provider_group_ids: &'a [Uuid],
    excluded_provider_group_ids: &'a [Uuid],
    public_provider_ids: &'a [String],
}

pub(super) async fn list_model_picker_options(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<ModelPickerQuery>,
) -> Result<impl IntoResponse, AppError> {
    let service = require_service(&headers, &state, "routes:read").await?;
    if !service.allows("providers:read") {
        return Err(AppError::Forbidden);
    }
    let requested_tenant = query.tenant_external_id.trim().to_owned();
    if requested_tenant.is_empty() || requested_tenant.len() > 200 {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain 1 to 200 characters".into(),
        ));
    }
    let tenant = management_tenant(&service, Some(requested_tenant))?
        .ok_or_else(|| AppError::BadRequest("tenant_external_id is required".into()))?;
    let search = query.q.trim();
    if search.chars().count() > MAX_MODEL_PICKER_SEARCH_CHARS {
        return Err(AppError::BadRequest(
            "model picker search contains too many characters".into(),
        ));
    }
    if !(1..=MODEL_PICKER_ITEM_LIMIT).contains(&query.limit) {
        return Err(AppError::BadRequest(format!(
            "model picker limit must be between 1 and {MODEL_PICKER_ITEM_LIMIT}"
        )));
    }

    let explicit_accounts = parse_uuid_filter(query.account_ids.as_deref())?;
    let included_groups = parse_uuid_filter(query.include_provider_group_ids.as_deref())?;
    let excluded_groups = parse_uuid_filter(query.exclude_provider_group_ids.as_deref())?;
    let providers = state
        .providers
        .list()
        .iter()
        .map(|provider| (provider.id.clone(), provider.clone()))
        .collect::<BTreeMap<_, _>>();
    let public_provider_ids = providers.keys().cloned().collect::<Vec<_>>();
    let cursor_scope = ModelPickerCursorScope {
        tenant_external_id: &tenant,
        selection_kind: query.selection_kind,
        query: search,
        explicit_account_ids: &explicit_accounts,
        included_provider_group_ids: &included_groups,
        excluded_provider_group_ids: &excluded_groups,
        public_provider_ids: &public_provider_ids,
    };
    let scope_sha256 = cursor_scope_sha256(&cursor_scope)?;
    let (after_sort_label, after_identity) =
        decode_cursor(query.cursor.as_deref(), &scope_sha256, query.selection_kind)?;
    let mut data = state
        .db
        .model_picker_projection(ModelPickerProjectionFilter {
            tenant_external_id: &tenant,
            selection_kind: query.selection_kind,
            query: search,
            after_sort_label: &after_sort_label,
            after_identity: &after_identity,
            explicit_account_ids: &explicit_accounts,
            included_provider_group_ids: &included_groups,
            excluded_provider_group_ids: &excluded_groups,
            public_provider_ids: &public_provider_ids,
            limit: query.limit,
        })
        .await?;

    for item in &mut data {
        for source in &mut item.sources {
            let provider = providers
                .get(&source.provider.id)
                .ok_or(AppError::Internal)?;
            source.provider.label = provider.display_name.clone();
            source.provider.protocols = provider.protocols.clone();
            source.provider.modalities = provider.modalities.clone();
        }
    }

    let has_more = data.len() > query.limit as usize;
    if has_more {
        data.truncate(query.limit as usize);
    }
    let next_cursor = if has_more {
        let last = data.last().ok_or(AppError::Internal)?;
        Some(encode_cursor(ModelPickerCursor {
            version: 1,
            selection_kind: query.selection_kind,
            scope_sha256,
            sort_label: last.sort_label.clone(),
            identity: last.sort_identity.clone(),
        })?)
    } else {
        None
    };
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(ModelPickerPage {
            contract_version: "model_picker_projection_v1",
            generated_at: unix_millis(),
            data,
            next_cursor,
        }),
    ))
}

fn parse_uuid_filter(value: Option<&str>) -> Result<Vec<Uuid>, AppError> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(Vec::new());
    };
    if value.len() > MAX_MODEL_PICKER_FILTER_BYTES
        || value.split(',').count() > MAX_MODEL_PICKER_FILTER_IDS
    {
        return Err(AppError::BadRequest(
            "model picker candidate filter is too large".into(),
        ));
    }
    let mut values = value
        .split(',')
        .map(|value| {
            value
                .parse::<Uuid>()
                .map_err(|_| AppError::BadRequest("invalid model picker candidate ID".into()))
        })
        .collect::<Result<Vec<_>, _>>()?;
    values.sort_unstable();
    values.dedup();
    if values.len() > MAX_MODEL_PICKER_FILTER_IDS {
        return Err(AppError::BadRequest(
            "model picker candidate filter is too large".into(),
        ));
    }
    Ok(values)
}

fn cursor_scope_sha256(scope: &ModelPickerCursorScope<'_>) -> Result<String, AppError> {
    let encoded = serde_json::to_vec(scope).map_err(|_| AppError::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(Sha256::digest(encoded)))
}

fn encode_cursor(cursor: ModelPickerCursor) -> Result<String, AppError> {
    let bytes = serde_json::to_vec(&cursor).map_err(|_| AppError::Internal)?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

fn decode_cursor(
    cursor: Option<&str>,
    expected_scope_sha256: &str,
    selection_kind: ModelPickerSelectionKind,
) -> Result<(String, String), AppError> {
    let Some(cursor) = cursor else {
        return Ok((String::new(), String::new()));
    };
    if cursor.is_empty() || cursor.len() > MAX_MODEL_PICKER_CURSOR_BYTES {
        return Err(AppError::BadRequest("invalid model picker cursor".into()));
    }
    let bytes = URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| AppError::BadRequest("invalid model picker cursor".into()))?;
    let cursor: ModelPickerCursor = serde_json::from_slice(&bytes)
        .map_err(|_| AppError::BadRequest("invalid model picker cursor".into()))?;
    let valid_identity = match selection_kind {
        ModelPickerSelectionKind::Route => Uuid::parse_str(&cursor.identity).is_ok(),
        ModelPickerSelectionKind::Model => {
            !cursor.identity.is_empty() && cursor.identity.len() <= 200
        }
    };
    if cursor.version != 1
        || cursor.selection_kind != selection_kind
        || cursor.scope_sha256 != expected_scope_sha256
        || cursor.sort_label.len() > 200
        || !valid_identity
    {
        return Err(AppError::BadRequest("invalid model picker cursor".into()));
    }
    Ok((cursor.sort_label, cursor.identity))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cursor_is_bound_to_the_complete_projection_scope() {
        let account = Uuid::now_v7();
        let scope = ModelPickerCursorScope {
            tenant_external_id: "tenant-a",
            selection_kind: ModelPickerSelectionKind::Route,
            query: "model",
            explicit_account_ids: &[account],
            included_provider_group_ids: &[],
            excluded_provider_group_ids: &[],
            public_provider_ids: &["http-json".to_owned()],
        };
        let digest = cursor_scope_sha256(&scope).unwrap();
        let route = Uuid::now_v7();
        let encoded = encode_cursor(ModelPickerCursor {
            version: 1,
            selection_kind: ModelPickerSelectionKind::Route,
            scope_sha256: digest.clone(),
            sort_label: "model".to_owned(),
            identity: route.to_string(),
        })
        .unwrap();
        assert_eq!(
            decode_cursor(Some(&encoded), &digest, ModelPickerSelectionKind::Route).unwrap(),
            ("model".to_owned(), route.to_string())
        );
        assert!(
            decode_cursor(Some(&encoded), "different", ModelPickerSelectionKind::Route).is_err()
        );
        assert!(decode_cursor(Some(&encoded), &digest, ModelPickerSelectionKind::Model).is_err());
    }

    #[test]
    fn candidate_filters_are_canonical_and_hard_bounded() {
        let first = Uuid::now_v7();
        let second = Uuid::now_v7();
        let parsed = parse_uuid_filter(Some(&format!("{second},{first},{second}"))).unwrap();
        assert_eq!(parsed, vec![first, second]);
        let oversized = (0..=MAX_MODEL_PICKER_FILTER_IDS)
            .map(|_| Uuid::now_v7().to_string())
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_uuid_filter(Some(&oversized)).is_err());
        let repeated = std::iter::repeat_n(first.to_string(), MAX_MODEL_PICKER_FILTER_IDS + 1)
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_uuid_filter(Some(&repeated)).is_err());
        assert!(parse_uuid_filter(Some(&"x".repeat(MAX_MODEL_PICKER_FILTER_BYTES + 1))).is_err());
    }
}
