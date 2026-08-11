use std::time::{SystemTime, UNIX_EPOCH};

use rust_decimal::{Decimal, prelude::ToPrimitive};
use sqlx::{AnyPool, Row, any::AnyPoolOptions};
use uuid::Uuid;

use crate::{
    conversation::{RelationKind, build_prefix, extract_atoms},
    crypto,
    error::AppError,
    model::{
        AuthenticatedKey, ConversationClusterDetail, ConversationClusterView, ConversationEdgeView,
        IssuedKey, KeyPolicy, KeyView, ModelPrice, RequestArchiveRefs, RequestView, SelfStats,
        StatsBucket, StatsSummary, UsageReservation, micros_to_decimal_string, priced_tokens,
    },
    provider::{
        ModelRouteView, ResolvedUpstream, UpstreamAccountView, UpstreamCredential, open_credential,
        seal_credential, validate_config,
    },
};

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

impl Database {
    pub async fn connect(database_url: &str) -> Result<Self, sqlx::Error> {
        sqlx::any::install_default_drivers();
        let backend = if database_url.starts_with("sqlite:") {
            DatabaseBackend::Sqlite
        } else {
            DatabaseBackend::PostgreSql
        };
        let pool = AnyPoolOptions::new()
            .max_connections(20)
            .connect(database_url)
            .await?;
        Ok(Self { pool, backend })
    }

