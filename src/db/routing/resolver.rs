use std::collections::BTreeMap;

use sha2::{Digest, Sha256};
use sqlx::{Row, any::AnyRow};
use uuid::Uuid;

use super::super::{AppError, Database, parse_uuid, unix_millis};
use crate::provider::{
    AuthorizedUpstreamCandidate, PROXY_ROUTING_POLICY, ResolvedUpstream, UpstreamTransportSnapshot,
    open_credential, validate_config,
};

use super::types::{GrantedModelCapabilitySource, RouteSelectionOptions};

impl Database {
    /// Reloads one coherent account/config/credential snapshot immediately
    /// before an outbound attempt. The expected revision and generation bind
    /// this read to the tuple used during request preparation.
    ///
    /// `None` is a normal readiness transition (rotation, reconfiguration,
    /// inactive, revoked, or expired); malformed encrypted/configured material
    /// remains an explicit error and must not be hidden by failover.
    pub(crate) async fn reload_prepared_upstream_snapshot(
        &self,
        upstream_account_id: Uuid,
        expected_transport_revision: i64,
        expected_credential_generation: i64,
        key_material: &[u8],
    ) -> Result<Option<UpstreamTransportSnapshot>, AppError> {
        let now = unix_millis();
        let row = sqlx::query(
            "SELECT account.updated_at AS transport_revision,
                    account.credential_generation, account.driver,
                    account.config_json, credential.credential_ciphertext
             FROM upstream_accounts account
             JOIN upstream_credentials credential
               ON credential.upstream_account_id = account.id
              AND credential.generation = account.credential_generation
              AND credential.revoked_at IS NULL
              AND (credential.expires_at IS NULL OR credential.expires_at > $4)
             WHERE account.id = $1 AND account.status = 'active'
               AND account.updated_at = $2
               AND account.credential_generation = $3",
        )
        .bind(upstream_account_id.to_string())
        .bind(expected_transport_revision)
        .bind(expected_credential_generation)
        .bind(now)
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
        let credential = open_credential(&ciphertext, key_material)?;
        if credential
            .expires_at()
            .is_some_and(|expires_at| expires_at <= now)
        {
            return Ok(None);
        }
        credential.validate(now)?;
        Ok(Some(UpstreamTransportSnapshot {
            transport_revision: row.try_get("transport_revision")?,
            credential_generation: row.try_get("credential_generation")?,
            driver: row.try_get("driver")?,
            base_url,
            config,
            credential,
        }))
    }

