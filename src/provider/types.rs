use std::num::NonZeroUsize;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::UpstreamCredential;

/// One validated policy owns both the request attempt budget and the bounded
/// resolver look-ahead. Keeping these values together prevents the database
/// and HTTP layers from silently applying different routing limits.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RoutingAttemptPolicy {
    max_attempts: NonZeroUsize,
    max_resolved_candidates: NonZeroUsize,
}

impl RoutingAttemptPolicy {
    const fn new(max_attempts: usize, max_resolved_candidates: usize) -> Self {
        assert!(max_resolved_candidates > max_attempts);
        assert!(max_resolved_candidates < i64::MAX as usize);
        let Some(max_attempts) = NonZeroUsize::new(max_attempts) else {
            panic!("routing must allow at least one upstream attempt");
        };
        let Some(max_resolved_candidates) = NonZeroUsize::new(max_resolved_candidates) else {
            panic!("routing must resolve at least one upstream candidate");
        };
        Self {
            max_attempts,
            max_resolved_candidates,
        }
    }

    pub(crate) const fn max_attempts(self) -> usize {
        self.max_attempts.get()
    }

    pub(crate) const fn max_resolved_candidates(self) -> usize {
        self.max_resolved_candidates.get()
    }

    pub(crate) const fn candidate_query_limit(self) -> i64 {
        self.max_resolved_candidates.get() as i64 + 1
    }
}

/// The database may inspect a larger defensive set, while component hooks and
/// outbound sends remain bounded by the much smaller attempt budget. Only
/// authorized candidate handles count against the resolver side of this
/// policy; transport materialization and request-local compatibility filters
/// run lazily before attempt accounting.
pub(crate) const PROXY_ROUTING_POLICY: RoutingAttemptPolicy = RoutingAttemptPolicy::new(3, 1_000);

/// A non-sensitive authorization and ordering handle for one proxy candidate.
///
/// Transport configuration and encrypted credentials are deliberately absent:
/// the proxy materializes only the candidate it is about to admit.  This keeps
/// a malformed standby from affecting a healthy preferred route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedUpstreamCandidate {
    pub route_id: Uuid,
    pub account_id: Uuid,
    pub(crate) transport_revision: i64,
    pub(crate) credential_generation: i64,
}

/// Account and credential fields read by one send-time database statement.
/// Route-specific fields are intentionally absent because they were already
/// authorized and prepared; a revision mismatch makes that preparation stale.
#[derive(Clone, Debug)]
pub(crate) struct UpstreamTransportSnapshot {
    pub(crate) transport_revision: i64,
    pub(crate) credential_generation: i64,
    pub(crate) driver: String,
    pub(crate) base_url: String,
    pub(crate) config: Value,
    pub(crate) credential: UpstreamCredential,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpstreamAccountView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: Option<String>,
    pub name: String,
    pub driver: String,
    pub auth_kind: String,
    /// How this provider was connected. This is presentation metadata only;
    /// every connection method uses the same stable upstream account model.
    pub connection_method: String,
    pub credential_generation: i64,
    pub status: String,
    pub config: Value,
    pub credential_expires_at: Option<i64>,
    /// Server-derived lifecycle capabilities. Clients must use these instead
    /// of inferring actions from `auth_kind` or `connection_method`.
    pub can_refresh: bool,
    pub can_rotate: bool,
    pub can_reauthorize: bool,
    /// Number of model routes that still reference this stable upstream
    /// identity, including disabled routes retained for audit purposes.
    pub route_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Read-only deletion preflight for one stable upstream identity.
///
/// Counts include disabled routes and immutable request/generation history so
/// an operator can distinguish route cleanup from the facts that DELETE will
/// preserve in a sanitized account snapshot.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpstreamDeletionReadiness {
    /// The only mutable lifecycle prerequisite: disable this exact upstream
    /// identity before it can be physically removed.
    pub requires_disabled: bool,
    /// Distinct model routes that explicitly name this upstream, including a
    /// multi-candidate association and disabled routes retained for audit.
    pub model_route_count: i64,
    /// Immutable text/request archive rows attributed to this upstream. These
    /// are retained with their stable account ID and do not block deletion.
    pub request_history_count: i64,
    /// Immutable asynchronous generation rows attributed to this upstream.
    /// They are retained with their stable account ID and do not block deletion.
    pub generation_history_count: i64,
    /// Imported accounts retain immutable source provenance and are never
    /// physically deleted through the upstream lifecycle API.
    pub imported_for_audit: bool,
    /// True only when DELETE may proceed at the instant this read completed.
    /// DELETE repeats every check transactionally; this is guidance, not an
    /// authorization grant.
    pub can_delete: bool,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelRouteView {
    pub id: Uuid,
    pub tenant_id: Uuid,
    pub tenant_external_id: Option<String>,
    pub public_model: String,
    pub upstream_account_id: Uuid,
    pub upstream_model: String,
    pub protocol: String,
    pub priority: i64,
    pub enabled: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug)]
pub struct ResolvedUpstream {
    pub route_id: Uuid,
    pub account_id: Uuid,
    /// Revision of the account transport tuple used to prepare this route.
    /// A send-time reload must match it before attaching credential material.
    pub transport_revision: i64,
    /// Generation of the encrypted credential selected with this attempt.
    /// Breaker and send transitions use it as a fence across OAuth rotation.
    pub credential_generation: i64,
    pub driver: String,
    pub base_url: String,
    pub config: Value,
    pub upstream_model: String,
    pub credential: UpstreamCredential,
}
