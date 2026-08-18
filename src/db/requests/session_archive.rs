use std::sync::{Arc, OnceLock};

use super::super::*;

static SQLITE_SESSION_ARCHIVE_IMPORT_LOCK: OnceLock<Arc<tokio::sync::Mutex<()>>> = OnceLock::new();

const CPAMP_IMPORT_LOCK_KEY: &str = "memeloop-token-center:cpamp:global-staging-v1";

const CPAMP_IMPORT_LOCK_SEED: i64 = 734_627_102_948_313;

const SESSION_ARCHIVE_IMPORT_LOCK_SEED: i64 = 734_627_102_948_314;

#[derive(Clone, Debug)]
pub struct SessionArchiveMatchInput<'a> {
    pub tenant_external_id: &'a str,
    pub cpamp_source: &'a str,
    pub archive_source: &'a str,
    pub external_request_id: &'a str,
    pub started_at: i64,
    pub requested_model: Option<&'a str>,
    pub resolved_model: Option<&'a str>,
    pub source_key_hash: &'a str,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub record_digest: &'a str,
    pub time_tolerance_ms: i64,
}

#[derive(Clone, Debug)]
pub struct SessionArchiveTarget {
    pub tenant_id: Uuid,
    pub target_request_id: Uuid,
    pub request_created_at: i64,
    pub key: AuthenticatedKey,
    pub external_event_hash: String,
    pub source_created_at: i64,
    pub source_model: String,
    pub replay: bool,
}

#[derive(Clone, Debug)]
pub struct SessionArchiveUnlinkedTarget {
    pub tenant_id: Uuid,
    pub archive_request_id: Uuid,
    pub key: AuthenticatedKey,
    pub identity_proof_kind: String,
    pub identity_proof_digest: String,
    pub correlation_proof_digest: String,
    pub replay: bool,
}

#[derive(Clone, Debug)]
pub enum SessionArchiveCorrelation {
    Exact {
        target: SessionArchiveTarget,
        identity_proof_kind: String,
        identity_proof_digest: String,
        correlation_proof_digest: String,
    },
    Unlinked(SessionArchiveUnlinkedTarget),
}

pub struct SessionArchiveImportLock {
    postgres: Option<(AnyConnection, String)>,
    sqlite: Option<tokio::sync::OwnedMutexGuard<()>>,
}

impl SessionArchiveCorrelation {
    pub fn key(&self) -> &AuthenticatedKey {
        match self {
            Self::Exact { target, .. } => &target.key,
            Self::Unlinked(target) => &target.key,
        }
    }

    pub fn replay(&self) -> bool {
        match self {
            Self::Exact { target, .. } => target.replay,
            Self::Unlinked(target) => target.replay,
        }
    }
}

impl SessionArchiveImportLock {
    pub async fn release(mut self) -> Result<(), AppError> {
        self.sqlite.take();
        let Some((mut connection, scoped_key)) = self.postgres.take() else {
            return Ok(());
        };
        let scoped_released: bool =
            sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended($1, $2))")
                .bind(&scoped_key)
                .bind(SESSION_ARCHIVE_IMPORT_LOCK_SEED)
                .fetch_one(&mut connection)
                .await?;
        let global_released: bool =
            sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended($1, $2))")
                .bind(CPAMP_IMPORT_LOCK_KEY)
                .bind(CPAMP_IMPORT_LOCK_SEED)
                .fetch_one(&mut connection)
                .await?;
        drop(connection);
        if scoped_released && global_released {
            Ok(())
        } else {
            Err(AppError::Internal)
        }
    }
}

