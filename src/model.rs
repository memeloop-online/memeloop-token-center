use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MONEY_SCALE: i64 = 1_000_000;
pub const JSON_SAFE_INTEGER_MAX: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPolicy {
    /// Migration-only source retained so old in-tree fixtures keep compiling.
    /// It is deliberately absent from every serialized or deserialized public
    /// policy contract; normalized routing grants are the sole authority.
    #[doc(hidden)]
    #[serde(default, skip_serializing)]
    pub allowed_models: Vec<String>,
    #[serde(default = "default_rpm")]
    pub requests_per_minute: u32,
    #[serde(default = "default_tpm")]
    pub tokens_per_minute: u64,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: u32,
    pub daily_budget: Option<String>,
    pub weekly_budget: Option<String>,
    pub lifetime_budget: Option<String>,
}

/// Strict public policy input. Routing authorization is intentionally absent:
/// callers grant stable route or route-group IDs alongside the credential.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KeyPolicyInput {
    #[serde(default = "default_rpm")]
    pub requests_per_minute: u32,
    #[serde(default = "default_tpm")]
    pub tokens_per_minute: u64,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: u32,
    pub daily_budget: Option<String>,
    pub weekly_budget: Option<String>,
    pub lifetime_budget: Option<String>,
}

impl Default for KeyPolicyInput {
    fn default() -> Self {
        KeyPolicy::default().into()
    }
}

impl From<KeyPolicyInput> for KeyPolicy {
    fn from(value: KeyPolicyInput) -> Self {
        Self {
            allowed_models: Vec::new(),
            requests_per_minute: value.requests_per_minute,
            tokens_per_minute: value.tokens_per_minute,
            max_concurrency: value.max_concurrency,
            daily_budget: value.daily_budget,
            weekly_budget: value.weekly_budget,
            lifetime_budget: value.lifetime_budget,
        }
    }
}

impl From<KeyPolicy> for KeyPolicyInput {
    fn from(value: KeyPolicy) -> Self {
        Self {
            requests_per_minute: value.requests_per_minute,
            tokens_per_minute: value.tokens_per_minute,
            max_concurrency: value.max_concurrency,
            daily_budget: value.daily_budget,
            weekly_budget: value.weekly_budget,
            lifetime_budget: value.lifetime_budget,
        }
    }
}

impl Default for KeyPolicy {
    fn default() -> Self {
        Self {
            allowed_models: Vec::new(),
            requests_per_minute: default_rpm(),
            tokens_per_minute: default_tpm(),
            max_concurrency: default_concurrency(),
            daily_budget: None,
            weekly_budget: None,
            lifetime_budget: None,
        }
    }
}

fn default_rpm() -> u32 {
    60
}

fn default_tpm() -> u64 {
    100_000
}

fn default_concurrency() -> u32 {
    4
}

#[cfg(test)]
mod key_policy_contract_tests {
    use serde_json::json;

    use super::{KeyPolicy, KeyPolicyInput};

    #[test]
    fn public_policy_rejects_legacy_model_names_and_views_never_emit_them() {
        assert!(
            serde_json::from_value::<KeyPolicyInput>(json!({
                "allowed_models": ["*"] ,
                "requests_per_minute": 60,
                "tokens_per_minute": 100_000,
                "max_concurrency": 4,
                "daily_budget": null,
                "weekly_budget": null,
                "lifetime_budget": null
            }))
            .is_err()
        );

        let legacy: KeyPolicy = serde_json::from_value(json!({
            "allowed_models": ["legacy-model"],
            "requests_per_minute": 60,
            "tokens_per_minute": 100_000,
            "max_concurrency": 4,
            "daily_budget": null,
            "weekly_budget": null,
            "lifetime_budget": null
        }))
        .unwrap();
        assert_eq!(legacy.allowed_models, ["legacy-model"]);
        assert!(
            serde_json::to_value(legacy)
                .unwrap()
                .get("allowed_models")
                .is_none()
        );
    }
}

