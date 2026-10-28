use super::*;
use crate::http_settings::{HttpSettings, HttpSettingsDocument};
use std::sync::Arc;

pub(crate) const HTTP_SETTINGS_NAMESPACE: &str = "system-settings";
pub(crate) const HTTP_SETTINGS_KEY: &str = "http";

impl Database {
    pub(crate) async fn refresh_http_settings(&self) -> Result<(), AppError> {
        let document = self
            .plugin_kv_get(HTTP_SETTINGS_NAMESPACE, HTTP_SETTINGS_KEY)
            .await?;
        let document = decode(document.as_deref())?;
        self.http_clients.publish(document);
        Ok(())
    }

    pub(crate) async fn replace_http_settings(
        &self,
        expected_revision: u64,
        settings: HttpSettings,
        actor: Option<Uuid>,
    ) -> Result<HttpSettingsDocument, AppError> {
        settings
            .validate()
            .map_err(|message| AppError::BadRequest(message.into()))?;
        let mut transaction = self.begin_write_transaction().await?;
        let row = sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id = $1 AND key = $2")
            .bind(HTTP_SETTINGS_NAMESPACE)
            .bind(HTTP_SETTINGS_KEY)
            .fetch_optional(&mut *transaction)
            .await?;
        let previous: Option<Vec<u8>> = row.map(|row| row.try_get("value")).transpose()?;
        let current = decode(previous.as_deref())?;
        if current.revision != expected_revision {
            return Err(conflict());
        }
        let document = HttpSettingsDocument {
            revision: expected_revision
                .checked_add(1)
                .filter(|value| *value <= 9_007_199_254_740_991)
                .ok_or_else(conflict)?,
            settings,
        };
        let bytes = serde_json::to_vec(&document)
            .map_err(|_| AppError::internal("http_settings_encode"))?;
        let now = unix_millis();
        let changed = if let Some(previous) = previous {
            sqlx::query("UPDATE plugin_kv SET value=$1, updated_at=$2 WHERE plugin_id=$3 AND key=$4 AND value=$5")
                .bind(&bytes).bind(now).bind(HTTP_SETTINGS_NAMESPACE).bind(HTTP_SETTINGS_KEY).bind(previous)
                .execute(&mut *transaction).await?.rows_affected()
        } else {
            sqlx::query("INSERT INTO plugin_kv (plugin_id,key,value,updated_at) VALUES ($1,$2,$3,$4) ON CONFLICT(plugin_id,key) DO NOTHING")
                .bind(HTTP_SETTINGS_NAMESPACE).bind(HTTP_SETTINGS_KEY).bind(&bytes).bind(now)
                .execute(&mut *transaction).await?.rows_affected()
        };
        if changed != 1 {
            return Err(conflict());
        }
        let audit = serde_json::to_vec(&serde_json::json!({
            "actor_service_id": actor, "document": document, "created_at": now,
        }))
        .map_err(|_| AppError::internal("http_settings_audit"))?;
        sqlx::query("INSERT INTO plugin_kv (plugin_id,key,value,updated_at) VALUES ('http-settings-audit',$1,$2,$3)")
            .bind(Uuid::now_v7().to_string()).bind(audit).bind(now)
            .execute(&mut *transaction).await?;
        transaction.commit().await?;
        self.http_clients.publish(document.clone());
        Ok(document)
    }

    pub(crate) fn start_http_settings_refresh(&self) {
        let factory = Arc::downgrade(&self.http_clients);
        let pool = self.pool.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(5));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                ticker.tick().await;
                let Some(factory) = factory.upgrade() else {
                    break;
                };
                let result = async {
                    let row =
                        sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id=$1 AND key=$2")
                            .bind(HTTP_SETTINGS_NAMESPACE)
                            .bind(HTTP_SETTINGS_KEY)
                            .fetch_optional(&pool)
                            .await?;
                    let bytes: Option<Vec<u8>> = row.map(|row| row.try_get("value")).transpose()?;
                    decode(bytes.as_deref())
                }
                .await;
                match result {
                    Ok(document) => factory.publish(document),
                    Err(_) => {
                        tracing::warn!("HTTP settings refresh failed; retaining installed revision")
                    }
                }
            }
        });
    }
}

fn conflict() -> AppError {
    AppError::Conflict("HTTP settings changed; reload before saving".into())
}

fn decode(bytes: Option<&[u8]>) -> Result<HttpSettingsDocument, AppError> {
    let document: HttpSettingsDocument = bytes
        .map(serde_json::from_slice)
        .transpose()
        .map_err(|_| AppError::internal("http_settings_decode"))?
        .unwrap_or_default();
    document
        .settings
        .validate()
        .map_err(|_| AppError::internal("http_settings_invalid"))?;
    Ok(document)
}