impl Database {
    pub async fn match_session_archive_request(
        &self,
        input: SessionArchiveMatchInput<'_>,
    ) -> Result<SessionArchiveTarget, AppError> {
        let rows = sqlx::query(
            "SELECT t.id AS tenant_id, l.target_request_id, rl.created_at AS request_created_at, l.external_event_hash, l.source_created_at, l.source_model, l.source_key_hash, r.input_tokens, r.output_tokens, r.key_id, k.principal_id, k.account_id, k.alias, k.currency, k.credential_generation, k.policy_json FROM import_request_links l JOIN tenants t ON t.id = l.tenant_id JOIN request_record_locators rl ON rl.id = l.target_request_id AND rl.tenant_id = l.tenant_id JOIN request_records r ON r.id = rl.id AND r.created_at = rl.created_at AND r.tenant_id = rl.tenant_id JOIN key_records k ON k.id = r.key_id AND k.tenant_id = l.tenant_id WHERE t.external_id = $1 AND l.source = $2 AND l.external_request_id = $3 ORDER BY l.source_created_at, l.external_event_hash",
        )
        .bind(input.tenant_external_id)
        .bind(input.cpamp_source)
        .bind(input.external_request_id)
        .fetch_all(&self.pool)
        .await?;

        if !is_sha256_hex(input.source_key_hash) {
            return Err(AppError::BadRequest(
                "archive request has no verified credential hash".into(),
            ));
        }

        let mut matches = Vec::new();
        for row in rows {
            let source_created_at: i64 = row.try_get("source_created_at")?;
            if source_created_at.abs_diff(input.started_at) > input.time_tolerance_ms.max(0) as u64
            {
                continue;
            }
            let source_model: String = row.try_get("source_model")?;
            let model_matches = [input.requested_model, input.resolved_model]
                .into_iter()
                .flatten()
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .any(|value| value == source_model);
            if !model_matches {
                continue;
            }
            let source_key_hash: String = row.try_get("source_key_hash")?;
            if !input.source_key_hash.eq_ignore_ascii_case(&source_key_hash) {
                continue;
            }
            matches.push((row, source_created_at, source_model));
        }
        if matches.len() > 1 {
            let usage = input
                .input_tokens
                .zip(input.output_tokens)
                .filter(|(input_tokens, output_tokens)| *input_tokens >= 0 && *output_tokens >= 0);
            if let Some((input_tokens, output_tokens)) = usage {
                let mut usage_matches = Vec::with_capacity(matches.len());
                for candidate in matches {
                    let candidate_input: i64 = candidate.0.try_get("input_tokens")?;
                    let candidate_output: i64 = candidate.0.try_get("output_tokens")?;
                    if candidate_input == input_tokens && candidate_output == output_tokens {
                        usage_matches.push(candidate);
                    }
                }
                matches = usage_matches;
            }
        }
        if matches.len() != 1 {
            return Err(AppError::BadRequest(
                "archive request does not map uniquely to a CPAMP event".into(),
            ));
        }
        let (row, source_created_at, source_model) = matches.pop().expect("one match");
        let tenant_id = parse_uuid(row.try_get("tenant_id")?)?;
        let target_request_id = parse_uuid(row.try_get("target_request_id")?)?;
        let key_id = parse_uuid(row.try_get("key_id")?)?;
        let policy_json: String = row.try_get("policy_json")?;
        let policy = serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?;

        let imported = sqlx::query(
            "SELECT target_request_id, record_digest FROM session_archive_import_records WHERE tenant_id = $1 AND source = $2 AND external_request_id = $3",
        )
        .bind(tenant_id.to_string())
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .fetch_optional(&self.pool)
        .await?;
        let replay = if let Some(existing) = imported {
            let existing_target: String = existing.try_get("target_request_id")?;
            let existing_digest: String = existing.try_get("record_digest")?;
            if existing_target != target_request_id.to_string()
                || existing_digest != input.record_digest
            {
                return Err(AppError::BadRequest(
                    "archive request changed after it was imported".into(),
                ));
            }
            true
        } else {
            false
        };

        Ok(SessionArchiveTarget {
            tenant_id,
            target_request_id,
            request_created_at: row.try_get("request_created_at")?,
            key: AuthenticatedKey {
                key_id,
                tenant_id,
                principal_id: parse_uuid(row.try_get("principal_id")?)?,
                account_id: parse_uuid(row.try_get("account_id")?)?,
                alias: row.try_get("alias")?,
                currency: row.try_get("currency")?,
                credential_generation: row.try_get("credential_generation")?,
                policy,
            },
            external_event_hash: row.try_get("external_event_hash")?,
            source_created_at,
            source_model,
            replay,
        })
    }

