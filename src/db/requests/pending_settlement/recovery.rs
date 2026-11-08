//! Exact-manifest, read-only planning. Deliberately has no apply entry point.
use std::{collections::HashSet, path::Path, time::Duration};

use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{AnyConnection, Connection, Row, any::AnyRow};
use tokio::io::AsyncReadExt;
use uuid::Uuid;

use super::{PendingProxySettlementClaim, contract, digest};
use crate::{
    archive::ArchiveStore,
    config::{ArchiveBackend, Config},
    db::{PendingProxyOperationalTerminal, ProxyRequestOwner},
    error::AppError,
    model::{EnforcementMode, ModelPrice, UsageReservation},
};

const MAX_REQUESTS: usize = 2;
const MAX_RESPONSE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    requests: Vec<Target>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    request_id: Uuid,
    tenant_id: Uuid,
    key_id: Uuid,
}

struct Material {
    pending: AnyRow,
    request: AnyRow,
    reservation: AnyRow,
    operational: AnyRow,
    spool: AnyRow,
}

/// A single physical connection; no Database constructor, migration, AppState,
/// workers, locks, readiness writes, upstream requests, or financial mutations.
/// The transaction ends before archive I/O. A plan is not an apply-time CAS.
pub async fn plan_pending_settlement_recovery(
    config: &Config,
    manifest_path: &Path,
) -> Result<Value, &'static str> {
    if config.archive_backend != ArchiveBackend::S3
        || !(config.database_url.starts_with("postgres://")
            || config.database_url.starts_with("postgresql://"))
    {
        return Err("plan_requires_postgres_and_s3");
    }
    let file = tokio::fs::File::open(manifest_path)
        .await
        .map_err(|_| "manifest_unreadable")?;
    let mut bytes = Vec::new();
    file.take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "manifest_unreadable")?;
    if bytes.len() > 16 * 1024 {
        return Err("manifest_too_large");
    }
    let manifest: Manifest = serde_json::from_slice(&bytes).map_err(|_| "manifest_invalid")?;
    let mut ids = HashSet::new();
    if manifest.version != 1
        || manifest.requests.is_empty()
        || manifest.requests.len() > MAX_REQUESTS
        || manifest.requests.iter().any(|target| {
            target.request_id.is_nil()
                || target.tenant_id.is_nil()
                || target.key_id.is_nil()
                || !ids.insert(target.request_id)
        })
    {
        return Err("manifest_requires_one_or_two_distinct_exact_identities");
    }
    sqlx::any::install_default_drivers();
    let mut connection = tokio::time::timeout(
        Duration::from_secs(15),
        AnyConnection::connect(&config.database_url),
    )
    .await
    .map_err(|_| "database_connect_timeout")?
    .map_err(|_| "database_connect_failed")?;
    let mut tx = connection
        .begin()
        .await
        .map_err(|_| "database_begin_failed")?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
        .execute(&mut *tx)
        .await
        .map_err(|_| "database_read_only_failed")?;
    sqlx::query("SET LOCAL statement_timeout = '10s'")
        .execute(&mut *tx)
        .await
        .map_err(|_| "database_timeout_setup_failed")?;
    let mut materials = Vec::new();
    for target in &manifest.requests {
        materials.push(load(&mut tx, target).await);
    }
    tx.rollback()
        .await
        .map_err(|_| "database_rollback_failed")?;
    connection
        .close()
        .await
        .map_err(|_| "database_close_failed")?;
    // S3 construction does not probe readiness or write an object.
    let archive = ArchiveStore::from_config(config)
        .await
        .map_err(|_| "archive_configuration_invalid")?;
    let mut results = Vec::new();
    for (target, material) in manifest.requests.iter().zip(materials) {
        let result = match material {
            Ok(material) => {
                match tokio::time::timeout(
                    Duration::from_secs(90),
                    verify(&archive, target, material),
                )
                .await
                {
                    Ok(result) => result,
                    Err(_) => Err("archive_verification_timeout"),
                }
            }
            Err(_) => Err("required_database_evidence_missing_or_unreadable"),
        };
        results.push(match result {
            Ok(result) => result,
            Err(reason) => json!({
                "request_id": target.request_id,
                "status": "blocked",
                "missing_evidence": [reason],
            }),
        });
    }
    Ok(json!({
        "version": 1,
        "read_only": true,
        "apply_available": false,
        "manifest_blake3": blake3::hash(&bytes).to_hex().to_string(),
        "limitations": [
            "database_snapshot_ended_before_archive_read_revalidate_before_any_apply",
            "owner_exit_not_proven_lease_expiry_does_not_fence_a_live_owner",
            "original_financial_routing_fields_not_reconstructed",
            "no_financial_receipt_created_or_settlement_performed"
        ],
        "requests": results,
    }))
}

