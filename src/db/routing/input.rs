use std::collections::{BTreeMap, BTreeSet};

use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::super::{AppError, parse_uuid};

const MAX_GROUP_NAME_BYTES: usize = 100;
const MAX_GROUP_MEMBERS: usize = 500;

pub(super) fn bounded_unique_ids(ids: Vec<Uuid>, label: &str) -> Result<Vec<Uuid>, AppError> {
    if ids.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::BadRequest(format!(
            "{label} cannot contain more than {MAX_GROUP_MEMBERS} entries"
        )));
    }
    Ok(ids
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect())
}

pub(super) fn bounded_group_names(names: Vec<String>) -> Result<Vec<(String, String)>, AppError> {
    if names.len() > MAX_GROUP_MEMBERS {
        return Err(AppError::BadRequest(format!(
            "route group names cannot contain more than {MAX_GROUP_MEMBERS} entries"
        )));
    }
    let mut normalized = BTreeMap::new();
    for raw in names {
        let (name, normalized_name) = normalize_group_name(&raw)?;
        normalized.entry(normalized_name).or_insert(name);
    }
    Ok(normalized
        .into_iter()
        .map(|(normalized_name, name)| (name, normalized_name))
        .collect())
}

pub(super) async fn resolve_existing_route_group_names(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    groups: &[(String, String)],
    route_group_ids: &mut Vec<Uuid>,
) -> Result<bool, AppError> {
    let mut all_exist = true;
    for (_, normalized_name) in groups {
        let existing = sqlx::query(
            "SELECT id FROM route_groups WHERE tenant_id = $1 AND normalized_name = $2",
        )
        .bind(tenant_id)
        .bind(normalized_name)
        .fetch_optional(&mut **tx)
        .await?;
        if let Some(row) = existing {
            route_group_ids.push(parse_uuid(row.try_get("id")?)?);
        } else {
            all_exist = false;
        }
    }
    Ok(all_exist)
}

pub(super) async fn create_or_find_route_groups(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    groups: Vec<(String, String)>,
    route_group_ids: &mut Vec<Uuid>,
    now: i64,
) -> Result<(), AppError> {
    for (name, normalized_name) in groups {
        let group_id = Uuid::now_v7();
        sqlx::query("INSERT INTO route_groups (id, tenant_id, name, normalized_name, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(tenant_id, normalized_name) DO NOTHING")
            .bind(group_id.to_string()).bind(tenant_id).bind(name).bind(&normalized_name)
            .bind(now).bind(now).execute(&mut **tx).await?;
        let existing: String = sqlx::query(
            "SELECT id FROM route_groups WHERE tenant_id = $1 AND normalized_name = $2",
        )
        .bind(tenant_id)
        .bind(normalized_name)
        .fetch_one(&mut **tx)
        .await?
        .try_get("id")?;
        route_group_ids.push(parse_uuid(existing)?);
    }
    Ok(())
}

pub(super) fn validate_route_fields(
    public_model: &str,
    upstream_model: &str,
    protocol: &str,
    priority: i64,
) -> Result<(), AppError> {
    let public_model = public_model.trim();
    let upstream_model = upstream_model.trim();
    if public_model.is_empty() || upstream_model.is_empty() {
        return Err(AppError::BadRequest(
            "public_model and upstream_model are required".into(),
        ));
    }
    if public_model.len() > 200 || upstream_model.len() > 500 {
        return Err(AppError::BadRequest(
            "public_model and upstream_model exceed their length limit".into(),
        ));
    }
    if public_model.chars().any(char::is_control) || upstream_model.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "model names must not contain control characters".into(),
        ));
    }
    if !matches!(protocol, "openai" | "anthropic" | "generation") {
        return Err(AppError::BadRequest(
            "route protocol must be openai, anthropic, or generation".into(),
        ));
    }
    if !(-1_000_000..=1_000_000).contains(&priority) {
        return Err(AppError::BadRequest(
            "route priority must be between -1000000 and 1000000".into(),
        ));
    }
    Ok(())
}

fn normalize_group_name(raw: &str) -> Result<(String, String), AppError> {
    let name = raw.trim();
    if name.is_empty() || name.len() > MAX_GROUP_NAME_BYTES || name.chars().any(char::is_control) {
        return Err(AppError::BadRequest(
            "group name must contain 1 to 100 non-control bytes".into(),
        ));
    }
    Ok((name.to_owned(), name.to_lowercase()))
}