    /// Correlate an archive record without inventing a CPAMP edge.  An exact
    /// target is used only when the existing deterministic matcher proves one;
    /// otherwise the source credential hash must still prove exactly one stable
    /// key/principal identity before an archive-only row can be planned.
    pub async fn correlate_session_archive_request(
        &self,
        input: SessionArchiveMatchInput<'_>,
    ) -> Result<SessionArchiveCorrelation, AppError> {
        if !is_sha256_hex(input.source_key_hash)
            || !is_sha256_hex(input.record_digest)
            || !valid_archive_identifier(input.cpamp_source, 256)
            || !valid_archive_identifier(input.archive_source, 256)
            || !valid_archive_identifier(input.external_request_id, 512)
        {
            return Err(AppError::BadRequest(
                "archive correlation proof input is invalid".into(),
            ));
        }

        if let Some(existing) = self.existing_session_archive_correlation(&input).await? {
            return Ok(existing);
        }

        match self.match_session_archive_request(input.clone()).await {
            Ok(target) => {
                let identity_proof_kind = "cpamp-exact-target-v1".to_owned();
                let identity_proof_digest = archive_proof_digest(
                    "memeloop-session-archive-identity-v1",
                    &[
                        input.tenant_external_id,
                        input.cpamp_source,
                        input.source_key_hash,
                        &target.key.key_id.to_string(),
                        &target.key.principal_id.to_string(),
                        &target.target_request_id.to_string(),
                        &target.external_event_hash,
                    ],
                );
                let correlation_proof_digest = archive_proof_digest(
                    "memeloop-session-archive-correlation-v1",
                    &[
                        input.tenant_external_id,
                        input.archive_source,
                        input.external_request_id,
                        "exact",
                        &target.key.key_id.to_string(),
                        &target.key.principal_id.to_string(),
                        &target.target_request_id.to_string(),
                        input.record_digest,
                        &identity_proof_digest,
                    ],
                );
                Ok(SessionArchiveCorrelation::Exact {
                    target,
                    identity_proof_kind,
                    identity_proof_digest,
                    correlation_proof_digest,
                })
            }
            Err(error @ AppError::BadRequest(_)) => {
                // A provenance row from a previous exact import is authoritative.
                // Never downgrade a changed exact replay to archive-only.
                let prior_exact: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM session_archive_import_records r JOIN tenants t ON t.id = r.tenant_id WHERE t.external_id = $1 AND r.source = $2 AND r.external_request_id = $3",
                )
                .bind(input.tenant_external_id)
                .bind(input.archive_source)
                .bind(input.external_request_id)
                .fetch_one(&self.pool)
                .await?;
                if prior_exact != 0 {
                    return Err(AppError::BadRequest(
                        "existing exact archive provenance no longer verifies".into(),
                    ));
                }
                let raw_exact_candidates: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM import_request_links l JOIN tenants t ON t.id = l.tenant_id WHERE t.external_id = $1 AND l.source = $2 AND l.external_request_id = $3",
                )
                .bind(input.tenant_external_id)
                .bind(input.cpamp_source)
                .bind(input.external_request_id)
                .fetch_one(&self.pool)
                .await?;
                if raw_exact_candidates != 0 {
                    // A source row that claims an exact CPAMP id but changes its
                    // model/time/key evidence is inconsistent, not archive-only.
                    return Err(error);
                }

                let (key, identity_proof_kind) = self
                    .resolve_session_archive_identity(
                        input.tenant_external_id,
                        input.cpamp_source,
                        input.source_key_hash,
                    )
                    .await?;
                let identity_proof_digest = archive_proof_digest(
                    "memeloop-session-archive-identity-v1",
                    &[
                        input.tenant_external_id,
                        input.cpamp_source,
                        input.source_key_hash,
                        &key.key_id.to_string(),
                        &key.principal_id.to_string(),
                        &identity_proof_kind,
                    ],
                );
                let correlation_proof_digest = archive_proof_digest(
                    "memeloop-session-archive-correlation-v1",
                    &[
                        input.tenant_external_id,
                        input.archive_source,
                        input.external_request_id,
                        "unlinked",
                        &key.key_id.to_string(),
                        &key.principal_id.to_string(),
                        input.record_digest,
                        &identity_proof_digest,
                    ],
                );
                let archive_request_id = deterministic_archive_request_id(
                    input.tenant_external_id,
                    input.archive_source,
                    input.external_request_id,
                );
                Ok(SessionArchiveCorrelation::Unlinked(
                    SessionArchiveUnlinkedTarget {
                        tenant_id: key.tenant_id,
                        archive_request_id,
                        key,
                        identity_proof_kind,
                        identity_proof_digest,
                        correlation_proof_digest,
                        replay: false,
                    },
                ))
            }
            Err(error) => Err(error),
        }
    }

    async fn existing_session_archive_correlation(
        &self,
        input: &SessionArchiveMatchInput<'_>,
    ) -> Result<Option<SessionArchiveCorrelation>, AppError> {
        let row = sqlx::query(
            "SELECT c.tenant_id, c.disposition, c.key_id, c.principal_id, c.target_request_id, c.target_request_created_at, c.external_event_hash, c.record_digest, c.proof_digest, c.identity_proof_kind, c.identity_proof_digest, c.source_started_at, c.source_model, u.archive_request_id, k.account_id, k.alias, k.currency, k.credential_generation, k.policy_json FROM session_archive_correlations c JOIN tenants t ON t.id = c.tenant_id JOIN key_records k ON k.id = c.key_id AND k.tenant_id = c.tenant_id LEFT JOIN session_archive_unlinked_requests u ON u.tenant_id = c.tenant_id AND u.source = c.source AND u.external_request_id = c.external_request_id WHERE t.external_id = $1 AND c.source = $2 AND c.external_request_id = $3",
        )
        .bind(input.tenant_external_id)
        .bind(input.archive_source)
        .bind(input.external_request_id)
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        if row.try_get::<String, _>("record_digest")? != input.record_digest {
            return Err(AppError::BadRequest(
                "archive request changed after correlation".into(),
            ));
        }
        let key = archive_authenticated_key(&row)?;
        if key.principal_id.to_string() != row.try_get::<String, _>("principal_id")? {
            return Err(AppError::BadRequest(
                "archive correlation identity no longer verifies".into(),
            ));
        }
        let identity_proof_kind: String = row.try_get("identity_proof_kind")?;
        let identity_proof_digest: String = row.try_get("identity_proof_digest")?;
        let correlation_proof_digest: String = row.try_get("proof_digest")?;
        match row.try_get::<String, _>("disposition")?.as_str() {
            "exact" => {
                let target_request_id = parse_uuid(
                    row.try_get::<Option<String>, _>("target_request_id")?
                        .ok_or(AppError::Internal)?,
                )?;
                let external_event_hash = row
                    .try_get::<Option<String>, _>("external_event_hash")?
                    .ok_or(AppError::Internal)?;
                let expected_identity = archive_proof_digest(
                    "memeloop-session-archive-identity-v1",
                    &[
                        input.tenant_external_id,
                        input.cpamp_source,
                        input.source_key_hash,
                        &key.key_id.to_string(),
                        &key.principal_id.to_string(),
                        &target_request_id.to_string(),
                        &external_event_hash,
                    ],
                );
                let expected_correlation = archive_proof_digest(
                    "memeloop-session-archive-correlation-v1",
                    &[
                        input.tenant_external_id,
                        input.archive_source,
                        input.external_request_id,
                        "exact",
                        &key.key_id.to_string(),
                        &key.principal_id.to_string(),
                        &target_request_id.to_string(),
                        input.record_digest,
                        &expected_identity,
                    ],
                );
                if identity_proof_kind != "cpamp-exact-target-v1"
                    || identity_proof_digest != expected_identity
                    || correlation_proof_digest != expected_correlation
                {
                    return Err(AppError::BadRequest(
                        "stored exact archive correlation proof is invalid".into(),
                    ));
                }
                Ok(Some(SessionArchiveCorrelation::Exact {
                    target: SessionArchiveTarget {
                        tenant_id: key.tenant_id,
                        target_request_id,
                        request_created_at: row
                            .try_get::<Option<i64>, _>("target_request_created_at")?
                            .ok_or(AppError::Internal)?,
                        key,
                        external_event_hash,
                        source_created_at: row.try_get("source_started_at")?,
                        source_model: row.try_get("source_model")?,
                        replay: true,
                    },
                    identity_proof_kind,
                    identity_proof_digest,
                    correlation_proof_digest,
                }))
            }
            "unlinked" => {
                let (verified_key, expected_kind) = self
                    .resolve_session_archive_identity(
                        input.tenant_external_id,
                        input.cpamp_source,
                        input.source_key_hash,
                    )
                    .await?;
                let expected_identity = archive_proof_digest(
                    "memeloop-session-archive-identity-v1",
                    &[
                        input.tenant_external_id,
                        input.cpamp_source,
                        input.source_key_hash,
                        &key.key_id.to_string(),
                        &key.principal_id.to_string(),
                        &expected_kind,
                    ],
                );
                let expected_correlation = archive_proof_digest(
                    "memeloop-session-archive-correlation-v1",
                    &[
                        input.tenant_external_id,
                        input.archive_source,
                        input.external_request_id,
                        "unlinked",
                        &key.key_id.to_string(),
                        &key.principal_id.to_string(),
                        input.record_digest,
                        &expected_identity,
                    ],
                );
                let expected_request_id = deterministic_archive_request_id(
                    input.tenant_external_id,
                    input.archive_source,
                    input.external_request_id,
                );
                let archive_request_id = parse_uuid(
                    row.try_get::<Option<String>, _>("archive_request_id")?
                        .ok_or(AppError::Internal)?,
                )?;
                if verified_key.key_id != key.key_id
                    || verified_key.principal_id != key.principal_id
                    || identity_proof_kind != expected_kind
                    || identity_proof_digest != expected_identity
                    || correlation_proof_digest != expected_correlation
                    || archive_request_id != expected_request_id
                {
                    return Err(AppError::BadRequest(
                        "stored unlinked archive correlation proof is invalid".into(),
                    ));
                }
                Ok(Some(SessionArchiveCorrelation::Unlinked(
                    SessionArchiveUnlinkedTarget {
                        tenant_id: key.tenant_id,
                        archive_request_id,
                        key,
                        identity_proof_kind,
                        identity_proof_digest,
                        correlation_proof_digest,
                        replay: true,
                    },
                )))
            }
            _ => Err(AppError::Internal),
        }
    }

    async fn resolve_session_archive_identity(
        &self,
        tenant_external_id: &str,
        cpamp_source: &str,
        source_key_hash: &str,
    ) -> Result<(AuthenticatedKey, String), AppError> {
        let rows = sqlx::query(
            "SELECT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.credential_generation, k.policy_json, 'legacy-source-hash-v1' AS proof_kind FROM legacy_key_credentials c JOIN key_records k ON k.id = c.key_id JOIN tenants t ON t.id = k.tenant_id WHERE t.external_id = $1 AND LOWER(c.source_hash) = LOWER($2) UNION ALL SELECT DISTINCT k.id AS key_id, k.tenant_id, k.principal_id, k.account_id, k.alias, k.currency, k.credential_generation, k.policy_json, 'cpamp-source-key-hash-v1' AS proof_kind FROM import_request_links l JOIN tenants t ON t.id = l.tenant_id JOIN request_record_locators q ON q.id = l.target_request_id AND q.tenant_id = l.tenant_id JOIN request_records r ON r.id = q.id AND r.created_at = q.created_at AND r.tenant_id = q.tenant_id JOIN key_records k ON k.id = r.key_id AND k.tenant_id = l.tenant_id WHERE t.external_id = $1 AND l.source = $3 AND LOWER(l.source_key_hash) = LOWER($2)",
        )
        .bind(tenant_external_id)
        .bind(source_key_hash)
        .bind(cpamp_source)
        .fetch_all(&self.pool)
        .await?;
        let mut selected: Option<AuthenticatedKey> = None;
        let mut proof_kinds = std::collections::BTreeSet::new();
        for row in rows {
            let candidate = archive_authenticated_key(&row)?;
            if selected.as_ref().is_some_and(|current| {
                current.key_id != candidate.key_id
                    || current.tenant_id != candidate.tenant_id
                    || current.principal_id != candidate.principal_id
            }) {
                return Err(AppError::BadRequest(
                    "archive credential hash maps to multiple stable identities".into(),
                ));
            }
            proof_kinds.insert(row.try_get::<String, _>("proof_kind")?);
            selected = Some(candidate);
        }
        let key = selected.ok_or_else(|| {
            AppError::BadRequest(
                "archive request has no proven stable key/principal identity".into(),
            )
        })?;
        Ok((key, proof_kinds.into_iter().collect::<Vec<_>>().join("+")))
    }

    pub async fn acquire_session_archive_import_lock(
        &self,
        tenant_external_id: &str,
        archive_source: &str,
    ) -> Result<SessionArchiveImportLock, AppError> {
        if matches!(self.backend, DatabaseBackend::Sqlite) {
            let lock = SQLITE_SESSION_ARCHIVE_IMPORT_LOCK
                .get_or_init(|| Arc::new(tokio::sync::Mutex::new(())))
                .clone()
                .lock_owned()
                .await;
            return Ok(SessionArchiveImportLock {
                postgres: None,
                sqlite: Some(lock),
            });
        }

        // Detaching is deliberate: cancellation drops the raw connection instead
        // of returning a session-level advisory lock to the pool.
        let mut connection = self.pool.acquire().await?.detach();
        let global_acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, $2))")
                .bind(CPAMP_IMPORT_LOCK_KEY)
                .bind(CPAMP_IMPORT_LOCK_SEED)
                .fetch_one(&mut connection)
                .await?;
        if !global_acquired {
            return Err(AppError::BadRequest(
                "a CPAMP or session archive import is already running".into(),
            ));
        }

        let scoped_key =
            format!("memeloop-token-center:session-archive:{tenant_external_id}:{archive_source}");
        let scoped_acquired: bool =
            sqlx::query_scalar("SELECT pg_try_advisory_lock(hashtextextended($1, $2))")
                .bind(&scoped_key)
                .bind(SESSION_ARCHIVE_IMPORT_LOCK_SEED)
                .fetch_one(&mut connection)
                .await?;
        if !scoped_acquired {
            let _: bool = sqlx::query_scalar("SELECT pg_advisory_unlock(hashtextextended($1, $2))")
                .bind(CPAMP_IMPORT_LOCK_KEY)
                .bind(CPAMP_IMPORT_LOCK_SEED)
                .fetch_one(&mut connection)
                .await?;
            return Err(AppError::BadRequest(
                "this tenant and archive source are already being imported".into(),
            ));
        }
        Ok(SessionArchiveImportLock {
            postgres: Some((connection, scoped_key)),
            sqlite: None,
        })
    }

    /// Verify the independently migrated target schema without performing DDL.
    /// The one-shot archive importer must never acquire schema-owner privileges
    /// or mutate the target before the complete source and local plan are sealed.
    pub async fn ensure_session_archive_import_schema(&self) -> Result<(), AppError> {
        let migrations = match self.backend {
            DatabaseBackend::PostgreSql => POSTGRES_MIGRATIONS,
            DatabaseBackend::Sqlite => SQLITE_MIGRATIONS,
        };
        let expected = migrations
            .last()
            .map(|migration| migration.version)
            .unwrap_or(0);
        let row = sqlx::query(
            "SELECT COUNT(*) AS applied, COALESCE(MAX(version), 0) AS latest FROM schema_migrations WHERE version >= 1 AND version <= $1",
        )
        .bind(expected)
        .fetch_optional(&self.pool)
        .await
        .map_err(|_| {
            AppError::BadRequest(
                "target database must be migrated before session archive import".into(),
            )
        })?
        .ok_or_else(|| {
            AppError::BadRequest(
                "target database must be migrated before session archive import".into(),
            )
        })?;
        let applied: i64 = row.try_get("applied")?;
        let latest: i64 = row.try_get("latest")?;
        if applied != expected || latest != expected {
            return Err(AppError::BadRequest(format!(
                "target database schema is incomplete: expected migrations 1 through {expected}"
            )));
        }

        // Reference every importer-owned relation and the post-v21 locator/source
        // columns. A forged or drifted schema_migrations table therefore remains
        // fail-closed before source planning or CAS writes.
        sqlx::query(
            "SELECT l.source_digest, r.record_digest, c.watermark_ms, q.created_at, x.proof_digest, u.archive_request_id FROM import_request_links l CROSS JOIN session_archive_import_records r CROSS JOIN session_archive_import_checkpoints c CROSS JOIN request_record_locators q CROSS JOIN session_archive_correlations x CROSS JOIN session_archive_unlinked_requests u WHERE 1 = 0",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|_| {
            AppError::BadRequest(
                "target database session archive schema is incomplete or drifted".into(),
            )
        })?;
        Ok(())
    }

    pub async fn session_archive_lower_bound(
        &self,
        tenant_external_id: &str,
        archive_source: &str,
        overlap_ms: i64,
    ) -> Result<i64, AppError> {
        let row = sqlx::query(
            "SELECT c.watermark_ms FROM session_archive_import_checkpoints c JOIN tenants t ON t.id = c.tenant_id WHERE t.external_id = $1 AND c.source = $2",
        )
        .bind(tenant_external_id)
        .bind(archive_source)
        .fetch_optional(&self.pool)
        .await?;
        let watermark = row
            .map(|row| row.try_get::<i64, _>("watermark_ms"))
            .transpose()?
            .unwrap_or(0);
        Ok(watermark.saturating_sub(overlap_ms.max(0)).max(0))
    }
}