#[derive(Clone, Debug)]
pub struct AuthenticatedKey {
    pub key_id: Uuid,
    pub tenant_id: Uuid,
    pub principal_id: Uuid,
    pub account_id: Uuid,
    pub alias: String,
    pub currency: String,
    pub credential_generation: i64,
    pub policy: KeyPolicy,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct IssuedKey {
    pub key_id: Uuid,
    pub account_id: Uuid,
    pub alias: String,
    pub currency: String,
    pub credential_generation: i64,
    pub key: String,
    pub fingerprint: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyView {
    pub key_id: Uuid,
    pub alias: String,
    pub currency: String,
    pub credential_generation: i64,
    pub created_at: i64,
    pub policy: KeyPolicy,
    pub available_balance: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyAliasView {
    pub key_id: Uuid,
    pub alias: String,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyRateLimitSnapshot {
    pub limit: u64,
    pub used: u64,
    pub remaining: u64,
    pub reset_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyConcurrencySnapshot {
    pub limit: u32,
    pub active: u64,
    pub remaining: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyBudgetSnapshot {
    pub limit: Option<String>,
    pub settled: String,
    pub reserved: String,
    pub remaining: Option<String>,
    pub reset_at: Option<i64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct KeyLimitSnapshot {
    pub key_id: Uuid,
    pub captured_at: i64,
    pub currency: String,
    pub available_balance: String,
    pub reserved_balance: String,
    pub rpm: KeyRateLimitSnapshot,
    pub tpm: KeyRateLimitSnapshot,
    pub concurrency: KeyConcurrencySnapshot,
    pub daily_budget: KeyBudgetSnapshot,
    pub weekly_budget: KeyBudgetSnapshot,
    pub lifetime_budget: KeyBudgetSnapshot,
}

#[derive(Clone, Debug, Serialize)]
pub struct ManagedKeyView {
    pub key_id: Uuid,
    pub account_id: Uuid,
    pub tenant_external_id: String,
    pub principal_external_id: String,
    pub alias: String,
    pub currency: String,
    pub status: String,
    pub credential_generation: i64,
    pub fingerprint: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub policy: KeyPolicy,
    pub available_balance: String,
    pub reserved_balance: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct LegacyCredentialView {
    pub key_id: Uuid,
    pub generation: i64,
    pub fingerprint: String,
    pub source_hash: String,
}

#[derive(Clone, Debug)]
pub struct AuthenticatedService {
    pub service_id: Option<Uuid>,
    pub scopes: Vec<String>,
    pub tenant_external_id: Option<String>,
}

impl AuthenticatedService {
    pub fn bootstrap() -> Self {
        Self {
            service_id: None,
            scopes: vec!["*".to_owned()],
            tenant_external_id: None,
        }
    }

    pub fn allows(&self, required: &str) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope == required || (self.service_id.is_none() && scope.as_str() == "*"))
    }
}

#[cfg(test)]
mod authenticated_service_tests {
    use super::AuthenticatedService;

    #[test]
    fn only_bootstrap_identity_can_use_the_global_scope() {
        assert!(AuthenticatedService::bootstrap().allows("keys:write"));

        let managed = AuthenticatedService {
            service_id: Some(uuid::Uuid::now_v7()),
            scopes: vec!["*".to_owned(), "keys:*".to_owned(), "keys:read".to_owned()],
            tenant_external_id: None,
        };
        assert!(managed.allows("keys:read"));
        assert!(!managed.allows("keys:write"));
        assert!(!managed.allows("prices:read"));
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct IssuedServiceToken {
    pub service_id: Uuid,
    pub name: String,
    pub credential_generation: i64,
    pub token: String,
    pub fingerprint: String,
    pub scopes: Vec<String>,
    pub tenant_external_id: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ServiceTokenView {
    pub service_id: Uuid,
    pub name: String,
    pub status: String,
    pub credential_generation: i64,
    pub fingerprint: String,
    pub scopes: Vec<String>,
    pub tenant_external_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct LedgerEntryView {
    pub entry_id: Uuid,
    pub kind: String,
    pub amount: String,
    pub currency: String,
    pub source: String,
    pub idempotency_key: Option<String>,
    pub created_at: i64,
}

/// Stable subscription identity plus the currently effective billing-cycle
/// entitlement. Rotating downstream credentials never changes either ID.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntitlementView {
    pub entitlement_id: Uuid,
    pub cycle_id: Uuid,
    pub tenant_external_id: String,
    pub account_id: Uuid,
    pub provider: String,
    pub external_subscription_id: String,
    pub external_cycle_id: String,
    pub period_start: i64,
    pub period_end: i64,
    pub currency: String,
    pub desired: String,
    pub consumed: String,
    pub remaining: String,
    pub status: String,
    pub version: i64,
    pub replaced_by_entitlement_id: Option<Uuid>,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct EntitlementReconcileResult {
    #[serde(flatten)]
    pub entitlement: EntitlementView,
    /// Net credit-account movement caused by this atomic operation. A negative
    /// value is the unused entitlement that was revoked.
    pub ledger_delta: String,
    /// Populated by `replace`; it identifies the old stable subscription.
    pub replaced_entitlement_id: Option<Uuid>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestView {
    pub request_id: Uuid,
    pub created_at: i64,
    pub protocol: String,
    pub model: String,
    pub status_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost: String,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestEventView {
    pub event_id: Uuid,
    pub request_id: Uuid,
    pub event_at: i64,
    pub event_kind: String,
    pub key_id: Uuid,
    pub protocol: String,
    pub model: String,
    pub status_code: Option<i64>,
    pub duration_ms: Option<i64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost: String,
    pub error_code: Option<String>,
}

#[derive(Clone, Debug)]
pub struct RequestArchiveRefs {
    pub view: RequestView,
    pub request_object: String,
    pub response_object: Option<String>,
    pub response_json: Option<serde_json::Value>,
    pub provenance: Option<RequestProvenanceView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestProvenanceView {
    pub source: String,
    pub disposition: String,
    pub unlinked: bool,
    pub external_request_id: String,
    pub proof_digest: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestDetail {
    #[serde(flatten)]
    pub view: RequestView,
    pub request_body: serde_json::Value,
    pub response_body: serde_json::Value,
    pub archive_complete: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provenance: Option<RequestProvenanceView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationClusterView {
    pub cluster_id: Uuid,
    pub explicit_session_id: Option<String>,
    pub updated_at: i64,
    pub request_count: i64,
    pub candidate_edge_count: i64,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationCursor {
    pub before_created_at: i64,
    pub before_request_id: Uuid,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationEdgeView {
    pub from_request_id: Option<Uuid>,
    pub to_request_id: Uuid,
    pub relation: String,
    pub confidence: f64,
    pub evidence: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationRequestView {
    #[serde(flatten)]
    pub request: RequestView,
    pub source: String,
    pub provenance: String,
    pub unlinked: bool,
    pub currency: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archive_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_request_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub execution: Option<crate::conversation::ExecutionMetadata>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationClusterDetail {
    pub cluster: ConversationClusterView,
    pub requests: Vec<ConversationRequestView>,
    pub edges: Vec<ConversationEdgeView>,
    pub has_more: bool,
    pub next_cursor: Option<ConversationCursor>,
    pub edges_truncated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogicalSessionSummary {
    pub session_id: String,
    pub session_name: Option<String>,
    pub task_kind: Option<String>,
    pub cluster_id: Option<Uuid>,
    pub unlinked: bool,
    pub key_id: Uuid,
    pub key_alias: String,
    pub model: String,
    pub protocol: String,
    pub last_status: String,
    pub last_activity_at: i64,
    pub active_requests: i64,
    pub requests: i64,
    pub errors: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub avg_duration_ms: Option<f64>,
    pub costs: Vec<UsageAnalysisCost>,
    /// Imported archive entries that have no authoritative request fact.
    pub archived_only_requests: i64,
    pub archived_only_errors: i64,
    pub archived_only_input_tokens: i64,
    pub archived_only_output_tokens: i64,
    pub archived_only_avg_duration_ms: Option<f64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogicalSessionListCursor {
    pub before_last_activity_at: i64,
    pub before_session_id: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogicalSessionListResponse {
    pub generated_at: i64,
    pub sessions: Vec<LogicalSessionSummary>,
    pub next_cursor: Option<LogicalSessionListCursor>,
}

#[derive(Clone, Debug, Serialize)]
pub struct LogicalSessionDetail {
    pub session_id: String,
    pub cluster_id: Option<Uuid>,
    pub unlinked: bool,
    pub requests: Vec<ConversationRequestView>,
    pub edges: Vec<ConversationEdgeView>,
    pub has_more: bool,
    pub next_cursor: Option<ConversationCursor>,
    pub edges_truncated: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StatsSummary {
    pub total_requests: i64,
    pub successful_requests: i64,
    pub failed_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Single-currency compatibility value. `None` means the projection spans currencies.
    pub total_cost: Option<String>,
    pub costs: Vec<UsageAnalysisCost>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsBucket {
    pub name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Single-currency compatibility value. `None` means the bucket spans currencies.
    pub cost: Option<String>,
    pub costs: Vec<UsageAnalysisCost>,
}

#[derive(Clone, Debug, Serialize)]
pub struct SelfStats {
    pub key_id: Uuid,
    pub summary: StatsSummary,
    pub by_model: Vec<StatsBucket>,
    pub by_day: Vec<StatsBucket>,
    pub errors: Vec<StatsBucket>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OperatorStats {
    pub summary: StatsSummary,
    pub by_model: Vec<StatsBucket>,
    pub by_day: Vec<StatsBucket>,
    pub errors: Vec<StatsBucket>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageAnalysisCost {
    pub currency: String,
    pub cost: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageAnalysisGenerationUnitsByModality {
    pub modality: String,
    pub currency: String,
    pub units: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageAnalysisGenerationUnitsByBillingUnit {
    pub billing_unit: String,
    pub currency: String,
    pub units: i64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct UsageAnalysisMetrics {
    pub requests: i64,
    pub success: i64,
    pub failed: i64,
    /// Uncached input tokens. Cached reads and cache writes are reported separately.
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub generation_units: i64,
    pub avg_duration_ms: Option<f64>,
    pub p95_duration_ms: Option<i64>,
    pub costs: Vec<UsageAnalysisCost>,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageAnalysisBucket {
    pub id: String,
    pub label: String,
    #[serde(flatten)]
    pub metrics: UsageAnalysisMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageAnalysisSessionBucket {
    pub id: String,
    pub label: String,
    pub key_id: Uuid,
    pub key_alias: String,
    pub unlinked: bool,
    #[serde(flatten)]
    pub metrics: UsageAnalysisMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageAnalysisTimeBucket {
    /// Inclusive UTC bucket start as Unix epoch milliseconds.
    pub bucket_start: i64,
    #[serde(flatten)]
    pub metrics: UsageAnalysisMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageAnalysisHeatmapBucket {
    /// Monday 00:00 UTC is 0; Sunday 23:00 UTC is 167.
    pub hour_of_week: i64,
    #[serde(flatten)]
    pub metrics: UsageAnalysisMetrics,
}

#[derive(Clone, Debug, Serialize)]
pub struct UsageAnalysisResponse {
    pub from_created_at: i64,
    pub to_created_at: i64,
    pub granularity: String,
    pub time_zone: String,
    pub p95_is_approximate: bool,
    pub p95_method: String,
    /// `stable_account`: upstream usage stays attached to one provider account across API/OAuth
    /// credential rotations. Historical requests are never relabelled as the current generation.
    pub upstream_grouping: String,
    pub summary: UsageAnalysisMetrics,
    pub generation_units_by_modality: Vec<UsageAnalysisGenerationUnitsByModality>,
    pub generation_units_by_billing_unit: Vec<UsageAnalysisGenerationUnitsByBillingUnit>,
    pub time_series: Vec<UsageAnalysisTimeBucket>,
    pub by_model: Vec<UsageAnalysisBucket>,
    pub by_key: Vec<UsageAnalysisBucket>,
    pub by_session: Vec<UsageAnalysisSessionBucket>,
    pub by_upstream: Vec<UsageAnalysisBucket>,
    pub by_protocol: Vec<UsageAnalysisBucket>,
    pub by_status: Vec<UsageAnalysisBucket>,
    pub errors: Vec<UsageAnalysisBucket>,
    pub heatmap: Vec<UsageAnalysisHeatmapBucket>,
}

#[derive(Clone, Debug, Serialize)]
pub struct TenantView {
    pub external_id: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPrice {
    pub id: Uuid,
    pub input_micros_per_million: i64,
    pub output_micros_per_million: i64,
    #[serde(default)]
    pub tiers: Vec<ModelPriceTier>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModelPriceTier {
    pub service_tier: String,
    pub input_micros_per_million: i64,
    pub cached_input_micros_per_million: i64,
    pub cache_write_micros_per_million: i64,
    pub output_micros_per_million: i64,
    pub source: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// Uncached input tokens. Protocol parsers normalize OpenAI's inclusive
    /// prompt count before constructing this value.
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub cache_write_tokens: i64,
    pub output_tokens: i64,
    pub service_tier: Option<String>,
}

impl TokenUsage {
    pub fn total_input_tokens(&self) -> i64 {
        self.input_tokens
            .saturating_add(self.cached_input_tokens)
            .saturating_add(self.cache_write_tokens)
    }

    pub fn total_tokens(&self) -> i64 {
        self.total_input_tokens().saturating_add(self.output_tokens)
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelPriceView {
    pub model: String,
    pub currency: String,
    pub input_per_million: String,
    pub output_per_million: String,
    pub source: String,
    pub updated_at: i64,
    pub tiers: Vec<ModelPriceTierView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ModelPriceTierView {
    pub service_tier: String,
    pub input_per_million: String,
    pub cached_input_per_million: String,
    pub cache_write_per_million: String,
    pub output_per_million: String,
    pub source: String,
    pub updated_at: i64,
    pub cache_price_estimated: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct GenerationPrice {
    pub id: Uuid,
    pub model: String,
    pub currency: String,
    pub billing_unit: String,
    pub price_per_unit: String,
    #[serde(skip)]
    pub micros_per_unit: i64,
}

impl GenerationPrice {
    pub fn reservation_price(&self) -> Option<ModelPrice> {
        Some(ModelPrice {
            id: self.id,
            input_micros_per_million: 0,
            output_micros_per_million: self.micros_per_unit.checked_mul(1_000_000)?,
            tiers: Vec::new(),
        })
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct GenerationAssetView {
    pub asset_id: Uuid,
    pub index: i64,
    pub mime_type: String,
    pub size_bytes: i64,
    pub filename: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct ArchivedGenerationAsset {
    pub asset_id: Uuid,
    pub index: i64,
    pub object_locator: String,
    pub mime_type: String,
    pub size_bytes: i64,
    pub filename: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct GenerationStagedAssets {
    pub attempt_nonce: Uuid,
    pub billed_units: i64,
    pub assets: Vec<ArchivedGenerationAsset>,
}

#[derive(Clone, Debug)]
pub struct GenerationAssetDownload {
    pub view: GenerationAssetView,
    pub object_locator: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct GenerationJobView {
    pub job_id: Uuid,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub model: String,
    pub driver: String,
    pub billing_unit: String,
    pub status: String,
    pub upstream_job_id: Option<String>,
    pub estimated_units: i64,
    pub billed_units: Option<i64>,
    pub cost: String,
    pub error_code: Option<String>,
    pub result: Option<serde_json::Value>,
    pub assets: Vec<GenerationAssetView>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OperatorGenerationJobView {
    #[serde(flatten)]
    pub job: GenerationJobView,
    pub tenant_external_id: String,
    pub key_id: Uuid,
    pub key_alias: String,
    pub currency: String,
}

#[derive(Clone, Debug)]
pub struct GenerationJobWork {
    pub job_id: Uuid,
    pub created_at: i64,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    /// Immutable route selected at admission. Historical jobs created before
    /// the snapshot migration may not have an unambiguous route id.
    pub model_route_id: Option<Uuid>,
    pub upstream_account_id: Uuid,
    pub reservation: UsageReservation,
    pub public_model: String,
    pub upstream_model: String,
    pub driver: String,
    pub status: String,
    pub request_object: String,
    pub upstream_job_id: Option<String>,
    pub submission_nonce: Option<Uuid>,
    pub staged_assets: Option<GenerationStagedAssets>,
    pub billing_unit: String,
    pub estimated_units: i64,
    pub attempt_count: i64,
    pub failure_count: i64,
}

#[derive(Clone, Debug)]
pub struct UsageReservation {
    pub id: Uuid,
    pub account_id: Uuid,
    pub key_id: Uuid,
    pub reserved_micros: i64,
    pub input_micros_per_million: i64,
    pub output_micros_per_million: i64,
    pub price_tiers: Vec<ModelPriceTier>,
    pub rate_window_start: i64,
    pub reserved_tokens: i64,
}

pub fn micros_to_decimal_string(micros: i64) -> String {
    let sign = if micros < 0 { "-" } else { "" };
    let absolute = micros.unsigned_abs();
    let whole = absolute / MONEY_SCALE as u64;
    let fraction = absolute % MONEY_SCALE as u64;
    if fraction == 0 {
        format!("{sign}{whole}")
    } else {
        format!("{sign}{whole}.{fraction:06}")
            .trim_end_matches('0')
            .to_owned()
    }
}

pub fn priced_tokens(tokens: i64, micros_per_million: i64) -> i64 {
    if tokens <= 0 || micros_per_million <= 0 {
        return 0;
    }
    let numerator = i128::from(tokens) * i128::from(micros_per_million);
    i64::try_from((numerator + 999_999) / 1_000_000).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::priced_tokens;

    #[test]
    fn token_price_saturates_instead_of_wrapping() {
        assert_eq!(priced_tokens(i64::MAX, i64::MAX), i64::MAX);
        assert_eq!(priced_tokens(-1, i64::MAX), 0);
    }
}
