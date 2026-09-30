use serde_json::{Value, json};
use sqlx::{Any, Row, Transaction};
use uuid::Uuid;

use super::{
    associations::ensure_route_has_eligible_candidate,
    grant_revisions::{compare_and_bump_route_grant_revision, lock_routing_relation_writes},
};
use crate::{
    db::{Database, unix_millis},
    error::AppError,
};

impl Database {
    pub async fn plan_ledger_resource_route_switch(
        &self,
        tenant_external_id: &str,
        route_id: Uuid,
        source_account_id: Uuid,
        target_account_id: Uuid,
        expected_route_updated_at: i64,
        expected_source_updated_at: i64,
        expected_target_updated_at: i64,
        expected_grant_revision: i64,
        actor_service_id: Option<Uuid>,
    ) -> Result<Value, AppError> {
        validate_switch_input(
            tenant_external_id,
            source_account_id,
            target_account_id,
            expected_route_updated_at,
            expected_source_updated_at,
            expected_target_updated_at,
            expected_grant_revision,
        )?;
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = active_tenant_id(&mut tx, tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let before = read_switch_snapshot(
            &mut tx,
            &tenant_id,
            route_id,
            source_account_id,
            target_account_id,
        )
        .await?;
        check_expected_snapshot(
            &before,
            source_account_id,
            target_account_id,
            expected_route_updated_at,
            expected_source_updated_at,
            expected_target_updated_at,
            expected_grant_revision,
        )?;
        validate_route_and_accounts(&before, source_account_id, target_account_id)?;
        if before["route"]["upstream_candidates"]
            .as_array()
            .is_none_or(|candidates| candidates.len() != 1)
            || before["route"]["included_provider_group_ids"]
                .as_array()
                .is_none_or(|groups| !groups.is_empty())
            || before["route"]["excluded_provider_group_ids"]
                .as_array()
                .is_none_or(|groups| !groups.is_empty())
        {
            return Err(AppError::BadRequest(
                "resource switch requires one explicit candidate and no provider-group expansion"
                    .into(),
            ));
        }
        let mut after = before.clone();
        let candidates = after["route"]["upstream_candidates"]
            .as_array_mut()
            .ok_or(AppError::Internal)?;
        let mut found_source = false;
        let mut found_target = false;
        for candidate in candidates.iter_mut() {
            let account_id = candidate["upstream_account_id"]
                .as_str()
                .ok_or(AppError::Internal)?;
            found_source |= account_id == source_account_id.to_string();
            found_target |= account_id == target_account_id.to_string();
        }
        if !found_source || found_target {
            return Err(AppError::Conflict(
                "route candidates changed; reload before planning the switch".into(),
            ));
        }
        let source = candidates
            .iter_mut()
            .find(|candidate| {
                candidate["upstream_account_id"].as_str()
                    == Some(source_account_id.to_string().as_str())
            })
            .ok_or(AppError::Internal)?;
        source["upstream_account_id"] = json!(target_account_id.to_string());
        candidates.sort_by(|left, right| {
            left["upstream_account_id"]
                .as_str()
                .cmp(&right["upstream_account_id"].as_str())
        });
        if after["route"]["legacy_upstream_account_id"].as_str()
            == Some(source_account_id.to_string().as_str())
        {
            after["route"]["legacy_upstream_account_id"] = json!(target_account_id.to_string());
        }
        let route_updated_at = expected_route_updated_at.saturating_add(1);
        after["route"]["updated_at"] = json!(route_updated_at);

        let operation_id = Uuid::now_v7();
        let now = unix_millis();
        sqlx::query(
            "INSERT INTO ledger_resource_route_switches (operation_id, tenant_id, route_id, source_upstream_account_id, target_upstream_account_id, expected_route_updated_at, expected_source_updated_at, expected_target_updated_at, expected_grant_revision, before_snapshot_json, after_snapshot_json, status, actor_service_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, 'planned', $12, $13)",
        )
        .bind(operation_id.to_string())
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .bind(source_account_id.to_string())
        .bind(target_account_id.to_string())
        .bind(expected_route_updated_at)
        .bind(expected_source_updated_at)
        .bind(expected_target_updated_at)
        .bind(expected_grant_revision)
        .bind(serde_json::to_string(&before).map_err(|_| AppError::Internal)?)
        .bind(serde_json::to_string(&after).map_err(|_| AppError::Internal)?)
        .bind(actor_service_id.map(|id| id.to_string()))
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(operation_view(operation_id, "planned", before, after))
    }

    pub async fn apply_ledger_resource_route_switch(
        &self,
        tenant_external_id: &str,
        operation_id: Uuid,
    ) -> Result<Value, AppError> {
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = active_tenant_id(&mut tx, tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let operation = load_switch_operation(&mut tx, &tenant_id, operation_id).await?;
        let before = parse_snapshot(&operation, "before_snapshot_json")?;
        let mut after = parse_snapshot(&operation, "after_snapshot_json")?;
        let status: String = operation.try_get("status")?;
        if status != "planned" {
            return Err(AppError::Conflict(
                "only a planned route switch can be applied".into(),
            ));
        }
        let route_id = parse_uuid(&operation, "route_id")?;
        let source_id = parse_uuid(&operation, "source_upstream_account_id")?;
        let target_id = parse_uuid(&operation, "target_upstream_account_id")?;
        let expected_revision: i64 = operation.try_get("expected_grant_revision")?;
        let current =
            read_switch_snapshot(&mut tx, &tenant_id, route_id, source_id, target_id).await?;
        if current != before {
            return Err(AppError::Conflict(
                "route, candidates, or resource versions changed after planning".into(),
            ));
        }
        validate_route_and_accounts(&current, source_id, target_id)?;
        compare_and_bump_route_grant_revision(
            &mut tx,
            &tenant_id,
            route_id,
            expected_revision,
            false,
        )
        .await?;
        let current_route_updated_at: i64 = current["route"]["updated_at"]
            .as_i64()
            .ok_or(AppError::Internal)?;
        let next_updated_at = unix_millis().max(current_route_updated_at.saturating_add(1));
        let source_candidate = before["route"]["upstream_candidates"]
            .as_array()
            .and_then(|items| {
                items.iter().find(|candidate| {
                    candidate["upstream_account_id"].as_str()
                        == Some(source_id.to_string().as_str())
                })
            })
            .ok_or(AppError::Internal)?;
        sqlx::query(
            "DELETE FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 AND upstream_account_id = $3",
        )
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .bind(source_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO model_route_upstream_accounts (tenant_id, model_route_id, upstream_account_id, upstream_model, scheduling_weight, created_at, catalog_policy) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .bind(target_id.to_string())
        .bind(source_candidate["upstream_model"].as_str().ok_or(AppError::Internal)?)
        .bind(source_candidate["scheduling_weight"].as_i64().ok_or(AppError::Internal)?)
        .bind(source_candidate["created_at"].as_i64().ok_or(AppError::Internal)?)
        .bind(source_candidate["catalog_policy"].as_str().ok_or(AppError::Internal)?)
        .execute(&mut *tx)
        .await?;
        ensure_route_has_eligible_candidate(&mut tx, self.backend, &tenant_id, route_id).await?;
        let legacy_source = before["route"]["legacy_upstream_account_id"]
            .as_str()
            .ok_or(AppError::Internal)?;
        let legacy_target = after["route"]["legacy_upstream_account_id"]
            .as_str()
            .ok_or(AppError::Internal)?;
        let updated = sqlx::query(
            "UPDATE model_routes SET upstream_account_id = $1, updated_at = $2 WHERE tenant_id = $3 AND id = $4 AND upstream_account_id = $5 AND updated_at = $6",
        )
        .bind(legacy_target)
        .bind(next_updated_at)
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .bind(legacy_source)
        .bind(current_route_updated_at)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "model route changed during switch".into(),
            ));
        }
        after["route"]["updated_at"] = json!(next_updated_at);
        let now = unix_millis();
        let changed = sqlx::query(
            "UPDATE ledger_resource_route_switches SET after_snapshot_json = $1, status = 'applied', applied_at = $2 WHERE operation_id = $3 AND tenant_id = $4 AND status = 'planned'",
        )
        .bind(serde_json::to_string(&after).map_err(|_| AppError::Internal)?)
        .bind(now)
        .bind(operation_id.to_string())
        .bind(&tenant_id)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict("route switch operation changed".into()));
        }
        tx.commit().await?;
        Ok(operation_view(operation_id, "applied", before, after))
    }

    pub async fn rollback_ledger_resource_route_switch(
        &self,
        tenant_external_id: &str,
        operation_id: Uuid,
        expected_route_updated_at: i64,
        expected_grant_revision: i64,
    ) -> Result<Value, AppError> {
        if expected_route_updated_at < 0 || expected_grant_revision < 0 {
            return Err(AppError::BadRequest(
                "CAS values must be non-negative".into(),
            ));
        }
        let mut tx = self.begin_write_transaction().await?;
        let tenant_id = active_tenant_id(&mut tx, tenant_external_id).await?;
        lock_routing_relation_writes(&mut tx, &tenant_id).await?;
        let operation = load_switch_operation(&mut tx, &tenant_id, operation_id).await?;
        let before = parse_snapshot(&operation, "before_snapshot_json")?;
        let after = parse_snapshot(&operation, "after_snapshot_json")?;
        let status: String = operation.try_get("status")?;
        if status != "applied" {
            return Err(AppError::Conflict(
                "only an applied route switch can be rolled back".into(),
            ));
        }
        let route_id = parse_uuid(&operation, "route_id")?;
        let source_id = parse_uuid(&operation, "source_upstream_account_id")?;
        let target_id = parse_uuid(&operation, "target_upstream_account_id")?;
        let current =
            read_switch_snapshot(&mut tx, &tenant_id, route_id, source_id, target_id).await?;
        if current != after
            || current["route"]["updated_at"].as_i64() != Some(expected_route_updated_at)
            || current["route"]["grant_revision"].as_i64() != Some(expected_grant_revision)
        {
            return Err(AppError::Conflict(
                "route changed after the switch; rollback CAS failed".into(),
            ));
        }
        compare_and_bump_route_grant_revision(
            &mut tx,
            &tenant_id,
            route_id,
            expected_grant_revision,
            false,
        )
        .await?;
        sqlx::query(
            "DELETE FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2",
        )
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .execute(&mut *tx)
        .await?;
        insert_candidates(&mut tx, &tenant_id, route_id, &before).await?;
        let current_updated_at = current["route"]["updated_at"]
            .as_i64()
            .ok_or(AppError::Internal)?;
        let rollback_updated_at = unix_millis().max(current_updated_at.saturating_add(1));
        let old_legacy = before["route"]["legacy_upstream_account_id"]
            .as_str()
            .ok_or(AppError::Internal)?;
        let changed = sqlx::query(
            "UPDATE model_routes SET upstream_account_id = $1, updated_at = $2 WHERE tenant_id = $3 AND id = $4 AND upstream_account_id = $5 AND updated_at = $6",
        )
        .bind(old_legacy)
        .bind(rollback_updated_at)
        .bind(&tenant_id)
        .bind(route_id.to_string())
        .bind(current["route"]["legacy_upstream_account_id"].as_str().ok_or(AppError::Internal)?)
        .bind(current_updated_at)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict(
                "model route changed during rollback".into(),
            ));
        }
        let now = unix_millis();
        let updated = sqlx::query(
            "UPDATE ledger_resource_route_switches SET status = 'rolled_back', rolled_back_at = $1, rollback_updated_at = $2 WHERE operation_id = $3 AND tenant_id = $4 AND status = 'applied'",
        )
        .bind(now)
        .bind(rollback_updated_at)
        .bind(operation_id.to_string())
        .bind(&tenant_id)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            return Err(AppError::Conflict("route switch operation changed".into()));
        }
        tx.commit().await?;
        Ok(operation_view(operation_id, "rolled_back", before, after))
    }
}

