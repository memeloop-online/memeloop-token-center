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
        let json = serde_json::to_string(release).map_err(|_| AppError::internal("plugin_release_resolutions_save_plugin_release_resolution_1"))?;
        let retained = retained
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::internal("plugin_release_resolutions_save_plugin_release_resolution_2"))?;
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
            .map(|json| serde_json::from_str(&json).map_err(|_| AppError::internal("plugin_release_resolutions_resolution_1")))
            .transpose()?,
        release: serde_json::from_str(&json).map_err(|_| AppError::internal("plugin_release_resolutions_resolution_2"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(reference_digest: char) -> ResolvedPluginRelease {
        serde_json::from_value(serde_json::json!({
            "release_url":"https://github.com/example/plugin/releases/download/v1/release.json",
            "descriptor_sha256":format!("sha256:{}", reference_digest.to_string().repeat(64)),
            "source":"ghcr.io/example/plugin",
            "descriptor":{"format_version":1,
                "reference":format!("ghcr.io/example/plugin@sha256:{}", reference_digest.to_string().repeat(64)),
                "signer":{"issuer":"https://token.actions.githubusercontent.com",
                    "identity":"https://github.com/example/plugin/.github/workflows/publish.yml@refs/tags/v1"}}
        })).unwrap()
    }

    #[tokio::test]
    async fn release_confirmation_freezes_bytes_actor_and_single_installation() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("release.db").display()
        );
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        let original = release('a');
        let first = db
            .save_plugin_release_resolution("resolve-key", "operator-a", &original, 0, None)
            .await
            .unwrap();
        let concurrent = db
            .save_plugin_release_resolution("resolve-key", "operator-a", &release('b'), 9, None)
            .await
            .unwrap();
        assert_eq!(first.id, concurrent.id);
        assert_eq!(
            concurrent.release.descriptor_sha256,
            original.descriptor_sha256
        );
        assert_eq!(concurrent.base_revision, 0);
        assert!(
            db.confirm_plugin_release_resolution(
                &first.id,
                &original.descriptor_sha256,
                "operator-b",
                "install-key"
            )
            .await
            .is_err()
        );
        assert!(
            db.confirm_plugin_release_resolution(
                &first.id,
                &release('b').descriptor_sha256,
                "operator-a",
                "install-key"
            )
            .await
            .is_err()
        );
        db.confirm_plugin_release_resolution(
            &first.id,
            &original.descriptor_sha256,
            "operator-a",
            "install-key",
        )
        .await
        .unwrap();
        assert!(
            db.confirm_plugin_release_resolution(
                &first.id,
                &original.descriptor_sha256,
                "operator-a",
                "other-install"
            )
            .await
            .is_err()
        );
        drop(db);
        let db = Database::connect(&url).await.unwrap();
        let restored = db
            .installation_plugin_release_resolution("install-key", "operator-a")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(restored.id, first.id);
        let response = serde_json::to_value(&restored).unwrap();
        assert!(response.get("retained").is_none());
        assert!(response.get("operation_key").is_none());
        assert_eq!(response["base_revision"], 0);
    }

    #[tokio::test]
    async fn release_publication_cannot_rebase_to_a_different_expected_revision() {
        let directory = tempfile::tempdir().unwrap();
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("publication.db").display()
        );
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        let source = release('a');
        let resolution = db
            .save_plugin_release_resolution("resolve", "operator", &source, 0, None)
            .await
            .unwrap();
        db.confirm_plugin_release_resolution(
            &resolution.id,
            &source.descriptor_sha256,
            "operator",
            "install",
        )
        .await
        .unwrap();
        let (record, _) = db
            .begin_plugin_installation(
                "fresh",
                &serde_json::json!([source.descriptor.reference]),
                "request",
                "install",
                "operator",
                unix_millis() + 270000,
            )
            .await
            .unwrap();
        db.finish_plugin_installation(
            &record.id,
            &record.attempt_id,
            Some(("review", &serde_json::json!({"plugins":[]}))),
        )
        .await
        .unwrap();
        db.stage_application_plugin_candidate("fresh", "identity", "contract")
            .await
            .unwrap();
        db.register_plugin_installation(&record.id, "review", "operator")
            .await
            .unwrap();
        let replay = db
            .replay_plugin_installation("install", "request", "operator")
            .await
            .unwrap()
            .unwrap();
        assert_eq!(replay.base_revision, Some(0));
        assert!(matches!(
            db.publish_application_plugin(
                "fresh",
                1,
                "reload",
                "bad-rebase",
                "bad-hash",
                "operator"
            )
            .await,
            Err(AppError::Conflict(_))
        ));
        db.publish_application_plugin("fresh", 0, "initial", "initial", "initial-hash", "operator")
            .await
            .unwrap();
        // Even a direct database caller cannot approve a snapshot after its
        // base head has changed. Publication is separately guarded in its tx.
        assert!(matches!(
            db.register_plugin_installation(&record.id, "review", "operator")
                .await,
            Err(AppError::Conflict(_))
        ));
    }
}
