use super::super::*;
use serde::Serialize;

const CPA_MANAGED_OAUTH_IMPORT_KIND: &str = "cpa_managed_oauth";
const NATIVE_CODEX_UPGRADE_MAX_ACCOUNTS: usize = 64;

/// A compare-and-swap snapshot emitted by the review phase of the controlled
/// Codex account upgrade. It intentionally contains no account name, tenant,
/// provider configuration, or credential material.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeCodexUpgradeTarget {
    pub account_id: Uuid,
    pub expected_updated_at: i64,
    pub expected_credential_generation: i64,
    pub has_proxy: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proxy_network_scope: Option<crate::network::OutboundScope>,
}

#[derive(Clone, Debug, Serialize)]
pub struct NativeCodexUpgradeReport {
    pub upgraded_account_ids: Vec<Uuid>,
    pub already_native_account_ids: Vec<Uuid>,
}

impl Database {
    /// Read the exact, operator-selected imported Codex accounts that may be
    /// migrated. The companion apply operation requires the returned CAS
    /// values, so this endpoint is safe to use as a human review stage rather
    /// than a best-effort bulk rewrite.
    pub async fn prepare_native_codex_upgrade(
        &self,
        account_ids: &[Uuid],
        key_material: &[u8],
    ) -> Result<Vec<NativeCodexUpgradeTarget>, AppError> {
        validate_native_codex_upgrade_ids(account_ids)?;
        let mut targets = Vec::with_capacity(account_ids.len());
        for account_id in account_ids {
            self.require_managed_oauth_import(*account_id).await?;
            let (account, credential) = self
                .upstream_account_with_credential(*account_id, key_material)
                .await?;
            validate_native_codex_upgrade_candidate(&account, &credential)?;
            targets.push(NativeCodexUpgradeTarget {
                account_id: *account_id,
                expected_updated_at: account.updated_at,
                expected_credential_generation: account.credential_generation,
                has_proxy: credential.proxy().is_some(),
                proxy_network_scope: credential.proxy().map(|(_, scope)| scope),
            });
        }
        Ok(targets)
    }

