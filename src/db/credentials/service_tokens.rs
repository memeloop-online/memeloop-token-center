use super::super::*;

const SERVICE_TOKEN_ROTATION_RESOURCE: &str = "service_token";

// Managed credentials deliberately use an exact, versioned capability set.
// The bootstrap credential is the only principal that owns `*`; accepting a
// wildcard or an unknown value here would let a stored credential bypass the
// public service-token schema or silently acquire authority from a future
// release that introduces a matching scope.
const SUPPORTED_SERVICE_SCOPES: &[&str] = &[
    "credits:read",
    "credits:write",
    "entitlements:read",
    "entitlements:write",
    "imports:cpa:write",
    "imports:session_archive:quarantine:read",
    "imports:session_archive:quarantine:resolve",
    "keys:read",
    "keys:write",
    "metrics:read",
    "oauth:write",
    "plugins:read",
    "plugins:write",
    "prices:read",
    "prices:write",
    "providers:read",
    "providers:write",
    "requests:read",
    "routes:read",
    "routes:write",
    "schemas:read",
    "service_tokens:read",
    "service_tokens:write",
];

#[derive(Serialize, Deserialize)]
struct StoredIssuedServiceToken {
    service_id: Uuid,
    name: String,
    credential_generation: i64,
    token: String,
    fingerprint: String,
    scopes: Vec<String>,
    tenant_external_id: Option<String>,
}

impl From<&IssuedServiceToken> for StoredIssuedServiceToken {
    fn from(value: &IssuedServiceToken) -> Self {
        Self {
            service_id: value.service_id,
            name: value.name.clone(),
            credential_generation: value.credential_generation,
            token: value.token.clone(),
            fingerprint: value.fingerprint.clone(),
            scopes: value.scopes.clone(),
            tenant_external_id: value.tenant_external_id.clone(),
        }
    }
}

impl From<StoredIssuedServiceToken> for IssuedServiceToken {
    fn from(value: StoredIssuedServiceToken) -> Self {
        Self {
            service_id: value.service_id,
            name: value.name,
            credential_generation: value.credential_generation,
            token: value.token,
            fingerprint: value.fingerprint,
            scopes: value.scopes,
            tenant_external_id: value.tenant_external_id,
        }
    }
}

pub struct CreateServiceTokenInput {
    pub name: String,
    pub scopes: Vec<String>,
    pub tenant_external_id: Option<String>,
}

impl Database {
    pub async fn list_service_tokens(&self) -> Result<Vec<ServiceTokenView>, AppError> {
        self.list_service_tokens_page(None, None, 100).await
    }

    pub async fn list_service_tokens_page(
        &self,
        before_created_at: Option<i64>,
        before_id: Option<Uuid>,
        limit: i64,
    ) -> Result<Vec<ServiceTokenView>, AppError> {
        let before_created_at = before_created_at.unwrap_or(i64::MAX);
        let before_id = before_id
            .map(|value| value.to_string())
            .unwrap_or_else(|| "ffffffff-ffff-ffff-ffff-ffffffffffff".to_owned());
        let rows = sqlx::query(
            "SELECT p.id, p.name, p.status, p.credential_generation, p.created_at, p.updated_at, c.fingerprint, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation WHERE p.created_at < $1 OR (p.created_at = $1 AND p.id < $2) ORDER BY p.created_at DESC, p.id DESC LIMIT $3",
        )
        .bind(before_created_at)
        .bind(before_id)
        .bind(limit.clamp(1, 100))
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter().map(service_token_view).collect()
    }

