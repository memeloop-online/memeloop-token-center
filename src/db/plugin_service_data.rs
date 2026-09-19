use super::*;

const MAX_PLUGIN_ID_BYTES: usize = 64;
const MAX_ENDPOINT_ID_BYTES: usize = 64;
const MAX_LEASE_OWNER_BYTES: usize = 128;
const MAX_SOURCE_BYTES: usize = 64;
const MAX_ORIGIN_BYTES: usize = 2_048;
const MAX_DATA_JSON_BYTES: usize = 1024 * 1024;
const MAX_LEASE_MILLIS: i64 = 10 * 60 * 1000;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PluginServiceDataRefreshErrorCode {
    Timeout,
    Network,
    HttpStatus,
    ContentType,
    BodyLimit,
    InvalidJson,
    SchemaValidation,
    ComponentExecution,
    ComponentOutput,
    Database,
}

impl PluginServiceDataRefreshErrorCode {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Timeout => "timeout",
            Self::Network => "network",
            Self::HttpStatus => "http_status",
            Self::ContentType => "content_type",
            Self::BodyLimit => "body_limit",
            Self::InvalidJson => "invalid_json",
            Self::SchemaValidation => "schema_validation",
            Self::ComponentExecution => "component_execution",
            Self::ComponentOutput => "component_output",
            Self::Database => "database",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PluginServiceDataSnapshot {
    pub(crate) data_json: Option<String>,
    pub(crate) source: Option<String>,
    pub(crate) origin: Option<String>,
    pub(crate) fetched_at: Option<i64>,
    pub(crate) last_attempt_at: i64,
    pub(crate) next_attempt_at: i64,
    pub(crate) consecutive_failures: i64,
    pub(crate) last_error_code: Option<String>,
}

impl Database {
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn claim_plugin_service_data_refresh(
        &self,
        runtime_revision: i64,
        plugin_id: &str,
        endpoint_id: &str,
        endpoint_revision: &str,
        owner: &str,
        now: i64,
        lease_ms: i64,
        initial_next_at: i64,
    ) -> Result<bool, AppError> {
        validate_identity(runtime_revision, plugin_id, endpoint_id, endpoint_revision)?;
        validate_bounded_token(owner, MAX_LEASE_OWNER_BYTES, "service data lease owner")?;
        if now < 0 || initial_next_at < 0 || !(1..=MAX_LEASE_MILLIS).contains(&lease_ms) {
            return Err(AppError::BadRequest(
                "invalid plugin service data refresh schedule".into(),
            ));
        }
        let lease_until = now.checked_add(lease_ms).ok_or_else(|| {
            AppError::BadRequest("invalid plugin service data refresh schedule".into())
        })?;
        sqlx::query(
            "INSERT INTO plugin_service_data_snapshots
                (runtime_revision, plugin_id, endpoint_id, endpoint_revision, next_attempt_at)
             VALUES ($1, $2, $3, $4, $5)
             ON CONFLICT(runtime_revision, plugin_id, endpoint_id, endpoint_revision) DO NOTHING",
        )
        .bind(runtime_revision)
        .bind(plugin_id)
        .bind(endpoint_id)
        .bind(endpoint_revision)
        .bind(initial_next_at)
        .execute(&self.pool)
        .await?;
        let changed = sqlx::query(
            "UPDATE plugin_service_data_snapshots SET
                last_attempt_at=$1, lease_owner=$2, lease_until=$3
             WHERE runtime_revision=$4 AND plugin_id=$5 AND endpoint_id=$6
               AND endpoint_revision=$7 AND next_attempt_at<=$1 AND lease_until<=$1",
        )
        .bind(now)
        .bind(owner)
        .bind(lease_until)
        .bind(runtime_revision)
        .bind(plugin_id)
        .bind(endpoint_id)
        .bind(endpoint_revision)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_plugin_service_data_refresh_success(
        &self,
        runtime_revision: i64,
        plugin_id: &str,
        endpoint_id: &str,
        endpoint_revision: &str,
        owner: &str,
        data_json: &str,
        source: &str,
        origin: &str,
        fetched_at: i64,
        next_attempt_at: i64,
    ) -> Result<bool, AppError> {
        validate_identity(runtime_revision, plugin_id, endpoint_id, endpoint_revision)?;
        validate_bounded_token(owner, MAX_LEASE_OWNER_BYTES, "service data lease owner")?;
        validate_bounded_token(source, MAX_SOURCE_BYTES, "service data source")?;
        if origin.is_empty() || origin.len() > MAX_ORIGIN_BYTES {
            return Err(AppError::BadRequest(
                "invalid plugin service data origin".into(),
            ));
        }
        if data_json.len() > MAX_DATA_JSON_BYTES
            || serde_json::from_str::<serde_json::Value>(data_json).is_err()
        {
            return Err(AppError::BadRequest(
                "invalid plugin service data snapshot".into(),
            ));
        }
        if fetched_at < 0 || next_attempt_at < fetched_at {
            return Err(AppError::BadRequest(
                "invalid plugin service data refresh schedule".into(),
            ));
        }
        let changed = sqlx::query(
            "UPDATE plugin_service_data_snapshots SET
                data_json=$1, source=$2, origin=$3, fetched_at=$4,
                next_attempt_at=$5, consecutive_failures=0, last_error_code=NULL,
                lease_owner=NULL, lease_until=0
             WHERE runtime_revision=$6 AND plugin_id=$7 AND endpoint_id=$8
               AND endpoint_revision=$9 AND lease_owner=$10 AND lease_until>$4",
        )
        .bind(data_json)
        .bind(source)
        .bind(origin)
        .bind(fetched_at)
        .bind(next_attempt_at)
        .bind(runtime_revision)
        .bind(plugin_id)
        .bind(endpoint_id)
        .bind(endpoint_revision)
        .bind(owner)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn complete_plugin_service_data_refresh_failure(
        &self,
        runtime_revision: i64,
        plugin_id: &str,
        endpoint_id: &str,
        endpoint_revision: &str,
        owner: &str,
        error_code: PluginServiceDataRefreshErrorCode,
        attempted_at: i64,
        next_attempt_at: i64,
    ) -> Result<bool, AppError> {
        validate_identity(runtime_revision, plugin_id, endpoint_id, endpoint_revision)?;
        validate_bounded_token(owner, MAX_LEASE_OWNER_BYTES, "service data lease owner")?;
        if attempted_at < 0 || next_attempt_at < attempted_at {
            return Err(AppError::BadRequest(
                "invalid plugin service data refresh schedule".into(),
            ));
        }
        let changed = sqlx::query(
            "UPDATE plugin_service_data_snapshots SET
                last_attempt_at=CASE WHEN last_attempt_at>$1 THEN last_attempt_at ELSE $1 END,
                next_attempt_at=$2,
                consecutive_failures=CASE
                    WHEN consecutive_failures>=1000000 THEN 1000000
                    ELSE consecutive_failures+1
                END,
                last_error_code=$3, lease_owner=NULL, lease_until=0
             WHERE runtime_revision=$4 AND plugin_id=$5 AND endpoint_id=$6
               AND endpoint_revision=$7 AND lease_owner=$8 AND lease_until>$1",
        )
        .bind(attempted_at)
        .bind(next_attempt_at)
        .bind(error_code.as_str())
        .bind(runtime_revision)
        .bind(plugin_id)
        .bind(endpoint_id)
        .bind(endpoint_revision)
        .bind(owner)
        .execute(&self.pool)
        .await?
        .rows_affected();
        Ok(changed == 1)
    }

    pub(crate) async fn plugin_service_data_snapshot(
        &self,
        runtime_revision: i64,
        plugin_id: &str,
        endpoint_id: &str,
        endpoint_revision: &str,
    ) -> Result<Option<PluginServiceDataSnapshot>, AppError> {
        validate_identity(runtime_revision, plugin_id, endpoint_id, endpoint_revision)?;
        let row = sqlx::query(
            "SELECT data_json, source, origin, fetched_at,
                    last_attempt_at, next_attempt_at, consecutive_failures,
                    last_error_code
             FROM plugin_service_data_snapshots
             WHERE runtime_revision=$1 AND plugin_id=$2 AND endpoint_id=$3
               AND endpoint_revision=$4",
        )
        .bind(runtime_revision)
        .bind(plugin_id)
        .bind(endpoint_id)
        .bind(endpoint_revision)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        Ok(Some(PluginServiceDataSnapshot {
            data_json: row.try_get("data_json")?,
            source: row.try_get("source")?,
            origin: row.try_get("origin")?,
            fetched_at: row.try_get("fetched_at")?,
            last_attempt_at: row.try_get("last_attempt_at")?,
            next_attempt_at: row.try_get("next_attempt_at")?,
            consecutive_failures: row.try_get("consecutive_failures")?,
            last_error_code: row.try_get("last_error_code")?,
        }))
    }
}

fn validate_identity(
    runtime_revision: i64,
    plugin_id: &str,
    endpoint_id: &str,
    endpoint_revision: &str,
) -> Result<(), AppError> {
    if runtime_revision < 0
        || endpoint_revision.len() != 64
        || !endpoint_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(AppError::BadRequest(
            "invalid plugin service data revision".into(),
        ));
    }
    validate_bounded_token(plugin_id, MAX_PLUGIN_ID_BYTES, "plugin id")?;
    validate_bounded_token(
        endpoint_id,
        MAX_ENDPOINT_ID_BYTES,
        "service data endpoint id",
    )
}

fn validate_bounded_token(value: &str, maximum: usize, field: &str) -> Result<(), AppError> {
    if value.is_empty()
        || value.len() > maximum
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(AppError::BadRequest(format!("invalid {field}")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppState, config::Config};

    #[tokio::test]
    async fn sqlite_service_data_snapshot_leases_preserve_last_good() {
        let directory = tempfile::tempdir().unwrap();
        verify(Config::for_test(format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("service-data.db").display()
        )))
        .await;
    }

    #[tokio::test]
    async fn postgres_service_data_snapshot_leases_preserve_last_good() {
        if let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") {
            verify(Config::for_test(url)).await;
        }
    }