    /// Atomically switch an exact allowlist of imported Codex accounts to the
    /// native provider ABI. The stable account id, credential generation,
    /// refresh state and encrypted private SOCKS metadata are retained. Only
    /// the encrypted adapter-state schema and lifecycle metadata are changed.
    pub async fn apply_native_codex_upgrade(
        &self,
        targets: &[NativeCodexUpgradeTarget],
        key_material: &[u8],
    ) -> Result<NativeCodexUpgradeReport, AppError> {
        validate_native_codex_upgrade_targets(targets)?;
        let mut tx = self.begin_write_transaction().await?;
        let mut upgraded_account_ids = Vec::new();
        let mut already_native_account_ids = Vec::new();
        for target in targets {
            let row = native_codex_upgrade_row(self, &mut tx, target.account_id).await?;
            require_native_codex_upgrade_refresh_quiescence(&mut tx, target.account_id).await?;
            let driver: String = row.try_get("driver")?;
            let config: Value = serde_json::from_str(&row.try_get::<String, _>("config_json")?)
                .map_err(|_| AppError::Internal)?;
            let credential = open_credential(
                &row.try_get::<String, _>("credential_ciphertext")?,
                key_material,
            )?;
            if driver == crate::oauth::codex_device::PROVIDER_DRIVER {
                let repair_lifecycle = validate_native_codex_account_shape(
                    &row,
                    target.account_id,
                    &config,
                    &credential,
                )?;
                let (credential, repair_proxy) =
                    crate::oauth::managed::codex::restore_remote_dns_proxy(credential)?;
                if !repair_lifecycle && !repair_proxy {
                    already_native_account_ids.push(target.account_id);
                    continue;
                }
                let current_updated_at: i64 = row.try_get("updated_at")?;
                let current_generation: i64 = row.try_get("credential_generation")?;
                if current_updated_at != target.expected_updated_at
                    || current_generation != target.expected_credential_generation
                {
                    return Err(AppError::Conflict(
                        "an OpenAI Codex account changed after migration review".into(),
                    ));
                }
                crate::oauth::managed::codex::validate_native_credential(&credential)?;
                let updated_at = unix_millis().max(current_updated_at.saturating_add(1));
                let changed = sqlx::query(
                    "UPDATE upstream_accounts SET oauth_session_id = $1, oauth_driver = $2, oauth_refresh_url = $3, updated_at = $4 WHERE id = $5 AND updated_at = $6 AND credential_generation = $7",
                )
                .bind(target.account_id.to_string())
                .bind(crate::oauth::codex_device::OAUTH_DRIVER)
                .bind(crate::oauth::codex_device::TOKEN_ENDPOINT)
                .bind(updated_at)
                .bind(target.account_id.to_string())
                .bind(target.expected_updated_at)
                .bind(target.expected_credential_generation)
                .execute(&mut *tx)
                .await?;
                if changed.rows_affected() != 1 {
                    return Err(AppError::Conflict(
                        "an OpenAI Codex account changed during proxy repair".into(),
                    ));
                }
                if repair_proxy {
                    let ciphertext = seal_credential(&credential, key_material)?;
                    let sealed = sqlx::query(
                        "UPDATE upstream_credentials SET credential_ciphertext = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL",
                    )
                    .bind(ciphertext)
                    .bind(target.account_id.to_string())
                    .bind(target.expected_credential_generation)
                    .execute(&mut *tx)
                    .await?;
                    if sealed.rows_affected() != 1 {
                        return Err(AppError::Conflict(
                            "an OpenAI Codex credential changed during proxy repair".into(),
                        ));
                    }
                }
                upgraded_account_ids.push(target.account_id);
                continue;
            }
            validate_imported_codex_account_shape(&row, &config, &credential)?;
            let current_updated_at: i64 = row.try_get("updated_at")?;
            let current_generation: i64 = row.try_get("credential_generation")?;
            if current_updated_at != target.expected_updated_at
                || current_generation != target.expected_credential_generation
            {
                return Err(AppError::Conflict(
                    "an imported OpenAI Codex account changed after migration review".into(),
                ));
            }
            let native_config = crate::oauth::managed::codex::native_config_from_import(&config)?;
            let native_credential =
                crate::oauth::managed::codex::upgrade_imported_credential(credential)?;
            let ciphertext = seal_credential(&native_credential, key_material)?;
            let updated_at = unix_millis().max(current_updated_at.saturating_add(1));
            let changed = sqlx::query(
                "UPDATE upstream_accounts SET driver = $1, auth_kind = 'oauth', config_json = $2, oauth_session_id = id, oauth_driver = $3, oauth_refresh_url = $4, updated_at = $5 WHERE id = $6 AND updated_at = $7 AND credential_generation = $8",
            )
            .bind(crate::oauth::codex_device::PROVIDER_DRIVER)
            .bind(serde_json::to_string(&native_config).map_err(|_| AppError::Internal)?)
            .bind(crate::oauth::codex_device::OAUTH_DRIVER)
            .bind(crate::oauth::codex_device::TOKEN_ENDPOINT)
            .bind(updated_at)
            .bind(target.account_id.to_string())
            .bind(target.expected_updated_at)
            .bind(target.expected_credential_generation)
            .execute(&mut *tx)
            .await?;
            if changed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "an imported OpenAI Codex account changed during migration".into(),
                ));
            }
            let sealed = sqlx::query(
                "UPDATE upstream_credentials SET credential_ciphertext = $1 WHERE upstream_account_id = $2 AND generation = $3 AND revoked_at IS NULL",
            )
            .bind(ciphertext)
            .bind(target.account_id.to_string())
            .bind(target.expected_credential_generation)
            .execute(&mut *tx)
            .await?;
            if sealed.rows_affected() != 1 {
                return Err(AppError::Conflict(
                    "an imported OpenAI Codex credential changed during migration".into(),
                ));
            }
            upgraded_account_ids.push(target.account_id);
        }
        tx.commit().await?;
        Ok(NativeCodexUpgradeReport {
            upgraded_account_ids,
            already_native_account_ids,
        })
    }

    async fn require_managed_oauth_import(&self, account_id: Uuid) -> Result<(), AppError> {
        let imported = sqlx::query(
            "SELECT 1 FROM upstream_account_imports WHERE upstream_account_id = $1 AND import_kind = $2",
        )
        .bind(account_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        if imported {
            Ok(())
        } else {
            Err(AppError::NotFound)
        }
    }
}

