use super::super::*;
use super::jobs::generation_job_view;

impl Database {
    pub async fn operator_generation_jobs(
        &self,
        tenant_external_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<OperatorGenerationJobView>, AppError> {
        let rows = match tenant_external_id {
            Some(tenant) => {
                sqlx::query(
                    "SELECT j.id, j.created_at, j.updated_at, j.completed_at, j.public_model, j.driver, j.billing_unit_snapshot, j.status, j.upstream_job_id, j.estimated_units, j.billed_units, j.cost_micros, j.error_code, j.result_json, j.key_id, t.external_id AS tenant_external_id, k.alias AS key_alias, k.currency FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id JOIN key_records k ON k.id = j.key_id WHERE t.external_id = $1 ORDER BY j.created_at DESC, j.id DESC LIMIT $2",
                )
                .bind(tenant)
                .bind(limit.clamp(1, 200))
                .fetch_all(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT j.id, j.created_at, j.updated_at, j.completed_at, j.public_model, j.driver, j.billing_unit_snapshot, j.status, j.upstream_job_id, j.estimated_units, j.billed_units, j.cost_micros, j.error_code, j.result_json, j.key_id, t.external_id AS tenant_external_id, k.alias AS key_alias, k.currency FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id JOIN key_records k ON k.id = j.key_id ORDER BY j.created_at DESC, j.id DESC LIMIT $1",
                )
                .bind(limit.clamp(1, 200))
                .fetch_all(&self.pool)
                .await?
            }
        };
        rows.into_iter().map(operator_generation_job_view).collect()
    }

    pub async fn operator_generation_job(
        &self,
        tenant_external_id: Option<&str>,
        job_id: Uuid,
    ) -> Result<OperatorGenerationJobView, AppError> {
        let row = match tenant_external_id {
            Some(tenant) => {
                sqlx::query(
                    "SELECT j.id, j.created_at, j.updated_at, j.completed_at, j.public_model, j.driver, j.billing_unit_snapshot, j.status, j.upstream_job_id, j.estimated_units, j.billed_units, j.cost_micros, j.error_code, j.result_json, j.key_id, t.external_id AS tenant_external_id, k.alias AS key_alias, k.currency FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id JOIN key_records k ON k.id = j.key_id WHERE j.id = $1 AND t.external_id = $2",
                )
                .bind(job_id.to_string())
                .bind(tenant)
                .fetch_optional(&self.pool)
                .await?
            }
            None => {
                sqlx::query(
                    "SELECT j.id, j.created_at, j.updated_at, j.completed_at, j.public_model, j.driver, j.billing_unit_snapshot, j.status, j.upstream_job_id, j.estimated_units, j.billed_units, j.cost_micros, j.error_code, j.result_json, j.key_id, t.external_id AS tenant_external_id, k.alias AS key_alias, k.currency FROM generation_jobs j JOIN tenants t ON t.id = j.tenant_id JOIN key_records k ON k.id = j.key_id WHERE j.id = $1",
                )
                .bind(job_id.to_string())
                .fetch_optional(&self.pool)
                .await?
            }
        }
        .ok_or(AppError::NotFound)?;
        operator_generation_job_view(row)
    }
}

fn operator_generation_job_view(row: AnyRow) -> Result<OperatorGenerationJobView, AppError> {
    let tenant_external_id = row.try_get("tenant_external_id")?;
    let key_id = parse_uuid(row.try_get("key_id")?)?;
    let key_alias = row.try_get("key_alias")?;
    let currency = row.try_get("currency")?;
    Ok(OperatorGenerationJobView {
        job: generation_job_view(row)?,
        tenant_external_id,
        key_id,
        key_alias,
        currency,
    })
}