    async fn verify(config: Config) {
        let state = AppState::initialize(config).await.unwrap();
        let db = &state.db;
        let plugin_id = format!("service-data-{}", Uuid::now_v7());
        let endpoint_id = "health";
        let runtime_revision = 7;
        let endpoint_revision = "a".repeat(64);
        let next_endpoint_revision = "b".repeat(64);
        let future_endpoint = "future";

        assert!(
            db.plugin_service_data_snapshot(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
            )
            .await
            .unwrap()
            .is_none()
        );
        assert!(
            !db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                future_endpoint,
                &endpoint_revision,
                "future-lease",
                50,
                100,
                100,
            )
            .await
            .unwrap()
        );
        let future = db
            .plugin_service_data_snapshot(
                runtime_revision,
                &plugin_id,
                future_endpoint,
                &endpoint_revision,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(future.last_attempt_at, 0);
        assert_eq!(future.next_attempt_at, 100);
        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-a",
                100,
                50,
                100,
            )
            .await
            .unwrap()
        );
        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &next_endpoint_revision,
                "contract-lease",
                101,
                50,
                101,
            )
            .await
            .unwrap(),
            "a new endpoint contract owns an independent lease"
        );
        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision + 1,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "runtime-lease",
                101,
                50,
                101,
            )
            .await
            .unwrap(),
            "a new runtime revision owns an independent lease"
        );
        assert!(
            !db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-b",
                101,
                50,
                101,
            )
            .await
            .unwrap()
        );
        assert!(
            db.complete_plugin_service_data_refresh_failure(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-a",
                PluginServiceDataRefreshErrorCode::Timeout,
                110,
                200,
            )
            .await
            .unwrap()
        );
        let never_succeeded = db
            .plugin_service_data_snapshot(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(never_succeeded.data_json, None);
        assert_eq!(never_succeeded.fetched_at, None);
        assert_eq!(never_succeeded.consecutive_failures, 1);

        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-b",
                200,
                50,
                200,
            )
            .await
            .unwrap()
        );
        assert!(
            db.complete_plugin_service_data_refresh_success(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-b",
                r#"{"status":"healthy"}"#,
                "component_v1",
                "https://health.example",
                210,
                300,
            )
            .await
            .unwrap()
        );
        let last_good = db
            .plugin_service_data_snapshot(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            last_good.data_json.as_deref(),
            Some(r#"{"status":"healthy"}"#)
        );
        assert_eq!(last_good.consecutive_failures, 0);
        assert_eq!(last_good.next_attempt_at, 300);

        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-c",
                300,
                50,
                300,
            )
            .await
            .unwrap()
        );
        assert!(
            db.complete_plugin_service_data_refresh_failure(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-c",
                PluginServiceDataRefreshErrorCode::ComponentExecution,
                310,
                510,
            )
            .await
            .unwrap()
        );
        let failed = db
            .plugin_service_data_snapshot(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(failed.data_json, last_good.data_json);
        assert_eq!(failed.fetched_at, Some(210));
        assert_eq!(failed.consecutive_failures, 1);
        assert_eq!(
            failed.last_error_code.as_deref(),
            Some("component_execution")
        );
        assert_eq!(failed.next_attempt_at, 510);

        assert!(
            !db.complete_plugin_service_data_refresh_failure(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-b",
                PluginServiceDataRefreshErrorCode::Network,
                311,
                520,
            )
            .await
            .unwrap()
        );
        assert!(
            !db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-d",
                509,
                50,
                509,
            )
            .await
            .unwrap()
        );
        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-d",
                510,
                50,
                510,
            )
            .await
            .unwrap()
        );
        assert!(
            db.claim_plugin_service_data_refresh(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-e",
                561,
                50,
                561,
            )
            .await
            .unwrap()
        );
        assert!(
            !db.complete_plugin_service_data_refresh_failure(
                runtime_revision,
                &plugin_id,
                endpoint_id,
                &endpoint_revision,
                "lease-d",
                PluginServiceDataRefreshErrorCode::Database,
                562,
                600,
            )
            .await
            .unwrap()
        );
    }
}
