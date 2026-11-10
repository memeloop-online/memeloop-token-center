use super::*;
use crate::http_settings::{HttpSettings, HttpSettingsDocument, MAX_SAFE_INTEGER};
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
        if expected_revision > MAX_SAFE_INTEGER {
            return Err(AppError::BadRequest(
                "HTTP settings revision must be a nonnegative safe integer".into(),
            ));
        }
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
                .filter(|value| *value <= MAX_SAFE_INTEGER)
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
    if document.revision > MAX_SAFE_INTEGER {
        return Err(AppError::internal("http_settings_invalid"));
    }
    document
        .settings
        .validate()
        .map_err(|_| AppError::internal("http_settings_invalid"))?;
    Ok(document)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn assert_concurrent_writers(database_url: &str) {
        let first = Database::connect(database_url).await.unwrap();
        first.migrate().await.unwrap();
        let second = Database::connect(database_url).await.unwrap();
        let actor = Uuid::now_v7();
        for expected_revision in 0..2 {
            let first_settings: HttpSettings = serde_json::from_value(serde_json::json!({
                "defaults": {"request_timeout_ms": 1000 + expected_revision}
            }))
            .unwrap();
            let second_settings: HttpSettings = serde_json::from_value(serde_json::json!({
                "defaults": {"request_timeout_ms": 2000 + expected_revision}
            }))
            .unwrap();
            let (left, right) = tokio::join!(
                first.replace_http_settings(expected_revision, first_settings, Some(actor)),
                second.replace_http_settings(expected_revision, second_settings, Some(actor)),
            );
            let winner = match (left, right) {
                (Ok(winner), Err(AppError::Conflict(_)))
                | (Err(AppError::Conflict(_)), Ok(winner)) => winner,
                results => panic!("expected one durable CAS winner: {results:?}"),
            };
            assert_eq!(winner.revision, expected_revision + 1);
            first.refresh_http_settings().await.unwrap();
            second.refresh_http_settings().await.unwrap();
            assert_eq!(first.http_clients.snapshot().document, winner);
            assert_eq!(second.http_clients.snapshot().document, winner);
            let rows =
                sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id='http-settings-audit'")
                    .fetch_all(&first.pool)
                    .await
                    .unwrap();
            assert_eq!(rows.len() as u64, winner.revision);
            let audits: Vec<serde_json::Value> = rows
                .into_iter()
                .map(|row| serde_json::from_slice(&row.get::<Vec<u8>, _>("value")).unwrap())
                .collect();
            assert!(audits.iter().any(|audit| {
                audit["actor_service_id"] == serde_json::json!(actor)
                    && audit["document"] == serde_json::to_value(&winner).unwrap()
            }));
        }
        first
            .plugin_kv_put(
                HTTP_SETTINGS_NAMESPACE,
                HTTP_SETTINGS_KEY,
                &serde_json::to_vec(&serde_json::json!({
                    "revision": MAX_SAFE_INTEGER + 1, "settings": {}
                }))
                .unwrap(),
            )
            .await
            .unwrap();
        assert!(second.refresh_http_settings().await.is_err());
        assert_eq!(second.http_clients.snapshot().document.revision, 2);
        first.close().await;
        second.close().await;
    }

    #[tokio::test]
    async fn sqlite_http_settings_concurrent_cas_and_audit_are_atomic() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("http-settings-cas.db").display()
        );
        assert_concurrent_writers(&database_url).await;
    }

    #[tokio::test]
    async fn postgres_http_settings_concurrent_cas_and_audit_are_atomic() {
        let Ok(database_url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let schema = format!("http_settings_{}", Uuid::now_v7().simple());
        let admin = sqlx::PgPool::connect(&database_url).await.unwrap();
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE SCHEMA {schema}")))
            .execute(&admin)
            .await
            .unwrap();
        let mut isolated_url = url::Url::parse(&database_url).unwrap();
        isolated_url
            .query_pairs_mut()
            .append_pair("options", &format!("-csearch_path={schema}"));
        assert_concurrent_writers(isolated_url.as_str()).await;
        sqlx::query(sqlx::AssertSqlSafe(format!("DROP SCHEMA {schema} CASCADE")))
            .execute(&admin)
            .await
            .unwrap();
        admin.close().await;
    }
}