async fn load(connection: &mut AnyConnection, target: &Target) -> Result<Material, AppError> {
    let request_id = target.request_id.to_string();
    let created_at: i64 = sqlx::query_scalar(
        "SELECT created_at FROM request_record_locators WHERE id=$1 AND tenant_id=$2 AND key_id=$3",
    )
    .bind(&request_id)
    .bind(target.tenant_id.to_string())
    .bind(target.key_id.to_string())
    .fetch_optional(&mut *connection)
    .await?
    .ok_or(AppError::NotFound)?;
    let request = sqlx::query(
        "SELECT reservation_id,proxy_owner_token,proxy_owner_expires_at,protocol,response_object,upstream_account_id,model_route_id,upstream_model FROM request_records WHERE id=$1 AND created_at=$2 AND tenant_id=$3 AND key_id=$4",
    )
    .bind(&request_id)
    .bind(created_at)
    .bind(target.tenant_id.to_string())
    .bind(target.key_id.to_string())
    .fetch_optional(&mut *connection).await?.ok_or(AppError::NotFound)?;
    let pending = sqlx::query(
        "SELECT * FROM pending_proxy_settlements WHERE request_id=$1 AND tenant_id=$2 AND key_id=$3",
    )
    .bind(&request_id)
    .bind(target.tenant_id.to_string())
    .bind(target.key_id.to_string())
    .fetch_optional(&mut *connection).await?.ok_or(AppError::NotFound)?;
    let reservation = sqlx::query(
        "SELECT r.*,q.upstream_account_id,q.model_route_id,q.upstream_model FROM usage_reservations r JOIN request_records q ON q.reservation_id=r.id WHERE r.id=$1 AND r.key_id=$2 AND q.id=$3 AND q.created_at=$4",
    )
    .bind(pending.try_get::<String, _>("reservation_id")?)
    .bind(target.key_id.to_string())
    .bind(&request_id)
    .bind(created_at)
    .fetch_optional(&mut *connection).await?.ok_or(AppError::NotFound)?;
    let operational = sqlx::query("SELECT * FROM proxy_operational_terminals WHERE request_id=$1")
        .bind(&request_id)
        .fetch_optional(&mut *connection)
        .await?
        .ok_or(AppError::NotFound)?;
    let spool = sqlx::query("SELECT tenant_id,reservation_id,state,bound_locator,byte_count,body_blake3 FROM response_archive_spools WHERE request_id=$1")
        .bind(&request_id)
        .fetch_optional(&mut *connection).await?.ok_or(AppError::NotFound)?;
    Ok(Material {
        pending,
        request,
        reservation,
        operational,
        spool,
    })
}

fn text(row: &AnyRow, name: &str) -> Result<String, &'static str> {
    row.try_get(name).map_err(|_| "required_text_field_missing")
}

fn optional_text(row: &AnyRow, name: &str) -> Result<Option<String>, &'static str> {
    row.try_get(name)
        .map_err(|_| "optional_text_field_unreadable")
}

fn number(row: &AnyRow, name: &str) -> Result<i64, &'static str> {
    row.try_get(name)
        .map_err(|_| "required_numeric_field_missing")
}

fn optional_number(row: &AnyRow, name: &str) -> Result<Option<i64>, &'static str> {
    row.try_get(name)
        .map_err(|_| "optional_numeric_field_unreadable")
}

fn id(row: &AnyRow, name: &str) -> Result<Uuid, &'static str> {
    Uuid::parse_str(&text(row, name)?).map_err(|_| "stored_identity_invalid")
}

