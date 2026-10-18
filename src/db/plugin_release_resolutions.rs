use super::*;
use crate::plugin_release::ResolvedPluginRelease;

#[derive(Serialize)]
pub struct PluginReleaseResolution {
    pub id: String,
    #[serde(skip)]
    pub operation_key: String,
    pub base_revision: i64,
    #[serde(skip)]
    pub retained: Option<crate::plugin::application::PreinstalledInventory>,
    #[serde(flatten)]
    pub release: ResolvedPluginRelease,
}

impl Database {
    pub(crate) async fn plugin_release_base_revision(
        &self,
        inventory_id: &str,
    ) -> Result<Option<i64>, AppError> {
        Ok(sqlx::query_scalar("SELECT r.base_revision FROM application_plugin_release_resolutions r JOIN application_plugin_installations i ON i.idempotency_hash=r.install_key_hash WHERE i.inventory_id=$1")
            .bind(inventory_id).fetch_optional(&self.pool).await?)
    }
    pub(crate) async fn plugin_release_resolution_for_confirmation(
        &self,
        id: &str,
        digest: &str,
        actor: &str,
    ) -> Result<PluginReleaseResolution, AppError> {
        let row = sqlx::query("SELECT * FROM application_plugin_release_resolutions WHERE id=$1 AND descriptor_sha256=$2 AND actor=$3")
            .bind(id).bind(digest).bind(actor).fetch_optional(&self.pool).await?.ok_or(AppError::NotFound)?;
        resolution(row)
    }
    pub(crate) async fn replay_plugin_release_resolution(
        &self,
        operation_key: &str,
        actor: &str,
        url: &str,
    ) -> Result<Option<PluginReleaseResolution>, AppError> {
        let row = sqlx::query("SELECT * FROM application_plugin_release_resolutions WHERE operation_key=$1 AND actor=$2")
            .bind(operation_key).bind(actor).fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<String, _>("release_url")? != url {
            return Err(AppError::Conflict(
                "release resolution key already used".into(),
            ));
        }
        Ok(Some(resolution(row)?))
    }

    pub(crate) async fn save_plugin_release_resolution(
        &self,
        operation_key: &str,
        actor: &str,
        release: &ResolvedPluginRelease,
        base_revision: i64,
        retained: Option<&crate::plugin::application::PreinstalledInventory>,
    ) -> Result<PluginReleaseResolution, AppError> {
        let json = serde_json::to_string(release).map_err(|_| AppError::Internal)?;
        let retained = retained
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;
        sqlx::query("INSERT INTO application_plugin_release_resolutions (id,operation_key,actor,release_url,descriptor_sha256,resolved_json,created_at,base_revision,retained_json) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) ON CONFLICT(operation_key) DO NOTHING")
            .bind(Uuid::now_v7().to_string()).bind(operation_key).bind(actor)
            .bind(&release.release_url).bind(&release.descriptor_sha256).bind(json).bind(unix_millis()).bind(base_revision).bind(retained)
            .execute(&self.pool).await?;
        self.replay_plugin_release_resolution(operation_key, actor, &release.release_url)
            .await?
            .ok_or(AppError::Forbidden)
    }

    /// Bind explicit operator confirmation to this one installation operation.
    /// A later URL mutation never replaces the already frozen descriptor.
    pub(crate) async fn confirm_plugin_release_resolution(
        &self,
        id: &str,
        digest: &str,
        actor: &str,
        install_key: &str,
    ) -> Result<PluginReleaseResolution, AppError> {
        let changed = sqlx::query("UPDATE application_plugin_release_resolutions SET install_key_hash=$1 WHERE id=$2 AND descriptor_sha256=$3 AND actor=$4 AND (install_key_hash IS NULL OR install_key_hash=$1)")
            .bind(install_key).bind(id).bind(digest).bind(actor).execute(&self.pool).await?.rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict(
                "release confirmation changed or was already used".into(),
            ));
        }
        self.installation_plugin_release_resolution(install_key, actor)
            .await?
            .ok_or(AppError::NotFound)
    }

    pub(crate) async fn installation_plugin_release_resolution(
        &self,
        install_key: &str,
        actor: &str,
    ) -> Result<Option<PluginReleaseResolution>, AppError> {
        sqlx::query("SELECT * FROM application_plugin_release_resolutions WHERE install_key_hash=$1 AND actor=$2")
            .bind(install_key).bind(actor).fetch_optional(&self.pool).await?
            .map(resolution).transpose()
    }
}

fn resolution(row: AnyRow) -> Result<PluginReleaseResolution, AppError> {
    let json: String = row.try_get("resolved_json")?;
    if json.len() > 65536 {
        return Err(AppError::Forbidden);
    }
    Ok(PluginReleaseResolution {
        id: row.try_get("id")?,
        operation_key: row.try_get("operation_key")?,
        base_revision: row.try_get("base_revision")?,
        retained: row
            .try_get::<Option<String>, _>("retained_json")?
            .map(|json| serde_json::from_str(&json).map_err(|_| AppError::Internal))
            .transpose()?,
        release: serde_json::from_str(&json).map_err(|_| AppError::Internal)?,
    })
}