fn validate_switch_input(
    tenant_external_id: &str,
    source_id: Uuid,
    target_id: Uuid,
    route_updated_at: i64,
    source_updated_at: i64,
    target_updated_at: i64,
    grant_revision: i64,
) -> Result<(), AppError> {
    if tenant_external_id.trim().is_empty()
        || tenant_external_id.trim() == "*"
        || tenant_external_id.len() > 200
    {
        return Err(AppError::BadRequest(
            "an explicit tenant is required".into(),
        ));
    }
    if source_id == target_id {
        return Err(AppError::BadRequest(
            "source and target resources must differ".into(),
        ));
    }
    if [
        route_updated_at,
        source_updated_at,
        target_updated_at,
        grant_revision,
    ]
    .into_iter()
    .any(|value| value < 0)
    {
        return Err(AppError::BadRequest(
            "CAS values must be non-negative".into(),
        ));
    }
    Ok(())
}

async fn active_tenant_id(
    tx: &mut Transaction<'_, Any>,
    tenant_external_id: &str,
) -> Result<String, AppError> {
    sqlx::query_scalar::<_, String>(
        "SELECT id FROM tenants WHERE external_id = $1 AND status = 'active'",
    )
    .bind(tenant_external_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

async fn read_switch_snapshot(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<Value, AppError> {
    let route = sqlx::query(
        "SELECT id, public_model, protocol, priority, upstream_model, enabled, upstream_account_id, updated_at FROM model_routes WHERE tenant_id = $1 AND id = $2 AND archived_at IS NULL",
    )
    .bind(tenant_id)
    .bind(route_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)?;
    let candidates = sqlx::query(
        "SELECT upstream_account_id, upstream_model, scheduling_weight, created_at, catalog_policy FROM model_route_upstream_accounts WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY upstream_account_id",
    )
    .bind(tenant_id)
    .bind(route_id.to_string())
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| {
        Ok(json!({
            "upstream_account_id": row.try_get::<String, _>("upstream_account_id")?,
            "upstream_model": row.try_get::<String, _>("upstream_model")?,
            "scheduling_weight": row.try_get::<i64, _>("scheduling_weight")?,
            "created_at": row.try_get::<i64, _>("created_at")?,
            "catalog_policy": row.try_get::<String, _>("catalog_policy")?,
        }))
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;
    let included_provider_group_ids = read_relation_ids(
        tx,
        "SELECT provider_group_id AS id FROM model_route_included_provider_groups WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY provider_group_id",
        tenant_id,
        route_id,
    )
    .await?;
    let excluded_provider_group_ids = read_relation_ids(
        tx,
        "SELECT provider_group_id AS id FROM model_route_excluded_provider_groups WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY provider_group_id",
        tenant_id,
        route_id,
    )
    .await?;
    let route_group_ids = read_relation_ids(
        tx,
        "SELECT route_group_id AS id FROM model_route_group_memberships WHERE tenant_id = $1 AND model_route_id = $2 ORDER BY route_group_id",
        tenant_id,
        route_id,
    )
    .await?;
    let accounts = sqlx::query(
        "SELECT id, driver, status, credential_generation, updated_at FROM upstream_accounts WHERE tenant_id = $1 AND id IN ($2, $3) ORDER BY id",
    )
    .bind(tenant_id)
    .bind(source_id.to_string())
    .bind(target_id.to_string())
    .fetch_all(&mut **tx)
    .await?
    .into_iter()
    .map(|row| {
        Ok(json!({
            "id": row.try_get::<String, _>("id")?,
            "driver": row.try_get::<String, _>("driver")?,
            "status": row.try_get::<String, _>("status")?,
            "credential_generation": row.try_get::<i64, _>("credential_generation")?,
            "updated_at": row.try_get::<i64, _>("updated_at")?,
        }))
    })
    .collect::<Result<Vec<_>, sqlx::Error>>()?;
    if accounts.len() != 2 {
        return Err(AppError::NotFound);
    }
    let revision = sqlx::query_scalar::<_, i64>(
        "SELECT revision FROM routing_grant_relation_revisions WHERE tenant_id = $1 AND subject_kind = 'route' AND subject_id = $2",
    )
    .bind(tenant_id)
    .bind(route_id.to_string())
    .fetch_optional(&mut **tx)
    .await?
    .unwrap_or(0);
    let public_model: String = route.try_get("public_model")?;
    let protocol: String = route.try_get("protocol")?;
    let upstream_model: String = route.try_get("upstream_model")?;
    if public_model.contains('*')
        || protocol.contains('*')
        || upstream_model.contains('*')
        || candidates.iter().any(|candidate| {
            candidate["upstream_model"]
                .as_str()
                .is_some_and(|model| model.contains('*'))
        })
    {
        return Err(AppError::BadRequest(
            "wildcard routes cannot be switched through this API".into(),
        ));
    }
    Ok(json!({
        "tenant_id": tenant_id,
        "route": {
            "id": route.try_get::<String, _>("id")?,
            "public_model": public_model,
            "protocol": protocol,
            "priority": route.try_get::<i64, _>("priority")?,
            "upstream_model": upstream_model,
            "enabled": route.try_get::<i64, _>("enabled")? != 0,
            "legacy_upstream_account_id": route.try_get::<String, _>("upstream_account_id")?,
            "updated_at": route.try_get::<i64, _>("updated_at")?,
            "grant_revision": revision,
            "upstream_candidates": candidates,
            "included_provider_group_ids": included_provider_group_ids,
            "excluded_provider_group_ids": excluded_provider_group_ids,
            "route_group_ids": route_group_ids,
        },
        "resources": accounts,
    }))
}

async fn read_relation_ids(
    tx: &mut Transaction<'_, Any>,
    query: &'static str,
    tenant_id: &str,
    route_id: Uuid,
) -> Result<Vec<String>, AppError> {
    sqlx::query(query)
        .bind(tenant_id)
        .bind(route_id.to_string())
        .fetch_all(&mut **tx)
        .await?
        .into_iter()
        .map(|row| row.try_get("id").map_err(AppError::from))
        .collect()
}

fn check_expected_snapshot(
    snapshot: &Value,
    source_id: Uuid,
    target_id: Uuid,
    route_updated_at: i64,
    source_updated_at: i64,
    target_updated_at: i64,
    grant_revision: i64,
) -> Result<(), AppError> {
    let resources = snapshot["resources"].as_array().ok_or(AppError::Internal)?;
    if snapshot["route"]["updated_at"].as_i64() != Some(route_updated_at)
        || snapshot["route"]["grant_revision"].as_i64() != Some(grant_revision)
    {
        return Err(AppError::Conflict(
            "route CAS mismatch; reload before planning".into(),
        ));
    }
    let versions = resources
        .iter()
        .filter_map(|item| Some((item["id"].as_str()?, item["updated_at"].as_i64()?)))
        .map(|(id, updated_at)| (id.to_owned(), updated_at))
        .collect::<std::collections::BTreeMap<_, _>>();
    if versions.get(&source_id.to_string()) != Some(&source_updated_at)
        || versions.get(&target_id.to_string()) != Some(&target_updated_at)
    {
        return Err(AppError::Conflict(
            "resource CAS mismatch; reload before planning".into(),
        ));
    }
    Ok(())
}

fn validate_route_and_accounts(
    snapshot: &Value,
    source_id: Uuid,
    target_id: Uuid,
) -> Result<(), AppError> {
    let resources = snapshot["resources"].as_array().ok_or(AppError::Internal)?;
    let source = resources
        .iter()
        .find(|item| item["id"].as_str() == Some(source_id.to_string().as_str()))
        .ok_or(AppError::NotFound)?;
    let target = resources
        .iter()
        .find(|item| item["id"].as_str() == Some(target_id.to_string().as_str()))
        .ok_or(AppError::NotFound)?;
    if source["status"].as_str() != Some("active")
        || target["status"].as_str() != Some("active")
        || snapshot["route"]["enabled"].as_bool() != Some(true)
    {
        return Err(AppError::Conflict(
            "the route and both resources must be active".into(),
        ));
    }
    Ok(())
}

async fn load_switch_operation(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    operation_id: Uuid,
) -> Result<sqlx::any::AnyRow, AppError> {
    sqlx::query(
        "SELECT operation_id, route_id, source_upstream_account_id, target_upstream_account_id, expected_route_updated_at, expected_grant_revision, before_snapshot_json, after_snapshot_json, status FROM ledger_resource_route_switches WHERE operation_id = $1 AND tenant_id = $2",
    )
    .bind(operation_id.to_string())
    .bind(tenant_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(AppError::NotFound)
}

fn parse_snapshot(row: &sqlx::any::AnyRow, column: &str) -> Result<Value, AppError> {
    let value: String = row.try_get(column)?;
    serde_json::from_str(&value).map_err(|_| AppError::Internal)
}

fn parse_uuid(row: &sqlx::any::AnyRow, column: &str) -> Result<Uuid, AppError> {
    let value: String = row.try_get(column)?;
    Uuid::parse_str(&value).map_err(|_| AppError::Internal)
}

async fn insert_candidates(
    tx: &mut Transaction<'_, Any>,
    tenant_id: &str,
    route_id: Uuid,
    snapshot: &Value,
) -> Result<(), AppError> {
    for candidate in snapshot["route"]["upstream_candidates"]
        .as_array()
        .ok_or(AppError::Internal)?
    {
        sqlx::query(
            "INSERT INTO model_route_upstream_accounts (tenant_id, model_route_id, upstream_account_id, upstream_model, scheduling_weight, created_at, catalog_policy) VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(tenant_id)
        .bind(route_id.to_string())
        .bind(candidate["upstream_account_id"].as_str().ok_or(AppError::Internal)?)
        .bind(candidate["upstream_model"].as_str().ok_or(AppError::Internal)?)
        .bind(candidate["scheduling_weight"].as_i64().ok_or(AppError::Internal)?)
        .bind(candidate["created_at"].as_i64().ok_or(AppError::Internal)?)
        .bind(candidate["catalog_policy"].as_str().ok_or(AppError::Internal)?)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

fn operation_view(operation_id: Uuid, status: &str, before: Value, after: Value) -> Value {
    json!({
        "operation_id": operation_id,
        "status": status,
        "before_snapshot": before,
        "after_snapshot": after,
    })
}
