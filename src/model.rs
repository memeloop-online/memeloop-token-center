use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const MONEY_SCALE: i64 = 1_000_000;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyPolicy {
    #[serde(default)]
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

impl KeyPolicy {
    pub fn allows_model(&self, model: &str) -> bool {
        self.allowed_models.is_empty()
            || self
                .allowed_models
                .iter()
                .any(|allowed| allowed == "*" || allowed == model)
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
        self.scopes.iter().any(|scope| {
            scope == "*"
                || scope == required
                || scope
                    .strip_suffix(":*")
                    .is_some_and(|prefix| required.starts_with(&format!("{prefix}:")))
        })
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
}

#[derive(Clone, Debug, Serialize)]
pub struct RequestDetail {
    #[serde(flatten)]
    pub view: RequestView,
    pub request_body: serde_json::Value,
    pub response_body: serde_json::Value,
    pub archive_complete: bool,
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
pub struct ConversationEdgeView {
    pub from_request_id: Option<Uuid>,
    pub to_request_id: Uuid,
    pub relation: String,
    pub confidence: f64,
    pub evidence: serde_json::Value,
}

#[derive(Clone, Debug, Serialize)]
pub struct ConversationClusterDetail {
    pub cluster: ConversationClusterView,
    pub requests: Vec<RequestView>,
    pub edges: Vec<ConversationEdgeView>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct StatsSummary {
    pub total_requests: i64,
    pub successful_requests: i64,
    pub failed_requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_cost: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatsBucket {
    pub name: String,
    pub requests: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct SelfStats {
    pub key_id: Uuid,
    pub summary: StatsSummary,
    pub by_model: Vec<StatsBucket>,
    pub by_day: Vec<StatsBucket>,
    pub errors: Vec<StatsBucket>,
}

#[derive(Clone, Debug)]
pub struct ModelPrice {
    pub id: Uuid,
    pub input_micros_per_million: i64,
    pub output_micros_per_million: i64,
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
        })
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GenerationJobView {
    pub job_id: Uuid,
    pub created_at: i64,
    pub updated_at: i64,
    pub completed_at: Option<i64>,
    pub model: String,
    pub driver: String,
    pub status: String,
    pub upstream_job_id: Option<String>,
    pub estimated_units: i64,
    pub billed_units: Option<i64>,
    pub cost: String,
    pub error_code: Option<String>,
    pub result: Option<serde_json::Value>,
}

#[derive(Clone, Debug)]
pub struct GenerationJobWork {
    pub job_id: Uuid,
    pub created_at: i64,
    pub tenant_id: Uuid,
    pub key_id: Uuid,
    pub upstream_account_id: Uuid,
    pub reservation: UsageReservation,
    pub public_model: String,
    pub upstream_model: String,
    pub driver: String,
    pub status: String,
    pub request_object: String,
    pub upstream_job_id: Option<String>,
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
    ((numerator + 999_999) / 1_000_000) as i64
}
