use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Days, Utc};
use rust_decimal::{Decimal, prelude::ToPrimitive};
use sha2::{Digest, Sha256};
use sqlx::{
    AnyConnection, AnyPool, Row,
    any::{AnyPoolOptions, AnyQueryResult, AnyRow},
};
use uuid::Uuid;

use crate::{
    conversation::{RelationKind, build_prefix, extract_atoms},
    crypto,
    error::AppError,
    model::{
        AuthenticatedKey, AuthenticatedService, ConversationClusterDetail, ConversationClusterView,
        ConversationEdgeView, GenerationJobView, GenerationJobWork, GenerationPrice, IssuedKey,
        IssuedServiceToken, KeyPolicy, KeyView, ModelPrice, RequestArchiveRefs, RequestEventView,
        RequestView, SelfStats, StatsBucket, StatsSummary, UsageReservation,
        micros_to_decimal_string, priced_tokens,
    },
    provider::{
        ModelRouteView, ResolvedUpstream, UpstreamAccountView, UpstreamCredential, open_credential,
        open_private_json, seal_credential, seal_private_json, validate_config,
    },
};

const KEY_PROVISIONING_AAD: &[u8] = b"memeloop-token-center/key-provisioning-response/v1";

#[derive(Clone)]
pub struct Database {
    pool: AnyPool,
    backend: DatabaseBackend,
}

#[derive(Clone, Copy)]
enum DatabaseBackend {
    PostgreSql,
    Sqlite,
}

pub struct CreateKeyInput {
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub alias: String,
    pub currency: String,
    pub policy: KeyPolicy,
    pub initial_balance: Decimal,
    pub idempotency_key: Option<String>,
}

pub struct CreateServiceTokenInput {
    pub name: String,
    pub scopes: Vec<String>,
    pub tenant_external_id: Option<String>,
}

pub struct NewRequest {
    pub request_id: Uuid,
    pub key_id: Uuid,
    pub tenant_id: Uuid,
    pub protocol: String,
    pub model: String,
    pub request_object: String,
    pub reservation_id: Uuid,
    pub upstream_account_id: Option<Uuid>,
    pub model_route_id: Option<Uuid>,
}

pub struct FinishRequest {
    pub request_id: Uuid,
    pub status_code: i64,
    pub duration_ms: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_micros: i64,
    pub error_code: Option<String>,
    pub response_object: String,
}

pub struct CreateUpstreamAccountInput {
    pub tenant_external_id: String,
    pub name: String,
    pub driver: String,
    pub config: serde_json::Value,
    pub credential: UpstreamCredential,
    pub oauth_session_id: Option<Uuid>,
}

pub struct CreateModelRouteInput {
    pub tenant_external_id: String,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
}

pub struct CreateGenerationJobInput {
    pub job_id: Uuid,
    pub key: AuthenticatedKey,
    pub upstream_account_id: Uuid,
    pub reservation: UsageReservation,
    pub public_model: String,
    pub upstream_model: String,
    pub driver: String,
    pub request_object: String,
    pub estimated_units: i64,
}

pub struct FinishGenerationJobInput<'a> {
    pub job_id: Uuid,
    pub worker_id: &'a str,
    pub status: &'a str,
    pub billed_units: i64,
    pub cost_micros: i64,
    pub result: Option<&'a serde_json::Value>,
    pub error_code: Option<&'a str>,
}

