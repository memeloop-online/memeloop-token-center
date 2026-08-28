use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use sqlx::{Row, any::AnyRow};
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid};
use crate::provider::{ResolvedUpstream, open_credential, validate_config};

use super::types::{GrantedModelCapabilitySource, RouteSelectionOptions};

impl Database {
    pub async fn reload_persisted_generation_upstream(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        upstream_account_id: Uuid,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let row = sqlx::query(
            "SELECT r.id AS route_id, candidate.upstream_model, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidate
               ON candidate.tenant_id = r.tenant_id AND candidate.model_route_id = r.id
             JOIN upstream_accounts a ON a.tenant_id = r.tenant_id AND a.id = candidate.upstream_account_id AND a.status = 'active'
             JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = 'generation'
               AND candidate.upstream_account_id = $3
             ORDER BY r.priority, r.id LIMIT 1",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(upstream_account_id.to_string())
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
            config,
            upstream_model: row.try_get("upstream_model")?,
            credential: open_credential(&ciphertext, key_material)?,
        }))
    }

    /// Resolves exact-route and route-group grants only. Credential groups are
    /// presentation metadata and are intentionally absent from this query.
    pub async fn resolve_authorized_upstream_with_hint(
        &self,
        key_id: Uuid,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        selection: RouteSelectionOptions,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        Ok(self
            .resolve_authorized_upstream_candidates_with_hint(
                key_id,
                tenant_id,
                public_model,
                protocol,
                selection,
                key_material,
            )
            .await?
            .into_iter()
            .next())
    }

    /// Returns every currently authorized candidate in deterministic failover
    /// order. Lower route priorities are exhausted first; candidates at the
    /// same priority use weighted rendezvous ordering so a stable selection
    /// seed remains sticky while the candidate set is unchanged.
    pub async fn resolve_authorized_upstream_candidates_with_hint(
        &self,
        key_id: Uuid,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        selection: RouteSelectionOptions,
        key_material: &[u8],
    ) -> Result<Vec<ResolvedUpstream>, AppError> {
        let RouteSelectionOptions {
            upstream_account_hint,
            selection_seed,
        } = selection;
        let rows = sqlx::query(
             "SELECT r.id AS route_id, r.priority, candidates.upstream_model, candidates.scheduling_weight, a.id AS account_id, a.driver, a.config_json, c.credential_ciphertext
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidates
               ON candidates.tenant_id = r.tenant_id AND candidates.model_route_id = r.id
             JOIN upstream_accounts a ON a.id = candidates.upstream_account_id AND a.tenant_id = r.tenant_id
             JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3
               AND r.enabled = 1 AND a.status = 'active'
               AND ($4 = '' OR a.id = $4)
               AND (
                 EXISTS (SELECT 1 FROM routing_grants g WHERE g.tenant_id = r.tenant_id AND g.key_id = $5 AND g.model_route_id = r.id)
                 OR EXISTS (
                   SELECT 1 FROM routing_grants g
                   JOIN model_route_group_memberships membership
                     ON membership.tenant_id = g.tenant_id AND membership.route_group_id = g.route_group_id
                   WHERE g.tenant_id = r.tenant_id AND g.key_id = $5
                     AND g.route_group_id IS NOT NULL AND membership.model_route_id = r.id
                 )
               )
             ORDER BY r.priority ASC, r.id ASC, a.id ASC
             LIMIT 1001",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(protocol)
        .bind(
            upstream_account_hint
                .map(|id| id.to_string())
                .unwrap_or_default(),
        )
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > 1000 {
            return Err(AppError::BadRequest(
                "authorized routing candidate set exceeds the safety limit".into(),
            ));
        }
        let mut candidates = BTreeMap::<(Uuid, Uuid), RoutingCandidate>::new();
        for row in rows {
            let candidate = RoutingCandidate::from_row(row)?;
            let key = (candidate.route_id, candidate.account_id);
            match candidates.get(&key) {
                Some(existing) if existing.scheduling_weight >= candidate.scheduling_weight => {}
                _ => {
                    candidates.insert(key, candidate);
                }
            }
        }
        let mut candidates = candidates.into_values().collect::<Vec<_>>();
        candidates.sort_by(|left, right| {
            left.priority
                .cmp(&right.priority)
                .then_with(|| {
                    weighted_rendezvous_score(key_id, selection_seed, left)
                        .total_cmp(&weighted_rendezvous_score(key_id, selection_seed, right))
                })
                .then_with(|| left.route_id.cmp(&right.route_id))
                .then_with(|| left.account_id.cmp(&right.account_id))
        });
        candidates
            .into_iter()
            .map(|candidate| {
                let config: serde_json::Value =
                    serde_json::from_str(&candidate.config_json).map_err(|_| AppError::Internal)?;
                let base_url = validate_config(&config)?;
                Ok(ResolvedUpstream {
                    route_id: candidate.route_id,
                    account_id: candidate.account_id,
                    driver: candidate.driver,
                    base_url,
                    config,
                    upstream_model: candidate.upstream_model,
                    credential: open_credential(&candidate.credential_ciphertext, key_material)?,
                })
            })
            .collect()
    }

    pub async fn granted_available_models(
        &self,
        key_id: Uuid,
        tenant_id: Uuid,
    ) -> Result<Vec<String>, AppError> {
        let rows = sqlx::query(
            "SELECT DISTINCT r.public_model AS model
             FROM model_routes r
             WHERE r.tenant_id = $1 AND r.enabled = 1
               AND (
                 EXISTS (SELECT 1 FROM routing_grants g WHERE g.tenant_id = r.tenant_id AND g.key_id = $2 AND g.model_route_id = r.id)
                 OR EXISTS (
                   SELECT 1 FROM routing_grants g
                   JOIN model_route_group_memberships membership
                     ON membership.tenant_id = g.tenant_id AND membership.route_group_id = g.route_group_id
                   WHERE g.tenant_id = r.tenant_id AND g.key_id = $2
                     AND g.route_group_id IS NOT NULL AND membership.model_route_id = r.id
                 )
               )
               AND EXISTS (
                 SELECT 1 FROM model_route_eligible_upstream_accounts candidate
                 JOIN upstream_accounts account ON account.id = candidate.upstream_account_id AND account.tenant_id = r.tenant_id AND account.status = 'active'
                 WHERE candidate.tenant_id = r.tenant_id AND candidate.model_route_id = r.id
               )
             ORDER BY model",
        )
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        rows.into_iter()
            .map(|row| row.try_get::<String, _>("model").map_err(AppError::from))
            .collect()
    }

    /// Returns only candidates that can be selected right now by the normal
    /// exact-route or route-group authorization path.
    pub async fn granted_model_capability_sources(
        &self,
        key_id: Uuid,
        tenant_id: Uuid,
    ) -> Result<Vec<GrantedModelCapabilitySource>, AppError> {
        let rows = sqlx::query(
            "SELECT DISTINCT r.public_model, candidate.upstream_model, r.protocol, account.driver, account.config_json
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidate
               ON candidate.tenant_id = r.tenant_id AND candidate.model_route_id = r.id
             JOIN upstream_accounts account
               ON account.tenant_id = r.tenant_id AND account.id = candidate.upstream_account_id
              AND account.status = 'active'
             JOIN upstream_credentials credential
               ON credential.upstream_account_id = account.id
              AND credential.generation = account.credential_generation
              AND credential.revoked_at IS NULL
             WHERE r.tenant_id = $1 AND r.enabled = 1
               AND (
                 EXISTS (SELECT 1 FROM routing_grants g WHERE g.tenant_id = r.tenant_id AND g.key_id = $2 AND g.model_route_id = r.id)
                 OR EXISTS (
                   SELECT 1 FROM routing_grants g
                   JOIN model_route_group_memberships membership
                     ON membership.tenant_id = g.tenant_id AND membership.route_group_id = g.route_group_id
                   WHERE g.tenant_id = r.tenant_id AND g.key_id = $2
                     AND g.route_group_id IS NOT NULL AND membership.model_route_id = r.id
                 )
               )
             ORDER BY r.public_model, candidate.upstream_model, r.protocol, account.driver, account.config_json
             LIMIT 1001",
        )
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > 1000 {
            return Err(AppError::BadRequest(
                "authorized model capability set exceeds the safety limit".into(),
            ));
        }
        rows.into_iter()
            .map(|row| {
                Ok(GrantedModelCapabilitySource {
                    public_model: row.try_get("public_model")?,
                    upstream_model: row.try_get("upstream_model")?,
                    protocol: row.try_get("protocol")?,
                    driver: row.try_get("driver")?,
                    config_json: row.try_get("config_json")?,
                })
            })
            .collect()
    }

    pub async fn credential_has_available_route(
        &self,
        key_id: Uuid,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
    ) -> Result<bool, AppError> {
        let found = sqlx::query(
            "SELECT r.id
             FROM model_routes r
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.enabled = 1
               AND (
                 EXISTS (SELECT 1 FROM routing_grants g WHERE g.tenant_id = r.tenant_id AND g.key_id = $4 AND g.model_route_id = r.id)
                 OR EXISTS (
                   SELECT 1 FROM routing_grants g
                   JOIN model_route_group_memberships membership
                     ON membership.tenant_id = g.tenant_id AND membership.route_group_id = g.route_group_id
                   WHERE g.tenant_id = r.tenant_id AND g.key_id = $4
                     AND g.route_group_id IS NOT NULL AND membership.model_route_id = r.id
                 )
               )
               AND EXISTS (
                 SELECT 1 FROM model_route_eligible_upstream_accounts candidate
                 JOIN upstream_accounts account ON account.id = candidate.upstream_account_id AND account.tenant_id = r.tenant_id AND account.status = 'active'
                 WHERE candidate.tenant_id = r.tenant_id AND candidate.model_route_id = r.id
               )
             LIMIT 1",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(protocol)
        .bind(key_id.to_string())
        .fetch_optional(&self.pool)
        .await?;
        Ok(found.is_some())
    }
}

struct RoutingCandidate {
    route_id: Uuid,
    account_id: Uuid,
    priority: i64,
    scheduling_weight: i64,
    upstream_model: String,
    driver: String,
    config_json: String,
    credential_ciphertext: String,
}

impl RoutingCandidate {
    fn from_row(row: AnyRow) -> Result<Self, AppError> {
        let scheduling_weight: i64 = row.try_get("scheduling_weight")?;
        if !(1..=1_000_000).contains(&scheduling_weight) {
            return Err(AppError::Internal);
        }
        Ok(Self {
            route_id: parse_uuid(row.try_get("route_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            priority: row.try_get("priority")?,
            scheduling_weight,
            upstream_model: row.try_get("upstream_model")?,
            driver: row.try_get("driver")?,
            config_json: row.try_get("config_json")?,
            credential_ciphertext: row.try_get("credential_ciphertext")?,
        })
    }
}

fn weighted_rendezvous_score(
    key_id: Uuid,
    selection_seed: Uuid,
    candidate: &RoutingCandidate,
) -> f64 {
    let mut digest = Sha256::new();
    digest.update(b"memeloop-routing-rendezvous-v1");
    digest.update(key_id.as_bytes());
    digest.update(selection_seed.as_bytes());
    digest.update(candidate.route_id.as_bytes());
    digest.update(candidate.account_id.as_bytes());
    let output = digest.finalize();
    let mut first = [0_u8; 8];
    first.copy_from_slice(&output[..8]);
    let hash = u64::from_be_bytes(first);
    let uniform = (hash as f64 + 1.0) / (u64::MAX as f64 + 2.0);
    -uniform.ln() / candidate.scheduling_weight as f64
}
