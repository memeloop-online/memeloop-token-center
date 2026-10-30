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
        let persisted_config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        let credential = open_credential(&ciphertext, key_material)?;
        let config = credential.hydrate_provider_adapter_config(persisted_config)?;
        let base_url = validate_config(&config)?;
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
        let persisted_config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        let credential = open_credential(&ciphertext, key_material)?;
        let config = credential.hydrate_provider_adapter_config(persisted_config)?;
        let base_url = validate_config(&config)?;
        Ok(Some(ResolvedUpstream {
            route_id: parse_uuid(row.try_get("route_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            transport_revision: row.try_get("transport_revision")?,
            credential_generation: row.try_get("credential_generation")?,
            driver: row.try_get("driver")?,
            base_url,
            config,
            upstream_model: row.try_get("upstream_model")?,
            credential,
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
            avoid_route_account,
            selection_seed,
        } = selection;
        // Only an unhealthy probe sorts last. A healthy result and a missing
        // probe share a rank so configured priority still chooses; excluding
        // a non-healthy account made the probe unable to reach it again.
        let rows = sqlx::query(
             "SELECT r.id AS route_id, r.priority, candidates.scheduling_weight,
                     a.id AS account_id, a.driver, a.updated_at AS transport_revision,
                     a.credential_generation,
                     CAST(CASE
                       WHEN service_probe.status = 'unhealthy' THEN 1
                       ELSE 0
                     END AS BIGINT) AS probe_rank
             FROM model_routes r
             JOIN model_route_eligible_upstream_accounts candidates
               ON candidates.tenant_id = r.tenant_id AND candidates.model_route_id = r.id
             JOIN upstream_accounts a ON a.id = candidates.upstream_account_id AND a.tenant_id = r.tenant_id
             LEFT JOIN upstream_service_probes service_probe
               ON service_probe.upstream_account_id = a.id
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3
               AND r.enabled = 1 AND r.archived_at IS NULL AND a.status = 'active'
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
        let rows = if rows.is_empty() {
            // An empty authorized set must not stop the request. Catalog
            // eligibility and grants can hide every account; dispatch still
            // needs one active upstream on this route.
            tracing::info!(
                event = "dispatch_candidate_fallback",
                %tenant_id,
                %key_id,
                "authorized candidate set was empty; selecting a configured upstream"
            );
            self.fallback_dispatch_candidates(tenant_id, key_id, public_model, protocol)
                .await?
        } else {
            rows
        };
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
        let hint_disposition = order_authorized_candidates(
            &mut candidates,
            key_id,
            selection_seed,
            upstream_account_hint,
            avoid_route_account,
        );
        tracing::debug!(
            event = "authorized_candidate_order",
            %tenant_id,
            %key_id,
            %selection_seed,
            candidate_count = candidates.len(),
            hint_disposition = hint_disposition.as_str(),
            "host ordered the authorized candidate set"
        );
        let mut ordered = Vec::with_capacity(
            candidates
                .len()
                .min(PROXY_ROUTING_POLICY.max_resolved_candidates()),
        );
        for candidate in candidates {
            ordered.push(AuthorizedUpstreamCandidate {
                route_id: candidate.route_id,
                account_id: candidate.account_id,
                driver: candidate.driver,
                transport_revision: candidate.transport_revision,
                credential_generation: candidate.credential_generation,
            });
            if ordered.len() == PROXY_ROUTING_POLICY.max_resolved_candidates() {
                break;
            }
        }
        Ok(ordered)
    }

    async fn fallback_dispatch_candidates(
        &self,
        tenant_id: Uuid,
        key_id: Uuid,
        public_model: &str,
        protocol: &str,
    ) -> Result<Vec<AnyRow>, AppError> {
        let limit = PROXY_ROUTING_POLICY.candidate_query_limit();
        let configured = sqlx::query(
            "SELECT r.id AS route_id, r.priority, link.scheduling_weight,
                    a.id AS account_id, a.driver, a.updated_at AS transport_revision,
                    a.credential_generation,
                    CAST(CASE
                      WHEN service_probe.status = 'unhealthy' THEN 1
                      ELSE 0
                    END AS BIGINT) AS probe_rank
             FROM model_routes r
             JOIN (
               SELECT direct.tenant_id, direct.model_route_id, direct.upstream_account_id,
                      direct.scheduling_weight
               FROM model_route_upstream_accounts direct
               UNION ALL
               SELECT included.tenant_id, included.model_route_id, member.upstream_account_id,
                      100 AS scheduling_weight
               FROM model_route_included_provider_groups included
               JOIN upstream_account_provider_groups member
                 ON member.tenant_id = included.tenant_id
                AND member.provider_group_id = included.provider_group_id
             ) link
               ON link.tenant_id = r.tenant_id AND link.model_route_id = r.id
             JOIN upstream_accounts a
               ON a.tenant_id = r.tenant_id AND a.id = link.upstream_account_id
              AND a.status = 'active'
             LEFT JOIN upstream_service_probes service_probe
               ON service_probe.upstream_account_id = a.id
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3
               AND r.enabled = 1 AND r.archived_at IS NULL
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
             ORDER BY probe_rank ASC, r.priority ASC, r.id ASC, a.id ASC
             LIMIT $4",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(protocol)
        .bind(limit)
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await?;
        if !configured.is_empty() {
            return Ok(configured);
        }
        tracing::info!(
            event = "dispatch_candidate_fallback",
            %tenant_id,
            "no configured upstream was visible; selecting any active account on the route"
        );
        sqlx::query(
            "SELECT r.id AS route_id, r.priority, 100 AS scheduling_weight,
                    a.id AS account_id, a.driver, a.updated_at AS transport_revision,
                    a.credential_generation,
                    CAST(CASE
                      WHEN service_probe.status = 'unhealthy' THEN 1
                      ELSE 0
                    END AS BIGINT) AS probe_rank
             FROM model_routes r
             JOIN upstream_accounts a
               ON a.id = r.upstream_account_id
              AND a.tenant_id = r.tenant_id
              AND a.status = 'active'
             LEFT JOIN upstream_service_probes service_probe
               ON service_probe.upstream_account_id = a.id
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3
               AND r.enabled = 1 AND r.archived_at IS NULL
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
             ORDER BY probe_rank ASC, a.id ASC
             LIMIT $4",
        )
        .bind(tenant_id.to_string())
        .bind(public_model)
        .bind(protocol)
        .bind(limit)
        .bind(key_id.to_string())
        .fetch_all(&self.pool)
        .await
        .map_err(AppError::from)
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
              AND account.driver = $5
             JOIN upstream_credentials credential
               ON credential.upstream_account_id = account.id
              AND credential.generation = account.credential_generation
              AND credential.revoked_at IS NULL
              AND (credential.expires_at IS NULL OR credential.expires_at > $6)
             WHERE route.id = $1 AND route.enabled = 1 AND route.archived_at IS NULL",
        )
        .bind(candidate.route_id.to_string())
        .bind(candidate.account_id.to_string())
        .bind(candidate.transport_revision)
        .bind(candidate.credential_generation)
        .bind(&candidate.driver)
        .bind(unix_millis())
        .fetch_optional(&self.pool)
        .await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let config_json: String = row.try_get("config_json")?;
        let persisted_config: serde_json::Value =
            serde_json::from_str(&config_json).map_err(|_| AppError::Internal)?;
        let ciphertext: String = row.try_get("credential_ciphertext")?;
        let credential = open_credential(&ciphertext, key_material)?;
        let config = credential.hydrate_provider_adapter_config(persisted_config)?;
        let base_url = validate_config(&config)?;
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
             WHERE r.tenant_id = $1 AND r.enabled = 1 AND r.archived_at IS NULL
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
             WHERE r.tenant_id = $1 AND r.enabled = 1 AND r.archived_at IS NULL
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

    /// Checks only normalized route authorization. Route enablement, runtime account and
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
             WHERE r.tenant_id = $1 AND r.public_model = $2 AND r.protocol = $3 AND r.archived_at IS NULL
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
    driver: String,
    transport_revision: i64,
    credential_generation: i64,
    priority: i64,
    scheduling_weight: i64,
    probe_rank: u8,
}

impl RoutingCandidate {
    fn from_row(row: AnyRow) -> Result<Self, AppError> {
        let scheduling_weight: i64 = row.try_get("scheduling_weight")?;
        if !(1..=1_000_000).contains(&scheduling_weight) {
            return Err(AppError::Internal);
        }
        let probe_rank: i64 = row.try_get("probe_rank")?;
        if !(0..=2).contains(&probe_rank) {
            return Err(AppError::Internal);
        }
        Ok(Self {
            route_id: parse_uuid(row.try_get("route_id")?)?,
            account_id: parse_uuid(row.try_get("account_id")?)?,
            driver: row.try_get("driver")?,
            transport_revision: row.try_get("transport_revision")?,
            credential_generation: row.try_get("credential_generation")?,
            priority: row.try_get("priority")?,
            scheduling_weight,
            probe_rank: u8::try_from(probe_rank).map_err(|_| AppError::Internal)?,
        })
    }
}

#[derive(Debug, PartialEq, Eq)]
enum HintDisposition {
    Absent,
    Preferred,
    OutsideAuthorizedSet,
}

impl HintDisposition {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::Preferred => "preferred",
            Self::OutsideAuthorizedSet => "outside_authorized_set",
        }
    }
}

/// This boundary accepts only a mutable slice of host-authorized candidates:
/// a policy hint can reorder, but cannot append, remove, or materialize an
/// account. A service probe reorders the same set and never removes an
/// account. Cooldown admission, transport validation and budget reservation
/// remain later host-owned checks, including for the preferred candidate.
fn order_authorized_candidates(
    candidates: &mut [RoutingCandidate],
    key_id: Uuid,
    selection_seed: Uuid,
    hint: Option<Uuid>,
    avoid: Option<(Uuid, Uuid)>,
) -> HintDisposition {
    let disposition = match hint {
        None => HintDisposition::Absent,
        Some(id)
            if candidates
                .iter()
                .any(|candidate| candidate.account_id == id) =>
        {
            HintDisposition::Preferred
        }
        Some(_) => HintDisposition::OutsideAuthorizedSet,
    };
    candidates.sort_by(|left, right| {
        hint_rank(hint, left)
            .cmp(&hint_rank(hint, right))
            .then_with(|| avoid_rank(avoid, left).cmp(&avoid_rank(avoid, right)))
            .then_with(|| left.probe_rank.cmp(&right.probe_rank))
            .then_with(|| left.priority.cmp(&right.priority))
            .then_with(|| {
                weighted_rendezvous_score(key_id, selection_seed, left)
                    .total_cmp(&weighted_rendezvous_score(key_id, selection_seed, right))
            })
            .then_with(|| left.route_id.cmp(&right.route_id))
            .then_with(|| left.account_id.cmp(&right.account_id))
    });
    disposition
}

fn avoid_rank(avoid: Option<(Uuid, Uuid)>, candidate: &RoutingCandidate) -> u8 {
    u8::from(avoid.is_some_and(|(route_id, account_id)| {
        route_id == candidate.route_id && account_id == candidate.account_id
    }))
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
    // The same account may occur behind several authorized routes for one
    // model. Those links are not extra scheduling weight for that account.
    digest.update(b"memeloop-routing-account-rendezvous-v2");
    digest.update(key_id.as_bytes());
    digest.update(selection_seed.as_bytes());
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
            driver: "http-json".to_owned(),
            transport_revision: 1,
            credential_generation: 1,
            priority: 0,
            scheduling_weight: 100,
            probe_rank: 0,
        }
    }

    fn identities(candidates: &[RoutingCandidate]) -> Vec<(Uuid, Uuid, i64, i64)> {
        candidates
            .iter()
            .map(|c| {
                (
                    c.route_id,
                    c.account_id,
                    c.transport_revision,
                    c.credential_generation,
                )
            })
            .collect()
    }

    #[test]
    fn duplicate_route_links_do_not_multiply_an_accounts_scheduling_weight() {
        let account = Uuid::from_u128(101);
        let peer = Uuid::from_u128(102);
        for seed in 1..=256 {
            let mut original = vec![candidate(account), candidate(peer)];
            order_authorized_candidates(
                &mut original,
                Uuid::nil(),
                Uuid::from_u128(seed),
                None,
                None,
            );
            let mut linked = vec![candidate(account), candidate(peer), candidate(account)];
            linked[2].route_id = Uuid::from_u128(999);
            order_authorized_candidates(
                &mut linked,
                Uuid::nil(),
                Uuid::from_u128(seed),
                None,
                None,
            );
            assert_eq!(original[0].account_id, linked[0].account_id);
        }
    }

    #[test]
    fn unhealthy_probe_sorts_last_and_stays_eligible() {
        let healthy = Uuid::from_u128(101);
        let unknown = Uuid::from_u128(102);
        let unhealthy = Uuid::from_u128(103);
        let mut candidates = vec![candidate(unhealthy), candidate(unknown), candidate(healthy)];
        candidates[0].probe_rank = 2;
        candidates[0].priority = -100;
        candidates[1].probe_rank = 1;
        candidates[1].priority = -50;
        order_authorized_candidates(
            &mut candidates,
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            None,
            None,
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.account_id)
                .collect::<Vec<_>>(),
            vec![healthy, unknown, unhealthy]
        );
    }

    #[test]
    fn preference_preserves_every_authorized_candidate_and_revision() {
        let mut candidates = vec![
            candidate(Uuid::from_u128(101)),
            candidate(Uuid::from_u128(102)),
        ];
        candidates[1].priority = 10;
        candidates[1].transport_revision = 42;
        candidates[1].credential_generation = 7;
        let mut before = identities(&candidates);
        let disposition = order_authorized_candidates(
            &mut candidates,
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            Some(Uuid::from_u128(102)),
            None,
        );
        assert_eq!(disposition, HintDisposition::Preferred);
        assert_eq!(candidates[0].account_id, Uuid::from_u128(102));
        let mut after = identities(&candidates);
        before.sort();
        after.sort();
        assert_eq!(before, after);
    }

    #[test]
    fn outside_hint_is_observable_but_cannot_change_default_order() {
        let mut candidates = vec![
            candidate(Uuid::from_u128(101)),
            candidate(Uuid::from_u128(102)),
        ];
        let key = Uuid::from_u128(10);
        let seed = Uuid::from_u128(20);
        assert_eq!(
            order_authorized_candidates(&mut candidates, key, seed, None, None),
            HintDisposition::Absent
        );
        let before = identities(&candidates);
        candidates.reverse();
        assert_eq!(
            order_authorized_candidates(
                &mut candidates,
                key,
                seed,
                Some(Uuid::from_u128(999)),
                None,
            ),
            HintDisposition::OutsideAuthorizedSet
        );
        assert_eq!(before, identities(&candidates));
    }

    #[test]
    fn hint_cannot_create_a_candidate_in_an_empty_authorized_set() {
        let mut candidates = Vec::new();
        assert_eq!(
            order_authorized_candidates(
                &mut candidates,
                Uuid::nil(),
                Uuid::nil(),
                Some(Uuid::from_u128(999)),
                None
            ),
            HintDisposition::OutsideAuthorizedSet
        );
        assert!(candidates.is_empty());
    }

    #[test]
    fn same_account_on_distinct_authorized_routes_is_not_collapsed() {
        let account = Uuid::from_u128(101);
        let mut candidates = vec![candidate(account), candidate(account)];
        candidates[1].route_id = Uuid::from_u128(2);
        candidates[1].priority = -1;
        order_authorized_candidates(
            &mut candidates,
            Uuid::nil(),
            Uuid::nil(),
            Some(account),
            None,
        );
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].route_id, Uuid::from_u128(2));
        assert_eq!(candidates[1].route_id, Uuid::from_u128(1));
    }

    #[test]
    fn latest_session_transport_route_account_is_only_deprioritized() {
        let avoided = Uuid::from_u128(101);
        let alternative = Uuid::from_u128(102);
        let mut candidates = vec![candidate(avoided), candidate(alternative)];
        candidates[1].priority = 50;
        let mut before = identities(&candidates);

        let disposition = order_authorized_candidates(
            &mut candidates,
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            None,
            Some((Uuid::from_u128(1), avoided)),
        );

        assert_eq!(disposition, HintDisposition::Absent);
        assert_eq!(candidates[0].account_id, alternative);
        assert_eq!(candidates[1].account_id, avoided);
        let mut after = identities(&candidates);
        before.sort();
        after.sort();
        assert_eq!(before, after);
    }

    #[test]
    fn explicit_hint_overrides_session_avoid_ordering() {
        let hinted_and_avoided = Uuid::from_u128(101);
        let mut candidates = vec![
            candidate(hinted_and_avoided),
            candidate(Uuid::from_u128(102)),
        ];

        let disposition = order_authorized_candidates(
            &mut candidates,
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            Some(hinted_and_avoided),
            Some((Uuid::from_u128(1), hinted_and_avoided)),
        );

        assert_eq!(disposition, HintDisposition::Preferred);
        assert_eq!(candidates[0].account_id, hinted_and_avoided);
    }

    #[test]
    fn sole_avoided_candidate_remains_available() {
        let account = Uuid::from_u128(101);
        let mut candidates = vec![candidate(account)];
        order_authorized_candidates(
            &mut candidates,
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            None,
            Some((Uuid::from_u128(1), account)),
        );
        assert_eq!(candidates[0].account_id, account);
    }

    #[test]
    fn avoid_evidence_does_not_deprioritize_same_account_on_another_route() {
        let account = Uuid::from_u128(101);
        let mut candidates = vec![candidate(account), candidate(account)];
        candidates[1].route_id = Uuid::from_u128(2);
        candidates[1].priority = 50;

        order_authorized_candidates(
            &mut candidates,
            Uuid::from_u128(10),
            Uuid::from_u128(20),
            None,
            Some((Uuid::from_u128(1), account)),
        );

        assert_eq!(candidates[0].route_id, Uuid::from_u128(2));
        assert_eq!(candidates[1].route_id, Uuid::from_u128(1));
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

    #[tokio::test]
    async fn empty_authorized_set_still_dispatches_and_success_clears_probe() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("fallback.db").display()
        ))
        .await
        .unwrap();
        database.migrate().await.unwrap();
        let tenant = format!("fallback-{}", Uuid::now_v7());
        let account = database
            .create_upstream_account(
                crate::db::CreateUpstreamAccountInput {
                    tenant_external_id: tenant,
                    name: "fallback-account".into(),
                    driver: "http-json".into(),
                    config: serde_json::json!({"base_url":"http://127.0.0.1:1"}),
                    credential: crate::provider::UpstreamCredential::None,
                    oauth_session_id: None,
                    oauth_driver: None,
                    oauth_refresh_url: None,
                },
                b"fallback-dispatch-test",
            )
            .await
            .unwrap();
        let tenant_id: String =
            sqlx::query_scalar("SELECT tenant_id FROM upstream_accounts WHERE id = $1")
                .bind(account.id.to_string())
                .fetch_one(&database.pool)
                .await
                .unwrap();
        let route_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO model_routes (
                 id, tenant_id, public_model, upstream_account_id, upstream_model,
                 protocol, priority, enabled, created_at, updated_at
             ) VALUES ($1, $2, 'gpt-fallback', $3, 'gpt-fallback', 'openai', 0, 1, 1, 1)",
        )
        .bind(route_id.to_string())
        .bind(&tenant_id)
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO upstream_service_probes (
                 upstream_account_id, status, error_code, latency_ms, checked_at
             ) VALUES ($1, 'unhealthy', 'upstream_unavailable', 1, 1)",
        )
        .bind(account.id.to_string())
        .execute(&database.pool)
        .await
        .unwrap();
        let tenant_external: String =
            sqlx::query_scalar("SELECT external_id FROM tenants WHERE id = $1")
                .bind(&tenant_id)
                .fetch_one(&database.pool)
                .await
                .unwrap();
        let issued = database
            .create_key_with_routing(
                crate::db::CreateKeyInput {
                    tenant_external_id: tenant_external,
                    principal_external_id: "member".into(),
                    alias: "fallback".into(),
                    currency: "USD".into(),
                    policy: crate::model::KeyPolicy {
                        allowed_models: vec!["gpt-fallback".into()],
                        ..crate::model::KeyPolicy::default()
                    },
                    initial_balance: rust_decimal::Decimal::ONE,
                    idempotency_key: None,
                },
                &[route_id],
                &[],
                b"fallback-dispatch-test",
            )
            .await
            .unwrap();
        let selection = RouteSelectionOptions {
            upstream_account_hint: None,
            avoid_route_account: None,
            selection_seed: Uuid::now_v7(),
        };
        let tenant_uuid = Uuid::parse_str(&tenant_id).unwrap();
        let ungranted = database
            .list_authorized_upstream_candidates_with_hint(
                Uuid::now_v7(),
                tenant_uuid,
                "gpt-fallback",
                "openai",
                selection,
            )
            .await
            .unwrap();
        assert!(
            ungranted.is_empty(),
            "a key without a grant is not a candidate"
        );
        let found = database
            .list_authorized_upstream_candidates_with_hint(
                issued.key_id,
                tenant_uuid,
                "gpt-fallback",
                "openai",
                selection,
            )
            .await
            .unwrap();
        assert_eq!(
            found.len(),
            1,
            "a granted route still dispatches when catalog eligibility is empty"
        );
        assert_eq!(found[0].account_id, account.id);
        database.note_service_available(account.id).await;
        let status: String = sqlx::query_scalar(
            "SELECT status FROM upstream_service_probes WHERE upstream_account_id = $1",
        )
        .bind(account.id.to_string())
        .fetch_one(&database.pool)
        .await
        .unwrap();
        assert_eq!(status, "healthy");
        sqlx::query("UPDATE upstream_accounts SET status = 'disabled' WHERE id = $1")
            .bind(account.id.to_string())
            .execute(&database.pool)
            .await
            .unwrap();
        let found = database
            .list_authorized_upstream_candidates_with_hint(
                issued.key_id,
                tenant_uuid,
                "gpt-fallback",
                "openai",
                selection,
            )
            .await
            .unwrap();
        assert!(
            found.is_empty(),
            "a disabled account is not a dispatch target"
        );
    }
}