impl Database {
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        // `$n` placeholders are accepted by both PostgreSQL and SQLite. `sqlx::Any` deliberately
        // does not translate `?` into PostgreSQL placeholders, so all queries in this module use
        // the shared `$n` form.
        sqlx::any::install_default_drivers();
        let backend = if database_url.starts_with("sqlite:") {
            DatabaseBackend::Sqlite
        } else {
            DatabaseBackend::PostgreSql
        };
        let pool = AnyPoolOptions::new()
            .min_connections(0)
            .max_connections(8)
            .acquire_timeout(Duration::from_secs(10))
            .idle_timeout(Some(Duration::from_secs(5 * 60)))
            .max_lifetime(Some(Duration::from_secs(30 * 60)))
            .connect(database_url)
            .await?;
        Ok(Self { pool, backend })
    }

    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(734627102948311)")
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS schema_migrations (version BIGINT PRIMARY KEY, name TEXT NOT NULL, applied_at BIGINT NOT NULL)",
        )
        .execute(&mut *transaction)
        .await?;
        let migrations = match self.backend {
            DatabaseBackend::PostgreSql => POSTGRES_MIGRATIONS,
            DatabaseBackend::Sqlite => SQLITE_MIGRATIONS,
        };
        apply_migration_range(&mut transaction, migrations, i64::MIN, 1).await?;
        for column in ["upstream_account_id", "model_route_id"] {
            let exists = match self.backend {
                DatabaseBackend::PostgreSql => sqlx::query(
                    "SELECT column_name::TEXT AS column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'request_records' AND column_name = $1",
                )
                .bind(column)
                .fetch_optional(&mut *transaction)
                .await?
                .is_some(),
                DatabaseBackend::Sqlite => sqlx::query(
                    "SELECT name FROM pragma_table_info('request_records') WHERE name = $1",
                )
                .bind(column)
                .fetch_optional(&mut *transaction)
                .await?
                .is_some(),
            };
            if !exists {
                sqlx::query(&format!(
                    "ALTER TABLE request_records ADD COLUMN {column} TEXT"
                ))
                .execute(&mut *transaction)
                .await?;
            }
        }
        let oauth_session_column_exists = match self.backend {
            DatabaseBackend::PostgreSql => sqlx::query(
                "SELECT column_name::TEXT AS column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'upstream_accounts' AND column_name = 'oauth_session_id'",
            )
            .fetch_optional(&mut *transaction)
            .await?
            .is_some(),
            DatabaseBackend::Sqlite => sqlx::query(
                "SELECT name FROM pragma_table_info('upstream_accounts') WHERE name = 'oauth_session_id'",
            )
            .fetch_optional(&mut *transaction)
            .await?
            .is_some(),
        };
        if !oauth_session_column_exists {
            sqlx::query("ALTER TABLE upstream_accounts ADD COLUMN oauth_session_id TEXT")
                .execute(&mut *transaction)
                .await?;
        }
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS upstream_accounts_oauth_session_idx ON upstream_accounts (oauth_session_id) WHERE oauth_session_id IS NOT NULL",
        )
        .execute(&mut *transaction)
        .await?;
        apply_migration_range(&mut transaction, migrations, 2, i64::MAX).await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            maintain_postgres_partitions(&mut transaction).await?;
        }
        transaction.commit().await?;
        Ok(())
    }

    pub async fn maintain_partitions(&self) -> Result<(), sqlx::Error> {
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            let mut connection = self.pool.acquire().await?;
            maintain_postgres_partitions(&mut connection).await?;
        }
        Ok(())
    }

    pub async fn create_key(
        &self,
        input: CreateKeyInput,
        pepper: &[u8],
    ) -> Result<IssuedKey, AppError> {
        validate_currency(&input.currency)?;
        validate_policy_budgets(&input.policy)?;
        let idempotency_key = input
            .idempotency_key
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty());
        if input.idempotency_key.is_some() && idempotency_key.is_none() {
            return Err(AppError::BadRequest(
                "Idempotency-Key cannot be empty".into(),
            ));
        }
        if idempotency_key.is_some_and(|value| {
            value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic())
        }) {
            return Err(AppError::BadRequest(
                "Idempotency-Key must be at most 200 visible ASCII characters".into(),
            ));
        }
        let provisioning_request_hash = idempotency_key.map(|_| {
            let canonical = serde_json::to_vec(&serde_json::json!({
                "tenant_external_id": input.tenant_external_id.trim(),
                "principal_external_id": input.principal_external_id.trim(),
                "alias": input.alias.trim(),
                "currency": input.currency.to_uppercase(),
                "policy": input.policy,
                "initial_balance": input.initial_balance.normalize().to_string()
            }))
            .expect("key provisioning request is JSON serializable");
            format!("{:x}", Sha256::digest(canonical))
        });
        let now = unix_millis();
        let tenant_id = Uuid::now_v7();
        let principal_id = Uuid::now_v7();
        let account_id = Uuid::now_v7();
        let key_id = Uuid::now_v7();
        let issued = crypto::issue_credential(key_id, pepper);
        let policy_json = serde_json::to_string(&input.policy).map_err(|_| AppError::Internal)?;
        let initial_balance_micros = decimal_to_micros(input.initial_balance)?;
        let mut tx = self.pool.begin().await?;

        if let Some(idempotency_key) = idempotency_key {
            if matches!(self.backend, DatabaseBackend::PostgreSql) {
                sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948312))")
                    .bind(idempotency_key)
                    .execute(&mut *tx)
                    .await?;
            }
            let existing = sqlx::query(
                "SELECT provisioning_request_hash, issued_key_ciphertext FROM key_records WHERE provisioning_idempotency_key = $1",
            )
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                let existing_hash: Option<String> =
                    existing.try_get("provisioning_request_hash")?;
                if existing_hash.as_deref() != provisioning_request_hash.as_deref() {
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used with a different key request".into(),
                    ));
                }
                let ciphertext: Option<String> = existing.try_get("issued_key_ciphertext")?;
                let issued = open_private_json(
                    ciphertext.as_deref().ok_or_else(|| {
                        AppError::BadRequest(
                            "idempotent key provisioning response is no longer available; rotate the key"
                                .into(),
                        )
                    })?,
                    pepper,
                    KEY_PROVISIONING_AAD,
                )?;
                tx.commit().await?;
                return Ok(issued);
            }
        }

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_id.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;

        sqlx::query(
            "INSERT INTO principals (id, tenant_id, external_id, created_at) VALUES ($1, $2, $3, $4) ON CONFLICT(tenant_id, external_id) DO NOTHING",
        )
        .bind(principal_id.to_string())
        .bind(&tenant_id)
        .bind(&input.principal_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let principal_id: String =
            sqlx::query("SELECT id FROM principals WHERE tenant_id = $1 AND external_id = $2")
                .bind(&tenant_id)
                .bind(&input.principal_external_id)
                .fetch_one(&mut *tx)
                .await?
                .try_get("id")?;

        sqlx::query(
            "INSERT INTO credit_accounts (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, 0, $6, $7)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(input.currency.to_uppercase())
        .bind(initial_balance_micros)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        let issued_key = IssuedKey {
            key_id,
            account_id,
            alias: input.alias.clone(),
            currency: input.currency.to_uppercase(),
            credential_generation: 1,
            key: issued.secret.clone(),
            fingerprint: issued.fingerprint.clone(),
        };
        let issued_key_ciphertext = idempotency_key
            .map(|_| seal_private_json(&issued_key, pepper, KEY_PROVISIONING_AAD))
            .transpose()?;
        sqlx::query(
            "INSERT INTO key_records (id, tenant_id, principal_id, account_id, alias, currency, policy_json, status, credential_generation, provisioning_idempotency_key, provisioning_request_hash, issued_key_ciphertext, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 'active', 1, $8, $9, $10, $11, $12)",
        )
        .bind(key_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(account_id.to_string())
        .bind(&input.alias)
        .bind(input.currency.to_uppercase())
        .bind(policy_json)
        .bind(idempotency_key)
        .bind(provisioning_request_hash)
        .bind(issued_key_ciphertext)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        insert_credential(&mut tx, &issued, 1, now).await?;
        if initial_balance_micros != 0 {
            sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES ($1, $2, $3, 'grant', $4, $5, 'initial', $6)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(account_id.to_string())
            .bind(key_id.to_string())
            .bind(initial_balance_micros)
            .bind(input.currency.to_uppercase())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(issued_key)
    }

    pub async fn create_service_token(
        &self,
        input: CreateServiceTokenInput,
        pepper: &[u8],
    ) -> Result<IssuedServiceToken, AppError> {
        validate_service_token_input(&input)?;
        let now = unix_millis();
        let service_id = Uuid::now_v7();
        let issued = crypto::issue_service_credential(service_id, pepper);
        let scopes_json = serde_json::to_string(&input.scopes).map_err(|_| AppError::Internal)?;
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO service_principals (id, name, status, credential_generation, created_at, updated_at) VALUES ($1, $2, 'active', 1, $3, $4)",
        )
        .bind(service_id.to_string())
        .bind(input.name.trim())
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO service_credentials (id, service_principal_id, generation, secret_hash, fingerprint, scopes_json, tenant_external_id, created_at) VALUES ($1, $2, 1, $3, $4, $5, $6, $7)",
        )
        .bind(issued.credential_id.to_string())
        .bind(service_id.to_string())
        .bind(&issued.secret_hash)
        .bind(&issued.fingerprint)
        .bind(scopes_json)
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(IssuedServiceToken {
            service_id,
            name: input.name.trim().to_owned(),
            credential_generation: 1,
            token: issued.secret,
            fingerprint: issued.fingerprint,
            scopes: input.scopes,
            tenant_external_id: input.tenant_external_id,
        })
    }

    pub async fn update_key_policy(
        &self,
        key_id: Uuid,
        policy: KeyPolicy,
    ) -> Result<KeyPolicy, AppError> {
        validate_policy_budgets(&policy)?;
        let policy_json = serde_json::to_string(&policy).map_err(|_| AppError::Internal)?;
        let result = sqlx::query(
            "UPDATE key_records SET policy_json = $1, updated_at = $2 WHERE id = $3 AND status = 'active'",
        )
        .bind(policy_json)
        .bind(unix_millis())
        .bind(key_id.to_string())
        .execute(&self.pool)
        .await?;
        if result.rows_affected() == 0 {
            return Err(AppError::NotFound);
        }
        Ok(policy)
    }

    pub async fn rotate_service_token(
        &self,
        service_id: Uuid,
        pepper: &[u8],
    ) -> Result<IssuedServiceToken, AppError> {
        let now = unix_millis();
        let issued = crypto::issue_service_credential(service_id, pepper);
        let mut transaction = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT p.name, p.status, p.credential_generation, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1",
        )
        .bind(service_id.to_string())
        .fetch_optional(&mut *transaction)
        .await?
        .ok_or(AppError::NotFound)?;
        if row.try_get::<String, _>("status")? != "active" {
            return Err(AppError::Forbidden);
        }
        let generation = row.try_get::<i64, _>("credential_generation")? + 1;
        let scopes_json: String = row.try_get("scopes_json")?;
        let scopes: Vec<String> =
            serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?;
        let tenant_external_id: Option<String> = row.try_get("tenant_external_id")?;
        let name: String = row.try_get("name")?;
        sqlx::query(
            "UPDATE service_credentials SET revoked_at = $1 WHERE service_principal_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(service_id.to_string())
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO service_credentials (id, service_principal_id, generation, secret_hash, fingerprint, scopes_json, tenant_external_id, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(issued.credential_id.to_string())
        .bind(service_id.to_string())
        .bind(generation)
        .bind(&issued.secret_hash)
        .bind(&issued.fingerprint)
        .bind(scopes_json)
        .bind(&tenant_external_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "UPDATE service_principals SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(now)
        .bind(service_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(IssuedServiceToken {
            service_id,
            name,
            credential_generation: generation,
            token: issued.secret,
            fingerprint: issued.fingerprint,
            scopes,
            tenant_external_id,
        })
    }

    pub async fn authenticate_service_token(
        &self,
        value: &str,
        pepper: &[u8],
    ) -> Result<AuthenticatedService, AppError> {
        let parsed = crypto::parse_service_credential(value).ok_or(AppError::Unauthorized)?;
        let row = sqlx::query(
            "SELECT p.status, c.secret_hash, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1",
        )
        .bind(parsed.key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Unauthorized)?;
        let expected: Vec<u8> = row.try_get("secret_hash")?;
        if row.try_get::<String, _>("status")? != "active"
            || !crypto::verify_credential(value, pepper, &expected)
        {
            return Err(AppError::Unauthorized);
        }
        let scopes_json: String = row.try_get("scopes_json")?;
        Ok(AuthenticatedService {
            service_id: Some(parsed.key_id),
            scopes: serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?,
            tenant_external_id: row.try_get("tenant_external_id")?,
        })
    }

    pub async fn rotate_key(&self, key_id: Uuid, pepper: &[u8]) -> Result<IssuedKey, AppError> {
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT account_id, alias, currency, credential_generation, status FROM key_records WHERE id = $1",
        )
        .bind(key_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if status != "active" {
            return Err(AppError::Forbidden);
        }
        let generation: i64 = row.try_get::<i64, _>("credential_generation")? + 1;
        let account_id: String = row.try_get("account_id")?;
        let alias: String = row.try_get("alias")?;
        let currency: String = row.try_get("currency")?;
        let issued = crypto::issue_credential(key_id, pepper);

        sqlx::query(
            "UPDATE key_credentials SET revoked_at = $1 WHERE key_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        insert_credential(&mut tx, &issued, generation, now).await?;
        sqlx::query(
            "UPDATE key_records SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(IssuedKey {
            key_id,
            account_id: Uuid::parse_str(&account_id).map_err(|_| AppError::Internal)?,
            alias,
            currency,
            credential_generation: generation,
            key: issued.secret,
            fingerprint: issued.fingerprint,
        })
    }

    pub async fn create_upstream_account(
        &self,
        input: CreateUpstreamAccountInput,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        if input.name.trim().is_empty() {
            return Err(AppError::BadRequest(
                "upstream account name is required".into(),
            ));
        }
        let _ = validate_config(&input.config)?;
        let now = unix_millis();
        let account_id = Uuid::now_v7();
        let tenant_candidate = Uuid::now_v7();
        let config_json = serde_json::to_string(&input.config).map_err(|_| AppError::Internal)?;
        let credential_ciphertext = seal_credential(&input.credential, key_material)?;
        let auth_kind = input.credential.auth_kind();
        let credential_expires_at = input.credential.expires_at();
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES ($1, $2, $3) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_candidate.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;
        if let Some(session_id) = input.oauth_session_id {
            let existing = sqlx::query(
                "SELECT a.id, a.tenant_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation WHERE a.oauth_session_id = $1",
            )
            .bind(session_id.to_string())
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(existing) = existing {
                tx.commit().await?;
                return upstream_account_view(existing);
            }
        }
        sqlx::query(
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, 'active', 1, $7, $8, $9)",
        )
        .bind(account_id.to_string())
        .bind(&tenant_id)
        .bind(input.name.trim())
        .bind(&input.driver)
        .bind(auth_kind)
        .bind(config_json)
        .bind(input.oauth_session_id.map(|id| id.to_string()))
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, 1, $3, $4, $5)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(credential_ciphertext)
        .bind(credential_expires_at)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(UpstreamAccountView {
            id: account_id,
            tenant_id: parse_uuid(tenant_id)?,
            name: input.name.trim().to_owned(),
            driver: input.driver,
            auth_kind: auth_kind.to_owned(),
            credential_generation: 1,
            status: "active".to_owned(),
            config: input.config,
            credential_expires_at,
            created_at: now,
            updated_at: now,
        })
    }

    pub async fn rotate_upstream_credential(
        &self,
        account_id: Uuid,
        credential: UpstreamCredential,
        key_material: &[u8],
    ) -> Result<UpstreamAccountView, AppError> {
        let now = unix_millis();
        let ciphertext = seal_credential(&credential, key_material)?;
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at FROM upstream_accounts WHERE id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let status: String = row.try_get("status")?;
        if status != "active" {
            return Err(AppError::Forbidden);
        }
        let auth_kind: String = row.try_get("auth_kind")?;
        if auth_kind != credential.auth_kind() {
            return Err(AppError::BadRequest(
                "credential rotation cannot change auth type; create a new upstream account".into(),
            ));
        }
        let generation: i64 = row.try_get::<i64, _>("credential_generation")? + 1;
        sqlx::query(
            "UPDATE upstream_credentials SET revoked_at = $1 WHERE upstream_account_id = $2 AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(generation)
        .bind(ciphertext)
        .bind(credential.expires_at())
        .bind(now)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE upstream_accounts SET credential_generation = $1, updated_at = $2 WHERE id = $3",
        )
        .bind(generation)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        let config_json: String = row.try_get("config_json")?;
        Ok(UpstreamAccountView {
            id: account_id,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            name: row.try_get("name")?,
            driver: row.try_get("driver")?,
            auth_kind,
            credential_generation: generation,
            status,
            config: serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?,
            credential_expires_at: credential.expires_at(),
            created_at: row.try_get("created_at")?,
            updated_at: now,
        })
    }

    pub async fn upstream_account_with_credential(
        &self,
        account_id: Uuid,
        key_material: &[u8],
    ) -> Result<(UpstreamAccountView, UpstreamCredential), AppError> {
        let row = sqlx::query(
            "SELECT a.id, a.tenant_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = $1",
        )
        .bind(account_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        let credential = open_credential(&ciphertext, key_material)?;
        Ok((upstream_account_view(row)?, credential))
    }

    pub async fn create_model_route(
        &self,
        input: CreateModelRouteInput,
    ) -> Result<ModelRouteView, AppError> {
        if input.public_model.trim().is_empty() || input.upstream_model.trim().is_empty() {
            return Err(AppError::BadRequest(
                "public_model and upstream_model are required".into(),
            ));
        }
        if !matches!(
            input.protocol.as_str(),
            "openai" | "anthropic" | "generation"
        ) {
            return Err(AppError::BadRequest(
                "route protocol must be openai, anthropic, or generation".into(),
            ));
        }
        let now = unix_millis();
        let route_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = $1")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("id")?;
        let account_tenant: String = sqlx::query(
            "SELECT tenant_id FROM upstream_accounts WHERE id = $1 AND status = 'active'",
        )
        .bind(input.upstream_account_id.to_string())
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?
        .try_get("tenant_id")?;
        if account_tenant != tenant_id {
            return Err(AppError::Forbidden);
        }
        sqlx::query(
            "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 1, $8, $9)",
        )
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .bind(input.public_model.trim())
        .bind(input.upstream_account_id.to_string())
        .bind(input.upstream_model.trim())
        .bind(&input.protocol)
        .bind(input.priority)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ModelRouteView {
            id: route_id,
            tenant_id: parse_uuid(tenant_id)?,
            public_model: input.public_model.trim().to_owned(),
            upstream_account_id: input.upstream_account_id,
            upstream_model: input.upstream_model.trim().to_owned(),
            protocol: input.protocol,
            priority: input.priority,
            enabled: true,
        })
    }

    pub async fn list_upstream_accounts(
        &self,
        tenant_external_id: &str,
    ) -> Result<Vec<UpstreamAccountView>, AppError> {
        let rows = sqlx::query(
            "SELECT a.id, a.tenant_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id LEFT JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE t.external_id = $1 ORDER BY a.created_at DESC, a.id DESC",
        )
        .bind(tenant_external_id)
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(upstream_account_view).collect()
    }

    pub async fn resolve_upstream(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        self.resolve_upstream_with_hint(tenant_id, public_model, protocol, None, key_material)
            .await
    }

    pub async fn resolve_upstream_with_hint(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        upstream_account_id: Option<Uuid>,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let sql = if upstream_account_id.is_some() {
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND a.id = $4 AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1"
        } else {
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1"
        };
        let query = sqlx::query(sql)
            .bind(tenant_id.to_string())
            .bind(public_model)
            .bind(protocol);
        let query = if let Some(account_id) = upstream_account_id {
            query.bind(account_id.to_string())
        } else {
            query
        };
        let row = query.fetch_optional(&self.pool).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let config_json: String = row.try_get("config_json")?;
        let config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let base_url = validate_config(&config)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        Ok(Some(ResolvedUpstream {
            route_id: parse_uuid(row.try_get("route_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            driver: row.try_get("driver")?,
            base_url,
            config,
            upstream_model: row.try_get("upstream_model")?,
            credential: open_credential(&ciphertext, key_material)?,
        }))
    }

    pub async fn authenticate_key(
        &self,
        value: &str,
        pepper: &[u8],
    ) -> Result<AuthenticatedKey, AppError> {
        let parsed = crypto::parse_credential(value).ok_or(AppError::Unauthorized)?;
        let row = sqlx::query(
            "SELECT k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.status, c.generation, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id WHERE k.id = $1 AND c.revoked_at IS NULL ORDER BY c.generation DESC",
        )
        .bind(parsed.key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Unauthorized)?;
        let status: String = row.try_get("status")?;
        let expected: Vec<u8> = row.try_get("secret_hash")?;
        if status != "active" || !crypto::verify_credential(value, pepper, &expected) {
            return Err(AppError::Unauthorized);
        }

        let policy_json: String = row.try_get("policy_json")?;
        Ok(AuthenticatedKey {
            key_id: parsed.key_id,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            principal_id: parse_uuid(row.try_get("principal_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            alias: row.try_get("alias")?,
            currency: row.try_get("currency")?,
            credential_generation: row.try_get("generation")?,
            policy: serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?,
        })
    }

    pub async fn key_view(&self, key: &AuthenticatedKey) -> Result<KeyView, AppError> {
        let row = sqlx::query(
            "SELECT k.created_at, a.available_micros FROM key_records k JOIN credit_accounts a ON a.id = k.account_id WHERE k.id = $1",
        )
        .bind(key.key_id.to_string())
        .fetch_one(&self.pool)
        .await?;
        Ok(KeyView {
            key_id: key.key_id,
            alias: key.alias.clone(),
            currency: key.currency.clone(),
            credential_generation: key.credential_generation,
            created_at: row.try_get("created_at")?,
            policy: key.policy.clone(),
            available_balance: micros_to_decimal_string(row.try_get("available_micros")?),
        })
    }

    pub async fn require_key_tenant(
        &self,
        key_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT k.id FROM key_records k JOIN tenants t ON t.id = k.tenant_id WHERE k.id = $1 AND t.external_id = $2",
        )
        .bind(key_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }

    pub async fn require_account_tenant(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT a.id FROM credit_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }

    pub async fn require_upstream_tenant(
        &self,
        account_id: Uuid,
        tenant_external_id: &str,
    ) -> Result<(), AppError> {
        let exists = sqlx::query(
            "SELECT a.id FROM upstream_accounts a JOIN tenants t ON t.id = a.tenant_id WHERE a.id = $1 AND t.external_id = $2",
        )
        .bind(account_id.to_string())
        .bind(tenant_external_id)
        .fetch_optional(&self.pool)
        .await?
        .is_some();
        exists.then_some(()).ok_or(AppError::Forbidden)
    }

    pub async fn upsert_model_price(
        &self,
        model: &str,
        currency: &str,
        input_per_million: Decimal,
        output_per_million: Decimal,
    ) -> Result<ModelPrice, AppError> {
        validate_currency(currency)?;
        let input_micros = decimal_to_micros(input_per_million)?;
        let output_micros = decimal_to_micros(output_per_million)?;
        if input_micros < 0 || output_micros < 0 {
            return Err(AppError::BadRequest(
                "model prices cannot be negative".into(),
            ));
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES ($1, $2, $3, $4, $5, 'manual', $6) ON CONFLICT(model, currency) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, updated_at = excluded.updated_at",
        )
        .bind(id.to_string())
        .bind(model)
        .bind(currency.to_uppercase())
        .bind(input_micros)
        .bind(output_micros)
        .bind(unix_millis())
        .execute(&self.pool)
        .await?;
        self.model_price(model, currency).await
    }

    pub async fn model_price(&self, model: &str, currency: &str) -> Result<ModelPrice, AppError> {
        let row = sqlx::query(
            "SELECT id, input_micros_per_million, output_micros_per_million FROM model_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        Ok(ModelPrice {
            id: parse_uuid(row.try_get("id")?)?,
            input_micros_per_million: row.try_get("input_micros_per_million")?,
            output_micros_per_million: row.try_get("output_micros_per_million")?,
        })
    }

    pub async fn upsert_generation_price(
        &self,
        model: &str,
        currency: &str,
        billing_unit: &str,
        price_per_unit: Decimal,
    ) -> Result<GenerationPrice, AppError> {
        validate_currency(currency)?;
        if !matches!(billing_unit, "job" | "second" | "image" | "megapixel") {
            return Err(AppError::BadRequest(
                "billing_unit must be job, second, image, or megapixel".into(),
            ));
        }
        let micros_per_unit = decimal_to_micros(price_per_unit)?;
        if micros_per_unit < 0 {
            return Err(AppError::BadRequest(
                "generation price cannot be negative".into(),
            ));
        }
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO generation_prices (id, model, currency, billing_unit, micros_per_unit, updated_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(model, currency) DO UPDATE SET billing_unit = excluded.billing_unit, micros_per_unit = excluded.micros_per_unit, updated_at = excluded.updated_at",
        )
        .bind(id.to_string())
        .bind(model)
        .bind(currency.to_uppercase())
        .bind(billing_unit)
        .bind(micros_per_unit)
        .bind(unix_millis())
        .execute(&self.pool)
        .await?;
        self.generation_price(model, currency).await
    }

    pub async fn generation_price(
        &self,
        model: &str,
        currency: &str,
    ) -> Result<GenerationPrice, AppError> {
        let row = sqlx::query(
            "SELECT id, model, currency, billing_unit, micros_per_unit FROM generation_prices WHERE model = $1 AND currency = $2",
        )
        .bind(model)
        .bind(currency.to_uppercase())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::UnpricedModel)?;
        let micros_per_unit: i64 = row.try_get("micros_per_unit")?;
        Ok(GenerationPrice {
            id: parse_uuid(row.try_get("id")?)?,
            model: row.try_get("model")?,
            currency: row.try_get("currency")?,
            billing_unit: row.try_get("billing_unit")?,
            price_per_unit: micros_to_decimal_string(micros_per_unit),
            micros_per_unit,
        })
    }

    pub async fn create_generation_job(
        &self,
        input: CreateGenerationJobInput,
    ) -> Result<GenerationJobView, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO generation_jobs (id, tenant_id, key_id, upstream_account_id, reservation_id, public_model, upstream_model, driver, status, request_object, estimated_units, next_attempt_at, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'queued', $9, $10, $11, $12, $13)",
        )
        .bind(input.job_id.to_string())
        .bind(input.key.tenant_id.to_string())
        .bind(input.key.key_id.to_string())
        .bind(input.upstream_account_id.to_string())
        .bind(input.reservation.id.to_string())
        .bind(&input.public_model)
        .bind(&input.upstream_model)
        .bind(&input.driver)
        .bind(input.request_object)
        .bind(input.estimated_units)
        .bind(now)
        .bind(now)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', 'generation', $6, 0, 0, 0)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(input.key.tenant_id.to_string())
        .bind(input.key.key_id.to_string())
        .bind(input.job_id.to_string())
        .bind(now)
        .bind(&input.public_model)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(GenerationJobView {
            job_id: input.job_id,
            created_at: now,
            updated_at: now,
            completed_at: None,
            model: input.public_model,
            driver: input.driver,
            status: "queued".to_owned(),
            upstream_job_id: None,
            estimated_units: input.estimated_units,
            billed_units: None,
            cost: "0".to_owned(),
            error_code: None,
            result: None,
        })
    }

    pub async fn list_generation_jobs(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<GenerationJobView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json FROM generation_jobs WHERE key_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(key_id.to_string())
        .bind(limit.clamp(1, 200))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(generation_job_view).collect()
    }

    pub async fn generation_job(
        &self,
        key_id: Uuid,
        job_id: Uuid,
    ) -> Result<GenerationJobView, AppError> {
        let row = sqlx::query(
            "SELECT id, created_at, updated_at, completed_at, public_model, driver, status, upstream_job_id, estimated_units, billed_units, cost_micros, error_code, result_json FROM generation_jobs WHERE id = $1 AND key_id = $2",
        )
        .bind(job_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        generation_job_view(row)
    }

    pub async fn claim_generation_job(
        &self,
        worker_id: &str,
    ) -> Result<Option<GenerationJobWork>, AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT id FROM generation_jobs WHERE status IN ('queued', 'running') AND next_attempt_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at < $2) ORDER BY next_attempt_at, created_at, id FOR UPDATE SKIP LOCKED LIMIT 1"
            }
            DatabaseBackend::Sqlite => {
                "SELECT id FROM generation_jobs WHERE status IN ('queued', 'running') AND next_attempt_at <= $1 AND (lease_expires_at IS NULL OR lease_expires_at < $2) ORDER BY next_attempt_at, created_at, id LIMIT 1"
            }
        };
        let candidate = sqlx::query(select)
            .bind(now)
            .bind(now)
            .fetch_optional(&mut *transaction)
            .await?;
        let Some(candidate) = candidate else {
            transaction.commit().await?;
            return Ok(None);
        };
        let job_id: String = candidate.try_get("id")?;
        let claimed = sqlx::query(
            "UPDATE generation_jobs SET lease_owner = $1, lease_expires_at = $2, attempt_count = attempt_count + 1, updated_at = $3 WHERE id = $4 AND status IN ('queued', 'running') AND (lease_expires_at IS NULL OR lease_expires_at < $5)",
        )
        .bind(worker_id)
        .bind(now.saturating_add(60_000))
        .bind(now)
        .bind(&job_id)
        .bind(now)
        .execute(&mut *transaction)
        .await?;
        if claimed.rows_affected() == 0 {
            transaction.commit().await?;
            return Ok(None);
        }
        let row = sqlx::query(
            "SELECT j.id, j.created_at, j.tenant_id, j.key_id, j.upstream_account_id, j.public_model, j.upstream_model, j.driver, j.status, j.request_object, j.upstream_job_id, j.estimated_units, j.attempt_count, j.failure_count, r.id AS reservation_id, r.account_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start, p.micros_per_unit FROM generation_jobs j JOIN usage_reservations r ON r.id = j.reservation_id JOIN generation_prices p ON p.id = r.price_id WHERE j.id = $1",
        )
        .bind(&job_id)
        .fetch_one(&mut *transaction)
        .await?;
        transaction.commit().await?;
        let micros_per_unit: i64 = row.try_get("micros_per_unit")?;
        let key_id = parse_uuid(row.try_get("key_id")?)?;
        Ok(Some(GenerationJobWork {
            job_id: parse_uuid(row.try_get("id")?)?,
            created_at: row.try_get("created_at")?,
            tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
            key_id,
            upstream_account_id: parse_uuid(row.try_get("upstream_account_id")?)?,
            reservation: UsageReservation {
                id: parse_uuid(row.try_get("reservation_id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                key_id,
                reserved_micros: row.try_get("reserved_micros")?,
                input_micros_per_million: 0,
                output_micros_per_million: micros_per_unit
                    .checked_mul(1_000_000)
                    .ok_or(AppError::Internal)?,
                rate_window_start: row.try_get("rate_window_start")?,
                reserved_tokens: row.try_get("reserved_tokens")?,
            },
            public_model: row.try_get("public_model")?,
            upstream_model: row.try_get("upstream_model")?,
            driver: row.try_get("driver")?,
            status: row.try_get("status")?,
            request_object: row.try_get("request_object")?,
            upstream_job_id: row.try_get("upstream_job_id")?,
            estimated_units: row.try_get("estimated_units")?,
            attempt_count: row.try_get("attempt_count")?,
            failure_count: row.try_get("failure_count")?,
        }))
    }

    pub async fn mark_generation_submitted(
        &self,
        job_id: Uuid,
        worker_id: &str,
        upstream_job_id: &str,
    ) -> Result<(), AppError> {
        generation_update_claimed(
            sqlx::query("UPDATE generation_jobs SET status = 'running', upstream_job_id = $1, failure_count = 0, error_code = NULL, next_attempt_at = $2, lease_owner = NULL, lease_expires_at = NULL, updated_at = $3 WHERE id = $4 AND lease_owner = $5")
                .bind(upstream_job_id)
                .bind(unix_millis().saturating_add(2_000))
                .bind(unix_millis())
                .bind(job_id.to_string())
                .bind(worker_id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn reschedule_generation_job(
        &self,
        job_id: Uuid,
        worker_id: &str,
        delay_ms: i64,
        error_code: Option<&str>,
    ) -> Result<(), AppError> {
        let now = unix_millis();
        generation_update_claimed(
            sqlx::query("UPDATE generation_jobs SET next_attempt_at = $1, error_code = $2, failure_count = CASE WHEN $3 IS NULL THEN 0 ELSE failure_count + 1 END, lease_owner = NULL, lease_expires_at = NULL, updated_at = $4 WHERE id = $5 AND lease_owner = $6")
                .bind(now.saturating_add(delay_ms.max(500)))
                .bind(error_code)
                .bind(error_code)
                .bind(now)
                .bind(job_id.to_string())
                .bind(worker_id)
                .execute(&self.pool)
                .await?,
        )
    }

    pub async fn finish_generation_job(
        &self,
        input: FinishGenerationJobInput<'_>,
    ) -> Result<(), AppError> {
        if !matches!(input.status, "succeeded" | "failed" | "cancelled") {
            return Err(AppError::Internal);
        }
        let now = unix_millis();
        let result_json = input
            .result
            .map(serde_json::to_string)
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let mut transaction = self.pool.begin().await?;
        let updated = sqlx::query("UPDATE generation_jobs SET status = $1, billed_units = $2, cost_micros = $3, result_json = $4, error_code = $5, completed_at = $6, updated_at = $7, lease_owner = NULL, lease_expires_at = NULL WHERE id = $8 AND lease_owner = $9")
                .bind(input.status)
                .bind(input.billed_units)
                .bind(input.cost_micros)
                .bind(result_json)
                .bind(input.error_code)
                .bind(now)
                .bind(now)
                .bind(input.job_id.to_string())
                .bind(input.worker_id)
                .execute(&mut *transaction)
                .await?;
        if updated.rows_affected() != 1 {
            transaction.commit().await?;
            return Err(AppError::NotFound);
        }
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', 'generation', public_model, CASE WHEN status = 'succeeded' THEN 200 ELSE 502 END, $3 - created_at, 0, 0, cost_micros, error_code FROM generation_jobs WHERE id = $4",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(now)
        .bind(now)
        .bind(input.job_id.to_string())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn allowed_models(&self, key: &AuthenticatedKey) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query("SELECT model FROM model_prices WHERE currency = $1 UNION SELECT model FROM generation_prices WHERE currency = $2 ORDER BY model")
            .bind(&key.currency)
            .bind(&key.currency)
            .fetch_all(&self.pool)
            .await?;
        rows.into_iter()
            .map(|row| row.try_get::<String, _>("model").map_err(AppError::from))
            .filter(|result| {
                result
                    .as_ref()
                    .map(|model| key.policy.allows_model(model))
                    .unwrap_or(true)
            })
            .collect()
    }

    pub async fn reserve_usage(
        &self,
        key: &AuthenticatedKey,
        price: &ModelPrice,
        input_token_ceiling: i64,
        output_token_ceiling: i64,
    ) -> Result<UsageReservation, AppError> {
        let reserved_micros = priced_tokens(input_token_ceiling, price.input_micros_per_million)
            .checked_add(priced_tokens(
                output_token_ceiling,
                price.output_micros_per_million,
            ))
            .ok_or(AppError::QuotaExceeded)?;
        let reserved_tokens = input_token_ceiling
            .checked_add(output_token_ceiling)
            .ok_or(AppError::RateLimited)?;
        if reserved_tokens > key.policy.tokens_per_minute as i64 {
            return Err(AppError::RateLimited);
        }
        let now = unix_millis();
        let window_start = now / 60_000 * 60_000;
        let mut tx = self.pool.begin().await?;

        let rate_result = sqlx::query(
            "INSERT INTO rate_limit_windows (key_id, window_start, requests, tokens) VALUES ($1, $2, 1, $3) ON CONFLICT(key_id, window_start) DO UPDATE SET requests = rate_limit_windows.requests + 1, tokens = rate_limit_windows.tokens + $4 WHERE rate_limit_windows.requests < $5 AND rate_limit_windows.tokens + $6 <= $7",
        )
        .bind(key.key_id.to_string())
        .bind(window_start)
        .bind(reserved_tokens)
        .bind(reserved_tokens)
        .bind(i64::from(key.policy.requests_per_minute))
        .bind(reserved_tokens)
        .bind(key.policy.tokens_per_minute as i64)
        .execute(&mut *tx)
        .await?;
        if rate_result.rows_affected() == 0 {
            return Err(AppError::RateLimited);
        }

        let concurrency_result = sqlx::query(
            "INSERT INTO key_runtime_state (key_id, active_requests, updated_at) VALUES ($1, 1, $2) ON CONFLICT(key_id) DO UPDATE SET active_requests = CASE WHEN key_runtime_state.updated_at < $3 THEN 1 ELSE key_runtime_state.active_requests + 1 END, updated_at = excluded.updated_at WHERE key_runtime_state.updated_at < $4 OR key_runtime_state.active_requests < $5",
        )
        .bind(key.key_id.to_string())
        .bind(now)
        .bind(now.saturating_sub(30 * 60 * 1_000))
        .bind(now.saturating_sub(30 * 60 * 1_000))
        .bind(i64::from(key.policy.max_concurrency))
        .execute(&mut *tx)
        .await?;
        if concurrency_result.rows_affected() == 0 {
            return Err(AppError::RateLimited);
        }

        let active_reserved: i64 = sqlx::query(
            "SELECT CAST(COALESCE(SUM(reserved_micros), 0) AS BIGINT) AS amount FROM usage_reservations WHERE key_id = $1 AND status = 'reserved'",
        )
        .bind(key.key_id.to_string())
        .fetch_one(&mut *tx)
        .await?
        .try_get("amount")?;
        let budget_windows = [
            (
                key.policy.daily_budget.as_deref(),
                now / 86_400_000 * 86_400_000,
            ),
            (
                key.policy.weekly_budget.as_deref(),
                now.saturating_sub(7 * 86_400_000),
            ),
            (key.policy.lifetime_budget.as_deref(), 0),
        ];
        for (configured_budget, since) in budget_windows {
            let Some(configured_budget) = configured_budget else {
                continue;
            };
            let budget_micros = decimal_to_micros(
                Decimal::from_str_exact(configured_budget).map_err(|_| AppError::Internal)?,
            )?;
            let spent: i64 = sqlx::query(
                "SELECT CAST(COALESCE(SUM(-amount_micros), 0) AS BIGINT) AS amount FROM ledger_entries WHERE key_id = $1 AND kind = 'usage' AND created_at >= $2",
            )
            .bind(key.key_id.to_string())
            .bind(since)
            .fetch_one(&mut *tx)
            .await?
            .try_get("amount")?;
            if spent
                .saturating_add(active_reserved)
                .saturating_add(reserved_micros)
                > budget_micros
            {
                return Err(AppError::QuotaExceeded);
            }
        }

        let balance_result = sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros - $1, reserved_micros = reserved_micros + $2, updated_at = $3 WHERE id = $4 AND currency = $5 AND available_micros >= $6",
        )
        .bind(reserved_micros)
        .bind(reserved_micros)
        .bind(now)
        .bind(key.account_id.to_string())
        .bind(&key.currency)
        .bind(reserved_micros)
        .execute(&mut *tx)
        .await?;
        if balance_result.rows_affected() == 0 {
            return Err(AppError::QuotaExceeded);
        }

        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO usage_reservations (id, account_id, key_id, price_id, reserved_micros, reserved_tokens, rate_window_start, status, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 'reserved', $8)",
        )
        .bind(id.to_string())
        .bind(key.account_id.to_string())
        .bind(key.key_id.to_string())
        .bind(price.id.to_string())
        .bind(reserved_micros)
        .bind(reserved_tokens)
        .bind(window_start)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(UsageReservation {
            id,
            account_id: key.account_id,
            key_id: key.key_id,
            reserved_micros,
            input_micros_per_million: price.input_micros_per_million,
            output_micros_per_million: price.output_micros_per_million,
            rate_window_start: window_start,
            reserved_tokens,
        })
    }

    pub async fn settle_usage(
        &self,
        reservation: &UsageReservation,
        input_tokens: i64,
        output_tokens: i64,
    ) -> Result<i64, AppError> {
        let actual_micros = priced_tokens(input_tokens, reservation.input_micros_per_million)
            .checked_add(priced_tokens(
                output_tokens,
                reservation.output_micros_per_million,
            ))
            .ok_or(AppError::Internal)?;
        let released = reservation
            .reserved_micros
            .saturating_sub(actual_micros)
            .max(0);
        let overage = actual_micros
            .saturating_sub(reservation.reserved_micros)
            .max(0);
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE usage_reservations SET actual_micros = $1, status = 'settled', settled_at = $2 WHERE id = $3 AND status = 'reserved'",
        )
        .bind(actual_micros)
        .bind(now)
        .bind(reservation.id.to_string())
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            let existing: i64 = sqlx::query(
                "SELECT actual_micros FROM usage_reservations WHERE id = $1 AND status = 'settled'",
            )
            .bind(reservation.id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("actual_micros")?;
            tx.commit().await?;
            return Ok(existing);
        }
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1 - $2, reserved_micros = reserved_micros - $3, updated_at = $4 WHERE id = $5",
        )
        .bind(released)
        .bind(overage)
        .bind(reservation.reserved_micros)
        .bind(now)
        .bind(reservation.account_id.to_string())
        .execute(&mut *tx)
        .await?;
        let actual_tokens = input_tokens.saturating_add(output_tokens).max(0);
        sqlx::query(
            "UPDATE rate_limit_windows SET tokens = CASE WHEN tokens - $1 + $2 < 0 THEN 0 ELSE tokens - $3 + $4 END WHERE key_id = $5 AND window_start = $6",
        )
        .bind(reservation.reserved_tokens)
        .bind(actual_tokens)
        .bind(reservation.reserved_tokens)
        .bind(actual_tokens)
        .bind(reservation.key_id.to_string())
        .bind(reservation.rate_window_start)
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "UPDATE key_runtime_state SET active_requests = CASE WHEN active_requests > 0 THEN active_requests - 1 ELSE 0 END, updated_at = $1 WHERE key_id = $2",
        )
        .bind(now)
        .bind(reservation.key_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) SELECT $1, $2, $3, 'usage', $4, currency, $5, $6 FROM credit_accounts WHERE id = $7",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(reservation.account_id.to_string())
        .bind(reservation.key_id.to_string())
        .bind(-actual_micros)
        .bind(reservation.id.to_string())
        .bind(now)
        .bind(reservation.account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(actual_micros)
    }

    pub async fn release_orphaned_reservations(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(30 * 60 * 1_000);
        let rows = sqlx::query(
            "SELECT r.id, r.account_id, r.key_id, r.reserved_micros, r.reserved_tokens, r.rate_window_start FROM usage_reservations r WHERE r.status = 'reserved' AND r.created_at < $1 AND NOT EXISTS (SELECT 1 FROM request_records q WHERE q.reservation_id = r.id) AND NOT EXISTS (SELECT 1 FROM generation_jobs g WHERE g.reservation_id = r.id) ORDER BY r.created_at, r.id LIMIT $2",
        )
        .bind(cutoff)
        .bind(limit.clamp(1, 1_000))
        .fetch_all(&self.pool)
        .await?;
        let mut released = 0_u64;
        for row in rows {
            let reservation = UsageReservation {
                id: parse_uuid(row.try_get("id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                key_id: parse_uuid(row.try_get("key_id")?)?,
                reserved_micros: row.try_get("reserved_micros")?,
                input_micros_per_million: 0,
                output_micros_per_million: 0,
                rate_window_start: row.try_get("rate_window_start")?,
                reserved_tokens: row.try_get("reserved_tokens")?,
            };
            self.settle_usage(&reservation, 0, 0).await?;
            released = released.saturating_add(1);
        }
        Ok(released)
    }

    pub async fn expire_key_provisioning_responses(&self, limit: i64) -> Result<u64, AppError> {
        let cutoff = unix_millis().saturating_sub(24 * 60 * 60 * 1_000);
        let result = sqlx::query(
            "UPDATE key_records SET issued_key_ciphertext = NULL WHERE id IN (SELECT id FROM key_records WHERE issued_key_ciphertext IS NOT NULL AND created_at < $1 ORDER BY created_at, id LIMIT $2)",
        )
        .bind(cutoff)
        .bind(limit.clamp(1, 10_000))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    pub async fn plugin_kv_get(
        &self,
        plugin_id: &str,
        key: &str,
    ) -> Result<Option<Vec<u8>>, AppError> {
        validate_plugin_kv_key(plugin_id, key)?;
        let row = sqlx::query("SELECT value FROM plugin_kv WHERE plugin_id = $1 AND key = $2")
            .bind(plugin_id)
            .bind(key)
            .fetch_optional(&self.pool)
            .await?;
        row.map(|row| row.try_get("value").map_err(AppError::from))
            .transpose()
    }

    pub async fn plugin_kv_put(
        &self,
        plugin_id: &str,
        key: &str,
        value: &[u8],
    ) -> Result<(), AppError> {
        const MAX_VALUE_BYTES: usize = 1024 * 1024;
        const MAX_PLUGIN_BYTES: i64 = 16 * 1024 * 1024;
        validate_plugin_kv_key(plugin_id, key)?;
        if value.len() > MAX_VALUE_BYTES {
            return Err(AppError::BadRequest("plugin KV value exceeds 1 MiB".into()));
        }
        let mut transaction = self.pool.begin().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 734627102948313))")
                .bind(plugin_id)
                .execute(&mut *transaction)
                .await?;
        }
        let length_expression = match self.backend {
            DatabaseBackend::PostgreSql => "OCTET_LENGTH(value)",
            DatabaseBackend::Sqlite => "LENGTH(value)",
        };
        let usage_query = format!(
            "SELECT COALESCE(SUM({length_expression}), 0) AS total_bytes, COALESCE(MAX(CASE WHEN key = $2 THEN {length_expression} ELSE 0 END), 0) AS current_bytes FROM plugin_kv WHERE plugin_id = $1"
        );
        let usage = sqlx::query(&usage_query)
            .bind(plugin_id)
            .bind(key)
            .fetch_one(&mut *transaction)
            .await?;
        let total_bytes: i64 = usage.try_get("total_bytes")?;
        let current_bytes: i64 = usage.try_get("current_bytes")?;
        let next_bytes = total_bytes
            .saturating_sub(current_bytes)
            .saturating_add(value.len() as i64);
        if next_bytes > MAX_PLUGIN_BYTES {
            return Err(AppError::BadRequest(
                "plugin KV namespace exceeds 16 MiB".into(),
            ));
        }
        sqlx::query(
            "INSERT INTO plugin_kv (plugin_id, key, value, updated_at) VALUES ($1, $2, $3, $4) ON CONFLICT(plugin_id, key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        )
        .bind(plugin_id)
        .bind(key)
        .bind(value)
        .bind(unix_millis())
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn grant(
        &self,
        account_id: Uuid,
        amount: Decimal,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let source = source.trim();
        if source.is_empty() || source.len() > 200 {
            return Err(AppError::BadRequest(
                "source must contain 1 to 200 characters".into(),
            ));
        }
        let amount_micros = decimal_to_micros(amount)?;
        if amount_micros <= 0 {
            return Err(AppError::BadRequest("grant amount must be positive".into()));
        }
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT currency FROM credit_accounts WHERE id = $1")
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let currency: String = row.try_get("currency")?;
        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, created_at) VALUES ($1, $2, 'grant', $3, $4, $5, $6, $7) ON CONFLICT(idempotency_key) DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(amount_micros)
        .bind(&currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing: i64 = sqlx::query(
                "SELECT amount_micros FROM ledger_entries WHERE idempotency_key = $1 AND account_id = $2 AND kind = 'grant'",
            )
            .bind(idempotency_key)
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::Forbidden)?
            .try_get("amount_micros")?;
            tx.commit().await?;
            return Ok(micros_to_decimal_string(existing));
        }
        sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros + $1, updated_at = $2 WHERE id = $3",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }

    pub async fn reverse_grant(
        &self,
        account_id: Uuid,
        grant_idempotency_key: &str,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        validate_idempotency_key(grant_idempotency_key, "grant_idempotency_key")?;
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let source = source.trim();
        if source.is_empty() || source.len() > 200 {
            return Err(AppError::BadRequest(
                "source must contain 1 to 200 characters".into(),
            ));
        }

        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let account_lock =
            sqlx::query("UPDATE credit_accounts SET updated_at = updated_at WHERE id = $1")
                .bind(account_id.to_string())
                .execute(&mut *tx)
                .await?;
        if account_lock.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        let original = sqlx::query(
            "SELECT id, amount_micros, currency, created_at FROM ledger_entries WHERE account_id = $1 AND kind = 'grant' AND idempotency_key = $2",
        )
        .bind(account_id.to_string())
        .bind(grant_idempotency_key)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(AppError::NotFound)?;
        let original_id: String = original.try_get("id")?;
        let amount_micros: i64 = original.try_get("amount_micros")?;
        let currency: String = original.try_get("currency")?;
        let granted_at: i64 = original.try_get("created_at")?;
        let may_have_been_consumed = sqlx::query(
            "SELECT id FROM ledger_entries WHERE account_id = $1 AND kind = 'usage' AND created_at >= $2 LIMIT 1",
        )
        .bind(account_id.to_string())
        .bind(granted_at)
        .fetch_optional(&mut *tx)
        .await?
        .is_some();
        if may_have_been_consumed {
            return Err(AppError::BadRequest(
                "grant cannot be automatically reversed after account usage".into(),
            ));
        }

        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, reference_entry_id, created_at) VALUES ($1, $2, 'grant_reversal', $3, $4, $5, $6, $7, $8) ON CONFLICT DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(-amount_micros)
        .bind(&currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(&original_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let replay = sqlx::query(
                "SELECT amount_micros, reference_entry_id FROM ledger_entries WHERE account_id = $1 AND kind = 'grant_reversal' AND idempotency_key = $2",
            )
            .bind(account_id.to_string())
            .bind(idempotency_key)
            .fetch_optional(&mut *tx)
            .await?;
            if let Some(replay) = replay {
                let replay_reference: Option<String> = replay.try_get("reference_entry_id")?;
                if replay_reference.as_deref() != Some(original_id.as_str()) {
                    return Err(AppError::BadRequest(
                        "Idempotency-Key was already used for a different grant reversal".into(),
                    ));
                }
                let replay_amount: i64 = replay.try_get("amount_micros")?;
                tx.commit().await?;
                return Ok(micros_to_decimal_string(replay_amount.saturating_abs()));
            }
            let existing_idempotency =
                sqlx::query("SELECT kind FROM ledger_entries WHERE idempotency_key = $1")
                    .bind(idempotency_key)
                    .fetch_optional(&mut *tx)
                    .await?;
            if existing_idempotency.is_some() {
                return Err(AppError::BadRequest(
                    "Idempotency-Key was already used for a different ledger operation".into(),
                ));
            }
            return Err(AppError::BadRequest("grant was already reversed".into()));
        }

        let updated = sqlx::query(
            "UPDATE credit_accounts SET available_micros = available_micros - $1, updated_at = $2 WHERE id = $3 AND currency = $4 AND available_micros >= $5",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .bind(&currency)
        .bind(amount_micros)
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() != 1 {
            let exists = sqlx::query("SELECT id FROM credit_accounts WHERE id = $1")
                .bind(account_id.to_string())
                .fetch_optional(&mut *tx)
                .await?
                .is_some();
            return Err(if exists {
                AppError::QuotaExceeded
            } else {
                AppError::NotFound
            });
        }
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }

    pub async fn record_request_started(&self, request: NewRequest) -> Result<(), AppError> {
        let now = unix_millis();
        let mut transaction = self.pool.begin().await?;
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, 0, 0, 0)",
        )
        .bind(request.request_id.to_string())
        .bind(request.tenant_id.to_string())
        .bind(request.key_id.to_string())
        .bind(now)
        .bind(&request.protocol)
        .bind(&request.model)
        .bind(&request.request_object)
        .bind(request.reservation_id.to_string())
        .bind(request.upstream_account_id.map(|id| id.to_string()))
        .bind(request.model_route_id.map(|id| id.to_string()))
        .execute(&mut *transaction)
        .await?;
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, input_tokens, output_tokens, cost_micros) VALUES ($1, $2, $3, $4, $5, 'started', $6, $7, 0, 0, 0)",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(request.tenant_id.to_string())
        .bind(request.key_id.to_string())
        .bind(request.request_id.to_string())
        .bind(now)
        .bind(request.protocol)
        .bind(request.model)
        .execute(&mut *transaction)
        .await?;
        transaction.commit().await?;
        Ok(())
    }

    pub async fn record_conversation_observation(
        &self,
        key: &AuthenticatedKey,
        request_id: Uuid,
        request_json: &serde_json::Value,
        explicit_session_id: Option<&str>,
        client_name: Option<&str>,
    ) -> Result<Uuid, AppError> {
        let atoms = extract_atoms(request_json);
        let nodes = build_prefix(&atoms);
        let atom_hashes: Vec<_> = atoms.iter().map(|atom| atom.content_hash.clone()).collect();
        let atom_hashes_json =
            serde_json::to_string(&atom_hashes).map_err(|_| AppError::Internal)?;
        let leaf = nodes.last().map(|node| node.node_hash.clone());
        let now = unix_millis();
        let observation_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;

        for atom in &atoms {
            sqlx::query(
                "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7) ON CONFLICT(tenant_id, content_hash) DO NOTHING",
            )
            .bind(key.tenant_id.to_string())
            .bind(&atom.content_hash)
            .bind(&atom.instance_hash)
            .bind(&atom.role)
            .bind(&atom.kind)
            .bind(serde_json::to_string(&atom.content).map_err(|_| AppError::Internal)?)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        for node in &nodes {
            sqlx::query(
                "INSERT INTO context_nodes (tenant_id, node_hash, parent_hash, atom_hash, depth, created_at) VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT(tenant_id, node_hash) DO NOTHING",
            )
            .bind(key.tenant_id.to_string())
            .bind(&node.node_hash)
            .bind(&node.parent_hash)
            .bind(&node.atom_hash)
            .bind(node.depth as i64)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }

        let candidates = sqlx::query(
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.explicit_session_id, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = $1 AND c.principal_id = $2 ORDER BY o.created_at DESC LIMIT 50",
        )
        .bind(key.tenant_id.to_string())
        .bind(key.principal_id.to_string())
        .fetch_all(&mut *tx)
        .await?;

        let mut selected: Option<(String, String, RelationKind, i64, i64)> = None;
        for row in candidates {
            let candidate_session: Option<String> = row.try_get("explicit_session_id")?;
            let previous_hashes_json: String = row.try_get("atom_hashes_json")?;
            let previous_hashes: Vec<String> =
                serde_json::from_str(&previous_hashes_json).unwrap_or_default();
            let (relation, confidence) = infer_hash_relation(&previous_hashes, &atom_hashes);
            let created_at: i64 = row.try_get("created_at")?;
            let explicit_match = explicit_session_id.is_some()
                && explicit_session_id == candidate_session.as_deref();
            let exact_prefix = confidence >= 700;
            let recent_candidate = now.saturating_sub(created_at) <= 30 * 60 * 1_000;
            if explicit_match || exact_prefix || (selected.is_none() && recent_candidate) {
                let relation = if explicit_match && atom_hashes.len() * 2 < previous_hashes.len() {
                    RelationKind::Compacts
                } else {
                    relation
                };
                let confidence = if explicit_match {
                    confidence.max(990)
                } else {
                    confidence
                };
                selected = Some((
                    row.try_get("id")?,
                    row.try_get("cluster_id")?,
                    relation,
                    confidence,
                    created_at,
                ));
                if explicit_match || exact_prefix {
                    break;
                }
            }
        }

        let cluster_id = if let Some((_, cluster_id, _, _, _)) = &selected {
            parse_uuid(cluster_id.clone())?
        } else {
            let id = Uuid::now_v7();
            sqlx::query(
                "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6)",
            )
            .bind(id.to_string())
            .bind(key.tenant_id.to_string())
            .bind(key.principal_id.to_string())
            .bind(explicit_session_id)
            .bind(now)
            .bind(now)
            .execute(&mut *tx)
            .await?;
            id
        };

        sqlx::query(
            "INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, leaf_node_hash, atom_hashes_json, explicit_session_id, client_name, created_at, inference_version) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, 1)",
        )
        .bind(observation_id.to_string())
        .bind(cluster_id.to_string())
        .bind(request_id.to_string())
        .bind(key.key_id.to_string())
        .bind(leaf)
        .bind(atom_hashes_json)
        .bind(explicit_session_id)
        .bind(client_name)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        if let Some((previous_id, _, relation, confidence, _)) = selected {
            sqlx::query(
                "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES ($1, $2, $3, $4, $5, $6, $7, 0, 1, $8)",
            )
            .bind(Uuid::now_v7().to_string())
            .bind(cluster_id.to_string())
            .bind(previous_id)
            .bind(observation_id.to_string())
            .bind(relation_name(relation))
            .bind(confidence)
            .bind(serde_json::json!({
                "explicit_session": explicit_session_id.is_some(),
                "semantic_prefix": true,
                "inference_version": 1
            }).to_string())
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query("UPDATE conversation_clusters SET updated_at = $1 WHERE id = $2")
            .bind(now)
            .bind(cluster_id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE request_records SET conversation_cluster_id = $1 WHERE id = $2")
            .bind(cluster_id.to_string())
            .bind(request_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(cluster_id)
    }

    pub async fn record_request_finished(&self, request: FinishRequest) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let completed_at = unix_millis();
        let updated = sqlx::query(
            "UPDATE request_records SET status_code = $1, duration_ms = $2, input_tokens = $3, output_tokens = $4, cost_micros = $5, error_code = $6, response_object = $7, completed_at = $8 WHERE id = $9 AND completed_at IS NULL",
        )
        .bind(request.status_code)
        .bind(request.duration_ms)
        .bind(request.input_tokens)
        .bind(request.output_tokens)
        .bind(request.cost_micros)
        .bind(&request.error_code)
        .bind(&request.response_object)
        .bind(completed_at)
        .bind(request.request_id.to_string())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO usage_daily_aggregates (key_id, day_bucket, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros) SELECT key_id, created_at / 86400000, model, CASE WHEN status_code >= 200 AND status_code < 400 THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), 1, input_tokens, output_tokens, cost_micros FROM request_records WHERE id = $1 ON CONFLICT(key_id, day_bucket, model, status_class, error_code) DO UPDATE SET requests = usage_daily_aggregates.requests + 1, input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens, cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros",
        )
        .bind(request.request_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO request_events (event_id, tenant_id, key_id, request_id, event_at, event_kind, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code) SELECT $1, tenant_id, key_id, id, $2, 'finished', protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE id = $3",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(completed_at)
        .bind(request.request_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn request_events_after(
        &self,
        tenant_external_id: &str,
        after_event_at: i64,
        after_event_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<RequestEventView>, AppError> {
        let after_event_id = after_event_id
            .map(|event_id| event_id.to_string())
            .unwrap_or_default();
        let rows = sqlx::query(
            "SELECT e.event_id, e.request_id, e.event_at, e.event_kind, e.key_id, e.protocol, e.model, e.status_code, e.duration_ms, e.input_tokens, e.output_tokens, e.cost_micros, e.error_code FROM request_events e JOIN tenants t ON t.id = e.tenant_id WHERE t.external_id = $1 AND (e.event_at > $2 OR (e.event_at = $3 AND e.event_id > $4)) ORDER BY e.event_at ASC, e.event_id ASC LIMIT $5",
        )
        .bind(tenant_external_id)
        .bind(after_event_at)
        .bind(after_event_at)
        .bind(after_event_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestEventView {
                    event_id: parse_uuid(row.try_get("event_id")?)?,
                    request_id: parse_uuid(row.try_get("request_id")?)?,
                    event_at: row.try_get("event_at")?,
                    event_kind: row.try_get("event_kind")?,
                    key_id: parse_uuid(row.try_get("key_id")?)?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
    }

    pub async fn list_requests(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = $1 ORDER BY created_at DESC, id DESC LIMIT $2",
        )
        .bind(key_id.to_string())
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;

        rows.into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
    }

    pub async fn list_all_requests(
        &self,
        tenant_external_id: &str,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM (SELECT r.id, r.created_at, r.protocol, r.model, r.status_code, r.duration_ms, r.input_tokens, r.output_tokens, r.cost_micros, r.error_code FROM request_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1 UNION ALL SELECT g.id, g.created_at, 'generation' AS protocol, g.public_model AS model, CASE WHEN g.status = 'succeeded' THEN 200 WHEN g.status IN ('failed', 'cancelled') THEN 502 ELSE NULL END AS status_code, CASE WHEN g.completed_at IS NULL THEN NULL ELSE g.completed_at - g.created_at END AS duration_ms, 0 AS input_tokens, 0 AS output_tokens, g.cost_micros, g.error_code FROM generation_jobs g JOIN tenants t ON t.id = g.tenant_id WHERE t.external_id = $2) AS all_requests ORDER BY created_at DESC, id DESC LIMIT $3",
        )
        .bind(tenant_external_id)
        .bind(tenant_external_id)
        .bind(limit.clamp(1, 500))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect()
    }

    pub async fn request_archive_refs(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, request_object, response_object FROM request_records WHERE id = $1 AND key_id = $2",
        )
        .bind(request_id.to_string())
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::NotFound)?;
        Ok(RequestArchiveRefs {
            view: RequestView {
                request_id: parse_uuid(row.try_get("id")?)?,
                created_at: row.try_get("created_at")?,
                protocol: row.try_get("protocol")?,
                model: row.try_get("model")?,
                status_code: row.try_get("status_code")?,
                duration_ms: row.try_get("duration_ms")?,
                input_tokens: row.try_get("input_tokens")?,
                output_tokens: row.try_get("output_tokens")?,
                cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                error_code: row.try_get("error_code")?,
            },
            request_object: row.try_get("request_object")?,
            response_object: row.try_get("response_object")?,
        })
    }

    pub async fn conversation_clusters(
        &self,
        key_id: Uuid,
    ) -> Result<Vec<ConversationClusterView>, AppError> {
        let rows = sqlx::query(
            "SELECT c.id, c.explicit_session_id, c.updated_at, (SELECT COUNT(*) FROM conversation_observations count_o WHERE count_o.cluster_id = c.id AND count_o.key_id = $1) AS request_count, (SELECT COUNT(*) FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id WHERE e.cluster_id = c.id AND target_o.key_id = $2 AND e.relation_kind = 'candidate') AS candidate_edge_count FROM conversation_clusters c WHERE EXISTS (SELECT 1 FROM conversation_observations own_o WHERE own_o.cluster_id = c.id AND own_o.key_id = $3) ORDER BY c.updated_at DESC",
        )
        .bind(key_id.to_string())
        .bind(key_id.to_string())
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| {
                Ok(ConversationClusterView {
                    cluster_id: parse_uuid(row.try_get("id")?)?,
                    explicit_session_id: row.try_get("explicit_session_id")?,
                    updated_at: row.try_get("updated_at")?,
                    request_count: row.try_get("request_count")?,
                    candidate_edge_count: row.try_get("candidate_edge_count")?,
                })
            })
            .collect()
    }

    pub async fn conversation_cluster_detail(
        &self,
        key_id: Uuid,
        cluster_id: Uuid,
    ) -> Result<ConversationClusterDetail, AppError> {
        let cluster = self
            .conversation_clusters(key_id)
            .await?
            .into_iter()
            .find(|cluster| cluster.cluster_id == cluster_id)
            .ok_or(AppError::NotFound)?;
        let request_rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = $1 AND conversation_cluster_id = $2 ORDER BY created_at ASC, id ASC",
        )
        .bind(key_id.to_string())
        .bind(cluster_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let requests = request_rows
            .into_iter()
            .map(|row| {
                Ok(RequestView {
                    request_id: parse_uuid(row.try_get("id")?)?,
                    created_at: row.try_get("created_at")?,
                    protocol: row.try_get("protocol")?,
                    model: row.try_get("model")?,
                    status_code: row.try_get("status_code")?,
                    duration_ms: row.try_get("duration_ms")?,
                    input_tokens: row.try_get("input_tokens")?,
                    output_tokens: row.try_get("output_tokens")?,
                    cost: micros_to_decimal_string(row.try_get("cost_micros")?),
                    error_code: row.try_get("error_code")?,
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        let edge_rows = sqlx::query(
            "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = $1 AND target_o.key_id = $2 AND (source_o.key_id = $3 OR source_o.id IS NULL) ORDER BY target_o.created_at ASC",
        )
        .bind(cluster_id.to_string())
        .bind(key_id.to_string())
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        let edges = edge_rows
            .into_iter()
            .map(|row| {
                let from_request_id: Option<String> = row.try_get("from_request_id")?;
                let evidence: String = row.try_get("evidence_json")?;
                let confidence: i64 = row.try_get("confidence_millis")?;
                Ok(ConversationEdgeView {
                    from_request_id: from_request_id.map(parse_uuid).transpose()?,
                    to_request_id: parse_uuid(row.try_get("to_request_id")?)?,
                    relation: row.try_get("relation_kind")?,
                    confidence: confidence as f64 / 1_000.0,
                    evidence: serde_json::from_str(&evidence).unwrap_or(serde_json::Value::Null),
                })
            })
            .collect::<Result<Vec<_>, AppError>>()?;
        Ok(ConversationClusterDetail {
            cluster,
            requests,
            edges,
        })
    }

    pub async fn stats(&self, key_id: Uuid) -> Result<SelfStats, AppError> {
        let key_id = key_id.to_string();
        let summary_row = sqlx::query(
            "SELECT CAST(COALESCE(SUM(total_requests), 0) AS BIGINT) AS total_requests, CAST(COALESCE(SUM(successful_requests), 0) AS BIGINT) AS successful_requests, CAST(COALESCE(SUM(failed_requests), 0) AS BIGINT) AS failed_requests, CAST(COALESCE(SUM(input_tokens), 0) AS BIGINT) AS input_tokens, CAST(COALESCE(SUM(output_tokens), 0) AS BIGINT) AS output_tokens, CAST(COALESCE(SUM(cost_micros), 0) AS BIGINT) AS cost_micros FROM (SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT COUNT(*) AS total_requests, COALESCE(SUM(CASE WHEN status = 'succeeded' THEN 1 ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status IN ('failed', 'cancelled') THEN 1 ELSE 0 END), 0) AS failed_requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_jobs WHERE key_id = $2 AND status IN ('succeeded', 'failed', 'cancelled')) AS totals",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_one(&self.pool)
        .await?;
        let summary = StatsSummary {
            total_requests: summary_row.try_get("total_requests")?,
            successful_requests: summary_row.try_get("successful_requests")?,
            failed_requests: summary_row.try_get("failed_requests")?,
            input_tokens: summary_row.try_get("input_tokens")?,
            output_tokens: summary_row.try_get("output_tokens")?,
            total_cost: micros_to_decimal_string(summary_row.try_get("cost_micros")?),
        };

        let model_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT model AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT public_model AS name, COUNT(*) AS requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_jobs WHERE key_id = $2 AND status IN ('succeeded', 'failed', 'cancelled') GROUP BY public_model) AS model_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;

        let day_rows = sqlx::query(
            "SELECT day_bucket, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT day_bucket, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 UNION ALL SELECT created_at / 86400000 AS day_bucket, COUNT(*) AS requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_jobs WHERE key_id = $2 AND status IN ('succeeded', 'failed', 'cancelled') GROUP BY created_at / 86400000) AS day_totals GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_day = day_rows
            .into_iter()
            .map(|row| {
                let day_bucket: i64 = row.try_get("day_bucket")?;
                let name = chrono::DateTime::from_timestamp(day_bucket.saturating_mul(86_400), 0)
                    .map(|value| value.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "unknown".to_owned());
                aggregate_bucket(row, name)
            })
            .collect::<Result<Vec<_>, AppError>>()?;

        let error_rows = sqlx::query(
            "SELECT name, CAST(SUM(requests) AS BIGINT) AS requests, CAST(SUM(input_tokens) AS BIGINT) AS input_tokens, CAST(SUM(output_tokens) AS BIGINT) AS output_tokens, CAST(SUM(cost_micros) AS BIGINT) AS cost_micros FROM (SELECT error_code AS name, requests, input_tokens, output_tokens, cost_micros FROM usage_daily_aggregates WHERE key_id = $1 AND error_code <> '' UNION ALL SELECT error_code AS name, COUNT(*) AS requests, 0 AS input_tokens, 0 AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM generation_jobs WHERE key_id = $2 AND status IN ('failed', 'cancelled') AND error_code IS NOT NULL AND error_code <> '' GROUP BY error_code) AS error_totals GROUP BY name ORDER BY requests DESC, name ASC",
        )
        .bind(&key_id)
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let errors = aggregate_buckets(error_rows)?;

        Ok(SelfStats {
            key_id: parse_uuid(key_id)?,
            summary,
            by_model,
            by_day,
            errors,
        })
    }
}

async fn apply_migration_range(
    transaction: &mut sqlx::Transaction<'_, sqlx::Any>,
    migrations: &[Migration],
    first: i64,
    last: i64,
) -> Result<(), sqlx::Error> {
    for migration in migrations
        .iter()
        .filter(|migration| (first..=last).contains(&migration.version))
    {
        let applied = sqlx::query("SELECT version FROM schema_migrations WHERE version = $1")
            .bind(migration.version)
            .fetch_optional(&mut **transaction)
            .await?
            .is_some();
        if applied {
            continue;
        }
        for statement in migration
            .sql
            .split(';')
            .map(str::trim)
            .filter(|statement| !statement.is_empty())
        {
            sqlx::query(statement)
                .execute(&mut **transaction)
                .await
                .map_err(|error| {
                    sqlx::Error::Protocol(format!(
                        "migration {} ({}) failed at statement `{statement}`: {error}",
                        migration.version, migration.name
                    ))
                })?;
        }
        sqlx::query(
            "INSERT INTO schema_migrations (version, name, applied_at) VALUES ($1, $2, $3)",
        )
        .bind(migration.version)
        .bind(migration.name)
        .bind(unix_millis())
        .execute(&mut **transaction)
        .await?;
    }
    Ok(())
}

async fn insert_credential(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    issued: &crypto::IssuedCredential,
    generation: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO key_credentials (id, key_id, generation, secret_hash, fingerprint, created_at) VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(issued.credential_id.to_string())
    .bind(issued.key_id.to_string())
    .bind(generation)
    .bind(issued.secret_hash.clone())
    .bind(&issued.fingerprint)
    .bind(now)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn aggregate_buckets(rows: Vec<sqlx::any::AnyRow>) -> Result<Vec<StatsBucket>, AppError> {
    rows.into_iter()
        .map(|row| {
            let name: String = row.try_get("name")?;
            aggregate_bucket(row, name)
        })
        .collect()
}

fn upstream_account_view(row: sqlx::any::AnyRow) -> Result<UpstreamAccountView, AppError> {
    let config_json: String = row.try_get("config_json")?;
    Ok(UpstreamAccountView {
        id: parse_uuid(row.try_get("id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        name: row.try_get("name")?,
        driver: row.try_get("driver")?,
        auth_kind: row.try_get("auth_kind")?,
        credential_generation: row.try_get("credential_generation")?,
        status: row.try_get("status")?,
        config: serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?,
        credential_expires_at: row.try_get("expires_at")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn aggregate_bucket(row: sqlx::any::AnyRow, name: String) -> Result<StatsBucket, AppError> {
    Ok(StatsBucket {
        name,
        requests: row.try_get("requests")?,
        input_tokens: row.try_get("input_tokens")?,
        output_tokens: row.try_get("output_tokens")?,
        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
    })
}

fn infer_hash_relation(previous: &[String], current: &[String]) -> (RelationKind, i64) {
    let shared = previous
        .iter()
        .zip(current)
        .take_while(|(left, right)| left == right)
        .count();
    if shared == previous.len() && shared == current.len() {
        (RelationKind::Retry, 980)
    } else if shared == previous.len() && current.len() > previous.len() {
        (RelationKind::Continues, 950)
    } else if shared > 0 && shared + 1 >= previous.len().min(current.len()) {
        (RelationKind::Edit, 820)
    } else if shared >= 2 {
        (RelationKind::Branch, 720)
    } else {
        (RelationKind::Candidate, 350)
    }
}

fn relation_name(relation: RelationKind) -> &'static str {
    match relation {
        RelationKind::Continues => "continues",
        RelationKind::Retry => "retry",
        RelationKind::Edit => "edit",
        RelationKind::Branch => "branch",
        RelationKind::Compacts => "compacts",
        RelationKind::Subagent => "subagent",
        RelationKind::Candidate => "candidate",
    }
}

fn validate_currency(currency: &str) -> Result<(), AppError> {
    match currency.to_uppercase().as_str() {
        "USD" | "CNY" => Ok(()),
        _ => Err(AppError::BadRequest("currency must be USD or CNY".into())),
    }
}

fn validate_idempotency_key(value: &str, field: &str) -> Result<(), AppError> {
    let value = value.trim();
    if value.is_empty() || value.len() > 200 || !value.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(AppError::BadRequest(format!(
            "{field} must contain at most 200 visible ASCII characters"
        )));
    }
    Ok(())
}

fn validate_plugin_kv_key(plugin_id: &str, key: &str) -> Result<(), AppError> {
    if plugin_id.is_empty()
        || plugin_id.len() > 120
        || !plugin_id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    {
        return Err(AppError::BadRequest("invalid plugin id for KV".into()));
    }
    if key.is_empty()
        || key.len() > 256
        || !key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b'/'))
    {
        return Err(AppError::BadRequest(
            "plugin KV key must contain 1 to 256 safe ASCII characters".into(),
        ));
    }
    Ok(())
}

fn validate_service_token_input(input: &CreateServiceTokenInput) -> Result<(), AppError> {
    if input.name.trim().is_empty() || input.name.len() > 120 {
        return Err(AppError::BadRequest(
            "service token name must contain 1 to 120 characters".into(),
        ));
    }
    if input.scopes.is_empty() || input.scopes.len() > 32 {
        return Err(AppError::BadRequest(
            "service token must contain 1 to 32 scopes".into(),
        ));
    }
    if input.scopes.iter().any(|scope| {
        scope.is_empty()
            || scope.len() > 80
            || !scope.bytes().all(|byte| {
                byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'_' | b'-' | b'*')
            })
    }) {
        return Err(AppError::BadRequest(
            "service token scopes contain unsupported characters".into(),
        ));
    }
    if input
        .tenant_external_id
        .as_deref()
        .is_some_and(|tenant| tenant.trim().is_empty() || tenant.len() > 200)
    {
        return Err(AppError::BadRequest(
            "tenant_external_id must contain 1 to 200 characters".into(),
        ));
    }
    Ok(())
}

fn validate_policy_budgets(policy: &KeyPolicy) -> Result<(), AppError> {
    for value in [
        policy.daily_budget.as_deref(),
        policy.weekly_budget.as_deref(),
        policy.lifetime_budget.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        let decimal = Decimal::from_str_exact(value)
            .map_err(|_| AppError::BadRequest("key budgets must be decimal strings".into()))?;
        if decimal.is_sign_negative() {
            return Err(AppError::BadRequest(
                "key budgets cannot be negative".into(),
            ));
        }
        decimal_to_micros(decimal)?;
    }
    Ok(())
}

fn decimal_to_micros(value: Decimal) -> Result<i64, AppError> {
    let scaled = value * Decimal::from(crate::model::MONEY_SCALE);
    if !scaled.fract().is_zero() {
        return Err(AppError::BadRequest(
            "monetary values support at most 6 decimal places".into(),
        ));
    }
    scaled
        .to_i64()
        .ok_or_else(|| AppError::BadRequest("monetary value is out of range".into()))
}

fn parse_uuid(value: String) -> Result<Uuid, AppError> {
    Uuid::parse_str(&value).map_err(|_| AppError::Internal)
}

fn generation_job_view(row: AnyRow) -> Result<GenerationJobView, AppError> {
    let result_json: Option<String> = row.try_get("result_json")?;
    Ok(GenerationJobView {
        job_id: parse_uuid(row.try_get("id")?)?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
        completed_at: row.try_get("completed_at")?,
        model: row.try_get("public_model")?,
        driver: row.try_get("driver")?,
        status: row.try_get("status")?,
        upstream_job_id: row.try_get("upstream_job_id")?,
        estimated_units: row.try_get("estimated_units")?,
        billed_units: row.try_get("billed_units")?,
        cost: micros_to_decimal_string(row.try_get("cost_micros")?),
        error_code: row.try_get("error_code")?,
        result: result_json
            .map(|value| serde_json::from_str(&value).map_err(|_| AppError::Internal))
            .transpose()?,
    })
}

fn generation_update_claimed(result: AnyQueryResult) -> Result<(), AppError> {
    if result.rows_affected() == 1 {
        Ok(())
    } else {
        Err(AppError::NotFound)
    }
}

pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis() as i64
}

struct Migration {
    version: i64,
    name: &'static str,
    sql: &'static str,
}

const SQLITE_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial schema",
        sql: include_str!("../migrations/sqlite/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "high volume query indexes",
        sql: include_str!("../migrations/sqlite/0002_query_indexes.sql"),
    },
    Migration {
        version: 3,
        name: "scoped service credentials",
        sql: include_str!("../migrations/common/0003_service_tokens.sql"),
    },
    Migration {
        version: 4,
        name: "request event stream",
        sql: include_str!("../migrations/sqlite/0004_request_events.sql"),
    },
    Migration {
        version: 5,
        name: "asynchronous generation jobs",
        sql: include_str!("../migrations/sqlite/0005_generation_jobs.sql"),
    },
    Migration {
        version: 6,
        name: "idempotent key provisioning",
        sql: include_str!("../migrations/sqlite/0006_key_provisioning.sql"),
    },
    Migration {
        version: 7,
        name: "idempotent grant reversals",
        sql: include_str!("../migrations/sqlite/0007_grant_reversals.sql"),
    },
    Migration {
        version: 8,
        name: "bounded plugin KV",
        sql: include_str!("../migrations/sqlite/0008_plugin_kv.sql"),
    },
];

const POSTGRES_MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        name: "initial partitioned schema",
        sql: include_str!("../migrations/postgres/0001_initial.sql"),
    },
    Migration {
        version: 2,
        name: "high volume query indexes",
        sql: include_str!("../migrations/postgres/0002_query_indexes.sql"),
    },
    Migration {
        version: 3,
        name: "scoped service credentials",
        sql: include_str!("../migrations/common/0003_service_tokens.sql"),
    },
    Migration {
        version: 4,
        name: "partitioned request event stream",
        sql: include_str!("../migrations/postgres/0004_request_events.sql"),
    },
    Migration {
        version: 5,
        name: "asynchronous generation jobs",
        sql: include_str!("../migrations/postgres/0005_generation_jobs.sql"),
    },
    Migration {
        version: 6,
        name: "idempotent key provisioning",
        sql: include_str!("../migrations/postgres/0006_key_provisioning.sql"),
    },
    Migration {
        version: 7,
        name: "idempotent grant reversals",
        sql: include_str!("../migrations/postgres/0007_grant_reversals.sql"),
    },
    Migration {
        version: 8,
        name: "bounded plugin KV",
        sql: include_str!("../migrations/postgres/0008_plugin_kv.sql"),
    },
];

async fn maintain_postgres_partitions(connection: &mut AnyConnection) -> Result<(), sqlx::Error> {
    let today = Utc::now().date_naive();
    for offset in 0..=8_u64 {
        let day = today
            .checked_add_days(Days::new(offset))
            .expect("partition date is representable");
        let next_day = day
            .checked_add_days(Days::new(1))
            .expect("partition end date is representable");
        let start = day
            .and_hms_opt(0, 0, 0)
            .expect("midnight is representable")
            .and_utc()
            .timestamp_millis();
        let end = next_day
            .and_hms_opt(0, 0, 0)
            .expect("midnight is representable")
            .and_utc()
            .timestamp_millis();
        let suffix = day.format("%Y%m%d");
        for table in ["request_records", "request_events"] {
            let partition = format!("{table}_{suffix}");
            let statement = format!(
                "CREATE TABLE IF NOT EXISTS {partition} PARTITION OF {table} FOR VALUES FROM ({start}) TO ({end})"
            );
            sqlx::query(&statement).execute(&mut *connection).await?;
        }
    }
    for table in ["request_records", "request_events"] {
        let statement =
            format!("CREATE TABLE IF NOT EXISTS {table}_default PARTITION OF {table} DEFAULT");
        sqlx::query(&statement).execute(&mut *connection).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn sqlite_upgrade_adds_request_routing_columns() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("upgrade.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        sqlx::query(
            "CREATE TABLE request_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, completed_at BIGINT, protocol TEXT NOT NULL, model TEXT NOT NULL, status_code BIGINT, duration_ms BIGINT, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, error_code TEXT, request_object TEXT NOT NULL, response_object TEXT, reservation_id TEXT NOT NULL, conversation_cluster_id TEXT)",
        )
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "CREATE TABLE upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, name TEXT NOT NULL, driver TEXT NOT NULL, auth_kind TEXT NOT NULL, config_json TEXT NOT NULL, status TEXT NOT NULL, credential_generation BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, name))",
        )
        .execute(&database.pool)
        .await
        .unwrap();

        database.migrate().await.unwrap();

        for column in ["upstream_account_id", "model_route_id"] {
            let present = sqlx::query(
                "SELECT name FROM pragma_table_info('request_records') WHERE name = $1",
            )
            .bind(column)
            .fetch_optional(&database.pool)
            .await
            .unwrap()
            .is_some();
            assert!(present, "missing upgraded column {column}");
        }
        let oauth_session_present = sqlx::query(
            "SELECT name FROM pragma_table_info('upstream_accounts') WHERE name = 'oauth_session_id'",
        )
        .fetch_optional(&database.pool)
        .await
        .unwrap()
        .is_some();
        assert!(oauth_session_present);
    }

    #[tokio::test]
    async fn service_token_rotation_preserves_identity_and_revokes_old_generation() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("service-token.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a service credential pepper longer than thirty-two bytes";
        let first = database
            .create_service_token(
                CreateServiceTokenInput {
                    name: "memeloop-web".to_owned(),
                    scopes: vec!["keys:write".to_owned(), "credits:write".to_owned()],
                    tenant_external_id: Some("memeloop".to_owned()),
                },
                pepper,
            )
            .await
            .unwrap();
        let authenticated = database
            .authenticate_service_token(&first.token, pepper)
            .await
            .unwrap();
        assert_eq!(authenticated.service_id, Some(first.service_id));
        assert!(authenticated.allows("keys:write"));
        assert!(!authenticated.allows("prices:write"));

        let rotated = database
            .rotate_service_token(first.service_id, pepper)
            .await
            .unwrap();
        assert_eq!(rotated.service_id, first.service_id);
        assert_eq!(rotated.credential_generation, 2);
        assert!(matches!(
            database
                .authenticate_service_token(&first.token, pepper)
                .await,
            Err(AppError::Unauthorized)
        ));
        assert!(
            database
                .authenticate_service_token(&rotated.token, pepper)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn key_provisioning_replays_one_encrypted_response_for_an_idempotency_key() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("key-idempotency.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let request = |alias: &str| CreateKeyInput {
            tenant_external_id: "tenant".to_owned(),
            principal_external_id: "member".to_owned(),
            alias: alias.to_owned(),
            currency: "USD".to_owned(),
            policy: KeyPolicy::default(),
            initial_balance: Decimal::ONE,
            idempotency_key: Some("registration-event-1".to_owned()),
        };

        let first = database
            .create_key(request("primary"), pepper)
            .await
            .unwrap();
        let replay = database
            .create_key(request("primary"), pepper)
            .await
            .unwrap();
        assert_eq!(replay.key_id, first.key_id);
        assert_eq!(replay.account_id, first.account_id);
        assert_eq!(replay.key, first.key);
        assert!(matches!(
            database.create_key(request("different"), pepper).await,
            Err(AppError::BadRequest(_))
        ));

        let count: i64 = sqlx::query("SELECT COUNT(*) AS count FROM key_records")
            .fetch_one(&database.pool)
            .await
            .unwrap()
            .try_get("count")
            .unwrap();
        assert_eq!(count, 1);
        let ciphertext: String =
            sqlx::query("SELECT issued_key_ciphertext FROM key_records WHERE id = $1")
                .bind(first.key_id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap()
                .try_get("issued_key_ciphertext")
                .unwrap();
        assert!(!ciphertext.contains(&first.key));
    }

    #[tokio::test]
    async fn grant_reversal_is_idempotent_and_only_revokes_unspent_credit() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("grant-reversal.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "refund-test".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ZERO,
                    idempotency_key: None,
                },
                b"a downstream key pepper longer than thirty-two bytes",
            )
            .await
            .unwrap();

        assert_eq!(
            database
                .grant(
                    issued.account_id,
                    Decimal::new(10, 0),
                    "subscription:pro",
                    "subscription:one:grant",
                )
                .await
                .unwrap(),
            "10"
        );
        assert_eq!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:one:grant",
                    "subscription_cancelled",
                    "subscription:one:reversal",
                )
                .await
                .unwrap(),
            "10"
        );
        assert_eq!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:one:grant",
                    "subscription_cancelled",
                    "subscription:one:reversal",
                )
                .await
                .unwrap(),
            "10"
        );
        assert!(matches!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:one:grant",
                    "duplicate",
                    "subscription:one:other-reversal",
                )
                .await,
            Err(AppError::BadRequest(_))
        ));

        database
            .grant(
                issued.account_id,
                Decimal::new(5, 0),
                "subscription:basic",
                "subscription:two:grant",
            )
            .await
            .unwrap();
        sqlx::query("UPDATE credit_accounts SET available_micros = 4000000 WHERE id = $1")
            .bind(issued.account_id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        assert!(matches!(
            database
                .reverse_grant(
                    issued.account_id,
                    "subscription:two:grant",
                    "subscription_cancelled",
                    "subscription:two:reversal",
                )
                .await,
            Err(AppError::QuotaExceeded)
        ));
        let reversals: i64 = sqlx::query(
            "SELECT COUNT(*) AS count FROM ledger_entries WHERE kind = 'grant_reversal'",
        )
        .fetch_one(&database.pool)
        .await
        .unwrap()
        .try_get("count")
        .unwrap();
        assert_eq!(reversals, 1);
    }

    #[tokio::test]
    async fn plugin_kv_is_namespaced_and_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("plugin-kv.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();

        database
            .plugin_kv_put("routing-plugin", "oauth/state", b"encrypted-state")
            .await
            .unwrap();
        assert_eq!(
            database
                .plugin_kv_get("routing-plugin", "oauth/state")
                .await
                .unwrap(),
            Some(b"encrypted-state".to_vec())
        );
        assert_eq!(
            database
                .plugin_kv_get("other-plugin", "oauth/state")
                .await
                .unwrap(),
            None
        );
        database
            .plugin_kv_put("routing-plugin", "oauth/state", b"next-state")
            .await
            .unwrap();
        assert_eq!(
            database
                .plugin_kv_get("routing-plugin", "oauth/state")
                .await
                .unwrap(),
            Some(b"next-state".to_vec())
        );
        assert!(
            database
                .plugin_kv_put("routing-plugin", "unsafe key", b"value")
                .await
                .is_err()
        );
        assert!(
            database
                .plugin_kv_put("routing-plugin", "too-large", &vec![0_u8; 1024 * 1024 + 1])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn maintenance_releases_old_unlinked_reservations() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("orphan-reservation.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a downstream key pepper longer than thirty-two bytes";
        let issued = database
            .create_key(
                CreateKeyInput {
                    tenant_external_id: "tenant".to_owned(),
                    principal_external_id: "member".to_owned(),
                    alias: "orphan-test".to_owned(),
                    currency: "USD".to_owned(),
                    policy: KeyPolicy::default(),
                    initial_balance: Decimal::ONE,
                    idempotency_key: None,
                },
                pepper,
            )
            .await
            .unwrap();
        let key = database
            .authenticate_key(&issued.key, pepper)
            .await
            .unwrap();
        let price = database
            .upsert_model_price("orphan-model", "USD", Decimal::ZERO, Decimal::ONE)
            .await
            .unwrap();
        let reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();
        assert_eq!(reservation.reserved_micros, 1_000);
        let reserved_account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(reserved_account.get::<i64, _>("available_micros"), 999_000);
        assert_eq!(reserved_account.get::<i64, _>("reserved_micros"), 1_000);
        sqlx::query("UPDATE usage_reservations SET created_at = $1 WHERE id = $2")
            .bind(unix_millis().saturating_sub(31 * 60 * 1_000))
            .bind(reservation.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();

        assert_eq!(
            database.release_orphaned_reservations(100).await.unwrap(),
            1
        );
        let reservation_row =
            sqlx::query("SELECT status, actual_micros FROM usage_reservations WHERE id = $1")
                .bind(reservation.id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        assert_eq!(reservation_row.get::<String, _>("status"), "settled");
        assert_eq!(reservation_row.get::<i64, _>("actual_micros"), 0);
        let account_row = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(account_row.get::<i64, _>("available_micros"), 1_000_000);
        assert_eq!(account_row.get::<i64, _>("reserved_micros"), 0);

        let overage_reservation = database
            .reserve_usage(&key, &price, 0, 1_000)
            .await
            .unwrap();
        assert_eq!(
            database
                .settle_usage(&overage_reservation, 0, 2_000)
                .await
                .unwrap(),
            2_000
        );
        let overage_account = sqlx::query(
            "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
        )
        .bind(issued.account_id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(overage_account.get::<i64, _>("available_micros"), 998_000);
        assert_eq!(overage_account.get::<i64, _>("reserved_micros"), 0);
    }
}