fn archive_authenticated_key(row: &AnyRow) -> Result<AuthenticatedKey, AppError> {
    let policy_json: String = row.try_get("policy_json")?;
    Ok(AuthenticatedKey {
        key_id: parse_uuid(row.try_get("key_id")?)?,
        tenant_id: parse_uuid(row.try_get("tenant_id")?)?,
        principal_id: parse_uuid(row.try_get("principal_id")?)?,
        account_id: parse_uuid(row.try_get("account_id")?)?,
        alias: row.try_get("alias")?,
        currency: row.try_get("currency")?,
        credential_generation: row.try_get("credential_generation")?,
        policy: serde_json::from_str(&policy_json).map_err(|_| AppError::Internal)?,
    })
}

fn archive_proof_digest(domain: &str, fields: &[&str]) -> String {
    let mut digest = Sha256::new();
    digest.update(domain.as_bytes());
    for field in fields {
        digest.update([0]);
        digest.update(field.as_bytes());
    }
    format!("{:x}", digest.finalize())
}

fn deterministic_archive_request_id(
    tenant_external_id: &str,
    source: &str,
    external_request_id: &str,
) -> Uuid {
    let digest = Sha256::digest(
        [
            b"memeloop-session-archive-request-v1".as_slice(),
            &[0],
            tenant_external_id.as_bytes(),
            &[0],
            source.as_bytes(),
            &[0],
            external_request_id.as_bytes(),
        ]
        .concat(),
    );
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // UUIDv8 gives the stable opaque request identity standard variant/version bits.
    bytes[6] = (bytes[6] & 0x0f) | 0x80;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

pub(crate) fn valid_archive_identifier(value: &str, max_bytes: usize) -> bool {
    !value.is_empty()
        && value.len() <= max_bytes
        && !value.bytes().any(|byte| byte.is_ascii_control())
}
