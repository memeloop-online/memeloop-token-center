use sqlx::Row;
use uuid::Uuid;

use super::*;

const CLAIM_LEASE_MILLIS: i64 = 30_000;

#[derive(Clone, Debug)]
pub struct BeginOAuthLoginSession {
    pub session_id: Uuid,
    pub flow_kind: String,
    pub tenant_external_id: String,
    pub operator_service_id: Option<Uuid>,
    pub state_ciphertext: String,
    pub next_poll_at: i64,
    pub expires_at: i64,
}

#[derive(Clone, Debug)]
pub struct OAuthLoginSessionReference {
    pub session_id: Uuid,
    pub flow_kind: String,
    pub tenant_external_id: String,
    pub operator_service_id: Option<Uuid>,
    pub expires_at: i64,
}

#[derive(Clone, Debug)]
pub enum OAuthLoginClaim {
    Pending {
        retry_after_seconds: u64,
    },
    Claimed {
        lease_owner: Uuid,
        state_ciphertext: String,
    },
    Ready {
        lease_owner: Uuid,
        ready_ciphertext: String,
    },
    Consumed {
        account_id: Uuid,
    },
}

impl Database {
    pub async fn begin_oauth_login_session(
        &self,
        input: BeginOAuthLoginSession,
    ) -> Result<(), AppError> {
        validate_session_scope(
            &input.flow_kind,
            &input.tenant_external_id,
            input.next_poll_at,
            input.expires_at,
            &input.state_ciphertext,
        )?;
        let now = unix_millis();
        sqlx::query(
            "INSERT INTO oauth_login_sessions (id, flow_kind, tenant_external_id, operator_service_id, operator_is_bootstrap, state_ciphertext, ready_ciphertext, next_poll_at, expires_at, status, lease_owner, lease_expires_at, result_account_id, created_at, updated_at) VALUES ($1, $2, $3, $4, $5, $6, NULL, $7, $8, 'pending', NULL, NULL, NULL, $9, $9)",
        )
        .bind(input.session_id.to_string())
        .bind(input.flow_kind)
        .bind(input.tenant_external_id)
        .bind(input.operator_service_id.map(|id| id.to_string()))
        .bind(input.operator_service_id.is_none())
        .bind(input.state_ciphertext)
        .bind(input.next_poll_at)
        .bind(input.expires_at)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub async fn claim_oauth_login_poll(
        &self,
        reference: &OAuthLoginSessionReference,
        now: i64,
        poll_interval_seconds: u64,
    ) -> Result<OAuthLoginClaim, AppError> {
        let interval_millis = i64::try_from(poll_interval_seconds.clamp(1, 60))
            .unwrap_or(60)
            .saturating_mul(1_000);
        let lease_owner = Uuid::now_v7();
        let lease_expires_at = now.saturating_add(CLAIM_LEASE_MILLIS);
        let next_poll_at = now.saturating_add(interval_millis);
        let operator_id = reference.operator_service_id.map(|id| id.to_string());
        let changed = sqlx::query(
            "UPDATE oauth_login_sessions SET status = 'polling', lease_owner = $1, lease_expires_at = $2, next_poll_at = $3, updated_at = $4 WHERE id = $5 AND flow_kind = $6 AND tenant_external_id = $7 AND operator_is_bootstrap = $8 AND ((operator_service_id IS NULL AND $9 IS NULL) OR operator_service_id = $9) AND expires_at = $10 AND expires_at > $4 AND next_poll_at <= $4 AND (status = 'pending' OR (status = 'polling' AND lease_expires_at <= $4))",
        )
        .bind(lease_owner.to_string())
        .bind(lease_expires_at)
        .bind(next_poll_at)
        .bind(now)
        .bind(reference.session_id.to_string())
        .bind(&reference.flow_kind)
        .bind(&reference.tenant_external_id)
        .bind(reference.operator_service_id.is_none())
        .bind(&operator_id)
        .bind(reference.expires_at)
        .execute(&self.pool)
        .await?;
        if changed.rows_affected() == 1 {
            let state_ciphertext: String = sqlx::query(
                "SELECT state_ciphertext FROM oauth_login_sessions WHERE id = $1 AND lease_owner = $2",
            )
            .bind(reference.session_id.to_string())
            .bind(lease_owner.to_string())
            .fetch_one(&self.pool)
            .await?
            .try_get("state_ciphertext")?;
            return Ok(OAuthLoginClaim::Claimed {
                lease_owner,
                state_ciphertext,
            });
        }
        let row = sqlx::query(
            "SELECT s.tenant_external_id, s.operator_service_id, CASE WHEN s.operator_is_bootstrap THEN 1 ELSE 0 END AS operator_is_bootstrap_int, s.expires_at, s.status, s.next_poll_at, s.lease_expires_at, s.ready_ciphertext, s.result_account_id, (SELECT a.id FROM upstream_accounts a WHERE a.oauth_session_id = s.id) AS recovered_account_id FROM oauth_login_sessions s WHERE s.id = $1 AND s.flow_kind = $2",
        )
        .bind(reference.session_id.to_string())
        .bind(&reference.flow_kind)
        .fetch_optional(&self.pool)
        .await?
        .ok_or(AppError::Forbidden)?;
        require_matching_reference(&row, reference)?;
        let expires_at: i64 = row.try_get("expires_at")?;
        if expires_at <= now {
            let _ = sqlx::query(
                "UPDATE oauth_login_sessions SET status = 'failed', lease_owner = NULL, lease_expires_at = NULL, updated_at = $1 WHERE id = $2 AND status IN ('pending', 'polling')",
            )
            .bind(now)
            .bind(reference.session_id.to_string())
            .execute(&self.pool)
            .await;
            return Err(AppError::BadRequest("OAuth login session expired".into()));
        }
        if let Some(account_id) = row
            .try_get::<Option<String>, _>("result_account_id")?
            .or(row.try_get::<Option<String>, _>("recovered_account_id")?)
        {
            let account_id = parse_uuid(account_id)?;
            sqlx::query(
                "UPDATE oauth_login_sessions SET status = 'consumed', result_account_id = $1, lease_owner = NULL, lease_expires_at = NULL, updated_at = $2 WHERE id = $3 AND ready_ciphertext IS NOT NULL AND status IN ('ready', 'finalizing', 'consumed')",
            )
            .bind(account_id.to_string())
            .bind(now)
            .bind(reference.session_id.to_string())
            .execute(&self.pool)
            .await?;
            return Ok(OAuthLoginClaim::Consumed { account_id });
        }
        match row.try_get::<String, _>("status")?.as_str() {
            "ready" | "finalizing" => {
                let changed = sqlx::query(
                    "UPDATE oauth_login_sessions SET status = 'finalizing', lease_owner = $1, lease_expires_at = $2, updated_at = $3 WHERE id = $4 AND (status = 'ready' OR (status = 'finalizing' AND lease_expires_at <= $3))",
                )
                .bind(lease_owner.to_string())
                .bind(lease_expires_at)
                .bind(now)
                .bind(reference.session_id.to_string())
                .execute(&self.pool)
                .await?;
                if changed.rows_affected() == 1 {
                    Ok(OAuthLoginClaim::Ready {
                        lease_owner,
                        ready_ciphertext: row
                            .try_get::<Option<String>, _>("ready_ciphertext")?
                            .ok_or(AppError::Internal)?,
                    })
                } else {
                    Ok(OAuthLoginClaim::Pending {
                        retry_after_seconds: millis_until(
                            now,
                            row.try_get::<Option<i64>, _>("lease_expires_at")?
                                .unwrap_or(lease_expires_at),
                        ),
                    })
                }
            }
            "failed" => Err(AppError::BadRequest(
                "OAuth login session is no longer active".into(),
            )),
            "consumed" => Err(AppError::Internal),
            "pending" | "polling" => {
                let next = row.try_get::<i64, _>("next_poll_at")?.max(
                    row.try_get::<Option<i64>, _>("lease_expires_at")?
                        .unwrap_or(i64::MIN),
                );
                Ok(OAuthLoginClaim::Pending {
                    retry_after_seconds: millis_until(now, next),
                })
            }
            _ => Err(AppError::Internal),
        }
    }

    pub async fn release_oauth_login_poll(
        &self,
        session_id: Uuid,
        lease_owner: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        let changed = sqlx::query(
            "UPDATE oauth_login_sessions SET status = 'pending', lease_owner = NULL, lease_expires_at = NULL, updated_at = $1 WHERE id = $2 AND status = 'polling' AND lease_owner = $3",
        )
        .bind(now)
        .bind(session_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&self.pool)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict("OAuth login poll lease changed".into()));
        }
        Ok(())
    }

    pub async fn stage_oauth_login_ready(
        &self,
        session_id: Uuid,
        lease_owner: Uuid,
        ready_ciphertext: String,
        now: i64,
    ) -> Result<(), AppError> {
        if ready_ciphertext.is_empty() || ready_ciphertext.len() > 512 * 1024 {
            return Err(AppError::BadRequest("OAuth result is invalid".into()));
        }
        let changed = sqlx::query(
            "UPDATE oauth_login_sessions SET status = 'ready', ready_ciphertext = $1, lease_owner = NULL, lease_expires_at = NULL, updated_at = $2 WHERE id = $3 AND status = 'polling' AND lease_owner = $4 AND expires_at > $2",
        )
        .bind(ready_ciphertext)
        .bind(now)
        .bind(session_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&self.pool)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(AppError::Conflict("OAuth login poll lease changed".into()));
        }
        Ok(())
    }

    pub async fn finish_oauth_login_session(
        &self,
        session_id: Uuid,
        lease_owner: Uuid,
        account_id: Uuid,
        now: i64,
    ) -> Result<(), AppError> {
        let changed = sqlx::query(
            "UPDATE oauth_login_sessions SET status = 'consumed', result_account_id = $1, lease_owner = NULL, lease_expires_at = NULL, updated_at = $2 WHERE id = $3 AND status = 'finalizing' AND lease_owner = $4 AND (result_account_id IS NULL OR result_account_id = $1)",
        )
        .bind(account_id.to_string())
        .bind(now)
        .bind(session_id.to_string())
        .bind(lease_owner.to_string())
        .execute(&self.pool)
        .await?;
        if changed.rows_affected() == 0 {
            let existing = sqlx::query(
                "SELECT result_account_id FROM oauth_login_sessions WHERE id = $1 AND status = 'consumed'",
            )
            .bind(session_id.to_string())
            .fetch_optional(&self.pool)
            .await?;
            let expected_account_id = account_id.to_string();
            if existing
                .and_then(|row| {
                    row.try_get::<Option<String>, _>("result_account_id")
                        .ok()
                        .flatten()
                })
                .as_deref()
                != Some(expected_account_id.as_str())
            {
                return Err(AppError::Conflict("OAuth login result changed".into()));
            }
        }
        Ok(())
    }

    pub async fn cleanup_oauth_login_sessions(
        &self,
        now: i64,
        limit: i64,
    ) -> Result<u64, AppError> {
        let result = sqlx::query(
            "DELETE FROM oauth_login_sessions WHERE id IN (SELECT id FROM oauth_login_sessions WHERE expires_at < $1 ORDER BY expires_at, id LIMIT $2)",
        )
        .bind(now.saturating_sub(24 * 60 * 60 * 1_000))
        .bind(limit.clamp(1, 1_000))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

fn validate_session_scope(
    flow_kind: &str,
    tenant: &str,
    next_poll_at: i64,
    expires_at: i64,
    ciphertext: &str,
) -> Result<(), AppError> {
    if !matches!(
        flow_kind,
        "openai_codex_device"
            | "cursor_pkce"
            | "provider_adapter_cursor_pkce"
            | "claude_manual_pkce"
            | "github_copilot_device"
    ) || tenant.is_empty()
        || tenant.len() > 200
        || tenant.trim() != tenant
        || tenant.chars().any(char::is_control)
        || next_poll_at >= expires_at
        || ciphertext.is_empty()
        || ciphertext.len() > 256 * 1024
    {
        return Err(AppError::BadRequest(
            "OAuth login session is invalid".into(),
        ));
    }
    Ok(())
}

fn require_matching_reference(
    row: &sqlx::any::AnyRow,
    reference: &OAuthLoginSessionReference,
) -> Result<(), AppError> {
    let stored_operator = row.try_get::<Option<String>, _>("operator_service_id")?;
    if row.try_get::<String, _>("tenant_external_id")? != reference.tenant_external_id
        || (row.try_get::<i64, _>("operator_is_bootstrap_int")? != 0)
            != reference.operator_service_id.is_none()
        || stored_operator.as_deref()
            != reference
                .operator_service_id
                .map(|id| id.to_string())
                .as_deref()
        || row.try_get::<i64, _>("expires_at")? != reference.expires_at
    {
        return Err(AppError::Forbidden);
    }
    Ok(())
}

fn millis_until(now: i64, next: i64) -> u64 {
    u64::try_from((next.saturating_sub(now).saturating_add(999) / 1_000).max(1)).unwrap_or(60)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn sqlite_databases() -> (tempfile::TempDir, Database, Database) {
        let directory = tempfile::tempdir().expect("OAuth session temporary directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("oauth-sessions.db").display()
        );
        let database = Database::connect(&database_url)
            .await
            .expect("connect OAuth session database");
        database
            .migrate()
            .await
            .expect("migrate OAuth session database");
        let second = Database::connect(&database_url)
            .await
            .expect("connect second OAuth session database");
        (directory, database, second)
    }

    fn reference(session_id: Uuid, expires_at: i64) -> OAuthLoginSessionReference {
        OAuthLoginSessionReference {
            session_id,
            flow_kind: "openai_codex_device".to_owned(),
            tenant_external_id: "oauth-session-test".to_owned(),
            operator_service_id: None,
            expires_at,
        }
    }

    #[tokio::test]
    async fn poll_and_finalize_leases_are_single_owner_and_replayable() {
        let (_directory, database, second_database) = sqlite_databases().await;
        let now = 1_000_000;
        let session_id = Uuid::now_v7();
        let expires_at = now + 60_000;
        database
            .begin_oauth_login_session(BeginOAuthLoginSession {
                session_id,
                flow_kind: "openai_codex_device".to_owned(),
                tenant_external_id: "oauth-session-test".to_owned(),
                operator_service_id: None,
                state_ciphertext: "encrypted-state".to_owned(),
                next_poll_at: now,
                expires_at,
            })
            .await
            .expect("begin OAuth session");

        let first_reference = reference(session_id, expires_at);
        let second_reference = first_reference.clone();
        let (first, second) = tokio::join!(
            database.claim_oauth_login_poll(&first_reference, now, 5),
            second_database.claim_oauth_login_poll(&second_reference, now, 5)
        );
        let first = first.expect("first replica poll result");
        let second = second.expect("second replica poll result");
        let poll_owner = match (first, second) {
            (
                OAuthLoginClaim::Claimed {
                    lease_owner,
                    state_ciphertext,
                },
                OAuthLoginClaim::Pending { .. },
            )
            | (
                OAuthLoginClaim::Pending { .. },
                OAuthLoginClaim::Claimed {
                    lease_owner,
                    state_ciphertext,
                },
            ) => {
                assert_eq!(state_ciphertext, "encrypted-state");
                lease_owner
            }
            other => panic!("expected exactly one poll owner: {other:?}"),
        };
        assert!(matches!(
            second_database
                .claim_oauth_login_poll(&reference(session_id, expires_at), now, 5)
                .await
                .expect("observe claimed poll"),
            OAuthLoginClaim::Pending { .. }
        ));

        database
            .stage_oauth_login_ready(session_id, poll_owner, "encrypted-ready".to_owned(), now)
            .await
            .expect("stage ready login");
        let first_reference = reference(session_id, expires_at);
        let second_reference = first_reference.clone();
        let (first, second) = tokio::join!(
            database.claim_oauth_login_poll(&first_reference, now, 5),
            second_database.claim_oauth_login_poll(&second_reference, now, 5)
        );
        let first = first.expect("first replica finalize result");
        let second = second.expect("second replica finalize result");
        let finalize_owner = match (first, second) {
            (
                OAuthLoginClaim::Ready {
                    lease_owner,
                    ready_ciphertext,
                },
                OAuthLoginClaim::Pending { .. },
            )
            | (
                OAuthLoginClaim::Pending { .. },
                OAuthLoginClaim::Ready {
                    lease_owner,
                    ready_ciphertext,
                },
            ) => {
                assert_eq!(ready_ciphertext, "encrypted-ready");
                lease_owner
            }
            other => panic!("expected exactly one finalize owner: {other:?}"),
        };
        assert!(matches!(
            second_database
                .claim_oauth_login_poll(&reference(session_id, expires_at), now, 5)
                .await
                .expect("observe finalizing login"),
            OAuthLoginClaim::Pending { .. }
        ));

        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "oauth-session-test".to_owned(),
                    name: "native-codex".to_owned(),
                    driver: "openai-codex".to_owned(),
                    config: serde_json::json!({
                        "base_url": "https://chatgpt.com/backend-api/codex",
                        "network_scope": "public",
                        "reservation_token_bounds": {}
                    }),
                    credential: UpstreamCredential::OAuth {
                        access_token: "access-token".to_owned(),
                        refresh_token: Some("refresh-token".to_owned()),
                        expires_at: Some(expires_at),
                        header: "authorization".to_owned(),
                        prefix: "Bearer ".to_owned(),
                        adapter_state: Some(serde_json::json!({
                            "schema": "openai-codex-oauth-v1",
                            "account_id": "account-test"
                        })),
                    },
                    oauth_session_id: Some(session_id),
                    oauth_driver: Some("openai_codex_device".to_owned()),
                    oauth_refresh_url: Some("https://auth.openai.com/oauth/token".to_owned()),
                },
                b"oauth-session-test-pepper-at-least-32-bytes",
            )
            .await
            .expect("create authorized upstream");
        database
            .finish_oauth_login_session(session_id, finalize_owner, account.id, now)
            .await
            .expect("finish OAuth session");
        assert!(matches!(
            database
                .claim_oauth_login_poll(&reference(session_id, expires_at), now, 5)
                .await
                .expect("replay consumed OAuth session"),
            OAuthLoginClaim::Consumed { account_id } if account_id == account.id
        ));

        sqlx::query("DELETE FROM upstream_accounts WHERE id = $1")
            .bind(account.id.to_string())
            .execute(&database.pool)
            .await
            .expect("delete upstream and cascade OAuth session");
        let remaining: i64 =
            sqlx::query("SELECT COUNT(*) AS count FROM oauth_login_sessions WHERE id = $1")
                .bind(session_id.to_string())
                .fetch_one(&database.pool)
                .await
                .expect("count OAuth sessions")
                .try_get("count")
                .expect("OAuth session count");
        assert_eq!(remaining, 0);
    }

    #[tokio::test]
    async fn session_token_scope_mismatch_fails_closed() {
        let (_directory, database, _second_database) = sqlite_databases().await;
        let now = 2_000_000;
        let session_id = Uuid::now_v7();
        let expires_at = now + 60_000;
        database
            .begin_oauth_login_session(BeginOAuthLoginSession {
                session_id,
                flow_kind: "openai_codex_device".to_owned(),
                tenant_external_id: "oauth-session-test".to_owned(),
                operator_service_id: None,
                state_ciphertext: "encrypted-state".to_owned(),
                next_poll_at: now,
                expires_at,
            })
            .await
            .expect("begin OAuth session");
        let mismatched = OAuthLoginSessionReference {
            tenant_external_id: "other-tenant".to_owned(),
            ..reference(session_id, expires_at)
        };
        assert!(matches!(
            database.claim_oauth_login_poll(&mismatched, now, 5).await,
            Err(AppError::Forbidden)
        ));
    }

    #[tokio::test]
    async fn reauthorization_preserves_stable_account_route_and_request_history() {
        let (_directory, database, _second_database) = sqlite_databases().await;
        let pepper = b"oauth-reauthorize-pepper-at-least-32-bytes";
        let account = database
            .create_upstream_account(
                CreateUpstreamAccountInput {
                    tenant_external_id: "oauth-reauthorize-test".to_owned(),
                    name: "stable-codex".to_owned(),
                    driver: "openai-codex".to_owned(),
                    config: serde_json::json!({
                        "base_url": "https://chatgpt.com/backend-api/codex",
                        "network_scope": "public",
                        "reservation_token_bounds": {"gpt-codex": 1000}
                    }),
                    credential: oauth_credential("access-v1", "refresh-v1", 4_102_444_800_000),
                    oauth_session_id: Some(Uuid::now_v7()),
                    oauth_driver: Some("openai_codex_device".to_owned()),
                    oauth_refresh_url: Some("https://auth.openai.com/oauth/token".to_owned()),
                },
                pepper,
            )
            .await
            .expect("create initial Codex account");
        let route = database
            .create_model_route(CreateModelRouteInput {
                tenant_external_id: "oauth-reauthorize-test".to_owned(),
                public_model: "codex-public".to_owned(),
                upstream_account_id: account.id,
                upstream_model: "gpt-codex".to_owned(),
                protocol: "openai".to_owned(),
                priority: 0,
            })
            .await
            .expect("create stable route");
        let historical_request_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO request_records (id, tenant_id, key_id, created_at, protocol, model, input_tokens, output_tokens, cost_micros, request_object, reservation_id, upstream_account_id, model_route_id) VALUES ($1, $2, $3, $4, 'openai', 'codex-public', 1, 1, 0, '{}', $5, $6, $7)",
        )
        .bind(historical_request_id.to_string())
        .bind(account.tenant_id.to_string())
        .bind(Uuid::now_v7().to_string())
        .bind(unix_millis())
        .bind(Uuid::now_v7().to_string())
        .bind(account.id.to_string())
        .bind(route.id.to_string())
        .execute(&database.pool)
        .await
        .expect("insert historical request");

        let second_session = Uuid::now_v7();
        let updated = database
            .reauthorize_upstream_account(
                account.id,
                ReauthorizeUpstreamAccountInput {
                    tenant_external_id: "oauth-reauthorize-test".to_owned(),
                    expected_updated_at: account.updated_at,
                    driver: "openai-codex".to_owned(),
                    oauth_session_id: second_session,
                    oauth_driver: "openai_codex_device".to_owned(),
                    oauth_refresh_url: Some("https://auth.openai.com/oauth/token".to_owned()),
                    credential: oauth_credential("access-v2", "refresh-v2", 4_102_444_800_000),
                },
                pepper,
            )
            .await
            .expect("reauthorize stable Codex account");
        assert_eq!(updated.id, account.id);
        assert_eq!(updated.credential_generation, 2);
        let replay = database
            .reauthorize_upstream_account(
                account.id,
                ReauthorizeUpstreamAccountInput {
                    tenant_external_id: "oauth-reauthorize-test".to_owned(),
                    expected_updated_at: account.updated_at,
                    driver: "openai-codex".to_owned(),
                    oauth_session_id: second_session,
                    oauth_driver: "openai_codex_device".to_owned(),
                    oauth_refresh_url: Some("https://auth.openai.com/oauth/token".to_owned()),
                    credential: oauth_credential(
                        "must-not-install",
                        "must-not-install",
                        4_102_444_800_000,
                    ),
                },
                pepper,
            )
            .await
            .expect("replay completed reauthorization");
        assert_eq!(replay.id, account.id);
        assert_eq!(replay.credential_generation, 2);

        let routes = database
            .list_model_routes(Some("oauth-reauthorize-test"))
            .await
            .expect("list stable routes");
        assert!(routes.iter().any(
            |candidate| candidate.id == route.id && candidate.upstream_account_id == account.id
        ));
        let history = sqlx::query(
            "SELECT upstream_account_id, model_route_id FROM request_records WHERE id = $1",
        )
        .bind(historical_request_id.to_string())
        .fetch_one(&database.pool)
        .await
        .expect("read historical request");
        let account_id = account.id.to_string();
        let route_id = route.id.to_string();
        assert_eq!(
            history
                .try_get::<Option<String>, _>("upstream_account_id")
                .expect("historical account")
                .as_deref(),
            Some(account_id.as_str())
        );
        assert_eq!(
            history
                .try_get::<Option<String>, _>("model_route_id")
                .expect("historical route")
                .as_deref(),
            Some(route_id.as_str())
        );
        let generations = sqlx::query(
            "SELECT generation, revoked_at FROM upstream_credentials WHERE upstream_account_id = $1 ORDER BY generation",
        )
        .bind(account.id.to_string())
        .fetch_all(&database.pool)
        .await
        .expect("list credential generations");
        assert_eq!(generations.len(), 2);
        assert!(
            generations[0]
                .try_get::<Option<i64>, _>("revoked_at")
                .expect("old revoked_at")
                .is_some()
        );
        assert!(
            generations[1]
                .try_get::<Option<i64>, _>("revoked_at")
                .expect("new revoked_at")
                .is_none()
        );
    }

    fn oauth_credential(
        access_token: &str,
        refresh_token: &str,
        expires_at: i64,
    ) -> UpstreamCredential {
        UpstreamCredential::OAuth {
            access_token: access_token.to_owned(),
            refresh_token: Some(refresh_token.to_owned()),
            expires_at: Some(expires_at),
            header: "authorization".to_owned(),
            prefix: "Bearer ".to_owned(),
            adapter_state: Some(serde_json::json!({
                "schema": "openai-codex-oauth-v1",
                "account_id": "account-stable"
            })),
        }
    }
}
