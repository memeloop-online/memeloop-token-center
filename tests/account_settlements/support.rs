use super::helpers::{create_text_settlement, create_text_settlement_with_id, service_token};
use memeloop_token_center::{
    AppState,
    config::Config,
    db::{CreateGenerationJobInput, CreateKeyInput, FinishGenerationJobInput},
    model::{ArchivedGenerationAsset, AuthenticatedKey, KeyPolicy},
};
use rust_decimal::Decimal;
use tempfile::TempDir;
use uuid::Uuid;
const PEPPER: &[u8] = b"account settlement feed test pepper is long enough";

pub(super) struct Fixture {
    _directory: TempDir,
    pub(super) state: AppState,
    pub(super) target: AuthenticatedKey,
    pub(super) other: AuthenticatedKey,
    pub(super) target_token: String,
    pub(super) other_token: String,
    pub(super) credits_only_token: String,
    pub(super) requests_only_token: String,
    pub(super) text_request_id: Uuid,
    pub(super) other_text_request_id: Uuid,
    pub(super) generation_request_id: Uuid,
}

impl Fixture {
    pub(super) async fn new() -> Self {
        let directory = tempfile::tempdir().expect("settlement fixture directory");
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("account-settlements.db").display()
        );
        let mut config = Config::for_test(database_url.clone());
        config.key_pepper = String::from_utf8(PEPPER.to_vec()).expect("UTF-8 pepper");
        let state = AppState::initialize(config)
            .await
            .expect("initialize settlement fixture");

        let unique = Uuid::now_v7();
        let target_tenant = format!("settlement-feed-a-{unique}");
        let other_tenant = format!("settlement-feed-b-{unique}");
        let issue = |tenant: String, principal: &str, alias: &str| CreateKeyInput {
            tenant_external_id: tenant,
            principal_external_id: principal.to_owned(),
            alias: alias.to_owned(),
            currency: "USD".to_owned(),
            policy: KeyPolicy {
                allowed_models: vec!["*".to_owned()],
                ..KeyPolicy::default()
            },
            initial_balance: Decimal::from(100),
            idempotency_key: None,
        };
        let target_issued = state
            .db
            .create_key(
                issue(target_tenant.clone(), "settlement-member-a", "target"),
                PEPPER,
            )
            .await
            .expect("create target key");
        let other_issued = state
            .db
            .create_key(
                issue(other_tenant.clone(), "settlement-member-b", "other"),
                PEPPER,
            )
            .await
            .expect("create other key");
        let target = state
            .db
            .authenticate_key(&target_issued.key, PEPPER)
            .await
            .expect("authenticate target key");
        let other = state
            .db
            .authenticate_key(&other_issued.key, PEPPER)
            .await
            .expect("authenticate other key");

        let target_token = service_token(
            &state,
            "settlement-target-reader",
            vec!["credits:read", "requests:read"],
            Some(target_tenant.clone()),
        )
        .await;
        let other_token = service_token(
            &state,
            "settlement-other-reader",
            vec!["credits:read", "requests:read"],
            Some(other_tenant),
        )
        .await;
        let credits_only_token = service_token(
            &state,
            "settlement-credits-only",
            vec!["credits:read"],
            Some(target_tenant.clone()),
        )
        .await;
        let requests_only_token = service_token(
            &state,
            "settlement-requests-only",
            vec!["requests:read"],
            Some(target_tenant),
        )
        .await;

        let text_model = format!("settlement-text-{unique}");
        let text_price = state
            .db
            .upsert_model_price(
                &text_model,
                "USD",
                Decimal::from(1_000_000),
                Decimal::from(1_000_000),
            )
            .await
            .expect("store text price");
        let text_request_id = Uuid::now_v7();
        create_text_settlement_with_id(
            &state,
            &target,
            &text_price,
            &text_model,
            "target-request-body-sentinel",
            "target-response-body-sentinel",
            text_request_id,
        )
        .await;
        let other_text_request_id = create_text_settlement(
            &state,
            &other,
            &text_price,
            &text_model,
            "other-request-body-sentinel",
            "other-response-body-sentinel",
        )
        .await;

        // The generation API's worker/archive lifecycle is intentionally kept
        // out of this read-model test. A real reservation, generation job,
        // terminal ledger entry, and feed publication are still produced by
        // the same DB APIs used by production; no upstream HTTP is involved.
        let generation_model = format!("settlement-generation-{unique}");
        let generation_price = state
            .db
            .upsert_generation_price(&generation_model, "USD", "job", Decimal::from(2))
            .await
            .expect("store generation price");
        let generation_reservation_price = generation_price
            .reservation_price()
            .expect("generation reservation price");
        let generation_reservation = state
            .db
            .reserve_usage(&target, &generation_reservation_price, 0, 1)
            .await
            .expect("reserve generation usage");
        let generation_request_id = text_request_id;
        state
            .db
            .create_generation_job(CreateGenerationJobInput {
                job_id: generation_request_id,
                key: target.clone(),
                upstream_account_id: Uuid::now_v7(),
                reservation: generation_reservation,
                public_model: generation_model,
                upstream_model: "fixture-workflow".to_owned(),
                driver: "comfyui".to_owned(),
                request_object: "generation-request-body-sentinel".to_owned(),
                estimated_units: 1,
                billing_unit: generation_price.billing_unit.clone(),
                micros_per_unit: generation_price.micros_per_unit,
            })
            .await
            .expect("create generation job");
        let claimed = state
            .db
            .claim_generation_job("settlement-feed-test-worker")
            .await
            .expect("claim generation job")
            .expect("queued generation job");
        assert_eq!(claimed.job_id, generation_request_id);
        let asset = ArchivedGenerationAsset {
            asset_id: Uuid::now_v7(),
            index: 0,
            object_locator: format!("objects/blake3/{generation_request_id}-0"),
            mime_type: "image/png".to_owned(),
            size_bytes: 1,
            filename: "fixture.png".to_owned(),
        };
        state
            .db
            .finish_generation_job(FinishGenerationJobInput {
                job_id: generation_request_id,
                worker_id: "settlement-feed-test-worker",
                status: "succeeded",
                billed_units: 1,
                error_code: None,
                assets: std::slice::from_ref(&asset),
                staged_assets: None,
            })
            .await
            .expect("finish generation job");

        Self {
            _directory: directory,
            state,
            target,
            other,
            target_token,
            other_token,
            credits_only_token,
            requests_only_token,
            text_request_id,
            other_text_request_id,
            generation_request_id,
        }
    }
}