async fn verify(
    archive: &ArchiveStore,
    target: &Target,
    material: Material,
) -> Result<Value, &'static str> {
    let Material {
        pending,
        request,
        reservation,
        operational,
        spool,
    } = material;
    if text(&pending, "state")? != "pending"
        || optional_text(&pending, "receipt_id")?.is_some()
        || optional_text(&pending, "receipt_digest")?.is_some()
        || text(&reservation, "status")? != "reserved"
    {
        return Err("financial_state_changed_or_receipt_already_present");
    }
    let contract_json: Value = serde_json::from_str(&text(&pending, "contract_json")?)
        .map_err(|_| "contract_json_invalid")?;
    let claim = PendingProxySettlementClaim {
        request_id: target.request_id,
        tenant_id: target.tenant_id,
        key_id: target.key_id,
        account_id: id(&pending, "account_id")?,
        reservation_id: id(&pending, "reservation_id")?,
        owner_token: id(&pending, "owner_token")?,
        fence: id(&pending, "fence")?,
        contract_digest: text(&pending, "contract_digest")?,
        input_token_ceiling: contract_json["input_ceiling"]
            .as_i64()
            .ok_or("input_ceiling_missing")?,
        output_token_ceiling: contract_json["output_ceiling"]
            .as_i64()
            .ok_or("output_ceiling_missing")?,
        requested_service_tier: contract_json["tier"]
            .as_str()
            .ok_or("requested_tier_missing")?
            .to_owned(),
    };
    if id(&request, "reservation_id")? != claim.reservation_id
        || id(&request, "proxy_owner_token")? != claim.owner_token
        || id(&reservation, "account_id")? != claim.account_id
        || text(&request, "protocol")? != "openai"
    {
        return Err("request_contract_identity_mismatch");
    }
    let owner = ProxyRequestOwner {
        request_id: claim.request_id,
        tenant_id: claim.tenant_id,
        key_id: claim.key_id,
        reservation_id: claim.reservation_id,
        token: claim.owner_token,
    };
    let reconstructed = contract(
        &owner,
        &reservation,
        claim.input_token_ceiling,
        claim.output_token_ceiling,
        &claim.requested_service_tier,
    )
    .map_err(|_| "reservation_contract_invalid")?;
    if digest(&contract_json).map_err(|_| "contract_digest_failed")? != claim.contract_digest
        || digest(&reconstructed).map_err(|_| "contract_digest_failed")? != claim.contract_digest
    {
        return Err("reservation_contract_digest_mismatch");
    }
    for (field, expected) in [
        ("tenant_id", claim.tenant_id),
        ("key_id", claim.key_id),
        ("account_id", claim.account_id),
        ("reservation_id", claim.reservation_id),
        ("owner_token", claim.owner_token),
        ("fence", claim.fence),
    ] {
        if id(&operational, field)? != expected {
            return Err("operational_claim_identity_mismatch");
        }
    }
    if text(&operational, "contract_digest")? != claim.contract_digest {
        return Err("operational_contract_digest_mismatch");
    }
    let receipt_id = id(&operational, "receipt_id")?;
    let terminal = PendingProxyOperationalTerminal {
        status_code: number(&operational, "status_code")?,
        error_code: optional_text(&operational, "error_code")?,
        terminal_cause: optional_text(&operational, "terminal_cause_code")?,
        stage: optional_text(&operational, "stage")?,
        completed_at: number(&operational, "completed_at")?,
        duration_ms: number(&operational, "duration_ms")?,
        first_output_ms: optional_number(&operational, "first_output_ms")?,
        generation_duration_ms: optional_number(&operational, "generation_duration_ms")?,
        response_object: text(&operational, "response_object")?,
    };
    if terminal.status_code != 200
        || terminal.error_code.is_some()
        || terminal.terminal_cause.is_some()
    {
        return Err("operational_terminal_not_successful");
    }
    let mut attributions = vec![
        json!({"kind":"keep_selected"}),
        json!({"kind":"last_dispatched","ids":null}),
    ];
    if let (Some(account), Some(route)) = (
        optional_text(&operational, "upstream_account_id")?,
        optional_text(&operational, "model_route_id")?,
    ) {
        let pair = (
            Uuid::parse_str(&account).map_err(|_| "upstream_identity_invalid")?,
            Uuid::parse_str(&route).map_err(|_| "route_identity_invalid")?,
        );
        attributions.push(json!({"kind":"last_dispatched","ids":pair}));
    }
    let operational_digest = text(&operational, "operational_digest")?;
    let mut matching = Vec::new();
    for attribution in attributions {
        if digest(&json!({
            "version":1, "claim":claim, "receipt_id":receipt_id,
            "terminal":terminal, "attribution":attribution
        }))
        .map_err(|_| "operational_digest_failed")?
            == operational_digest
        {
            matching.push(attribution);
        }
    }
    if matching.len() != 1 {
        return Err("original_attribution_not_uniquely_verified");
    }
    if id(&spool, "tenant_id")? != target.tenant_id
        || id(&spool, "reservation_id")? != claim.reservation_id
        || text(&spool, "state")? != "bound"
    {
        return Err("response_source_not_bound_to_original_reservation");
    }
    let location = text(&spool, "bound_locator")?;
    if location != text(&request, "response_object")? || location.contains("://") {
        return Err("response_source_locator_mismatch_or_gap");
    }
    let body = archive
        .get_bounded(&location, MAX_RESPONSE_BYTES)
        .await
        .map_err(|_| "response_archive_unreadable_or_oversize")?;
    if usize::try_from(number(&spool, "byte_count")?).ok() != Some(body.len()) {
        return Err("response_archive_plain_length_mismatch");
    }
    let body_digest = blake3::hash(&body).to_hex().to_string();
    if crate::archive::is_tenant_cas_location(target.tenant_id, &location)
        && location
            .rsplit('/')
            .next()
            .map(|name| name.strip_suffix(".mtcz1").unwrap_or(name))
            != Some(body_digest.as_str())
    {
        return Err("response_archive_content_digest_mismatch");
    }
    if optional_text(&spool, "body_blake3")?.is_some_and(|expected| expected != body_digest) {
        return Err("response_spool_content_digest_mismatch");
    }
    let reported = crate::api::settlement_recovery_usage::parse(&body)?;
    let price_json = text(&reservation, "price_snapshot_json")?;
    let price: ModelPrice =
        serde_json::from_str(&price_json).map_err(|_| "price_snapshot_invalid")?;
    let output_limit = super::super::lifecycle::output_token_limit_from_snapshot(
        Some(&price_json),
        claim.output_token_ceiling,
    )
    .map_err(|_| "original_output_limit_invalid")?;
    let usage = crate::db::normalize_proxy_usage_with_output_limit(
        &reported,
        claim.input_token_ceiling,
        output_limit,
        Some(&claim.requested_service_tier),
    )
    .map_err(|_| "provider_usage_rejected_by_original_settlement_contract")?;
    let usage_reservation = UsageReservation {
        id: claim.reservation_id,
        account_id: claim.account_id,
        key_id: claim.key_id,
        enforcement_mode: EnforcementMode::from_storage(&text(&reservation, "enforcement_mode")?)
            .ok_or("enforcement_mode_invalid")?,
        reserved_micros: number(&reservation, "reserved_micros")?,
        input_micros_per_million: price.input_micros_per_million,
        output_micros_per_million: price.output_micros_per_million,
        price_tiers: price.tiers,
        rate_window_start: number(&reservation, "rate_window_start")?,
        reserved_tokens: number(&reservation, "reserved_tokens")?,
    };
    let cost = crate::db::price_token_usage(&usage_reservation, &usage)
        .map_err(|_| "original_price_calculation_failed")?;
    Ok(json!({
        "request_id": target.request_id, "tenant_id": target.tenant_id, "key_id": target.key_id,
        "reservation_id": claim.reservation_id, "receipt_id": receipt_id,
        "status": "evidence_verified_apply_blocked",
        "contract_digest": claim.contract_digest, "operational_digest": operational_digest,
        "response_plaintext_blake3": body_digest,
        "price_snapshot_blake3": blake3::hash(price_json.as_bytes()).to_hex().to_string(),
        "verified_attribution": matching[0],
        "source_binding_verified": true,
        "usage": {
            "input_tokens": usage.input_tokens, "cached_input_tokens": usage.cached_input_tokens,
            "cache_write_tokens": usage.cache_write_tokens, "output_tokens": usage.output_tokens,
            "total_input_tokens": usage.total_input_tokens(),
            "reported_service_tier": reported.service_tier, "settlement_service_tier": usage.service_tier,
            "cache_coverage": usage.cache_coverage.map(|c| json!({
                "read_tokens":c.read_tokens,"inclusive_input_tokens":c.inclusive_input_tokens
            })),
        },
        "snapshot_priced_cost_micros": cost,
        "missing_evidence": ["original_owner_process_exit", "original_financial_routing_fields"],
    }))
}