    pub async fn migrate(&self) -> Result<(), sqlx::Error> {
        let mut connection = self.pool.acquire().await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_lock(734627102948311)")
                .execute(&mut *connection)
                .await?;
        }
        for statement in SCHEMA {
            if matches!(self.backend, DatabaseBackend::PostgreSql)
                && statement.starts_with("CREATE TABLE IF NOT EXISTS request_records ")
            {
                sqlx::query(POSTGRES_REQUEST_RECORDS)
                    .execute(&mut *connection)
                    .await?;
                sqlx::raw_sql(POSTGRES_REQUEST_PARTITIONS)
                    .execute(&mut *connection)
                    .await?;
            } else {
                sqlx::query(statement).execute(&mut *connection).await?;
            }
        }
        for column in ["upstream_account_id", "model_route_id"] {
            let exists = match self.backend {
                DatabaseBackend::PostgreSql => sqlx::query(
                    "SELECT column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'request_records' AND column_name = ?",
                )
                .bind(column)
                .fetch_optional(&mut *connection)
                .await?
                .is_some(),
                DatabaseBackend::Sqlite => sqlx::query(
                    "SELECT name FROM pragma_table_info('request_records') WHERE name = ?",
                )
                .bind(column)
                .fetch_optional(&mut *connection)
                .await?
                .is_some(),
            };
            if !exists {
                sqlx::query(&format!(
                    "ALTER TABLE request_records ADD COLUMN {column} TEXT"
                ))
                .execute(&mut *connection)
                .await?;
            }
        }
        let oauth_session_column_exists = match self.backend {
            DatabaseBackend::PostgreSql => sqlx::query(
                "SELECT column_name FROM information_schema.columns WHERE table_schema = current_schema() AND table_name = 'upstream_accounts' AND column_name = 'oauth_session_id'",
            )
            .fetch_optional(&mut *connection)
            .await?
            .is_some(),
            DatabaseBackend::Sqlite => sqlx::query(
                "SELECT name FROM pragma_table_info('upstream_accounts') WHERE name = 'oauth_session_id'",
            )
            .fetch_optional(&mut *connection)
            .await?
            .is_some(),
        };
        if !oauth_session_column_exists {
            sqlx::query("ALTER TABLE upstream_accounts ADD COLUMN oauth_session_id TEXT")
                .execute(&mut *connection)
                .await?;
        }
        sqlx::query(
            "CREATE UNIQUE INDEX IF NOT EXISTS upstream_accounts_oauth_session_idx ON upstream_accounts (oauth_session_id) WHERE oauth_session_id IS NOT NULL",
        )
        .execute(&mut *connection)
        .await?;
        if matches!(self.backend, DatabaseBackend::PostgreSql) {
            sqlx::query("SELECT pg_advisory_unlock(734627102948311)")
                .execute(&mut *connection)
                .await?;
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
        let now = unix_millis();
        let tenant_id = Uuid::now_v7();
        let principal_id = Uuid::now_v7();
        let account_id = Uuid::now_v7();
        let key_id = Uuid::now_v7();
        let issued = crypto::issue_credential(key_id, pepper);
        let policy_json = serde_json::to_string(&input.policy).map_err(|_| AppError::Internal)?;
        let initial_balance_micros = decimal_to_micros(input.initial_balance)?;
        let mut tx = self.pool.begin().await?;

        sqlx::query(
            "INSERT INTO tenants (id, external_id, created_at) VALUES (?, ?, ?) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_id.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = ?")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;

        sqlx::query(
            "INSERT INTO principals (id, tenant_id, external_id, created_at) VALUES (?, ?, ?, ?) ON CONFLICT(tenant_id, external_id) DO NOTHING",
        )
        .bind(principal_id.to_string())
        .bind(&tenant_id)
        .bind(&input.principal_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let principal_id: String =
            sqlx::query("SELECT id FROM principals WHERE tenant_id = ? AND external_id = ?")
                .bind(&tenant_id)
                .bind(&input.principal_external_id)
                .fetch_one(&mut *tx)
                .await?
                .try_get("id")?;

        sqlx::query(
            "INSERT INTO credit_accounts (id, tenant_id, principal_id, currency, available_micros, reserved_micros, created_at, updated_at) VALUES (?, ?, ?, ?, ?, 0, ?, ?)",
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

        sqlx::query(
            "INSERT INTO key_records (id, tenant_id, principal_id, account_id, alias, currency, policy_json, status, credential_generation, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'active', 1, ?, ?)",
        )
        .bind(key_id.to_string())
        .bind(&tenant_id)
        .bind(&principal_id)
        .bind(account_id.to_string())
        .bind(&input.alias)
        .bind(input.currency.to_uppercase())
        .bind(policy_json)
        .bind(now)
        .bind(now)
        .execute(&mut *tx)
        .await?;

        insert_credential(&mut tx, &issued, 1, now).await?;
        if initial_balance_micros != 0 {
            sqlx::query(
                "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) VALUES (?, ?, ?, 'grant', ?, ?, 'initial', ?)",
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
        Ok(IssuedKey {
            key_id,
            account_id,
            alias: input.alias,
            currency: input.currency.to_uppercase(),
            credential_generation: 1,
            key: issued.secret,
            fingerprint: issued.fingerprint,
        })
    }

    pub async fn rotate_key(&self, key_id: Uuid, pepper: &[u8]) -> Result<IssuedKey, AppError> {
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query(
            "SELECT account_id, alias, currency, credential_generation, status FROM key_records WHERE id = ?",
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
            "UPDATE key_credentials SET revoked_at = ? WHERE key_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(key_id.to_string())
        .execute(&mut *tx)
        .await?;
        insert_credential(&mut tx, &issued, generation, now).await?;
        sqlx::query(
            "UPDATE key_records SET credential_generation = ?, updated_at = ? WHERE id = ?",
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
            "INSERT INTO tenants (id, external_id, created_at) VALUES (?, ?, ?) ON CONFLICT(external_id) DO NOTHING",
        )
        .bind(tenant_candidate.to_string())
        .bind(&input.tenant_external_id)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = ?")
            .bind(&input.tenant_external_id)
            .fetch_one(&mut *tx)
            .await?
            .try_get("id")?;
        if let Some(session_id) = input.oauth_session_id {
            let existing = sqlx::query(
                "SELECT a.id, a.tenant_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation WHERE a.oauth_session_id = ?",
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
            "INSERT INTO upstream_accounts (id, tenant_id, name, driver, auth_kind, config_json, status, credential_generation, oauth_session_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, 'active', 1, ?, ?, ?)",
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
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES (?, ?, 1, ?, ?, ?)",
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
            "SELECT tenant_id, name, driver, auth_kind, config_json, status, credential_generation, created_at FROM upstream_accounts WHERE id = ?",
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
            "UPDATE upstream_credentials SET revoked_at = ? WHERE upstream_account_id = ? AND revoked_at IS NULL",
        )
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO upstream_credentials (id, upstream_account_id, generation, credential_ciphertext, expires_at, created_at) VALUES (?, ?, ?, ?, ?, ?)",
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
            "UPDATE upstream_accounts SET credential_generation = ?, updated_at = ? WHERE id = ?",
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
            "SELECT a.id, a.tenant_id, a.name, a.driver, a.auth_kind, a.config_json, a.status, a.credential_generation, a.created_at, a.updated_at, c.expires_at, c.credential_ciphertext FROM upstream_accounts a JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE a.id = ?",
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
        if !matches!(input.protocol.as_str(), "openai" | "anthropic") {
            return Err(AppError::BadRequest(
                "route protocol must be openai or anthropic".into(),
            ));
        }
        let now = unix_millis();
        let route_id = Uuid::now_v7();
        let mut tx = self.pool.begin().await?;
        let tenant_id: String = sqlx::query("SELECT id FROM tenants WHERE external_id = ?")
            .bind(&input.tenant_external_id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?
            .try_get("id")?;
        let account_tenant: String = sqlx::query(
            "SELECT tenant_id FROM upstream_accounts WHERE id = ? AND status = 'active'",
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
            "INSERT INTO model_routes (id, tenant_id, public_model, upstream_account_id, upstream_model, protocol, priority, enabled, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, 1, ?, ?)",
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

    pub async fn resolve_upstream(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let row = sqlx::query(
            "SELECT r.id AS route_id, r.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext FROM model_routes r JOIN upstream_accounts a ON a.id = r.upstream_account_id JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL WHERE r.tenant_id = ? AND r.public_model = ? AND r.protocol = ? AND r.enabled = 1 AND a.status = 'active' ORDER BY r.priority ASC, r.id ASC LIMIT 1",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(protocol)
        .fetch_optional(&self.pool)
        .await?;
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
            "SELECT k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.policy_json, k.status, c.generation, c.secret_hash FROM key_records k JOIN key_credentials c ON c.key_id = k.id WHERE k.id = ? AND c.revoked_at IS NULL ORDER BY c.generation DESC",
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
            "SELECT k.created_at, a.available_micros FROM key_records k JOIN credit_accounts a ON a.id = k.account_id WHERE k.id = ?",
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
            "INSERT INTO model_prices (id, model, currency, input_micros_per_million, output_micros_per_million, source, updated_at) VALUES (?, ?, ?, ?, ?, 'manual', ?) ON CONFLICT(model, currency) DO UPDATE SET input_micros_per_million = excluded.input_micros_per_million, output_micros_per_million = excluded.output_micros_per_million, updated_at = excluded.updated_at",
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
            "SELECT id, input_micros_per_million, output_micros_per_million FROM model_prices WHERE model = ? AND currency = ?",
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

    pub async fn allowed_models(&self, key: &AuthenticatedKey) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query("SELECT model FROM model_prices WHERE currency = ? ORDER BY model")
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
            "INSERT INTO rate_limit_windows (key_id, window_start, requests, tokens) VALUES (?, ?, 1, ?) ON CONFLICT(key_id, window_start) DO UPDATE SET requests = rate_limit_windows.requests + 1, tokens = rate_limit_windows.tokens + ? WHERE rate_limit_windows.requests < ? AND rate_limit_windows.tokens + ? <= ?",
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
            "INSERT INTO key_runtime_state (key_id, active_requests, updated_at) VALUES (?, 1, ?) ON CONFLICT(key_id) DO UPDATE SET active_requests = CASE WHEN key_runtime_state.updated_at < ? THEN 1 ELSE key_runtime_state.active_requests + 1 END, updated_at = excluded.updated_at WHERE key_runtime_state.updated_at < ? OR key_runtime_state.active_requests < ?",
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
            "SELECT COALESCE(SUM(reserved_micros), 0) AS amount FROM usage_reservations WHERE key_id = ? AND status = 'reserved'",
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
                "SELECT COALESCE(SUM(-amount_micros), 0) AS amount FROM ledger_entries WHERE key_id = ? AND kind = 'usage' AND created_at >= ?",
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
            "UPDATE credit_accounts SET available_micros = available_micros - ?, reserved_micros = reserved_micros + ?, updated_at = ? WHERE id = ? AND currency = ? AND available_micros >= ?",
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
            "INSERT INTO usage_reservations (id, account_id, key_id, price_id, reserved_micros, reserved_tokens, rate_window_start, status, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, 'reserved', ?)",
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
        let released = reservation.reserved_micros.saturating_sub(actual_micros);
        let overage = actual_micros.saturating_sub(reservation.reserved_micros);
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let claimed = sqlx::query(
            "UPDATE usage_reservations SET actual_micros = ?, status = 'settled', settled_at = ? WHERE id = ? AND status = 'reserved'",
        )
        .bind(actual_micros)
        .bind(now)
        .bind(reservation.id.to_string())
        .execute(&mut *tx)
        .await?;
        if claimed.rows_affected() == 0 {
            let existing: i64 = sqlx::query(
                "SELECT actual_micros FROM usage_reservations WHERE id = ? AND status = 'settled'",
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
            "UPDATE credit_accounts SET available_micros = available_micros + ? - ?, reserved_micros = reserved_micros - ?, updated_at = ? WHERE id = ?",
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
            "UPDATE rate_limit_windows SET tokens = CASE WHEN tokens - ? + ? < 0 THEN 0 ELSE tokens - ? + ? END WHERE key_id = ? AND window_start = ?",
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
            "UPDATE key_runtime_state SET active_requests = CASE WHEN active_requests > 0 THEN active_requests - 1 ELSE 0 END, updated_at = ? WHERE key_id = ?",
        )
        .bind(now)
        .bind(reservation.key_id.to_string())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, key_id, kind, amount_micros, currency, source, created_at) SELECT ?, ?, ?, 'usage', ?, currency, ?, ? FROM credit_accounts WHERE id = ?",
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

    pub async fn grant(
        &self,
        account_id: Uuid,
        amount: Decimal,
        source: &str,
        idempotency_key: &str,
    ) -> Result<String, AppError> {
        let amount_micros = decimal_to_micros(amount)?;
        if amount_micros <= 0 {
            return Err(AppError::BadRequest("grant amount must be positive".into()));
        }
        let now = unix_millis();
        let mut tx = self.pool.begin().await?;
        let row = sqlx::query("SELECT currency FROM credit_accounts WHERE id = ?")
            .bind(account_id.to_string())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(AppError::NotFound)?;
        let currency: String = row.try_get("currency")?;
        let inserted = sqlx::query(
            "INSERT INTO ledger_entries (id, account_id, kind, amount_micros, currency, source, idempotency_key, created_at) VALUES (?, ?, 'grant', ?, ?, ?, ?, ?) ON CONFLICT(idempotency_key) DO NOTHING",
        )
        .bind(Uuid::now_v7().to_string())
        .bind(account_id.to_string())
        .bind(amount_micros)
        .bind(currency)
        .bind(source)
        .bind(idempotency_key)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        if inserted.rows_affected() == 0 {
            let existing: i64 = sqlx::query(
                "SELECT amount_micros FROM ledger_entries WHERE idempotency_key = ? AND account_id = ? AND kind = 'grant'",
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
            "UPDATE credit_accounts SET available_micros = available_micros + ?, updated_at = ? WHERE id = ?",
        )
        .bind(amount_micros)
        .bind(now)
        .bind(account_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(micros_to_decimal_string(amount_micros))
    }

    pub async fn record_request_started(&self, request: NewRequest) -> Result<(), AppError> {
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, request_object, reservation_id, upstream_account_id, model_route_id, input_tokens, output_tokens, cost_micros) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 0, 0, 0)",
        )
        .bind(request.request_id.to_string())
        .bind(request.tenant_id.to_string())
        .bind(request.key_id.to_string())
        .bind(unix_millis())
        .bind(request.protocol)
        .bind(request.model)
        .bind(request.request_object)
        .bind(request.reservation_id.to_string())
        .bind(request.upstream_account_id.map(|id| id.to_string()))
        .bind(request.model_route_id.map(|id| id.to_string()))
        .execute(&self.pool)
        .await?;
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
                "INSERT INTO semantic_atoms (tenant_id, content_hash, instance_hash, role, kind, content_json, created_at) VALUES (?, ?, ?, ?, ?, ?, ?) ON CONFLICT(tenant_id, content_hash) DO NOTHING",
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
                "INSERT INTO context_nodes (tenant_id, node_hash, parent_hash, atom_hash, depth, created_at) VALUES (?, ?, ?, ?, ?, ?) ON CONFLICT(tenant_id, node_hash) DO NOTHING",
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
            "SELECT o.id, o.cluster_id, o.atom_hashes_json, o.explicit_session_id, o.created_at FROM conversation_observations o JOIN conversation_clusters c ON c.id = o.cluster_id WHERE c.tenant_id = ? AND c.principal_id = ? ORDER BY o.created_at DESC LIMIT 50",
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
                "INSERT INTO conversation_clusters (id, tenant_id, principal_id, explicit_session_id, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?)",
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
            "INSERT INTO conversation_observations (id, cluster_id, request_id, key_id, leaf_node_hash, atom_hashes_json, explicit_session_id, client_name, created_at, inference_version) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 1)",
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
                "INSERT INTO conversation_edges (id, cluster_id, from_observation_id, to_observation_id, relation_kind, confidence_millis, evidence_json, pinned, inference_version, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, 0, 1, ?)",
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
        sqlx::query("UPDATE conversation_clusters SET updated_at = ? WHERE id = ?")
            .bind(now)
            .bind(cluster_id.to_string())
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE request_records SET conversation_cluster_id = ? WHERE id = ?")
            .bind(cluster_id.to_string())
            .bind(request_id.to_string())
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(cluster_id)
    }

    pub async fn record_request_finished(&self, request: FinishRequest) -> Result<(), AppError> {
        let mut tx = self.pool.begin().await?;
        let updated = sqlx::query(
            "UPDATE request_records SET status_code = ?, duration_ms = ?, input_tokens = ?, output_tokens = ?, cost_micros = ?, error_code = ?, response_object = ?, completed_at = ? WHERE id = ? AND completed_at IS NULL",
        )
        .bind(request.status_code)
        .bind(request.duration_ms)
        .bind(request.input_tokens)
        .bind(request.output_tokens)
        .bind(request.cost_micros)
        .bind(request.error_code)
        .bind(request.response_object)
        .bind(unix_millis())
        .bind(request.request_id.to_string())
        .execute(&mut *tx)
        .await?;
        if updated.rows_affected() == 0 {
            tx.commit().await?;
            return Ok(());
        }
        sqlx::query(
            "INSERT INTO usage_daily_aggregates (key_id, day_bucket, model, status_class, error_code, requests, input_tokens, output_tokens, cost_micros) SELECT key_id, created_at / 86400000, model, CASE WHEN status_code >= 200 AND status_code < 400 THEN 'success' ELSE 'failure' END, COALESCE(error_code, ''), 1, input_tokens, output_tokens, cost_micros FROM request_records WHERE id = ? ON CONFLICT(key_id, day_bucket, model, status_class, error_code) DO UPDATE SET requests = usage_daily_aggregates.requests + 1, input_tokens = usage_daily_aggregates.input_tokens + excluded.input_tokens, output_tokens = usage_daily_aggregates.output_tokens + excluded.output_tokens, cost_micros = usage_daily_aggregates.cost_micros + excluded.cost_micros",
        )
        .bind(request.request_id.to_string())
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn list_requests(
        &self,
        key_id: Uuid,
        limit: i64,
    ) -> Result<Vec<RequestView>, AppError> {
        let rows = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = ? ORDER BY created_at DESC, id DESC LIMIT ?",
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

    pub async fn request_archive_refs(
        &self,
        key_id: Uuid,
        request_id: Uuid,
    ) -> Result<RequestArchiveRefs, AppError> {
        let row = sqlx::query(
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code, request_object, response_object FROM request_records WHERE id = ? AND key_id = ?",
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
            "SELECT c.id, c.explicit_session_id, c.updated_at, (SELECT COUNT(*) FROM conversation_observations count_o WHERE count_o.cluster_id = c.id AND count_o.key_id = ?) AS request_count, (SELECT COUNT(*) FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id WHERE e.cluster_id = c.id AND target_o.key_id = ? AND e.relation_kind = 'candidate') AS candidate_edge_count FROM conversation_clusters c WHERE EXISTS (SELECT 1 FROM conversation_observations own_o WHERE own_o.cluster_id = c.id AND own_o.key_id = ?) ORDER BY c.updated_at DESC",
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
            "SELECT id, created_at, protocol, model, status_code, duration_ms, input_tokens, output_tokens, cost_micros, error_code FROM request_records WHERE key_id = ? AND conversation_cluster_id = ? ORDER BY created_at ASC, id ASC",
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
            "SELECT source_o.request_id AS from_request_id, target_o.request_id AS to_request_id, e.relation_kind, e.confidence_millis, e.evidence_json FROM conversation_edges e JOIN conversation_observations target_o ON target_o.id = e.to_observation_id LEFT JOIN conversation_observations source_o ON source_o.id = e.from_observation_id WHERE e.cluster_id = ? AND target_o.key_id = ? AND (source_o.key_id = ? OR source_o.id IS NULL) ORDER BY target_o.created_at ASC",
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
            "SELECT COALESCE(SUM(requests), 0) AS total_requests, COALESCE(SUM(CASE WHEN status_class = 'success' THEN requests ELSE 0 END), 0) AS successful_requests, COALESCE(SUM(CASE WHEN status_class = 'failure' THEN requests ELSE 0 END), 0) AS failed_requests, COALESCE(SUM(input_tokens), 0) AS input_tokens, COALESCE(SUM(output_tokens), 0) AS output_tokens, COALESCE(SUM(cost_micros), 0) AS cost_micros FROM usage_daily_aggregates WHERE key_id = ?",
        )
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
            "SELECT model AS name, SUM(requests) AS requests, SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, SUM(cost_micros) AS cost_micros FROM usage_daily_aggregates WHERE key_id = ? GROUP BY model ORDER BY requests DESC, model ASC",
        )
        .bind(&key_id)
        .fetch_all(&self.pool)
        .await?;
        let by_model = aggregate_buckets(model_rows)?;

        let day_rows = sqlx::query(
            "SELECT day_bucket, SUM(requests) AS requests, SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, SUM(cost_micros) AS cost_micros FROM usage_daily_aggregates WHERE key_id = ? GROUP BY day_bucket ORDER BY day_bucket ASC",
        )
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
            "SELECT error_code AS name, SUM(requests) AS requests, SUM(input_tokens) AS input_tokens, SUM(output_tokens) AS output_tokens, SUM(cost_micros) AS cost_micros FROM usage_daily_aggregates WHERE key_id = ? AND error_code <> '' GROUP BY error_code ORDER BY requests DESC, error_code ASC",
        )
        .bind(key_id.to_string())
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

async fn insert_credential(
    tx: &mut sqlx::Transaction<'_, sqlx::Any>,
    issued: &crypto::IssuedCredential,
    generation: i64,
    now: i64,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO key_credentials (id, key_id, generation, secret_hash, fingerprint, created_at) VALUES (?, ?, ?, ?, ?, ?)",
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

pub fn unix_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock after Unix epoch")
        .as_millis() as i64
}

const SCHEMA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS tenants (id TEXT PRIMARY KEY, external_id TEXT NOT NULL UNIQUE, created_at BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS principals (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, external_id TEXT NOT NULL, created_at BIGINT NOT NULL, UNIQUE(tenant_id, external_id))",
    "CREATE TABLE IF NOT EXISTS credit_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, principal_id TEXT NOT NULL, currency TEXT NOT NULL, available_micros BIGINT NOT NULL, reserved_micros BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS key_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, principal_id TEXT NOT NULL, account_id TEXT NOT NULL, alias TEXT NOT NULL, currency TEXT NOT NULL, policy_json TEXT NOT NULL, status TEXT NOT NULL, credential_generation BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS key_credentials (id TEXT PRIMARY KEY, key_id TEXT NOT NULL, generation BIGINT NOT NULL, secret_hash BYTEA NOT NULL, fingerprint TEXT NOT NULL, created_at BIGINT NOT NULL, revoked_at BIGINT, UNIQUE(key_id, generation))",
    "CREATE TABLE IF NOT EXISTS ledger_entries (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, key_id TEXT, kind TEXT NOT NULL, amount_micros BIGINT NOT NULL, currency TEXT NOT NULL, source TEXT NOT NULL, idempotency_key TEXT UNIQUE, created_at BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS model_prices (id TEXT PRIMARY KEY, model TEXT NOT NULL, currency TEXT NOT NULL, input_micros_per_million BIGINT NOT NULL, output_micros_per_million BIGINT NOT NULL, source TEXT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(model, currency))",
    "CREATE TABLE IF NOT EXISTS upstream_accounts (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, name TEXT NOT NULL, driver TEXT NOT NULL, auth_kind TEXT NOT NULL, config_json TEXT NOT NULL, status TEXT NOT NULL, credential_generation BIGINT NOT NULL, oauth_session_id TEXT, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, name))",
    "CREATE TABLE IF NOT EXISTS upstream_credentials (id TEXT PRIMARY KEY, upstream_account_id TEXT NOT NULL, generation BIGINT NOT NULL, credential_ciphertext TEXT NOT NULL, expires_at BIGINT, created_at BIGINT NOT NULL, revoked_at BIGINT, UNIQUE(upstream_account_id, generation))",
    "CREATE TABLE IF NOT EXISTS model_routes (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, public_model TEXT NOT NULL, upstream_account_id TEXT NOT NULL, upstream_model TEXT NOT NULL, protocol TEXT NOT NULL, priority BIGINT NOT NULL, enabled BIGINT NOT NULL, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL, UNIQUE(tenant_id, public_model, protocol, priority))",
    "CREATE TABLE IF NOT EXISTS usage_reservations (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, key_id TEXT NOT NULL, price_id TEXT NOT NULL, reserved_micros BIGINT NOT NULL, reserved_tokens BIGINT NOT NULL, rate_window_start BIGINT NOT NULL, actual_micros BIGINT, status TEXT NOT NULL, created_at BIGINT NOT NULL, settled_at BIGINT)",
    "CREATE TABLE IF NOT EXISTS rate_limit_windows (key_id TEXT NOT NULL, window_start BIGINT NOT NULL, requests BIGINT NOT NULL, tokens BIGINT NOT NULL, PRIMARY KEY(key_id, window_start))",
    "CREATE TABLE IF NOT EXISTS key_runtime_state (key_id TEXT PRIMARY KEY, active_requests BIGINT NOT NULL, updated_at BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS request_records (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, completed_at BIGINT, protocol TEXT NOT NULL, model TEXT NOT NULL, status_code BIGINT, duration_ms BIGINT, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, error_code TEXT, request_object TEXT NOT NULL, response_object TEXT, reservation_id TEXT NOT NULL, conversation_cluster_id TEXT, upstream_account_id TEXT, model_route_id TEXT)",
    "CREATE TABLE IF NOT EXISTS usage_daily_aggregates (key_id TEXT NOT NULL, day_bucket BIGINT NOT NULL, model TEXT NOT NULL, status_class TEXT NOT NULL, error_code TEXT NOT NULL, requests BIGINT NOT NULL, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, PRIMARY KEY(key_id, day_bucket, model, status_class, error_code))",
    "CREATE TABLE IF NOT EXISTS semantic_atoms (tenant_id TEXT NOT NULL, content_hash TEXT NOT NULL, instance_hash TEXT NOT NULL, role TEXT NOT NULL, kind TEXT NOT NULL, content_json TEXT NOT NULL, created_at BIGINT NOT NULL, PRIMARY KEY(tenant_id, content_hash))",
    "CREATE TABLE IF NOT EXISTS context_nodes (tenant_id TEXT NOT NULL, node_hash TEXT NOT NULL, parent_hash TEXT, atom_hash TEXT NOT NULL, depth BIGINT NOT NULL, created_at BIGINT NOT NULL, PRIMARY KEY(tenant_id, node_hash))",
    "CREATE TABLE IF NOT EXISTS conversation_clusters (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, principal_id TEXT NOT NULL, explicit_session_id TEXT, created_at BIGINT NOT NULL, updated_at BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS conversation_observations (id TEXT PRIMARY KEY, cluster_id TEXT NOT NULL, request_id TEXT NOT NULL UNIQUE, key_id TEXT NOT NULL, leaf_node_hash TEXT, atom_hashes_json TEXT NOT NULL, explicit_session_id TEXT, client_name TEXT, created_at BIGINT NOT NULL, inference_version BIGINT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS conversation_edges (id TEXT PRIMARY KEY, cluster_id TEXT NOT NULL, from_observation_id TEXT, to_observation_id TEXT NOT NULL, relation_kind TEXT NOT NULL, confidence_millis BIGINT NOT NULL, evidence_json TEXT NOT NULL, pinned BIGINT NOT NULL DEFAULT 0, inference_version BIGINT NOT NULL, created_at BIGINT NOT NULL)",
    "CREATE INDEX IF NOT EXISTS request_records_key_time_idx ON request_records (key_id, created_at DESC, id DESC)",
    "CREATE INDEX IF NOT EXISTS request_records_id_idx ON request_records (id)",
    "CREATE INDEX IF NOT EXISTS request_records_tenant_time_idx ON request_records (tenant_id, created_at DESC)",
    "CREATE INDEX IF NOT EXISTS request_records_error_idx ON request_records (tenant_id, error_code, created_at DESC) WHERE error_code IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS ledger_entries_key_time_idx ON ledger_entries (key_id, created_at DESC) WHERE key_id IS NOT NULL",
    "CREATE INDEX IF NOT EXISTS usage_reservations_key_status_idx ON usage_reservations (key_id, status, created_at DESC)",
    "CREATE INDEX IF NOT EXISTS upstream_accounts_tenant_driver_idx ON upstream_accounts (tenant_id, driver, status)",
    "CREATE INDEX IF NOT EXISTS upstream_credentials_active_idx ON upstream_credentials (upstream_account_id, revoked_at, generation DESC)",
    "CREATE INDEX IF NOT EXISTS model_routes_lookup_idx ON model_routes (tenant_id, public_model, protocol, enabled, priority)",
    "CREATE INDEX IF NOT EXISTS conversation_observations_key_time_idx ON conversation_observations (key_id, created_at DESC)",
    "CREATE INDEX IF NOT EXISTS conversation_observations_cluster_time_idx ON conversation_observations (cluster_id, created_at ASC)",
    "CREATE INDEX IF NOT EXISTS conversation_edges_cluster_target_idx ON conversation_edges (cluster_id, to_observation_id)",
];

const POSTGRES_REQUEST_RECORDS: &str = "CREATE TABLE IF NOT EXISTS request_records (id TEXT NOT NULL, tenant_id TEXT NOT NULL, key_id TEXT NOT NULL, created_at BIGINT NOT NULL, completed_at BIGINT, protocol TEXT NOT NULL, model TEXT NOT NULL, status_code BIGINT, duration_ms BIGINT, input_tokens BIGINT NOT NULL, output_tokens BIGINT NOT NULL, cost_micros BIGINT NOT NULL, error_code TEXT, request_object TEXT NOT NULL, response_object TEXT, reservation_id TEXT NOT NULL, conversation_cluster_id TEXT, upstream_account_id TEXT, model_route_id TEXT) PARTITION BY RANGE (created_at)";

const POSTGRES_REQUEST_PARTITIONS: &str = r#"
DO $$
DECLARE
    day_offset integer;
    day_start bigint;
    day_end bigint;
    partition_name text;
BEGIN
    FOR day_offset IN 0..8 LOOP
        day_start := (extract(epoch FROM date_trunc('day', now() + make_interval(days => day_offset))) * 1000)::bigint;
        day_end := (extract(epoch FROM date_trunc('day', now() + make_interval(days => day_offset + 1))) * 1000)::bigint;
        partition_name := 'request_records_' || to_char(now() + make_interval(days => day_offset), 'YYYYMMDD');
        EXECUTE format(
            'CREATE TABLE IF NOT EXISTS %I PARTITION OF request_records FOR VALUES FROM (%s) TO (%s)',
            partition_name,
            day_start,
            day_end
        );
    END LOOP;
END $$;
CREATE TABLE IF NOT EXISTS request_records_default PARTITION OF request_records DEFAULT;
"#;

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
            let present =
                sqlx::query("SELECT name FROM pragma_table_info('request_records') WHERE name = ?")
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
}
