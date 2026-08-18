use super::super::*;

const SERVICE_TOKEN_ROTATION_RESOURCE: &str = "service_token";

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
        let rows = sqlx::query(
            "SELECT p.id, p.name, p.status, p.credential_generation, p.created_at, p.updated_at, c.fingerprint, c.scopes_json, c.tenant_external_id FROM service_principals p JOIN service_credentials c ON c.service_principal_id = p.id AND c.generation = p.credential_generation ORDER BY p.created_at DESC, p.id DESC",
        )
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
        let mut transaction = self.pool.begin().await?;
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
        Ok(AuthenticatedService {
            service_id: Some(parsed.key_id),
            scopes: serde_json::from_str(&scopes_json).map_err(|_| AppError::Internal)?,
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

#[cfg(test)]
mod tests {
    use super::super::super::*;

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
}