async fn native_codex_upgrade_row(
    database: &Database,
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    account_id: Uuid,
) -> Result<sqlx::any::AnyRow, AppError> {
    let select = match database.backend {
        DatabaseBackend::PostgreSql => {
            "SELECT a.driver, a.auth_kind, a.config_json, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.updated_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_account_imports i ON i.upstream_account_id = a.id AND i.import_kind = $2 JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1 FOR UPDATE OF a, c"
        }
        DatabaseBackend::Sqlite => {
            "SELECT a.driver, a.auth_kind, a.config_json, a.credential_generation, a.oauth_session_id, a.oauth_driver, a.oauth_refresh_url, a.updated_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_account_imports i ON i.upstream_account_id = a.id AND i.import_kind = $2 JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1"
        }
    };
    sqlx::query(select)
        .bind(account_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(AppError::NotFound)
}

/// The account row is locked before this check. A refresh claimant takes the
/// same account lock before it can create a lease. Keep every lease fenced,
/// including an expired unfinalized one: an old refresher may still have its
/// result and attempt to stage it against the unchanged generation.
async fn require_native_codex_upgrade_refresh_quiescence(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    account_id: Uuid,
) -> Result<(), AppError> {
    let refresh_in_progress =
        sqlx::query("SELECT 1 FROM upstream_oauth_refresh_leases WHERE account_id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&mut **tx)
            .await?
            .is_some();
    if refresh_in_progress {
        return Err(AppError::Conflict(
            "OpenAI Codex migration conflicts with an active OAuth refresh".into(),
        ));
    }
    Ok(())
}

fn validate_native_codex_upgrade_ids(account_ids: &[Uuid]) -> Result<(), AppError> {
    if account_ids.is_empty() || account_ids.len() > NATIVE_CODEX_UPGRADE_MAX_ACCOUNTS {
        return Err(AppError::BadRequest(
            "native OpenAI Codex migration requires 1 to 64 account ids".into(),
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    if account_ids
        .iter()
        .any(|account_id| !unique.insert(*account_id))
    {
        return Err(AppError::BadRequest(
            "native OpenAI Codex migration account ids must be unique".into(),
        ));
    }
    Ok(())
}

fn validate_native_codex_upgrade_targets(
    targets: &[NativeCodexUpgradeTarget],
) -> Result<(), AppError> {
    validate_native_codex_upgrade_ids(
        &targets
            .iter()
            .map(|target| target.account_id)
            .collect::<Vec<_>>(),
    )?;
    if targets.iter().any(|target| {
        target.expected_credential_generation < 1
            || !target.has_proxy
            || target.proxy_network_scope != Some(crate::network::OutboundScope::Private)
    }) {
        return Err(AppError::BadRequest(
            "native OpenAI Codex migration requires an approved private proxy".into(),
        ));
    }
    Ok(())
}

fn validate_native_codex_upgrade_candidate(
    account: &UpstreamAccountView,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    if account.driver == crate::oauth::codex_device::PROVIDER_DRIVER {
        crate::oauth::managed::codex::native_config_from_import(&account.config)?;
        crate::oauth::managed::codex::validate_native_credential(credential)?;
    } else if account.driver == crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER {
        crate::oauth::managed::codex::native_config_from_import(&account.config)?;
        let _ = crate::oauth::managed::codex::upgrade_imported_credential(credential.clone())?;
    } else {
        return Err(AppError::BadRequest(
            "selected account is not an imported OpenAI Codex account".into(),
        ));
    }
    require_private_codex_proxy(credential)
}

fn validate_imported_codex_account_shape(
    row: &sqlx::any::AnyRow,
    config: &Value,
    credential: &UpstreamCredential,
) -> Result<(), AppError> {
    if row.try_get::<String, _>("driver")? != crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER
        || row.try_get::<String, _>("auth_kind")? != "oauth"
    {
        return Err(AppError::BadRequest(
            "imported OpenAI Codex account has an unsupported lifecycle".into(),
        ));
    }
    crate::oauth::managed::codex::native_config_from_import(config)?;
    let _ = crate::oauth::managed::codex::upgrade_imported_credential(credential.clone())?;
    require_private_codex_proxy(credential)
}

fn validate_native_codex_account_shape(
    row: &sqlx::any::AnyRow,
    account_id: Uuid,
    config: &Value,
    credential: &UpstreamCredential,
) -> Result<bool, AppError> {
    if row.try_get::<String, _>("auth_kind")? != "oauth" {
        return Err(AppError::BadRequest(
            "native OpenAI Codex account has an unsupported lifecycle".into(),
        ));
    }
    let oauth_session_id = row.try_get::<Option<String>, _>("oauth_session_id")?;
    let oauth_driver = row.try_get::<Option<String>, _>("oauth_driver")?;
    let oauth_refresh_url = row.try_get::<Option<String>, _>("oauth_refresh_url")?;
    let expected_session_id = account_id.to_string();
    let repair_lifecycle = oauth_session_id.as_deref() != Some(expected_session_id.as_str())
        || oauth_driver.as_deref() != Some(crate::oauth::codex_device::OAUTH_DRIVER)
        || oauth_refresh_url.as_deref() != Some(crate::oauth::codex_device::TOKEN_ENDPOINT);
    crate::oauth::managed::codex::native_config_from_import(config)?;
    crate::oauth::managed::codex::validate_native_credential(credential)?;
    require_private_codex_proxy(credential)?;
    Ok(repair_lifecycle)
}

fn require_private_codex_proxy(credential: &UpstreamCredential) -> Result<(), AppError> {
    match credential.proxy() {
        Some((_, crate::network::OutboundScope::Private)) => Ok(()),
        _ => Err(AppError::BadRequest(
            "native OpenAI Codex migration requires an approved private proxy".into(),
        )),
    }
}

#[cfg(test)]
mod native_codex_upgrade_tests {
    use rust_decimal::Decimal;

    use super::*;

    #[derive(Clone, Copy)]
    struct NativeCodexFixture {
        label: &'static str,
        schema: &'static str,
        proxy_url: &'static str,
        expected_proxy_url: &'static str,
    }

    async fn assert_native_codex_upgrade_fixture(database: &Database, fixture: NativeCodexFixture) {
        let key_material = b"native Codex upgrade key material longer than thirty-two bytes";
        let tenant = format!("native-upgrade-{}", fixture.label);
        let expires_at = unix_millis() + 3_600_000;
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: tenant.clone(),
                    name: format!("Imported Codex {}", fixture.label),
                    driver: crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER.to_owned(),
                    config: serde_json::json!({
                        "base_url": crate::oauth::codex_device::BASE_URL,
                        "network_scope": "public",
                        "reservation_token_bounds": {"gpt-5.6-sol": 128000}
                    }),
                    credential: UpstreamCredential::OAuth {
                        access_token: "access-secret".to_owned(),
                        refresh_token: Some("refresh-secret".to_owned()),
                        expires_at: Some(expires_at),
                        header: "authorization".to_owned(),
                        prefix: "Bearer ".to_owned(),
                        adapter_state: Some(serde_json::json!({
                            "schema": fixture.schema,
                            "account_id": "account-123"
                        })),
                        proxy_url: Some(fixture.proxy_url.to_owned()),
                        proxy_network_scope: Some(crate::network::OutboundScope::Private),
                    },
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some("damaged-driver".to_owned()),
                    oauth_refresh_url: Some("https://damaged.invalid".to_owned()),
                },
                key_material,
            )
            .await
            .unwrap();
        let route = database
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: tenant.clone(),
                public_model: format!("native-upgrade-model-{}", fixture.label),
                upstream_account_id: account.id,
                upstream_model: "gpt-5.6-sol".to_owned(),
                protocol: "openai".to_owned(),
                priority: 0,
            })
            .await
            .unwrap();
        let issued_key = database
            .create_key_with_routing(
                CreateKeyInput {
                    tenant_external_id: tenant.clone(),
                    principal_external_id: format!("native-upgrade-principal-{}", fixture.label),
                    alias: format!("native-upgrade-key-{}", fixture.label),
                    currency: "USD".to_owned(),
                    policy: crate::model::KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                &[route.id],
                &[],
                key_material,
            )
            .await
            .unwrap();
        let ciphertext: String = sqlx::query(
            "SELECT credential_ciphertext FROM upstream_credentials WHERE upstream_account_id = $1 AND generation = 1",
        )
        .bind(account.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .try_get("credential_ciphertext")
        .unwrap();
        let history_created_at = unix_millis();
        sqlx::query(
            "UPDATE upstream_credentials SET generation = 5 WHERE upstream_account_id = $1 AND generation = 1",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        for generation in 1_i64..5 {
            sqlx::query(
                "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at, revoked_at) VALUES ($1, $2, $3, $4, $5, $6, $7)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(account.id.to_string())
            .bind(generation)
            .bind(&ciphertext)
            .bind(expires_at)
            .bind(history_created_at.saturating_sub(generation))
            .bind(history_created_at.saturating_sub(generation))
            .execute(&database.pool)
            .await
            .unwrap();
        }
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = 5, status = 'disabled' WHERE id = $1",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upstream_account_imports (tenant_id, import_kind, source_key, contract_version, payload_digest, upstream_account_id, created_at) VALUES ($1, $2, $3, 1, $4, $5, $6)",
        )
        .bind(account.tenant_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind("a".repeat(64))
        .bind("b".repeat(64))
        .bind(account.id.to_string())
        .bind(unix_millis())
        .execute(&database.pool)
        .await
        .unwrap();

        let plan = database
            .prepare_native_codex_upgrade(&[account.id], key_material)
            .await
            .unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].account_id, account.id);
        assert_eq!(plan[0].expected_credential_generation, 5);
        assert!(plan[0].has_proxy);
        assert_eq!(
            plan[0].proxy_network_scope,
            Some(crate::network::OutboundScope::Private)
        );
        let result = database
            .apply_native_codex_upgrade(&plan, key_material)
            .await
            .unwrap();
        assert_eq!(result.upgraded_account_ids, vec![account.id]);
        let (upgraded, credential) = database
            .upstream_account_with_credential(account.id, key_material)
            .await
            .unwrap();
        assert_eq!(upgraded.id, account.id);
        assert_eq!(upgraded.driver, crate::oauth::codex_device::PROVIDER_DRIVER);
        assert_eq!(upgraded.auth_kind, "oauth");
        assert_eq!(upgraded.status, "disabled");
        assert_eq!(upgraded.credential_generation, 5);
        assert_eq!(upgraded.credential_expires_at, Some(expires_at));
        assert_eq!(upgraded.route_count, 1);
        assert_eq!(
            credential.adapter_state(),
            Some(&serde_json::json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "account-123"
            }))
        );
        assert_eq!(
            credential.proxy(),
            Some((
                fixture.expected_proxy_url,
                crate::network::OutboundScope::Private
            ))
        );
        let UpstreamCredential::OAuth {
            access_token,
            refresh_token,
            expires_at: actual_expires_at,
            ..
        } = credential
        else {
            panic!("native upgrade must retain an OAuth credential");
        };
        assert_eq!(access_token, "access-secret");
        assert_eq!(refresh_token.as_deref(), Some("refresh-secret"));
        assert_eq!(actual_expires_at, Some(expires_at));
        assert_eq!(
            database
                .credential_routing(issued_key.key_id, &tenant)
                .await
                .unwrap()
                .route_ids,
            vec![route.id]
        );
        let grant_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM routing_grants WHERE tenant_id = $1 AND key_id = $2 AND model_route_id = $3",
        )
        .bind(account.tenant_id.to_string())
        .bind(issued_key.key_id.to_string())
        .bind(route.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(grant_count, 1);
        let credential_history = sqlx::query(
            "SELECT generation, revoked_at FROM upstream_credentials WHERE upstream_account_id = $1 ORDER BY generation",
        )
        .bind(account.id.to_string())
        .fetch_all(&database.pool)
        .await
        .unwrap();
        assert_eq!(credential_history.len(), 5);
        for row in &credential_history[..4] {
            assert!(
                row.try_get::<Option<i64>, _>("revoked_at")
                    .unwrap()
                    .is_some()
            );
        }
        assert_eq!(
            credential_history[4]
                .try_get::<Option<i64>, _>("revoked_at")
                .unwrap(),
            None
        );
        let preserved_history_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upstream_credentials WHERE upstream_account_id = $1 AND generation < 5 AND credential_ciphertext = $2 AND revoked_at IS NOT NULL",
        )
        .bind(account.id.to_string())
        .bind(&ciphertext)
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(preserved_history_count, 4);
        let upgraded_lifecycle = sqlx::query(
            "SELECT oauth_session_id, oauth_driver, oauth_refresh_url FROM upstream_accounts WHERE id = $1",
        )
        .bind(account.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        let expected_session_id = account.id.to_string();
        assert_eq!(
            upgraded_lifecycle
                .try_get::<Option<String>, _>("oauth_session_id")
                .unwrap()
                .as_deref(),
            Some(expected_session_id.as_str())
        );
        assert_eq!(
            upgraded_lifecycle
                .try_get::<Option<String>, _>("oauth_driver")
                .unwrap()
                .as_deref(),
            Some(crate::oauth::codex_device::OAUTH_DRIVER)
        );
        assert_eq!(
            upgraded_lifecycle
                .try_get::<Option<String>, _>("oauth_refresh_url")
                .unwrap()
                .as_deref(),
            Some(crate::oauth::codex_device::TOKEN_ENDPOINT)
        );
        let repeated = database
            .apply_native_codex_upgrade(&plan, key_material)
            .await
            .unwrap();
        assert_eq!(repeated.upgraded_account_ids, Vec::<Uuid>::new());
        assert_eq!(repeated.already_native_account_ids, vec![account.id]);

        let damaged_updated_at = upgraded.updated_at.saturating_add(1);
        sqlx::query(
            "UPDATE upstream_accounts SET oauth_session_id = 'damaged-session', oauth_driver = 'damaged-driver', oauth_refresh_url = 'https://damaged.invalid', updated_at = $1 WHERE id = $2",
        )
        .bind(damaged_updated_at)
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        let repair_plan = database
            .prepare_native_codex_upgrade(&[account.id], key_material)
            .await
            .unwrap();
        let repaired = database
            .apply_native_codex_upgrade(&repair_plan, key_material)
            .await
            .unwrap();
        assert_eq!(repaired.upgraded_account_ids, vec![account.id]);
        assert_eq!(repaired.already_native_account_ids, Vec::<Uuid>::new());

        let lifecycle = sqlx::query(
            "SELECT oauth_session_id, oauth_driver, oauth_refresh_url, credential_generation FROM upstream_accounts WHERE id = $1",
        )
        .bind(account.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        let expected_session_id = account.id.to_string();
        assert_eq!(
            lifecycle
                .try_get::<Option<String>, _>("oauth_session_id")
                .unwrap()
                .as_deref(),
            Some(expected_session_id.as_str())
        );
        assert_eq!(
            lifecycle
                .try_get::<Option<String>, _>("oauth_driver")
                .unwrap()
                .as_deref(),
            Some(crate::oauth::codex_device::OAUTH_DRIVER)
        );
        assert_eq!(
            lifecycle
                .try_get::<Option<String>, _>("oauth_refresh_url")
                .unwrap()
                .as_deref(),
            Some(crate::oauth::codex_device::TOKEN_ENDPOINT)
        );
        assert_eq!(
            lifecycle
                .try_get::<i64, _>("credential_generation")
                .unwrap(),
            5
        );
        let (_, repaired_credential) = database
            .upstream_account_with_credential(account.id, key_material)
            .await
            .unwrap();
        assert_eq!(
            repaired_credential.proxy(),
            Some((
                fixture.expected_proxy_url,
                crate::network::OutboundScope::Private
            ))
        );
        let repeated_repair = database
            .apply_native_codex_upgrade(&repair_plan, key_material)
            .await
            .unwrap();
        assert_eq!(repeated_repair.already_native_account_ids, vec![account.id]);
    }

    async fn create_native_codex_upgrade_refresh_lease_fixture(database: &Database) -> Uuid {
        let key_material = b"native Codex refresh lease key material longer than thirty-two bytes";
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "native-upgrade-refresh-lease".to_owned(),
                    name: "Imported Codex refresh lease".to_owned(),
                    driver: crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER.to_owned(),
                    config: serde_json::json!({
                        "base_url": crate::oauth::codex_device::BASE_URL,
                        "network_scope": "public",
                        "reservation_token_bounds": {}
                    }),
                    credential: UpstreamCredential::OAuth {
                        access_token: "lease-access-secret".to_owned(),
                        refresh_token: Some("lease-refresh-secret".to_owned()),
                        expires_at: Some(unix_millis() + 3_600_000),
                        header: "authorization".to_owned(),
                        prefix: "Bearer ".to_owned(),
                        adapter_state: Some(serde_json::json!({
                            "schema": "cpa-codex-oauth-v1",
                            "account_id": "refresh-lease-account-123"
                        })),
                        proxy_url: Some(
                            "socks5h://operator:proxy-secret@100.64.0.16:1080".to_owned(),
                        ),
                        proxy_network_scope: Some(crate::network::OutboundScope::Private),
                    },
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some("damaged-driver".to_owned()),
                    oauth_refresh_url: Some("https://damaged.invalid".to_owned()),
                },
                key_material,
            )
            .await
            .unwrap();
        sqlx::query(
            "UPDATE upstream_credentials SET generation = 5 WHERE upstream_account_id = $1 AND generation = 1",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = 5, status = 'disabled' WHERE id = $1",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upstream_account_imports (tenant_id, import_kind, source_key, contract_version, payload_digest, upstream_account_id, created_at) VALUES ($1, $2, $3, 1, $4, $5, $6)",
        )
        .bind(account.tenant_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind("e".repeat(64))
        .bind("f".repeat(64))
        .bind(account.id.to_string())
        .bind(unix_millis())
        .execute(&database.pool)
        .await
        .unwrap();
        account.id
    }

    #[tokio::test]
    async fn native_upgrade_recovers_generation_five_old_and_native_envelopes_without_touching_history()
     {
        let directory = tempfile::tempdir().unwrap();
        for fixture in [
            NativeCodexFixture {
                label: "old-envelope",
                schema: "cpa-codex-oauth-v1",
                proxy_url: "socks5://operator:proxy-secret@100.64.0.16:1080",
                expected_proxy_url: "socks5h://operator:proxy-secret@100.64.0.16:1080",
            },
            NativeCodexFixture {
                label: "native-envelope",
                schema: "openai-codex-oauth-v1",
                proxy_url: "socks5h://operator:proxy-secret@100.64.0.16:1080",
                expected_proxy_url: "socks5h://operator:proxy-secret@100.64.0.16:1080",
            },
        ] {
            let database = Database::connect(&format!(
                "sqlite://{}?mode=rwc",
                directory
                    .path()
                    .join(format!("native-codex-upgrade-{}.db", fixture.label))
                    .display()
            ))
            .await
            .unwrap();
            database.migrate().await.unwrap();
            assert_native_codex_upgrade_fixture(&database, fixture).await;
        }
    }

    #[tokio::test]
    async fn sqlite_native_upgrade_stale_plan_serializes_to_a_cas_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("native-codex-upgrade-cas.db")
                .display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let key_material = b"native Codex CAS key material longer than thirty-two bytes";
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "native-upgrade-cas".to_owned(),
                    name: "Imported Codex CAS".to_owned(),
                    driver: crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER.to_owned(),
                    config: serde_json::json!({
                        "base_url": crate::oauth::codex_device::BASE_URL,
                        "network_scope": "public",
                        "reservation_token_bounds": {}
                    }),
                    credential: UpstreamCredential::OAuth {
                        access_token: "cas-access-secret".to_owned(),
                        refresh_token: Some("cas-refresh-secret".to_owned()),
                        expires_at: Some(unix_millis() + 3_600_000),
                        header: "authorization".to_owned(),
                        prefix: "Bearer ".to_owned(),
                        adapter_state: Some(serde_json::json!({
                            "schema": "cpa-codex-oauth-v1",
                            "account_id": "cas-account-123"
                        })),
                        proxy_url: Some("socks5h://operator:secret@100.64.0.16:1080".to_owned()),
                        proxy_network_scope: Some(crate::network::OutboundScope::Private),
                    },
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some("damaged-driver".to_owned()),
                    oauth_refresh_url: Some("https://damaged.invalid".to_owned()),
                },
                key_material,
            )
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO upstream_account_imports (tenant_id, import_kind, source_key, contract_version, payload_digest, upstream_account_id, created_at) VALUES ($1, $2, $3, 1, $4, $5, $6)",
        )
        .bind(account.tenant_id.to_string())
        .bind(CPA_MANAGED_OAUTH_IMPORT_KIND)
        .bind("c".repeat(64))
        .bind("d".repeat(64))
        .bind(account.id.to_string())
        .bind(unix_millis())
        .execute(&database.pool)
        .await
        .unwrap();
        let plan = database
            .prepare_native_codex_upgrade(&[account.id], key_material)
            .await
            .unwrap();

        let mut winner = database.begin_write_transaction().await.unwrap();
        sqlx::query("UPDATE upstream_accounts SET updated_at = updated_at + 1 WHERE id = $1")
            .bind(account.id.to_string())
            .execute(&mut *winner)
            .await
            .unwrap();
        let apply_database = database.clone();
        let apply_task = tokio::spawn(async move {
            apply_database
                .apply_native_codex_upgrade(&plan, key_material)
                .await
        });
        tokio::task::yield_now().await;
        winner.commit().await.unwrap();

        let error = tokio::time::timeout(std::time::Duration::from_secs(2), apply_task)
            .await
            .expect("native upgrade must not remain blocked behind a committed writer")
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, crate::error::AppError::Conflict(_)));
    }

    #[tokio::test]
    async fn sqlite_native_upgrade_fences_an_in_flight_oauth_refresh() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory
                .path()
                .join("native-codex-upgrade-refresh-lease.db")
                .display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let key_material = b"native Codex refresh lease key material longer than thirty-two bytes";
        let account_id = create_native_codex_upgrade_refresh_lease_fixture(&database).await;
        let reviewed_plan = database
            .prepare_native_codex_upgrade(&[account_id], key_material)
            .await
            .unwrap();

        assert!(
            database
                .begin_upstream_oauth_refresh(
                    account_id,
                    "native-upgrade-refresh-lease",
                    key_material,
                )
                .await
                .unwrap()
                .is_none()
        );
        let blocked = database
            .apply_native_codex_upgrade(&reviewed_plan, key_material)
            .await
            .unwrap_err();
        assert!(matches!(blocked, crate::error::AppError::Conflict(_)));
        let active_lease_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upstream_oauth_refresh_leases WHERE account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(active_lease_count, 1);
        let (unmigrated, unmigrated_credential) = database
            .upstream_account_with_credential(account_id, key_material)
            .await
            .unwrap();
        assert_eq!(
            unmigrated.driver,
            crate::oauth::codex_device::IMPORTED_PROVIDER_DRIVER
        );
        assert_eq!(unmigrated.status, "disabled");
        assert_eq!(unmigrated.updated_at, reviewed_plan[0].expected_updated_at);
        assert_eq!(unmigrated.credential_generation, 5);
        assert_eq!(
            unmigrated_credential.adapter_state(),
            Some(&serde_json::json!({
                "schema": "cpa-codex-oauth-v1",
                "account_id": "refresh-lease-account-123"
            }))
        );
        assert_eq!(
            unmigrated_credential.proxy(),
            Some((
                "socks5h://operator:proxy-secret@100.64.0.16:1080",
                crate::network::OutboundScope::Private
            ))
        );

        database
            .abort_upstream_oauth_refresh(account_id, "native-upgrade-refresh-lease")
            .await
            .unwrap();
        let cleared_lease_count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM upstream_oauth_refresh_leases WHERE account_id = $1",
        )
        .bind(account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(cleared_lease_count, 0);
        let refreshed_plan = database
            .prepare_native_codex_upgrade(&[account_id], key_material)
            .await
            .unwrap();
        let report = database
            .apply_native_codex_upgrade(&refreshed_plan, key_material)
            .await
            .unwrap();
        assert_eq!(report.upgraded_account_ids, vec![account_id]);
        let (upgraded, upgraded_credential) = database
            .upstream_account_with_credential(account_id, key_material)
            .await
            .unwrap();
        assert_eq!(upgraded.id, account_id);
        assert_eq!(upgraded.status, "disabled");
        assert_eq!(upgraded.credential_generation, 5);
        assert_eq!(upgraded.driver, crate::oauth::codex_device::PROVIDER_DRIVER);
        assert_eq!(
            upgraded_credential.adapter_state(),
            Some(&serde_json::json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "refresh-lease-account-123"
            }))
        );

        let stale_finalize = database
            .finish_upstream_oauth_refresh(
                account_id,
                UpstreamCredential::OAuth {
                    access_token: "stale-access-secret".to_owned(),
                    refresh_token: Some("stale-refresh-secret".to_owned()),
                    expires_at: Some(unix_millis() + 3_600_000),
                    header: "authorization".to_owned(),
                    prefix: "Bearer ".to_owned(),
                    adapter_state: Some(serde_json::json!({
                        "schema": "cpa-codex-oauth-v1",
                        "account_id": "refresh-lease-account-123"
                    })),
                    proxy_url: Some("socks5h://operator:proxy-secret@100.64.0.16:1080".to_owned()),
                    proxy_network_scope: Some(crate::network::OutboundScope::Private),
                },
                "native-upgrade-refresh-lease",
                key_material,
            )
            .await
            .unwrap_err();
        assert!(matches!(
            stale_finalize,
            crate::error::AppError::BadRequest(_) | crate::error::AppError::Conflict(_)
        ));
        let (after_stale_finalize, credential_after_stale_finalize) = database
            .upstream_account_with_credential(account_id, key_material)
            .await
            .unwrap();
        assert_eq!(
            after_stale_finalize.driver,
            crate::oauth::codex_device::PROVIDER_DRIVER
        );
        assert_eq!(after_stale_finalize.credential_generation, 5);
        assert_eq!(
            credential_after_stale_finalize.adapter_state(),
            Some(&serde_json::json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "refresh-lease-account-123"
            }))
        );
    }
}
