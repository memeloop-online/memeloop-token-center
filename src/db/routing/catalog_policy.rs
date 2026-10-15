use sqlx::{Any, Transaction};
use uuid::Uuid;

use super::super::AppError;

pub(in crate::db) async fn record_group_catalog_evidence(
    transaction: &mut Transaction<'_, Any>,
    tenant_id: &str,
    account_id: Uuid,
    now: i64,
) -> Result<(), AppError> {
    sqlx::query(
        "INSERT INTO model_route_group_catalog_evidence \
         (tenant_id, model_route_id, upstream_account_id, upstream_model, protocol, discovered_at) \
         SELECT DISTINCT included.tenant_id, route.id, member.upstream_account_id, \
                route.upstream_model, route.protocol, $3 \
         FROM model_route_included_provider_groups included \
         JOIN upstream_account_provider_groups member \
           ON member.tenant_id = included.tenant_id \
          AND member.provider_group_id = included.provider_group_id \
         JOIN model_routes route \
           ON route.tenant_id = included.tenant_id AND route.id = included.model_route_id \
         WHERE included.tenant_id = $1 AND member.upstream_account_id = $2 \
           AND included.catalog_policy = 'explicit_custom' AND EXISTS (\
             SELECT 1 FROM upstream_models model \
             WHERE model.tenant_id = $1 AND model.upstream_account_id = $2 \
               AND model.model_id = route.upstream_model \
               AND (model.protocol = 'any' OR model.protocol = route.protocol)\
           ) ON CONFLICT (tenant_id, model_route_id, upstream_account_id, upstream_model, protocol) DO NOTHING",
    )
    .bind(tenant_id)
    .bind(account_id.to_string())
    .bind(now)
    .execute(&mut **transaction)
    .await?;
    Ok(())
}