    pub async fn reload_persisted_generation_upstream(
        &self,
        tenant_id: Uuid,
        public_model: &str,
        upstream_account_id: Uuid,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let row = sqlx::query(
            "SELECT r.id AS route_id, candidate.upstream_model, a.id AS account_id,
                    a.updated_at AS transport_revision, a.credential_generation,
                    a.driver, a.config_json, c.credential_ciphertext
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidate
               ON candidate.tenant_id = r.tenant_id AND candidate.model_route_id = r.id
             JOIN upstream_accounts a ON a.tenant_id = r.tenant_id AND a.id = candidate.upstream_account_id AND a.status = 'active'
             JOIN upstream_credentials c ON c.upstream_account_id = a.id AND c.generation = a.credential_generation AND c.revoked_at IS NULL AND (c.expires_at IS NULL OR c.expires_at > $4)
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = 'generation'
               AND candidate.upstream_account_id = $3
             ORDER BY r.priority, r.id LIMIT 1",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(upstream_account_id.to_string())
        .bind(unix_millis())
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
            transport_revision: row.try_get("transport_revision")?,
            credential_generation: row.try_get("credential_generation")?,
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
    /// order. A matching authorized account hint is preferred without
    /// narrowing the set, so health admission and request-local failover may
    /// continue with the remaining authorized candidates. Without a matching
    /// hint, lower route priorities are exhausted first; candidates at the
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
        let candidates = self
            .list_authorized_upstream_candidates_with_hint(
                key_id,
                tenant_id,
                public_model,
                protocol,
                selection,
            )
            .await?;
        let mut resolved = Vec::with_capacity(candidates.len());
        for candidate in candidates {
            if let Some(candidate) = self
                .materialize_authorized_upstream_candidate(&candidate, key_material)
                .await?
            {
                resolved.push(candidate);
            }
        }
        Ok(resolved)
    }

    /// Lists authorized candidates in deterministic preference order without
    /// reading their transport configuration or encrypted credentials.
    pub async fn list_authorized_upstream_candidates_with_hint(
        &self,
        key_id: Uuid,
        tenant_id: Uuid,
        public_model: &str,
        protocol: &str,
        selection: RouteSelectionOptions,
    ) -> Result<Vec<AuthorizedUpstreamCandidate>, AppError> {
        let RouteSelectionOptions {
            upstream_account_hint,
            selection_seed,
        } = selection;
        let rows = sqlx::query(
             "SELECT r.id AS route_id, r.priority, candidates.scheduling_weight,
                     a.id AS account_id, a.updated_at AS transport_revision,
                     a.credential_generation
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidates
               ON candidates.tenant_id = r.tenant_id AND candidates.model_route_id = r.id
             JOIN upstream_accounts a ON a.id = candidates.upstream_account_id AND a.tenant_id = r.tenant_id
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3
               AND r.enabled = 1 AND a.status = 'active'
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
             ORDER BY r.priority ASC, r.id ASC, a.id ASC
             LIMIT $5",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(protocol)
        .bind(key_id.to_string())
        .bind(PROXY_ROUTING_POLICY.candidate_query_limit())
        .fetch_all(&self.pool)
        .await?;
        if rows.len() > PROXY_ROUTING_POLICY.max_resolved_candidates() {
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
            hint_rank(upstream_account_hint, left)
                .cmp(&hint_rank(upstream_account_hint, right))
                .then_with(|| left.priority.cmp(&right.priority))
                .then_with(|| {
                    weighted_rendezvous_score(key_id, selection_seed, left)
                        .total_cmp(&weighted_rendezvous_score(key_id, selection_seed, right))
                })
                .then_with(|| left.route_id.cmp(&right.route_id))
                .then_with(|| left.account_id.cmp(&right.account_id))
        });
        let mut ordered = Vec::with_capacity(
            candidates
                .len()
                .min(PROXY_ROUTING_POLICY.max_resolved_candidates()),
        );
        for candidate in candidates {
            ordered.push(AuthorizedUpstreamCandidate {
                route_id: candidate.route_id,
                account_id: candidate.account_id,
                transport_revision: candidate.transport_revision,
                credential_generation: candidate.credential_generation,
            });
            if ordered.len() == PROXY_ROUTING_POLICY.max_resolved_candidates() {
                break;
            }
        }
        Ok(ordered)
    }

    /// Reads and validates one candidate immediately before request planning.
    /// A disappeared or rotated tuple is ordinary unavailability; malformed
    /// selected material remains an explicit fail-closed error.
    pub(crate) async fn materialize_authorized_upstream_candidate(
        &self,
        candidate: &AuthorizedUpstreamCandidate,
        key_material: &[u8],
    ) -> Result<Option<ResolvedUpstream>, AppError> {
        let row = sqlx::query(
            "SELECT route_candidate.upstream_model, account.driver,
                    account.config_json, credential.credential_ciphertext
             FROM model_routes route
             JOIN model_route_eligible_upstream_accounts route_candidate
               ON route_candidate.tenant_id = route.tenant_id
              AND route_candidate.model_route_id = route.id
              AND route_candidate.upstream_account_id = $2
             JOIN upstream_accounts account
               ON account.tenant_id = route.tenant_id
              AND account.id = route_candidate.upstream_account_id
              AND account.status = 'active'
              AND account.updated_at = $3
              AND account.credential_generation = $4
             JOIN upstream_credentials credential
               ON credential.upstream_account_id = account.id
              AND credential.generation = account.credential_generation
              AND credential.revoked_at IS NULL
              AND (credential.expires_at IS NULL OR credential.expires_at > $5)
             WHERE route.id = $1 AND route.enabled = 1",
        )
        .bind(candidate.route_id.to_string())
        .bind(candidate.account_id.to_string())
        .bind(candidate.transport_revision)
        .bind(candidate.credential_generation)
        .bind(unix_millis())
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
        let credential = open_credential(&ciphertext, key_material)?;
        let now = unix_millis();
        if credential
            .expires_at()
            .is_some_and(|expires_at| expires_at <= now)
        {
            return Ok(None);
        }
        credential.validate(now)?;
        Ok(Some(ResolvedUpstream {
            route_id: candidate.route_id,
            account_id: candidate.account_id,
            transport_revision: candidate.transport_revision,
            credential_generation: candidate.credential_generation,
            driver: row.try_get("driver")?,
            base_url,
            config,
            upstream_model: row.try_get("upstream_model")?,
            credential,
        }))
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
                 JOIN upstream_credentials credential ON credential.upstream_account_id = account.id AND credential.generation = account.credential_generation AND credential.revoked_at IS NULL AND (credential.expires_at IS NULL OR credential.expires_at > $3)
                 WHERE candidate.tenant_id = r.tenant_id AND candidate.model_route_id = r.id
               )
             ORDER BY model",
        )
        .bind(tenant_id.to_string())
        .bind(key_id.to_string())
        .bind(unix_millis())
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
              AND (credential.expires_at IS NULL OR credential.expires_at > $3)
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
        .bind(unix_millis())
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

