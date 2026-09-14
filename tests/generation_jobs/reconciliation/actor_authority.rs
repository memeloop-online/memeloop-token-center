use super::*;
use memeloop_token_center::db::ResolveGenerationQuarantine;

#[tokio::test]
async fn async_quarantine_revalidates_current_tenant_service_for_resolution_and_replay() {
    for replay in [false, true] {
        for mutation in [
            "principal_revoked",
            "credential_revoked",
            "scope_removed",
            "wildcard_only",
            "mixed_wildcard",
            "unknown_scope",
            "tenant_archived",
            "global",
            "foreign_tenant",
            "rotation",
        ] {
            let f = ReconcileFixture::new().await;
            f.expire().await;
            let actor: String = sqlx::query_scalar(
                "SELECT id FROM service_principals WHERE name = 'tenant-reconciler'",
            )
            .fetch_one(&f.pool)
            .await
            .unwrap();
            let actor = Uuid::parse_str(&actor).unwrap();
            let revision = f
                .state
                .db
                .generation_quarantine(&f.tenant, f.job)
                .await
                .unwrap()
                .revision;
            let input = || ResolveGenerationQuarantine {
                tenant_external_id: &f.tenant,
                job_id: f.job,
                actor_service_id: actor,
                actor_credential_generation: 1,
                idempotency_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                expected_revision: &revision,
                action: "confirmed_submitted",
                upstream_job_id: Some("provider-confirmed"),
                evidence_digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            };
            if replay {
                f.state
                    .db
                    .resolve_generation_quarantine(input())
                    .await
                    .unwrap();
            }
            let before = sqlx::query(
                "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
            )
            .bind(f.key.account_id.to_string())
            .fetch_one(&f.pool)
            .await
            .unwrap();
            let balance_before = (
                before.get::<i64, _>("available_micros"),
                before.get::<i64, _>("reserved_micros"),
            );
            match mutation {
                "mixed_wildcard" => {
                    sqlx::query("UPDATE service_credentials SET scopes_json = '[\"generations:reconcile\",\"*\"]' WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                "unknown_scope" => {
                    sqlx::query("UPDATE service_credentials SET scopes_json = '[\"generations:reconcile\",\"unknown:scope\"]' WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                "tenant_archived" => {
                    f.state
                        .db
                        .set_tenant_archived(&f.tenant, true, None)
                        .await
                        .unwrap();
                }
                "wildcard_only" => {
                    sqlx::query("UPDATE service_credentials SET scopes_json = '[\"*\"]' WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                "principal_revoked" => {
                    f.state
                        .db
                        .set_service_token_status(actor, "revoked")
                        .await
                        .unwrap();
                }
                "credential_revoked" => {
                    sqlx::query("UPDATE service_credentials SET revoked_at = 1 WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                "scope_removed" => {
                    sqlx::query("UPDATE service_credentials SET scopes_json = '[]' WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                "global" => {
                    sqlx::query("UPDATE service_credentials SET tenant_external_id = NULL WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                "foreign_tenant" => {
                    sqlx::query("UPDATE service_credentials SET tenant_external_id = 'different-tenant' WHERE service_principal_id = $1").bind(actor.to_string()).execute(&f.pool).await.unwrap();
                }
                _ => {
                    let rotated = f
                        .state
                        .db
                        .rotate_service_token(actor, "rotate-actor", PEPPER)
                        .await
                        .unwrap();
                    sqlx::query("UPDATE service_credentials SET scopes_json = '[]' WHERE service_principal_id = $1 AND generation = $2").bind(actor.to_string()).bind(rotated.credential_generation).execute(&f.pool).await.unwrap();
                }
            }
            assert!(
                matches!(
                    f.state.db.resolve_generation_quarantine(input()).await,
                    Err(memeloop_token_center::error::AppError::Forbidden)
                ),
                "{mutation}, replay={replay}, must reject at the database boundary"
            );
            if mutation == "rotation" {
                let mut fresh = input();
                fresh.actor_credential_generation = 2;
                assert!(
                    matches!(
                        f.state.db.resolve_generation_quarantine(fresh).await,
                        Err(memeloop_token_center::error::AppError::Forbidden)
                    ),
                    "current rotated credential lost its scope too"
                );
            }
            let receipts: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM generation_quarantine_resolutions WHERE job_id = $1",
            )
            .bind(f.job.to_string())
            .fetch_one(&f.pool)
            .await
            .unwrap();
            assert_eq!(receipts, i64::from(replay));
            let status: String =
                sqlx::query_scalar("SELECT status FROM generation_jobs WHERE id = $1")
                    .bind(f.job.to_string())
                    .fetch_one(&f.pool)
                    .await
                    .unwrap();
            assert_eq!(status, if replay { "running" } else { "submitting" });
            let after = sqlx::query(
                "SELECT available_micros, reserved_micros FROM credit_accounts WHERE id = $1",
            )
            .bind(f.key.account_id.to_string())
            .fetch_one(&f.pool)
            .await
            .unwrap();
            assert_eq!(
                (
                    after.get::<i64, _>("available_micros"),
                    after.get::<i64, _>("reserved_micros")
                ),
                balance_before
            );
        }
    }
}

#[tokio::test]
async fn async_rotation_with_unchanged_authority_fences_captured_old_generation_but_preserves_fresh_replay()
 {
    for replay in [false, true] {
        let f = ReconcileFixture::new().await;
        f.expire().await;
        let actor: String = sqlx::query_scalar(
            "SELECT id FROM service_principals WHERE name = 'tenant-reconciler'",
        )
        .fetch_one(&f.pool)
        .await
        .unwrap();
        let actor = Uuid::parse_str(&actor).unwrap();
        let revision = f
            .state
            .db
            .generation_quarantine(&f.tenant, f.job)
            .await
            .unwrap()
            .revision;
        let input = |generation| ResolveGenerationQuarantine {
            tenant_external_id: &f.tenant,
            job_id: f.job,
            actor_service_id: actor,
            actor_credential_generation: generation,
            idempotency_hash: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            expected_revision: &revision,
            action: "confirmed_submitted",
            upstream_job_id: Some("provider-confirmed"),
            evidence_digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        };
        let receipt = if replay {
            Some(
                f.state
                    .db
                    .resolve_generation_quarantine(input(1))
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        let rotated = f
            .state
            .db
            .rotate_service_token(actor, "same-scopes-rotation", PEPPER)
            .await
            .unwrap();
        assert_eq!(rotated.credential_generation, 2);
        assert!(matches!(
            f.state.db.resolve_generation_quarantine(input(1)).await,
            Err(memeloop_token_center::error::AppError::Forbidden)
        ));
        let resolved = f
            .state
            .db
            .resolve_generation_quarantine(input(2))
            .await
            .unwrap();
        if let Some(receipt) = receipt {
            assert_eq!(resolved, receipt);
        }
        let receipts: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM generation_quarantine_resolutions WHERE job_id = $1",
        )
        .bind(f.job.to_string())
        .fetch_one(&f.pool)
        .await
        .unwrap();
        assert_eq!(receipts, 1);
        let audited_generation: i64 = sqlx::query_scalar("SELECT actor_credential_generation FROM generation_quarantine_resolutions WHERE job_id = $1")
            .bind(f.job.to_string()).fetch_one(&f.pool).await.unwrap();
        assert_eq!(
            audited_generation,
            if replay { 1 } else { 2 },
            "fresh replay retains the original authorizing credential generation"
        );
    }
}