    pub async fn set_service_token_status(
        &self,
        service_id: Uuid,
        status: &str,
    ) -> Result<String, AppError> {
        if !matches!(status, "active" | "suspended" | "revoked") {
            return Err(AppError::BadRequest(
                "service credential status must be active, suspended, or revoked".into(),
            ));
        }
        let mut transaction = self.begin_write_transaction().await?;
        let current = sqlx::query("SELECT status FROM service_principals WHERE id = $1")
            .bind(service_id.to_string())
            .fetch_optional(&mut *transaction)
            .await?
            .ok_or(AppError::NotFound)?;
        if current.try_get::<String, _>("status")? == "revoked" && status != "revoked" {
            return Err(AppError::BadRequest(
                "a revoked service credential cannot be reactivated".into(),
            ));
        }
        let changed = sqlx::query(
            "UPDATE service_principals SET status = $1, updated_at = $2 WHERE id = $3 AND NOT (status = 'revoked' AND $4 <> 'revoked')",
        )
        .bind(status)
        .bind(unix_millis())
        .bind(service_id.to_string())
        .bind(status)
        .execute(&mut *transaction)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::NotFound);
        }
        if status == "revoked" {
            sqlx::query(
                "UPDATE service_credentials SET revoked_at = $1 WHERE service_principal_id = $2 AND revoked_at IS NULL",
            )
            .bind(unix_millis())
            .bind(service_id.to_string())
            .execute(&mut *transaction)
            .await?;
        }
        transaction.commit().await?;
        Ok(status.to_owned())
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

    pub async fn rotate_service_token(
        &self,
        service_id: Uuid,
        idempotency_key: &str,
        pepper: &[u8],
    ) -> Result<IssuedServiceToken, AppError> {
        validate_idempotency_key(idempotency_key, "Idempotency-Key")?;
        let idempotency_key = idempotency_key.trim();
        let now = unix_millis();
        let request_hash =
            credential_rotation_request_hash(SERVICE_TOKEN_ROTATION_RESOURCE, service_id);
        let expires_at = now.saturating_add(CREDENTIAL_ROTATION_REPLAY_TTL_MILLIS);
        let mut transaction = self.pool.begin().await?;
        if let Some(replay) = claim_credential_rotation(
            &mut transaction,
            SERVICE_TOKEN_ROTATION_RESOURCE,
            service_id,
            idempotency_key,
            &request_hash,
            now,
            expires_at,
        )
        .await?
        {
            let issued = open_rotation_replay::<StoredIssuedServiceToken>(
                replay,
                SERVICE_TOKEN_ROTATION_RESOURCE,
                service_id,
                idempotency_key,
                &request_hash,
                pepper,
                now,
            )?;
            transaction.commit().await?;
            return Ok(issued.into());
        }

        let select = match self.backend {
            DatabaseBackend::PostgreSql => {
                "SELECT p.name, p.status, p.credential_generation, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1 FOR UPDATE OF p"
            }
            DatabaseBackend::Sqlite => {
                "SELECT p.name, p.status, p.credential_generation, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation AND c.revoked_at IS NULL WHERE p.id = $1"
            }
        };
        let row = sqlx::query(select)
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
        validate_service_scopes(&scopes)?;
        let tenant_external_id: Option<String> = row.try_get("tenant_external_id")?;
        let name: String = row.try_get("name")?;
        let issued = crypto::issue_service_credential(service_id, pepper);
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
        let response = IssuedServiceToken {
            service_id,
            name,
            credential_generation: generation,
            token: issued.secret,
            fingerprint: issued.fingerprint,
            scopes,
            tenant_external_id,
        };
        store_credential_rotation_response(
            &mut transaction,
            idempotency_key,
            &StoredIssuedServiceToken::from(&response),
            SERVICE_TOKEN_ROTATION_RESOURCE,
            service_id,
            &request_hash,
            expires_at,
            pepper,
        )
        .await?;
        transaction.commit().await?;
        Ok(response)
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
        let scopes: Vec<String> =
            serde_json::from_str(&scopes_json).map_err(|_| AppError::Unauthorized)?;
        // Existing rows are untrusted input too. Fail closed instead of
        // allowing a legacy wildcard/unknown scope to become authoritative.
        if validate_service_scopes(&scopes).is_err() {
            return Err(AppError::Unauthorized);
        }
        Ok(AuthenticatedService {
            service_id: Some(parsed.key_id),
            scopes,
            tenant_external_id: row.try_get("tenant_external_id")?,
        })
    }
}

fn service_token_view(row: AnyRow) -> Result<ServiceTokenView, AppError> {
    let scopes_json: String = row.try_get("scopes_json")?;
    Ok(ServiceTokenView {
        service_id: parse_uuid(row.try_get("id")?)?,
        name: row.try_get("name")?,
        status: row.try_get("status")?,
        credential_generation: row.try_get("credential_generation")?,
        fingerprint: row.try_get("fingerprint")?,
        scopes: serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?,
        tenant_external_id: row.try_get("tenant_external_id")?,
        created_at: row.try_get("created_at")?,
        updated_at: row.try_get("updated_at")?,
    })
}

