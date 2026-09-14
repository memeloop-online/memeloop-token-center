use super::super::*;
use super::accounts::{upstream_account_view_with_transport, validate_upstream_account_name};
use super::native_oauth_imports::lock_native_oauth_import_tenant;

#[derive(Clone)]
pub struct NativeCursorImportInput {
    pub tenant_external_id: String,
    pub source_identity_hash: String,
    pub provider_subject_hash: String,
    pub source_document_sha256: String,
    pub payload_digest: String,
    pub source_layout: String,
    pub account_name: String,
    pub expected_account_id: Option<Uuid>,
    pub expected_document_sha256: Option<String>,
    pub expected_credential_generation: Option<i64>,
    pub credential: UpstreamCredential,
}

pub struct NativeCursorImportResult {
    pub account: UpstreamAccountView,
    pub disposition: &'static str,
}

impl Database {
    /// Import only provider-native OAuth material. A replay never overwrites a
    /// newer generation installed by the managed refresh owner.
    pub async fn import_native_cursor_source(
        &self,
        input: NativeCursorImportInput,
        key_material: &[u8],
    ) -> Result<NativeCursorImportResult, AppError> {
        validate_upstream_account_name(&input.account_name)?;
        input.credential.validate(i64::MIN)?;
        let subject = crate::oauth::cursor_account_id(&input.credential)?;
        let now = unix_millis();
        let mut tx = self.begin_write_transaction().await?;
        sqlx::query("INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING")
            .bind(Uuid::now_v7().to_string()).bind(&input.tenant_external_id).bind(now)
            .execute(&mut *tx).await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;
        lock_native_oauth_import_tenant(self.backend, &mut tx, &tenant_id).await?;
        let existing = cursor_source_row(
            self.backend,
            &mut tx,
            &tenant_id,
            &input.source_identity_hash,
        )
        .await?;
        let (account_id, disposition) = if let Some(row) = existing {
            let id: String = row.try_get("id")?;
            let generation: i64 = row.try_get("credential_generation")?;
            let updated_at: i64 = row.try_get("updated_at")?;
            let document: String = row.try_get("import_source_document_sha256")?;
            if row.try_get::<String, _>("driver")? != "cursor"
                || row.try_get::<String, _>("auth_kind")? != "oauth"
                || row.try_get::<Option<String>, _>("oauth_driver")?.as_deref() != Some("cursor")
                || row.try_get::<String, _>("provider_subject_hash")? != input.provider_subject_hash
            {
                return Err(AppError::Conflict(
                    "native Cursor source identity changed".into(),
                ));
            }
            let current = open_credential(
                &row.try_get::<String, _>("credential_ciphertext")?,
                key_material,
            )?;
            if crate::oauth::cursor_account_id(&current)? != subject {
                return Err(AppError::Conflict(
                    "native Cursor account identity changed".into(),
                ));
            }
            if document == input.source_document_sha256
                && row.try_get::<String, _>("payload_digest")? == input.payload_digest
            {
                // Old request retries may carry a pre-refresh generation. They
                // return the current one explicitly, and never reinstall tokens.
                if input
                    .expected_account_id
                    .is_some_and(|expected| expected.to_string() != id)
                {
                    return Err(AppError::Conflict(
                        "native Cursor source is bound to another account".into(),
                    ));
                }
                (id, "replayed")
            } else {
                if input
                    .expected_account_id
                    .map(|value| value.to_string())
                    .as_deref()
                    != Some(id.as_str())
                    || input.expected_document_sha256.as_deref() != Some(document.as_str())
                    || input.expected_credential_generation != Some(generation)
                {
                    return Err(AppError::Conflict(
                        "native Cursor source changed; reload its account and generation".into(),
                    ));
                }
                let lease_sql = match self.backend {
                    DatabaseBackend::PostgreSql => {
                        "SELECT account_id FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND credential_generation = $2 FOR UPDATE"
                    }
                    DatabaseBackend::Sqlite => {
                        "SELECT account_id FROM upstream_oauth_refresh_leases WHERE account_id = $1 AND credential_generation = $2"
                    }
                };
                if sqlx::query(lease_sql)
                    .bind(&id)
                    .bind(generation)
                    .fetch_optional(&mut *tx)
                    .await?
                    .is_some()
                {
                    return Err(AppError::Conflict(
                        "native Cursor refresh owns this generation; retry after it completes"
                            .into(),
                    ));
                }
                // Transport settings are independently managed. Omitting a
                // source proxy must not erase an already installed account proxy.
                let credential = input.credential.clone().preserve_proxy_from(&current);
                let next = generation.checked_add(1).ok_or(AppError::Internal)?;
                let revoked = sqlx::query("UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL")
                    .bind(now).bind(&id).bind(generation).execute(&mut *tx).await?;
                if revoked.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "native Cursor credential changed".into(),
                    ));
                }
                sqlx::query("INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)")
                    .bind(Uuid::now_v7().to_string()).bind(&id).bind(next).bind(seal_credential(&credential, key_material)?)
                    .bind(credential.expires_at()).bind(now).execute(&mut *tx).await?;
                let changed = sqlx::query("UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2 WHERE id = $3 AND tenant_id = $4 AND credential_generation = $5 AND updated_at = $6")
                    .bind(next).bind(now.max(updated_at.saturating_add(1))).bind(&id).bind(&tenant_id).bind(generation).bind(updated_at)
                    .execute(&mut *tx).await?;
                if changed.rows_affected() != 1 {
                    return Err(AppError::Conflict("native Cursor account changed".into()));
                }
                sqlx::query("UPDATE native_oauth_source_bindings SET source_document_sha256 = $1, payload_digest = $2, source_layout = $3, updated_at = $4 WHERE tenant_id = $5 AND provider_driver = 'cursor' AND source_identity_hash = $6 AND upstream_account_id = $7 AND source_document_sha256 = $8")
                    .bind(&input.source_document_sha256).bind(&input.payload_digest).bind(&input.source_layout).bind(now)
                    .bind(&tenant_id).bind(&input.source_identity_hash).bind(&id).bind(document).execute(&mut *tx).await?;
                (id, "rotated")
            }
        } else {
            if input.expected_account_id.is_some()
                || input.expected_document_sha256.is_some()
                || input.expected_credential_generation.is_some()
            {
                return Err(AppError::Conflict(
                    "native Cursor source binding no longer exists".into(),
                ));
            }
            // A renamed source path must not create a duplicate provider subject.
            if sqlx::query("SELECT upstream_account_id FROM native_oauth_source_bindings WHERE tenant_id = $1 AND provider_driver = 'cursor' AND provider_subject_hash = $2")
                .bind(&tenant_id).bind(&input.provider_subject_hash).fetch_optional(&mut *tx).await?.is_some() {
                return Err(AppError::Conflict("native Cursor identity already has a source binding".into()));
            }
            let id = Uuid::now_v7().to_string();
            sqlx::query("INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, oauth_driver, oauth_refresh_url, created_at, updated_at) VALUES ($1, $2, $3, 'cursor', 'oauth', $4, 'active', 1, $1, 'cursor', $5, $6, $6)")
                .bind(&id).bind(&tenant_id).bind(&input.account_name)
                .bind(serde_json::json!({"base_url":"https://api2.cursor.sh", "network_scope":"public", "reservation_token_bounds":{}}).to_string())
                .bind(crate::oauth::DEFAULT_CURSOR_REFRESH_URL).bind(now).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 1, $3, $4, $5)")
                .bind(Uuid::now_v7().to_string()).bind(&id).bind(seal_credential(&input.credential, key_material)?)
                .bind(input.credential.expires_at()).bind(now).execute(&mut *tx).await?;
            sqlx::query("INSERT INTO native_oauth_source_bindings (tenant_id, provider_driver, source_identity_hash, provider_subject_hash, source_document_sha256, payload_digest, source_layout, upstream_account_id, created_at, updated_at) VALUES ($1, 'cursor', $2, $3, $4, $5, $6, $7, $8, $8)")
                .bind(&tenant_id).bind(&input.source_identity_hash).bind(&input.provider_subject_hash)
                .bind(&input.source_document_sha256).bind(&input.payload_digest).bind(&input.source_layout).bind(&id).bind(now)
                .execute(&mut *tx).await?;
            (id, "created")
        };
        let row = cursor_source_row(
            self.backend,
            &mut tx,
            &tenant_id,
            &input.source_identity_hash,
        )
        .await?
        .ok_or(AppError::Internal)?;
        let account = upstream_account_view_with_transport(row, key_material)?;
        debug_assert_eq!(account.id.to_string(), account_id);
        tx.commit().await?;
        Ok(NativeCursorImportResult {
            account,
            disposition,
        })
    }
}