    /// Checks only normalized route authorization. Runtime account and
    /// credential readiness belongs to candidate resolution so an authorized
    /// key receives a typed availability failure instead of a misleading 403.
    pub async fn credential_has_authorized_route(
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
    transport_revision: i64,
    credential_generation: i64,
    priority: i64,
    scheduling_weight: i64,
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
            transport_revision: row.try_get("transport_revision")?,
            credential_generation: row.try_get("credential_generation")?,
            priority: row.try_get("priority")?,
            scheduling_weight,
        })
    }
}

fn hint_rank(hint: Option<Uuid>, candidate: &RoutingCandidate) -> u8 {
    u8::from(hint.is_some_and(|hint| hint != candidate.account_id))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(account_id: Uuid) -> RoutingCandidate {
        RoutingCandidate {
            route_id: Uuid::from_u128(1),
            account_id,
            transport_revision: 1,
            credential_generation: 1,
            priority: 0,
            scheduling_weight: 100,
            upstream_model: "same-model".to_owned(),
            driver: "http-json".to_owned(),
            config_json: "{}".to_owned(),
            credential_ciphertext: "unused".to_owned(),
        }
    }

    #[test]
    fn one_key_thousands_of_distinct_requests_are_evenly_dispatched() {
        let key_id = Uuid::from_u128(10);
        let candidates = [
            candidate(Uuid::from_u128(101)),
            candidate(Uuid::from_u128(102)),
            candidate(Uuid::from_u128(103)),
            candidate(Uuid::from_u128(104)),
        ];
        let mut selections = [0_usize; 4];

        // Each request has a new request UUID. The same downstream key must
        // therefore not pin an entire concurrent wave to one account; stable
        // session affinity is handled by passing a stable seed instead.
        for ordinal in 0..4_096_u128 {
            let seed = Uuid::from_u128(1_000 + ordinal);
            let selected = candidates
                .iter()
                .enumerate()
                .min_by(|(_, left), (_, right)| {
                    weighted_rendezvous_score(key_id, seed, left)
                        .total_cmp(&weighted_rendezvous_score(key_id, seed, right))
                })
                .map(|(index, _)| index)
                .expect("candidate pool is non-empty");
            selections[selected] += 1;
        }

        // The expected share is 1,024 each. Wide bounds deliberately avoid a
        // brittle statistical fixture while still catching a one-account
        // sticky key or deterministic first-row selection.
        for selection in &selections {
            assert!((700..=1_350).contains(selection), "{selections:?}");
        }
    }
}