fn validate_service_token_input(input: &CreateServiceTokenInput) -> Result<(), AppError> {
    if input.name.trim().is_empty() || input.name.len() > 120 {
        return Err(AppError::BadRequest(
            "service token name must contain 1 to 120 characters".into(),
        ));
    }
    validate_service_scopes(&input.scopes)?;
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

fn validate_service_scopes(scopes: &[String]) -> Result<(), AppError> {
    if scopes.is_empty() || scopes.len() > SUPPORTED_SERVICE_SCOPES.len() {
        return Err(AppError::BadRequest(
            "service token must contain a bounded set of supported scopes".into(),
        ));
    }
    let mut unique = std::collections::BTreeSet::new();
    if scopes.iter().any(|scope| {
        !SUPPORTED_SERVICE_SCOPES.contains(&scope.as_str()) || !unique.insert(scope.as_str())
    }) {
        return Err(AppError::BadRequest(
            "service token scopes must be unique supported exact scopes".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::super::*;
    use super::SUPPORTED_SERVICE_SCOPES;

    #[test]
    fn managed_service_scope_allowlist_matches_the_public_json_schema() {
        let schema: serde_json::Value =
            serde_json::from_str(include_str!("../../../schemas/service-token.schema.json"))
                .unwrap();
        let schema_scopes = schema
            .pointer("/properties/scopes/items/enum")
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .map(|scope| scope.as_str().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        let runtime_scopes = SUPPORTED_SERVICE_SCOPES
            .iter()
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(schema_scopes, runtime_scopes);
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
            .rotate_service_token(first.service_id, "rotate-service-token-1", pepper)
            .await
            .unwrap();
        let replay = database
            .rotate_service_token(first.service_id, "rotate-service-token-1", pepper)
            .await
            .unwrap();
        assert_eq!(rotated.service_id, first.service_id);
        assert_eq!(rotated.credential_generation, 2);
        assert_eq!(replay.credential_generation, 2);
        assert_eq!(replay.token, rotated.token);
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
    async fn managed_service_tokens_reject_wildcard_unknown_and_duplicate_scopes() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("service-token-scope.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a service credential pepper longer than thirty-two bytes";

        for scopes in [
            vec!["*".to_owned()],
            vec!["keys:*".to_owned()],
            vec!["future:admin".to_owned()],
            vec!["keys:read".to_owned(), "keys:read".to_owned()],
        ] {
            assert!(matches!(
                database
                    .create_service_token(
                        CreateServiceTokenInput {
                            name: "rejected-scope".to_owned(),
                            scopes,
                            tenant_external_id: None,
                        },
                        pepper,
                    )
                    .await,
                Err(AppError::BadRequest(_))
            ));
        }

        let supported = database
            .create_service_token(
                CreateServiceTokenInput {
                    name: "quarantine-operator".to_owned(),
                    scopes: vec![
                        "imports:session_archive:quarantine:read".to_owned(),
                        "imports:session_archive:quarantine:resolve".to_owned(),
                    ],
                    tenant_external_id: None,
                },
                pepper,
            )
            .await
            .unwrap();
        assert!(
            database
                .authenticate_service_token(&supported.token, pepper)
                .await
                .is_ok()
        );

        sqlx::query(
            "UPDATE service_credentials SET scopes_json = '[\"*\"]' WHERE service_principal_id = $1",
        )
        .bind(supported.service_id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        assert!(matches!(
            database
                .authenticate_service_token(&supported.token, pepper)
                .await,
            Err(AppError::Unauthorized)
        ));
        assert!(matches!(
            database
                .rotate_service_token(supported.service_id, "reject-stored-wildcard", pepper)
                .await,
            Err(AppError::BadRequest(_))
        ));
    }

    #[tokio::test]
    async fn service_token_pages_are_bounded_and_keyset_disjoint() {
        let directory = tempfile::tempdir().unwrap();
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("service-token-pages.db").display()
        );
        let database = Database::connect(&database_url).await.unwrap();
        database.migrate().await.unwrap();
        let pepper = b"a service credential pepper longer than thirty-two bytes";
        for name in ["page-a", "page-b", "page-c"] {
            database
                .create_service_token(
                    CreateServiceTokenInput {
                        name: name.to_owned(),
                        scopes: vec!["requests:read".to_owned()],
                        tenant_external_id: None,
                    },
                    pepper,
                )
                .await
                .unwrap();
        }

        let first = database
            .list_service_tokens_page(None, None, 2)
            .await
            .unwrap();
        assert_eq!(first.len(), 2);
        let cursor = first.last().unwrap();
        let second = database
            .list_service_tokens_page(Some(cursor.created_at), Some(cursor.service_id), 2)
            .await
            .unwrap();
        assert_eq!(second.len(), 1);
        assert!(first.iter().all(|left| {
            second
                .iter()
                .all(|right| left.service_id != right.service_id)
        }));
    }
}