async fn cursor_source_row(
    backend: DatabaseBackend,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    tenant_id: &str,
    source_identity: &str,
) -> Result<Option<sqlx::any::AnyRow>, AppError> {
    let sql = "SELECT b.provider_subject_hash, b.payload_digest, b.source_identity_hash AS import_source_identity_hash, b.source_document_sha256 AS import_source_document_sha256, a.*, t.external_id AS tenant_external_id, c.expires_at, c.credential_ciphertext, (SELECT COUNT(DISTINCT r.model_route_id) FROM model_route_upstream_accounts r WHERE r.tenant_id = a.tenant_id AND r.upstream_account_id = a.id) AS route_count FROM native_oauth_source_bindings b JOIN upstream_accounts a ON a.id = b.upstream_account_id AND a.tenant_id = b.tenant_id JOIN tenants t ON t.id = a.tenant_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE b.tenant_id = $1 AND b.provider_driver = 'cursor' AND b.source_identity_hash = $2";
    let sql = if backend == DatabaseBackend::PostgreSql {
        format!("{sql} FOR UPDATE OF b, a, c")
    } else {
        sql.to_owned()
    };
    Ok(sqlx::query(&sql)
        .bind(tenant_id)
        .bind(source_identity)
        .fetch_optional(&mut **tx)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(tenant: &str) -> NativeCursorImportInput {
        NativeCursorImportInput {
            tenant_external_id: tenant.into(),
            source_identity_hash: "1".repeat(64),
            provider_subject_hash: "2".repeat(64),
            source_document_sha256: "3".repeat(64),
            payload_digest: "4".repeat(64),
            source_layout: "cursor-auth-legacy-v1".into(),
            account_name: "Cursor Imported".into(),
            expected_account_id: None,
            expected_document_sha256: None,
            expected_credential_generation: None,
            credential: UpstreamCredential::OAuth {
                access_token: "synthetic-native-cursor-access".into(),
                refresh_token: Some("synthetic-native-cursor-refresh".into()),
                expires_at: Some(1000),
                header: "authorization".into(),
                prefix: "Bearer ".into(),
                adapter_state: Some(
                    serde_json::json!({"schema":"cursor-oauth-v1","account_id":"cursor-source-fixture"}),
                ),
                proxy_url: Some("socks5h://192.168.1.20:1080".into()),
                proxy_network_scope: Some(crate::network::OutboundScope::Private),
            },
        }
    }

    async fn contract(db: &Database, tenant: &str) {
        let key = b"cursor-native-source-test-encryption";
        let original = input(tenant);
        let created = db
            .import_native_cursor_source(original.clone(), key)
            .await
            .unwrap();
        assert_eq!(created.disposition, "created");
        assert_eq!(created.account.credential_generation, 1);
        assert_eq!(created.account.credential_expires_at, Some(1000));
        assert!(created.account.can_refresh && created.account.can_reauthorize);
        assert_eq!(created.account.route_count, 0);
        let id = created.account.id;
        let mut changed = original.clone();
        changed.source_document_sha256 = "5".repeat(64);
        changed.payload_digest = "6".repeat(64);
        // Changed sources cannot be installed without exact account/document/generation.
        assert!(matches!(
            db.import_native_cursor_source(changed.clone(), key).await,
            Err(AppError::Conflict(_))
        ));
        changed.expected_account_id = Some(id);
        changed.expected_document_sha256 = Some(original.source_document_sha256.clone());
        changed.expected_credential_generation = Some(1);
        sqlx::query("INSERT INTO upstream_oauth_refresh_leases (account_id, credential_generation, idempotency_key, lease_expires_at, created_at, request_started_at) VALUES ($1, 1, $2, 1, 0, 0)")
            .bind(id.to_string()).bind(Uuid::now_v7().to_string()).execute(&db.pool).await.unwrap();
        assert!(
            matches!(
                db.import_native_cursor_source(changed.clone(), key).await,
                Err(AppError::Conflict(_))
            ),
            "even an expired lease with an uncertain refresh outcome owns its generation"
        );
        let unchanged = db.list_upstream_accounts(tenant).await.unwrap();
        assert_eq!(unchanged[0].credential_generation, 1);
        assert_eq!(
            unchanged[0].import_source_document_sha256.as_deref(),
            Some(original.source_document_sha256.as_str())
        );
        sqlx::query("DELETE FROM upstream_oauth_refresh_leases WHERE account_id = $1")
            .bind(id.to_string())
            .execute(&db.pool)
            .await
            .unwrap();
        if let UpstreamCredential::OAuth {
            proxy_url,
            proxy_network_scope,
            ..
        } = &mut changed.credential
        {
            *proxy_url = None;
            *proxy_network_scope = None;
        }
        let rotated = db
            .import_native_cursor_source(changed.clone(), key)
            .await
            .unwrap();
        assert_eq!(rotated.disposition, "rotated");
        assert_eq!(rotated.account.id, id);
        assert_eq!(rotated.account.credential_generation, 2);
        assert!(
            rotated.account.has_proxy,
            "missing source proxy retains runtime proxy"
        );
        // A repeated rotate request carrying generation 1 must not rotate again.
        let replay = db
            .import_native_cursor_source(changed.clone(), key)
            .await
            .unwrap();
        assert_eq!(replay.disposition, "replayed");
        assert_eq!(replay.account.credential_generation, 2);
        // A source path rename must not create another upstream identity.
        let mut duplicate = original.clone();
        duplicate.source_identity_hash = "7".repeat(64);
        duplicate.account_name = "Cursor Duplicate".into();
        assert!(matches!(
            db.import_native_cursor_source(duplicate, key).await,
            Err(AppError::Conflict(_))
        ));
        // Simulate the existing managed lifecycle installing a later generation;
        // replay must report that live generation, not restore the source token.
        let credential = seal_credential(&changed.credential, key).unwrap();
        let mut tx = db.begin_write_transaction().await.unwrap();
        sqlx::query("UPDATE upstream_credentials SET revoked_at = 2000 WHERE upstream_account_id = $1 AND generation = 2").bind(id.to_string()).execute(&mut *tx).await.unwrap();
        sqlx::query("INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 3, $3, 1000, 2000)")
            .bind(Uuid::now_v7().to_string()).bind(id.to_string()).bind(credential).execute(&mut *tx).await.unwrap();
        sqlx::query("UPDATE upstream_accounts SET credential_generation = 3 WHERE id = $1")
            .bind(id.to_string())
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let replay = db
            .import_native_cursor_source(changed.clone(), key)
            .await
            .unwrap();
        assert_eq!(replay.account.credential_generation, 3);
        assert_eq!(replay.disposition, "replayed");
        // A stale source update must fail without changing receipt or generation.
        changed.source_document_sha256 = "8".repeat(64);
        changed.payload_digest = "9".repeat(64);
        assert!(matches!(
            db.import_native_cursor_source(changed, key).await,
            Err(AppError::Conflict(_))
        ));
        let account = db.list_upstream_accounts(tenant).await.unwrap();
        assert_eq!(account.len(), 1);
        assert_eq!(account[0].credential_generation, 3);
    }

    #[tokio::test]
    async fn sqlite_native_cursor_source_contract() {
        let directory = tempfile::tempdir().unwrap();
        let db = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("native-cursor.db").display()
        ))
        .await
        .unwrap();
        db.migrate().await.unwrap();
        contract(&db, "cursor-source-sqlite").await;
    }

    #[tokio::test]
    async fn postgres_native_cursor_source_contract() {
        let Ok(url) = std::env::var("MTC_TEST_POSTGRES_URL") else {
            return;
        };
        let db = Database::connect(&url).await.unwrap();
        db.migrate().await.unwrap();
        contract(&db, &format!("cursor-source-pg-{}", Uuid::now_v7())).await;
    }
}
