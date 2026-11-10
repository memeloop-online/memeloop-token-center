#[cfg(test)]
use std::sync::atomic::AtomicU64;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use super::{ProxyLifecycleActivity, finish_proxy_request_with_retry};
use crate::{
    db::{Database, FinishProxyRequest, FinishProxyRequestResult, ProxyRequestUpstreamAttribution},
    model::{AuthenticatedKey, TokenUsage, UsageReservation},
};

const RETRY_INTERVAL: Duration = Duration::from_secs(1);
const RECOVERY_MIN_INTERVAL: Duration = Duration::from_secs(5);
const RECOVERY_MAX_INTERVAL: Duration = Duration::from_secs(300);
const RECOVERY_BATCH_SIZE: usize = 32;
const RECEIPT_PUBLICATION_CONCURRENCY: usize = 4;

/// Admission rejection or explicit transport non-delivery permits a zero-cost
/// settlement. A request-stage error alone leaves provider execution unknown.
fn confirmed_not_executed(input: &FinishProxyRequest<'_>, transport_not_delivered: bool) -> bool {
    use crate::model::{RequestTerminalCause, RequestUsageBasis};
    (transport_not_delivered
        || matches!(
            input.terminal_cause,
            Some(RequestTerminalCause::GatewayPreAdmission)
        ))
        && input.usage == TokenUsage::default()
        && input.usage_basis == Some(RequestUsageBasis::NotObserved)
        && (400..=599).contains(&input.status_code)
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(remote = "UsageReservation")]
struct ReservationWire {
    id: Uuid,
    account_id: Uuid,
    key_id: Uuid,
    enforcement_mode: crate::model::EnforcementMode,
    reserved_micros: i64,
    input_micros_per_million: i64,
    output_micros_per_million: i64,
    price_tiers: Vec<crate::model::ModelPriceTier>,
    rate_window_start: i64,
    reserved_tokens: i64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(remote = "crate::model::KeyPolicy", deny_unknown_fields)]
struct PolicyWire {
    allowed_models: Vec<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    requests_per_minute: Option<u32>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    tokens_per_minute: Option<u64>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    max_concurrency: Option<u32>,
    enforcement_mode: crate::model::EnforcementMode,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    daily_budget: Option<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    weekly_budget: Option<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    lifetime_budget: Option<String>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(remote = "AuthenticatedKey", deny_unknown_fields)]
struct KeyWire {
    key_id: Uuid,
    tenant_id: Uuid,
    principal_id: Uuid,
    account_id: Uuid,
    alias: String,
    currency: String,
    credential_generation: i64,
    #[serde(with = "PolicyWire")]
    policy: crate::model::KeyPolicy,
}

#[derive(Clone, Serialize, Deserialize)]
struct UsageWire {
    input_tokens: i64,
    cached_input_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    service_tier: Option<String>,
    cache_coverage: Option<(i64, Option<i64>)>,
}

impl From<TokenUsage> for UsageWire {
    fn from(usage: TokenUsage) -> Self {
        Self {
            input_tokens: usage.input_tokens,
            cached_input_tokens: usage.cached_input_tokens,
            cache_write_tokens: usage.cache_write_tokens,
            output_tokens: usage.output_tokens,
            service_tier: usage.service_tier,
            cache_coverage: usage
                .cache_coverage
                .map(|coverage| (coverage.read_tokens, coverage.inclusive_input_tokens)),
        }
    }
}

impl UsageWire {
    fn usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: self.input_tokens,
            cached_input_tokens: self.cached_input_tokens,
            cache_write_tokens: self.cache_write_tokens,
            output_tokens: self.output_tokens,
            service_tier: self.service_tier.clone(),
            cache_coverage: self
                .cache_coverage
                .map(
                    |(read_tokens, inclusive_input_tokens)| crate::model::CacheUsageCoverage {
                        read_tokens,
                        inclusive_input_tokens,
                    },
                ),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct OwnedConversation {
    #[serde(with = "KeyWire")]
    key: AuthenticatedKey,
    request_json: serde_json::Value,
    hints: crate::conversation::ConversationHints,
    client_name: Option<String>,
    upstream_response_id: Option<String>,
}

impl OwnedConversation {
    pub(crate) fn capture(input: crate::db::ProxyConversationInput<'_>) -> Self {
        Self {
            key: input.key.clone(),
            request_json: input.request_json.clone(),
            hints: input.hints.clone(),
            client_name: input.client_name.map(str::to_owned),
            upstream_response_id: input.upstream_response_id.map(str::to_owned),
        }
    }
}

pub(crate) struct PreparedConversation {
    pub(crate) conversation: OwnedConversation,
    pub(crate) retained: Box<dyn Send>,
}

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
enum ConversationState {
    #[default]
    NotRequested,
    Pending,
    Enqueued,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Snapshot {
    version: u32,
    pub(crate) receipt_id: Uuid,
    pub(crate) request_id: Uuid,
    tenant_id: Uuid,
    #[serde(with = "ReservationWire")]
    reservation: UsageReservation,
    input_token_ceiling: i64,
    output_token_ceiling: i64,
    requested_service_tier: Option<String>,
    status_code: i64,
    duration_ms: i64,
    first_output_ms: Option<i64>,
    generation_duration_ms: Option<i64>,
    usage: UsageWire,
    usage_basis: Option<crate::model::RequestUsageBasis>,
    terminal_cause: Option<String>,
    error_code: Option<String>,
    response_object: String,
    #[serde(default)]
    supplier_summary: Option<String>,
    routing_session_id: Option<String>,
    routing_terminal_observed_at: Option<i64>,
    conversation: Option<OwnedConversation>,
    keep_selected: bool,
    last_dispatched: Option<(Uuid, Uuid)>,
    pub(crate) response_id: Option<String>,
    pub(crate) queued_billable: bool,
    claim: Option<crate::db::PendingProxySettlementClaim>,
    #[serde(default)]
    transport_not_delivered: bool,
    #[serde(default)]
    operational_terminal: Option<crate::db::PendingProxyOperationalTerminal>,
    #[serde(default)]
    financial_ack: Option<crate::db::PendingProxySettlementAck>,
    #[serde(default)]
    separate_conversation: bool,
    #[serde(default)]
    conversation_state: ConversationState,
    #[serde(default)]
    reconciliation_required: bool,
}

impl Snapshot {
    pub(crate) fn capture(
        input: FinishProxyRequest<'_>,
        attribution: ProxyRequestUpstreamAttribution,
        response_id: Option<String>,
        queued_billable: bool,
        stage: &'static str,
    ) -> Self {
        let (keep_selected, last_dispatched) = match attribution {
            ProxyRequestUpstreamAttribution::KeepSelected => (true, None),
            ProxyRequestUpstreamAttribution::LastDispatched(pair) => (false, pair),
        };
        let operational_terminal = crate::db::PendingProxyOperationalTerminal {
            status_code: input.status_code,
            error_code: input.error_code.map(str::to_owned),
            terminal_cause: input.terminal_cause.map(|cause| cause.as_str().to_owned()),
            stage: Some(stage.to_owned()),
            completed_at: crate::db::unix_millis(),
            duration_ms: input.duration_ms,
            first_output_ms: input.first_output_ms,
            generation_duration_ms: input.generation_duration_ms,
            response_object: input.response_object.to_owned(),
        };
        Self {
            version: 1,
            receipt_id: Uuid::now_v7(),
            request_id: input.request_id,
            tenant_id: input.tenant_id,
            reservation: input.reservation.clone(),
            input_token_ceiling: input.input_token_ceiling,
            output_token_ceiling: input.output_token_ceiling,
            requested_service_tier: input.requested_service_tier.map(str::to_owned),
            status_code: input.status_code,
            duration_ms: input.duration_ms,
            first_output_ms: input.first_output_ms,
            generation_duration_ms: input.generation_duration_ms,
            usage: input.usage.into(),
            usage_basis: input.usage_basis,
            terminal_cause: input.terminal_cause.map(|cause| cause.as_str().to_owned()),
            error_code: input.error_code.map(str::to_owned),
            response_object: input.response_object.to_owned(),
            supplier_summary: None,
            routing_session_id: input.routing_session_id.map(str::to_owned),
            routing_terminal_observed_at: input.routing_terminal_observed_at,
            conversation: input.conversation.map(OwnedConversation::capture),
            keep_selected,
            last_dispatched,
            response_id,
            queued_billable,
            claim: None,
            transport_not_delivered: false,
            operational_terminal: Some(operational_terminal),
            financial_ack: None,
            separate_conversation: true,
            conversation_state: ConversationState::NotRequested,
            reconciliation_required: false,
        }
    }

    fn attribution(&self) -> ProxyRequestUpstreamAttribution {
        if self.keep_selected {
            ProxyRequestUpstreamAttribution::KeepSelected
        } else {
            ProxyRequestUpstreamAttribution::LastDispatched(self.last_dispatched)
        }
    }

    pub(crate) fn with_supplier_summary(mut self, body: &[u8]) -> Self {
        if self.status_code >= 400
            && body.len() <= 2048
            && let Ok(body) = std::str::from_utf8(body)
        {
            let envelope = format!("inline-json:{body}");
            if crate::supplier_error::supplier_error_from_inline_json(Some(&envelope)).is_some() {
                self.supplier_summary = Some(envelope);
            }
        }
        self
    }

    async fn persist_supplier_summary(
        &self,
        database: &Database,
    ) -> Result<(), crate::error::AppError> {
        if let Some(summary) = &self.supplier_summary {
            database
                .body_archive()?
                .publish_supplier_summary(self.tenant_id, self.request_id, summary)
                .await?;
        }
        Ok(())
    }

    fn financial_evidence_unavailable(&self) -> bool {
        self.usage_basis != Some(crate::model::RequestUsageBasis::ProviderReported)
            && !self.transport_not_delivered
            && self.terminal_cause.as_deref()
                != Some(crate::model::RequestTerminalCause::GatewayPreAdmission.as_str())
    }

    fn operational_digest(&self) -> Result<String, crate::error::AppError> {
        let attribution = if self.keep_selected {
            serde_json::json!({"kind": "keep_selected"})
        } else {
            serde_json::json!({"kind": "last_dispatched", "ids": self.last_dispatched})
        };
        let bytes = serde_json::to_vec(&serde_json::json!({
            "version": 1, "claim": self.claim, "receipt_id": self.receipt_id,
            "terminal": self.operational_terminal, "attribution": attribution,
        }))
        .map_err(|_| crate::error::AppError::internal("pending_terminal_operational_digest_1"))?;
        Ok(blake3::hash(&bytes).to_hex().to_string())
    }

    fn terminal(&self) -> Result<FinishProxyRequest<'_>, crate::error::AppError> {
        use crate::model::RequestTerminalCause::*;
        let terminal_cause = self
            .terminal_cause
            .as_ref()
            .map(|value| {
                [
                    GatewayPreAdmission,
                    UpstreamHttpStatus,
                    ClientDisconnected,
                    TransportTimeout,
                    TransportOuterDeadline,
                    TransportConnectionReset,
                    TransportHttp2Reset,
                    TransportHttp2GoAway,
                    TransportBody,
                    TransportDecode,
                    TransportRequest,
                    TransportOther,
                    Http2Reset,
                    Http2GoAway,
                    ReadTimeout,
                    RequestTimeout,
                    StreamReadError,
                ]
                .into_iter()
                .find(|cause| cause.as_str() == value.as_str())
                .ok_or(crate::error::AppError::internal(
                    "pending_terminal_terminal_1",
                ))
            })
            .transpose()?;
        if self.version != 1
            || self
                .input_token_ceiling
                .checked_add(self.output_token_ceiling)
                != Some(self.reservation.reserved_tokens)
        {
            return Err(crate::error::AppError::internal(
                "pending_terminal_terminal_2",
            ));
        }
        Ok(FinishProxyRequest {
            terminal_cause,
            usage_basis: self.usage_basis,
            first_output_ms: self.first_output_ms,
            generation_duration_ms: self.generation_duration_ms,
            request_id: self.request_id,
            tenant_id: self.tenant_id,
            reservation: &self.reservation,
            input_token_ceiling: self.input_token_ceiling,
            output_token_ceiling: self.output_token_ceiling,
            requested_service_tier: self.requested_service_tier.as_deref(),
            status_code: self.status_code,
            duration_ms: self.duration_ms,
            usage: self.usage.usage(),
            error_code: self.error_code.as_deref(),
            response_object: &self.response_object,
            routing_session_id: self.routing_session_id.as_deref(),
            routing_terminal_observed_at: self.routing_terminal_observed_at,
            conversation: self
                .conversation
                .as_ref()
                .filter(|_| !self.separate_conversation || self.claim.is_none())
                .map(|conversation| crate::db::ProxyConversationInput {
                    key: &conversation.key,
                    request_json: &conversation.request_json,
                    hints: &conversation.hints,
                    client_name: conversation.client_name.as_deref(),
                    upstream_response_id: conversation.upstream_response_id.as_deref(),
                }),
        })
    }
}

struct Pending {
    snapshot: Arc<Snapshot>,
    activity: Option<ProxyLifecycleActivity>,
    retained: Option<Box<dyn Send>>,
    running: bool,
    retry_after: tokio::time::Instant,
    durable: bool,
    applied: bool,
    reconcile: bool,
    operational_running: bool,
    operational_acknowledged: bool,
    unknown_annotated: bool,
    conversation_enqueued: bool,
    operational_reconcile: bool,
    preparation: Option<Preparation>,
    projection_retained: Option<Box<dyn Send>>,
    /// 对话还在异步整理时，收据还不能对外比较。整理完才会带上 request_json。
    conversation_pending: bool,
    preparation_running: bool,
    recovered: bool,
    publication_running: bool,
    publication_retry_after: tokio::time::Instant,
    snapshot_revision: u64,
    published_revision: Option<u64>,
    retiring: bool,
}

type Preparation =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<PreparedConversation>> + Send>>;

const AWAITING_EVIDENCE_CAPACITY: usize = 65_536;

#[derive(Default)]
struct AwaitingEvidence {
    entries: HashMap<Uuid, ([u8; 32], Option<[u8; 32]>)>,
}

impl AwaitingEvidence {
    fn remember(&mut self, id: Uuid, digest: [u8; 32]) {
        // Keep the admitted working set stable across ordered scans. FIFO/LRU
        // eviction would turn an oversized archive into an all-miss ring.
        if !self.entries.contains_key(&id) && self.entries.len() == AWAITING_EVIDENCE_CAPACITY {
            return;
        }
        self.entries.insert(id, (digest, None));
    }
}

pub(crate) struct Journal {
    directory: PathBuf,
    archive: Option<crate::archive::ArchiveStore>,
    pending: Mutex<HashMap<Uuid, Pending>>,
    awaiting_evidence: Mutex<AwaitingEvidence>,
    claims: Mutex<HashMap<Uuid, ClaimState>>,
    wake: Notify,
    progress: Notify,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    recovery_worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    tasks: Mutex<Vec<(Work, tokio::task::JoinHandle<()>)>>,
    publication_permits: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    publication_failures: AtomicU64,
    #[cfg(test)]
    writer_gate: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    publication_gate: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    retirement_gate: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    preparation_gate: Arc<tokio::sync::Semaphore>,
}

struct ClaimState {
    claim: crate::db::PendingProxySettlementClaim,
    transport_not_delivered: bool,
    unknown: bool,
    running: bool,
}

enum Work {
    Receipt(Uuid),
    Operational(Uuid),
    Unknown(Uuid),
    Preparation(Uuid),
    Publication(Uuid),
}

impl Journal {
    #[cfg(test)]
    pub(crate) fn new(directory: PathBuf, database: Database) -> Arc<Self> {
        Self::create(directory, database, None)
    }

    pub(crate) fn with_archive(
        directory: PathBuf,
        database: Database,
        archive: crate::archive::ArchiveStore,
    ) -> Arc<Self> {
        Self::create(directory, database, Some(archive))
    }

    fn create(
        directory: PathBuf,
        database: Database,
        archive: Option<crate::archive::ArchiveStore>,
    ) -> Arc<Self> {
        let journal = Arc::new(Self {
            directory,
            archive,
            pending: Mutex::new(HashMap::new()),
            awaiting_evidence: Mutex::new(AwaitingEvidence::default()),
            claims: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            progress: Notify::new(),
            worker: Mutex::new(None),
            recovery_worker: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            publication_permits: Arc::new(tokio::sync::Semaphore::new(
                RECEIPT_PUBLICATION_CONCURRENCY,
            )),
            #[cfg(test)]
            publication_failures: AtomicU64::new(0),
            #[cfg(test)]
            writer_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            publication_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            retirement_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            preparation_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let recovery_database = database.clone();
        let weak = Arc::downgrade(&journal);
        let worker = tokio::spawn(async move {
            while let Some(journal) = weak.upgrade() {
                journal.reap().await;
                journal.start_operational(&database);
                journal.start_preparations();
                journal.start_pending(&database);
                journal.start_unknown(&database);
                journal.progress.notify_waiters();
                tokio::select! {
                    _ = journal.wake.notified() => {},
                    _ = tokio::time::sleep(RETRY_INTERVAL) => {},
                }
                drop(journal);
            }
        });
        *journal
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(worker);
        let weak = Arc::downgrade(&journal);
        let recovery = tokio::spawn(async move {
            let mut interval = RECOVERY_MIN_INTERVAL;
            while let Some(journal) = weak.upgrade() {
                let progressed = journal.recover(&recovery_database).await;
                journal.wake.notify_one();
                drop(journal);
                interval = if progressed {
                    RECOVERY_MIN_INTERVAL
                } else {
                    interval.saturating_mul(2).min(RECOVERY_MAX_INTERVAL)
                };
                tokio::time::sleep(interval).await;
            }
        });
        *journal
            .recovery_worker
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(recovery);
        journal
    }

    pub(crate) async fn drain(&self) {
        self.wake.notify_one();
        loop {
            let progress = self.progress.notified();
            tokio::pin!(progress);
            progress.as_mut().enable();
            let drained = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .values()
                .all(|entry| {
                    entry.reconcile
                        && entry.durable
                        && entry.published_revision == Some(entry.snapshot_revision)
                        && !entry.publication_running
                        && !entry.running
                        && !entry.operational_running
                        && !entry.conversation_pending
                        && (entry.snapshot.claim.is_none()
                            || entry.operational_acknowledged
                            || entry.operational_reconcile)
                        && (!entry.unknown_annotated || entry.conversation_enqueued)
                })
                && self
                    .claims
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_empty()
                && self
                    .tasks
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .is_empty();
            if drained {
                return;
            }
            progress.await;
        }
    }

    async fn reap(&self) {
        let finished = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|error| error.into_inner());
            let mut finished = Vec::new();
            let mut index = 0;
            while index < tasks.len() {
                if tasks[index].1.is_finished() {
                    finished.push(tasks.swap_remove(index));
                } else {
                    index += 1;
                }
            }
            finished
        };
        for (work, task) in finished {
            if task.await.is_err() {
                let request_id = match work {
                    Work::Receipt(receipt_id) => {
                        let mut pending = self
                            .pending
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if let Some(entry) = pending.get_mut(&receipt_id) {
                            entry.running = false;
                            entry.snapshot.request_id
                        } else {
                            continue;
                        }
                    }
                    Work::Operational(receipt_id) => {
                        let mut pending = self
                            .pending
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        if let Some(entry) = pending.get_mut(&receipt_id) {
                            entry.operational_running = false;
                            entry.snapshot.request_id
                        } else {
                            continue;
                        }
                    }
                    Work::Unknown(request_id) => {
                        if let Some(entry) = self
                            .claims
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .get_mut(&request_id)
                        {
                            entry.running = false;
                        }
                        request_id
                    }
                    Work::Preparation(receipt_id) => {
                        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(entry) = pending.get_mut(&receipt_id) else {
                            continue;
                        };
                        entry.preparation_running = false;
                        entry.conversation_pending = false;
                        // Persisted ConversationState::Pending stays unchanged:
                        // an interrupted future is not a completed projection.
                        entry.snapshot.request_id
                    }
                    Work::Publication(receipt_id) => {
                        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(entry) = pending.get_mut(&receipt_id) else {
                            continue;
                        };
                        entry.publication_running = false;
                        entry.publication_retry_after =
                            tokio::time::Instant::now() + RETRY_INTERVAL;
                        entry.snapshot.request_id
                    }
                };
                tracing::error!(%request_id, financial_state = "unknown", "pending terminal worker interrupted; evidence retained for supervised retry");
            }
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in pending
            .values_mut()
            .filter(|entry| entry.durable && entry.reconcile && !entry.running)
        {
            if entry.snapshot.separate_conversation {
                // Keep the small recovery seed until source ownership transfers.
                continue;
            }
            // An unknown receipt keeps its parsed conversation only while
            // that is the sole copy: once the projection outbox row holds
            // the payload, the evidence is durable and the compact disk
            // receipt identifies the financial hold. Retaining the
            // conversation after that is what made gateway memory grow
            // without bound under quota-rejection traffic, and a restart
            // already recovers from the conversation-less disk receipt, so
            // nothing after enqueue may depend on the in-memory value.
            let keep_unknown_conversation = entry.unknown_annotated
                && entry.snapshot.claim.is_some()
                && entry.snapshot.usage_basis
                    != Some(crate::model::RequestUsageBasis::ProviderReported)
                && !entry.conversation_enqueued;
            if !keep_unknown_conversation
                && let Some(snapshot) = Arc::get_mut(&mut entry.snapshot)
                && snapshot.conversation.take().is_some()
            {
                entry.snapshot_revision += 1;
            }
            // The budgeted body can leave once its projection is durable.
            // Until then the permit stays with the unenqueued evidence.
            if !keep_unknown_conversation || entry.conversation_enqueued {
                entry.projection_retained = None;
            }
        }
    }

    pub(crate) fn submit_with_preparation(
        &self,
        mut snapshot: Snapshot,
        retained: impl Send + 'static,
        conversation_seed: Option<OwnedConversation>,
        preparation: impl std::future::Future<Output = Option<PreparedConversation>> + Send + 'static,
    ) {
        snapshot.conversation_state = if conversation_seed.is_some() {
            ConversationState::Pending
        } else {
            ConversationState::NotRequested
        };
        let request_id = snapshot.request_id;
        let receipt_id = snapshot.receipt_id;
        snapshot.claim = self
            .claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&request_id)
            .map(|entry| {
                snapshot.transport_not_delivered = entry.transport_not_delivered;
                entry.claim
            });
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(existing) = pending
            .values()
            .find(|entry| entry.snapshot.request_id == request_id)
        {
            if snapshot.claim.is_none() {
                snapshot.claim = existing.snapshot.claim.clone();
                snapshot.transport_not_delivered = existing.snapshot.transport_not_delivered;
            }
            tracing::error!(%request_id, %receipt_id, financial_state = "unknown", "multiple terminal receipts retained for database arbitration");
        }
        if snapshot.claim.is_none() || self.archive.is_none() {
            snapshot.separate_conversation = false;
        } else if snapshot.separate_conversation {
            snapshot.conversation = conversation_seed;
        }
        let conversation_pending = !snapshot.separate_conversation
            || snapshot.conversation_state == ConversationState::Pending;
        pending.insert(
            receipt_id,
            Pending {
                snapshot: Arc::new(snapshot),
                activity: None,
                retained: Some(Box::new(retained)),
                running: false,
                retry_after: tokio::time::Instant::now(),
                durable: false,
                applied: false,
                reconcile: false,
                operational_running: false,
                operational_acknowledged: false,
                unknown_annotated: false,
                conversation_enqueued: false,
                operational_reconcile: false,
                preparation: Some(Box::pin(preparation)),
                projection_retained: None,
                conversation_pending,
                preparation_running: false,
                recovered: false,
                publication_running: false,
                publication_retry_after: tokio::time::Instant::now(),
                snapshot_revision: 0,
                published_revision: None,
                retiring: false,
            },
        );
        tracing::info!(%request_id, %receipt_id, financial_state = "pending_unacknowledged", "terminal responsibility accepted in memory; journal durability not acknowledged");
        self.wake.notify_one();
    }

    /// Persist the runtime outcome before the caller returns it to the client.
    /// Financial settlement and receipt publication stay asynchronous. Legacy
    /// claimless requests retain their combined background finish path.
    pub(crate) async fn record_operational_observation(
        &self,
        database: &Database,
        request_id: Uuid,
    ) -> Result<(), crate::error::AppError> {
        let receipt_id = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .find(|entry| entry.snapshot.request_id == request_id)
            .map(|entry| entry.snapshot.receipt_id);
        if let Some(receipt_id) = receipt_id {
            self.record_operational_receipt(database, receipt_id).await
        } else {
            Ok(())
        }
    }

    async fn record_operational_receipt(
        &self,
        database: &Database,
        receipt_id: Uuid,
    ) -> Result<(), crate::error::AppError> {
        let snapshot = {
            let pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let Some(entry) = pending.get(&receipt_id) else {
                return Ok(());
            };
            if entry.operational_acknowledged {
                return Ok(());
            }
            entry.snapshot.clone()
        };
        snapshot.persist_supplier_summary(database).await?;
        let request_id = snapshot.request_id;
        if snapshot.claim.is_none() {
            // Legacy claimless finish owns both the financial transaction and
            // conversation. Its background attempt waits for preparation.
            return Ok(());
        }
        let receipt_id = snapshot.receipt_id;
        let claim = snapshot
            .claim
            .as_ref()
            .ok_or(crate::error::AppError::internal(
                "pending_terminal_record_operational_observation_1",
            ))?;
        let terminal =
            snapshot
                .operational_terminal
                .as_ref()
                .ok_or(crate::error::AppError::internal(
                    "pending_terminal_record_operational_observation_2",
                ))?;
        let expected_digest = snapshot.operational_digest()?;
        let ack = database
            .record_pending_proxy_operational_terminal(
                claim,
                receipt_id,
                terminal,
                snapshot.attribution(),
            )
            .await?;
        if ack.request_id != request_id
            || ack.reservation_id != claim.reservation_id
            || ack.fence != claim.fence
            || ack.receipt_id != receipt_id
            || ack.operational_digest != expected_digest
        {
            return Err(crate::error::AppError::Conflict(
                "operational acknowledgement mismatch".into(),
            ));
        }
        if let Some(entry) = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&receipt_id)
        {
            entry.operational_acknowledged = true;
        }
        tracing::info!(%request_id, %receipt_id, operational_state = "acknowledged", "runtime terminal recorded before the client response returned");
        Ok(())
    }

    pub(crate) fn register_claim(&self, claim: crate::db::PendingProxySettlementClaim) {
        self.claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                claim.request_id,
                ClaimState {
                    claim,
                    transport_not_delivered: false,
                    unknown: false,
                    running: false,
                },
            );
    }

    pub(crate) fn has_claim(&self, request_id: Uuid) -> bool {
        self.claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .contains_key(&request_id)
    }

    pub(crate) fn record_transport_non_delivery(
        &self,
        request_id: Uuid,
        all_attempts_not_delivered: bool,
    ) {
        if let Some(entry) = self
            .claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&request_id)
        {
            entry.transport_not_delivered = all_attempts_not_delivered;
        }
    }

    pub(crate) fn submit_cancelled(
        &self,
        request_id: Uuid,
        reservation: &UsageReservation,
        duration_ms: i64,
        stage: &'static str,
    ) {
        self.submit_unobserved(
            request_id,
            reservation,
            duration_ms,
            (499, "client_cancelled"),
            Some(crate::model::RequestTerminalCause::ClientDisconnected),
            stage,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn submit_unobserved(
        &self,
        request_id: Uuid,
        reservation: &UsageReservation,
        duration_ms: i64,
        terminal: (i64, &'static str),
        cause: Option<crate::model::RequestTerminalCause>,
        stage: &'static str,
    ) {
        let claim = self
            .claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&request_id)
            .map(|entry| entry.claim.clone());
        let Some(claim) = claim else {
            return;
        };
        let response_object = format!("gap://{request_id}/response");
        let snapshot = Snapshot::capture(
            FinishProxyRequest {
                terminal_cause: cause,
                usage_basis: Some(crate::model::RequestUsageBasis::NotObserved),
                first_output_ms: None,
                generation_duration_ms: None,
                request_id,
                tenant_id: claim.tenant_id,
                reservation,
                input_token_ceiling: claim.input_token_ceiling,
                output_token_ceiling: claim.output_token_ceiling,
                requested_service_tier: Some(&claim.requested_service_tier),
                status_code: terminal.0,
                duration_ms,
                usage: TokenUsage::default(),
                error_code: Some(terminal.1),
                response_object: &response_object,
                routing_session_id: None,
                routing_terminal_observed_at: None,
                conversation: None,
            },
            ProxyRequestUpstreamAttribution::KeepSelected,
            None,
            false,
            stage,
        );
        self.submit_with_preparation(snapshot, (), None, async { None });
    }

    pub(crate) fn mark_unknown(&self, request_id: Uuid) {
        if let Some(entry) = self
            .claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get_mut(&request_id)
        {
            entry.unknown = true;
            tracing::error!(%request_id, financial_state = "unknown", "dispatched request has no authoritative terminal receipt; registered financial responsibility remains pending");
            self.wake.notify_one();
        }
    }

    fn start_unknown(self: &Arc<Self>, database: &Database) {
        let mut claims = self
            .claims
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in claims
            .values_mut()
            .filter(|entry| entry.unknown && !entry.running)
        {
            entry.running = true;
            let journal = self.clone();
            let database = database.clone();
            let claim = entry.claim.clone();
            let request_id = claim.request_id;
            let task = tokio::spawn(async move {
                let result = database.mark_pending_proxy_settlement_unknown(&claim).await;
                if let Err(error) = &result {
                    tracing::error!(%request_id, financial_state = "unknown", error_category = error.diagnostic_category(), "unknown financial responsibility annotation requires retry");
                }
                let mut claims = journal
                    .claims
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if result.is_ok() {
                    claims.remove(&claim.request_id);
                } else if let Some(entry) = claims.get_mut(&claim.request_id) {
                    entry.running = false;
                }
            });
            self.tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((Work::Unknown(request_id), task));
        }
    }

    pub(crate) fn attach_activity(
        &self,
        request_id: Uuid,
        activity: ProxyLifecycleActivity,
    ) -> Result<(), ProxyLifecycleActivity> {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let Some(entry) = pending.values_mut().find(|entry| {
            entry.snapshot.request_id == request_id
                && !entry.reconcile
                && !(entry.operational_acknowledged
                    && entry.snapshot.usage_basis
                        != Some(crate::model::RequestUsageBasis::ProviderReported))
        }) else {
            return Err(activity);
        };
        entry.activity = Some(activity);
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn hold_writer_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.writer_gate.clone().acquire_owned().await.unwrap()
    }

    #[cfg(test)]
    pub(crate) async fn hold_publication_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.publication_gate.clone().acquire_owned().await.unwrap()
    }

    #[cfg(test)]
    pub(crate) async fn hold_preparation_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.preparation_gate.clone().acquire_owned().await.unwrap()
    }

    #[cfg(test)]
    pub(crate) fn directory_for_test(&self) -> &std::path::Path {
        &self.directory
    }

    #[cfg(test)]
    pub(crate) fn receipt_jsons_for_test(&self) -> Vec<serde_json::Value> {
        let mut receipts: Vec<serde_json::Value> = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .values()
            .filter(|entry| !entry.conversation_pending || entry.snapshot.separate_conversation)
            .filter_map(|entry| serde_json::to_value(entry.snapshot.as_ref()).ok())
            .collect();
        if let Ok(entries) = std::fs::read_dir(&self.directory) {
            for entry in entries
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
            {
                if let Ok(bytes) = std::fs::read(entry.path())
                    && let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes)
                    && !receipts
                        .iter()
                        .any(|existing| existing["receipt_id"] == value["receipt_id"])
                {
                    receipts.push(value);
                }
            }
        }
        receipts
    }

    #[cfg(test)]
    pub(crate) fn contains_receipt_for_test(&self, receipt_id: Uuid) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&receipt_id)
            .is_some_and(|entry| entry.durable)
            || self.directory.join(format!("{receipt_id}.json")).is_file()
    }

    #[cfg(test)]
    pub(crate) async fn hold_retirement_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.retirement_gate.clone().acquire_owned().await.unwrap()
    }

    #[cfg(test)]
    pub(crate) fn requires_arbitration_for_test(&self, receipt_id: Uuid) -> bool {
        let in_memory = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&receipt_id)
            .is_some_and(|entry| entry.reconcile || entry.operational_reconcile);
        in_memory
            || self.receipt_jsons_for_test().iter().any(|receipt| {
                receipt["receipt_id"] == receipt_id.to_string()
                    && receipt["reconciliation_required"] == true
            })
    }

    #[cfg(test)]
    pub(crate) async fn idle_conversation_retained_for_test(
        &self,
        receipt_id: Uuid,
    ) -> Option<bool> {
        self.reap().await;
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&receipt_id)
            .filter(|entry| !entry.running)
            .map(|entry| {
                if entry.snapshot.separate_conversation {
                    entry.preparation.is_some() || entry.projection_retained.is_some()
                } else {
                    entry.snapshot.conversation.is_some()
                }
            })
            .or_else(|| {
                self.directory
                    .join(format!("{receipt_id}.json"))
                    .is_file()
                    .then_some(false)
            })
    }

    #[cfg(test)]
    pub(crate) fn failed_publications_for_test(&self) -> u64 {
        self.publication_failures
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) async fn stop_for_test(&self) {
        let recovery = self
            .recovery_worker
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(recovery) = recovery {
            recovery.abort();
            let _ = recovery.await;
        }
        let worker = self
            .worker
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(worker) = worker {
            worker.abort();
            let _ = worker.await;
        }
        let tasks =
            std::mem::take(&mut *self.tasks.lock().unwrap_or_else(|error| error.into_inner()));
        for (_, task) in tasks {
            task.abort();
            let _ = task.await;
        }
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
        self.claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clear();
    }

    async fn recover(&self, database: &Database) -> bool {
        let mut progressed = false;
        if let Some(archive) = &self.archive {
            progressed |= self.recover_archive(archive, database).await;
        }
        let mut entries = match tokio::fs::read_dir(&self.directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return progressed,
            Err(error) => {
                tracing::error!(financial_state = "unknown", io_error_kind = ?error.kind(), "pending terminal journal directory is unavailable; recovery deferred");
                return progressed;
            }
        };
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            let Some(receipt_id) = path
                .file_stem()
                .and_then(|value| value.to_str())
                .and_then(|value| Uuid::parse_str(value).ok())
            else {
                continue;
            };
            if self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains_key(&receipt_id)
            {
                continue;
            }
            if !entry.file_type().await.is_ok_and(|kind| kind.is_file()) {
                continue;
            }
            let bytes = match tokio::fs::read(&path).await {
                Ok(bytes) => bytes,
                Err(error) => {
                    tracing::error!(%receipt_id, financial_state = "unknown", io_error_kind = ?error.kind(), "pending terminal receipt cannot be read; recovery deferred without acknowledgement");
                    continue;
                }
            };
            let snapshot = match serde_json::from_slice::<Snapshot>(&bytes) {
                Ok(snapshot)
                    if snapshot.receipt_id == receipt_id && snapshot.terminal().is_ok() =>
                {
                    snapshot
                }
                _ => {
                    tracing::error!(%receipt_id, financial_state = "unknown", "invalid pending terminal receipt retained for reconciliation");
                    continue;
                }
            };
            if let Some(archive) = &self.archive {
                match archive
                    .source_object_exists(&self.receipt_location(receipt_id))
                    .await
                {
                    // Remote state is authoritative; never overwrite it from a
                    // cache that may predate its latest acknowledged update.
                    Ok(true) | Err(_) => continue,
                    Ok(false) if !self.publish_receipt(&snapshot, None).await => continue,
                    Ok(false) => {}
                }
            }
            progressed |= self.recover_one(database, snapshot).await;
        }
        progressed
    }

    fn start_operational(self: &Arc<Self>, database: &Database) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in pending.values_mut().filter(|entry| {
            entry.snapshot.claim.is_some()
                && !entry.operational_running
                && !entry.operational_reconcile
                && (!entry.operational_acknowledged
                    || (entry.snapshot.financial_evidence_unavailable()
                        && !entry.unknown_annotated))
        }) {
            entry.operational_running = true;
            let journal = self.clone();
            let database = database.clone();
            let snapshot = entry.snapshot.clone();
            let operational_acknowledged = entry.operational_acknowledged;
            let receipt_id = snapshot.receipt_id;
            let task = tokio::spawn(async move {
                let request_id = snapshot.request_id;
                let result = async {
                    snapshot.persist_supplier_summary(&database).await?;
                    let claim = snapshot
                        .claim
                        .as_ref()
                        .ok_or(crate::error::AppError::internal("pending_terminal_start_operational_1"))?;
                    if !operational_acknowledged {
                        let terminal = snapshot.operational_terminal.as_ref().ok_or_else(|| {
                            crate::error::AppError::Conflict(
                                "operational observation unavailable".into(),
                            )
                        })?;
                        let expected_digest = snapshot.operational_digest()?;
                        let ack = database
                            .record_pending_proxy_operational_terminal(
                                claim,
                                receipt_id,
                                terminal,
                                snapshot.attribution(),
                            )
                            .await?;
                        if ack.request_id != request_id
                            || ack.reservation_id != claim.reservation_id
                            || ack.fence != claim.fence
                            || ack.receipt_id != receipt_id
                            || ack.operational_digest != expected_digest
                        {
                            return Err(crate::error::AppError::Conflict(
                                "operational acknowledgement mismatch".into(),
                            ));
                        }
                        if let Some(entry) = journal.pending.lock()
                            .unwrap_or_else(|error| error.into_inner()).get_mut(&receipt_id)
                        {
                            entry.operational_acknowledged = true;
                        }
                        tracing::info!(%request_id, %receipt_id, operational_state = "acknowledged", "runtime terminal recorded independently of financial settlement");
                    }
                    if snapshot.financial_evidence_unavailable() {
                        database
                            .mark_pending_proxy_settlement_unknown(claim)
                            .await?;
                        if let Some(entry) = journal.pending.lock()
                            .unwrap_or_else(|error| error.into_inner()).get_mut(&receipt_id)
                        {
                            entry.unknown_annotated = true;
                        }
                    }
                    Ok(())
                }
                .await;
                let acknowledged = result.is_ok();
                let mut pending = journal
                    .pending
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                if let Some(entry) = pending.get_mut(&receipt_id) {
                    entry.operational_running = false;
                    match result {
                        Ok(()) => {
                            if snapshot.financial_evidence_unavailable() {
                                entry.activity = None;
                            }
                        }
                        Err(crate::error::AppError::Conflict(_)) => {
                            entry.operational_reconcile = true;
                            tracing::error!(%request_id, %receipt_id, operational_acknowledged = entry.operational_acknowledged, financial_state = "unknown", "terminal observation authority conflict; receipt retained for arbitration");
                        }
                        Err(error) => {
                            tracing::error!(%request_id, %receipt_id, operational_acknowledged = entry.operational_acknowledged, error_category = error.diagnostic_category(), "runtime terminal or unknown annotation requires supervised retry");
                        }
                    }
                }
                if acknowledged {
                    journal.wake.notify_one();
                }
            });
            self.tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((Work::Operational(receipt_id), task));
        }
    }

    fn start_preparations(self: &Arc<Self>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for entry in pending.values_mut() {
            // Claimed requests already have a durable-source metadata seed.
            // Keep the original future/body owned until source publication and
            // enqueue succeed, without rereading/parsing it on the settlement lane.
            if entry.snapshot.separate_conversation && entry.snapshot.claim.is_some() {
                continue;
            }
            let Some(preparation) = entry.preparation.take() else {
                continue;
            };
            entry.preparation_running = true;
            let receipt_id = entry.snapshot.receipt_id;
            let journal = self.clone();
            let task = tokio::spawn(async move {
                #[cfg(test)]
                let _preparation = journal.preparation_gate.acquire().await.unwrap();
                let prepared = preparation.await;
                let mut pending = journal.pending.lock().unwrap_or_else(|e| e.into_inner());
                if let Some(entry) = pending.get_mut(&receipt_id) {
                    if let Some(prepared) = prepared {
                        Arc::make_mut(&mut entry.snapshot).conversation =
                            Some(prepared.conversation);
                        entry.projection_retained = Some(prepared.retained);
                        entry.snapshot_revision += 1;
                    }
                    entry.conversation_pending = false;
                    entry.preparation_running = false;
                }
                journal.wake.notify_one();
            });
            self.tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((Work::Preparation(receipt_id), task));
        }
    }

    fn start_pending(self: &Arc<Self>, database: &Database) {
        self.start_preparations();
        self.start_publications();
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in pending.values_mut().filter(|entry| {
            !entry.running
                && !entry.recovered
                && (entry.snapshot.claim.is_some() || !entry.conversation_pending)
                && tokio::time::Instant::now() >= entry.retry_after
        }) {
            entry.running = true;
            let journal = self.clone();
            let database = database.clone();
            let snapshot = entry.snapshot.clone();
            let applied = entry.applied;
            let receipt_id = snapshot.receipt_id;
            let task = tokio::spawn(async move {
                journal.attempt(database, snapshot, applied).await;
            });
            self.tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((Work::Receipt(receipt_id), task));
        }
    }

    fn start_publications(self: &Arc<Self>) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        for entry in pending.values_mut().filter(|entry| {
            !entry.recovered
                && !entry.retiring
                && !entry.publication_running
                && entry.published_revision != Some(entry.snapshot_revision)
                && (entry.snapshot.separate_conversation || !entry.conversation_pending)
                && tokio::time::Instant::now() >= entry.publication_retry_after
        }) {
            // Background storage work stays in the existing responsibility map.
            // No waiting task or request-admission limit is created here.
            let Ok(permit) = self.publication_permits.clone().try_acquire_owned() else {
                break;
            };
            entry.publication_running = true;
            let snapshot = entry.snapshot.clone();
            let receipt_id = snapshot.receipt_id;
            let revision = entry.snapshot_revision;
            let journal = self.clone();
            let task = tokio::spawn(async move {
                let _permit = permit;
                #[cfg(test)]
                {
                    let _writer = journal.writer_gate.acquire().await.unwrap();
                }
                #[cfg(test)]
                let _publication = journal.publication_gate.acquire().await.unwrap();
                let published = journal.publish_receipt(&snapshot, Some(revision)).await;
                if let Some(entry) = journal
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&receipt_id)
                {
                    entry.publication_running = false;
                    entry.publication_retry_after = tokio::time::Instant::now()
                        + if published {
                            Duration::ZERO
                        } else {
                            RETRY_INTERVAL
                        };
                }
                journal.wake.notify_one();
            });
            self.tasks
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((Work::Publication(receipt_id), task));
        }
    }

    fn acknowledge_publication(&self, snapshot: &Snapshot, revision: Option<u64>) {
        if let Some(revision) = revision
            && let Some(entry) = self
                .pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get_mut(&snapshot.receipt_id)
        {
            entry.durable = true;
            entry.published_revision = Some(revision);
        }
        tracing::info!(request_id = %snapshot.request_id, receipt_id = %snapshot.receipt_id,
            financial_state = "pending_durable",
            "terminal receipt persisted independently of financial settlement");
    }

    fn receipt_namespace(&self) -> &str {
        self.directory
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("local")
    }

    fn receipt_location(&self, receipt_id: Uuid) -> String {
        format!(
            "terminal-receipts/v1/{}/{}.json",
            self.receipt_namespace(),
            receipt_id
        )
    }

    async fn publish_receipt(&self, snapshot: &Snapshot, revision: Option<u64>) -> bool {
        let compact = snapshot.clone();
        // New claimed receipts contain only the small source metadata seed;
        // legacy receipts preserve their original conversation/digest.
        let result = async {
            let bytes = serde_json::to_vec(&compact)
                .map_err(|_| crate::error::AppError::internal("terminal_receipt_encode"))?;
            if let Some(archive) = &self.archive {
                archive
                    .put(
                        &self.receipt_location(snapshot.receipt_id),
                        bytes.clone().into(),
                    )
                    .await?;
                self.acknowledge_publication(snapshot, revision);
                // A compact local cache preserves existing process-restart
                // recovery. Remote publication, not this cache, is the ACK.
                let _ = write_compact_receipt(&self.directory, &bytes, snapshot.receipt_id).await;
            } else {
                write_compact_receipt(&self.directory, &bytes, snapshot.receipt_id)
                    .await
                    .map_err(|_| crate::error::AppError::storage("terminal_receipt_write"))?;
                self.acknowledge_publication(snapshot, revision);
            }
            Ok::<(), crate::error::AppError>(())
        }
        .await;
        if let Err(error) = result {
            #[cfg(test)]
            self.publication_failures
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            tracing::error!(request_id = %snapshot.request_id, receipt_id = %snapshot.receipt_id,
                error_category = error.diagnostic_category(), "terminal receipt publication failed; in-memory responsibility retained");
            false
        } else {
            true
        }
    }

    async fn recover_archive(
        &self,
        archive: &crate::archive::ArchiveStore,
        database: &Database,
    ) -> bool {
        let Ok(entries) = archive.terminal_receipts(self.receipt_namespace()) else {
            return false;
        };
        tokio::pin!(entries);
        let mut progressed = false;
        let mut visited = 0;
        while let Some(entry) = entries.next().await {
            visited += 1;
            if visited % RECOVERY_BATCH_SIZE == 0 {
                // Keep the backend's pagination stream alive across batches.
                // A new scan from its first unknown receipt would starve later work.
                tokio::time::sleep(RECOVERY_MIN_INTERVAL).await;
            }
            let Ok((location, version)) = entry else {
                break;
            };
            let Some(receipt_id) = location
                .rsplit('/')
                .next()
                .and_then(|name| name.strip_suffix(".json"))
                .and_then(|id| Uuid::parse_str(id).ok())
            else {
                continue;
            };
            if location != self.receipt_location(receipt_id)
                || self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .contains_key(&receipt_id)
            {
                continue;
            }
            if self
                .awaiting_evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .get(&receipt_id)
                .is_some_and(|(_, known)| known.as_ref() == Some(&version))
            {
                continue;
            }
            let Ok(bytes) = archive.get(&location).await else {
                continue;
            };
            let Ok(snapshot) = serde_json::from_slice::<Snapshot>(&bytes) else {
                continue;
            };
            if snapshot.receipt_id != receipt_id || snapshot.terminal().is_err() {
                continue;
            }
            progressed |= self.recover_one(database, snapshot).await;
            if let Some((_, known)) = self
                .awaiting_evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .get_mut(&receipt_id)
            {
                *known = Some(version);
            }
        }
        progressed
    }

    /// One recovery attempt at a time. The object/local stream remains the
    /// checkpoint while waiting; unknown predecessors never fill an in-memory
    /// backlog or prevent later receipts from being visited.
    async fn recover_one(&self, database: &Database, snapshot: Snapshot) -> bool {
        let digest = serde_json::to_vec(&snapshot)
            .ok()
            .map(|bytes| *blake3::hash(&bytes).as_bytes());
        if digest.is_some_and(|digest| {
            self.awaiting_evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .get(&snapshot.receipt_id)
                .is_some_and(|(known, _)| *known == digest)
        }) {
            return false;
        }
        self.awaiting_evidence
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .remove(&snapshot.receipt_id);
        let snapshot = Arc::new(snapshot);
        let receipt_id = snapshot.receipt_id;
        let applied = snapshot.financial_ack.as_ref().is_some_and(|ack| {
            snapshot.claim.as_ref().is_some_and(|claim| {
                ack.request_id == snapshot.request_id
                    && ack.receipt_id == receipt_id
                    && ack.reservation_id == claim.reservation_id
                    && ack.fence == claim.fence
                    && !ack.receipt_digest.is_empty()
            })
        });
        {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            let std::collections::hash_map::Entry::Vacant(entry) = pending.entry(receipt_id) else {
                return false;
            };
            entry.insert(Pending {
                snapshot: snapshot.clone(),
                activity: None,
                retained: None,
                running: true,
                retry_after: tokio::time::Instant::now(),
                durable: true,
                applied,
                reconcile: false,
                operational_running: true,
                operational_acknowledged: false,
                unknown_annotated: false,
                conversation_enqueued: snapshot.conversation_state == ConversationState::Enqueued,
                operational_reconcile: false,
                preparation: None,
                projection_retained: None,
                conversation_pending: snapshot.separate_conversation
                    && snapshot.conversation_state == ConversationState::Pending
                    && snapshot.conversation.is_none(),
                preparation_running: false,
                recovered: true,
                publication_running: false,
                publication_retry_after: tokio::time::Instant::now(),
                snapshot_revision: 0,
                published_revision: Some(0),
                retiring: false,
            });
        }
        if let Err(error) = self.record_operational_receipt(database, receipt_id).await {
            tracing::warn!(request_id = %snapshot.request_id, error_category = error.diagnostic_category(),
                "recovered terminal observation remains pending");
        }
        self.attempt(database.clone(), snapshot, applied).await;
        // No process-owned body/future belongs to a recovered item. Its durable
        // receipt survives a failed or unknown attempt and will be revisited.
        let completed = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&receipt_id)
            .is_none();
        if digest.is_some_and(|digest| {
            self.awaiting_evidence
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entries
                .get(&receipt_id)
                .is_some_and(|(known, _)| *known == digest)
        }) {
            return false;
        }
        completed
    }

    async fn attempt(&self, database: Database, snapshot: Arc<Snapshot>, mut applied: bool) {
        let request_id = snapshot.request_id;
        let receipt_id = snapshot.receipt_id;
        #[cfg(test)]
        {
            let _writer = self.writer_gate.acquire().await.unwrap();
        }
        let mut financial_ack = snapshot.financial_ack.clone();
        let mut reconcile = false;
        if !applied {
            let attribution = snapshot.attribution();
            let result = match snapshot
                .persist_supplier_summary(&database)
                .await
                .and_then(|()| snapshot.terminal())
            {
                Ok(input) => {
                    if let Some(claim) = snapshot.claim.as_ref() {
                        let evidence = match input.usage_basis {
                            Some(crate::model::RequestUsageBasis::ProviderReported) => {
                                Some(crate::db::PendingProxySettlementEvidence::ProviderReported)
                            }
                            _ if confirmed_not_executed(
                                &input,
                                snapshot.transport_not_delivered,
                            ) =>
                            {
                                Some(
                                    crate::db::PendingProxySettlementEvidence::ConfirmedNotExecuted,
                                )
                            }
                            _ => None,
                        };
                        if let Some(evidence) = evidence {
                            database
                                .finish_pending_proxy_settlement(
                                    claim,
                                    receipt_id,
                                    evidence,
                                    input,
                                    attribution,
                                )
                                .await
                                .and_then(|ack| {
                                    if ack.request_id == request_id
                                        && ack.receipt_id == receipt_id
                                        && ack.reservation_id == claim.reservation_id
                                        && ack.fence == claim.fence
                                        && !ack.receipt_digest.is_empty()
                                    {
                                        financial_ack = Some(ack.clone());
                                        Ok(Some(FinishProxyRequestResult::Finished {
                                            cost_micros: ack.cost_micros,
                                            usage_invalid: false,
                                        }))
                                    } else {
                                        Err(crate::error::AppError::internal(
                                            "pending_terminal_attempt_1",
                                        ))
                                    }
                                })
                        } else {
                            match database.mark_pending_proxy_settlement_unknown(claim).await {
                                Ok(()) => {
                                    reconcile = true;
                                    if let Some(entry) = self
                                        .pending
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .get_mut(&receipt_id)
                                    {
                                        entry.unknown_annotated = true;
                                        entry.reconcile = true;
                                        entry.activity = None;
                                    }
                                    Ok(None)
                                }
                                Err(error) => Err(error),
                            }
                        }
                    } else {
                        finish_proxy_request_with_retry(&database, input, None, attribution)
                            .await
                            .map(Some)
                    }
                }
                Err(error) => Err(error),
            };
            match result {
                Ok(Some(FinishProxyRequestResult::Finished { .. })) => applied = true,
                Ok(None) => {
                    reconcile = true;
                    tracing::info!(%request_id, %receipt_id, financial_state = "awaiting_evidence", "terminal awaits authoritative provider usage; receipt retained without zero-usage settlement");
                }
                Ok(Some(FinishProxyRequestResult::AlreadyFinished { .. })) => {
                    reconcile = true;
                    tracing::error!(%request_id, %receipt_id, financial_state = "unknown", "terminal already finished without receipt acknowledgement; durable evidence retained for arbitration");
                }
                Err(crate::error::AppError::Conflict(_)) => {
                    reconcile = true;
                    tracing::error!(%request_id, %receipt_id, financial_state = "unknown", "pending terminal authority conflict; receipt retained without acknowledgement");
                }
                Err(error) => {
                    tracing::error!(%request_id, %receipt_id, financial_state = "pending_durable", error_category = error.diagnostic_category(), "terminal settlement requires retry")
                }
            }
        }
        // The DB transaction already owns authoritative usage and its outbox.
        // Publish its ACK before any source, metadata or object-store await.
        if let Some(entry) = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&receipt_id)
        {
            if entry.snapshot.financial_ack.is_none() && financial_ack.is_some() {
                Arc::make_mut(&mut entry.snapshot).financial_ack = financial_ack.clone();
                entry.snapshot_revision += 1;
            }
            if entry.snapshot.reconciliation_required != reconcile {
                Arc::make_mut(&mut entry.snapshot).reconciliation_required = reconcile;
                entry.snapshot_revision += 1;
            }
            if applied && !entry.applied {
                tracing::info!(%request_id, %receipt_id, financial_state = "acknowledged", "terminal settlement acknowledged independently of receipt publication");
            }
            entry.applied |= applied;
            entry.reconcile = reconcile;
            if applied || reconcile {
                entry.activity = None;
            }
        }
        let mut source_durable = false;
        let projection = {
            let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending
                .get(&receipt_id)
                .filter(|entry| {
                    !entry.conversation_enqueued
                        && (!entry.conversation_pending
                            || (entry.snapshot.separate_conversation
                                && entry.snapshot.conversation.is_some()))
                })
                .map(|entry| entry.snapshot.clone())
        };
        if let Some(projection) = projection {
            let result = if !projection.separate_conversation || projection.claim.is_none() {
                if applied {
                    Ok(())
                } else {
                    Err(crate::error::AppError::NotFound)
                }
            } else {
                match (&projection.claim, &projection.conversation) {
                    (Some(claim), Some(conversation)) => {
                        let input = crate::db::ProxyConversationInput {
                            key: &conversation.key,
                            request_json: &conversation.request_json,
                            hints: &conversation.hints,
                            client_name: conversation.client_name.as_deref(),
                            upstream_response_id: conversation.upstream_response_id.as_deref(),
                        };
                        // Identity is bounded receipt metadata, not archive
                        // content. Publish it even while the durable source is
                        // pending, without acknowledging conversation completion
                        // or releasing receipt/body ownership below.
                        let identity_enqueued = database
                            .enqueue_operational_conversation_projection(claim, receipt_id, &input)
                            .await;
                        let source = crate::gateway_body::request_spool::request_body_object_key(
                            &request_id.to_string(),
                        );
                        let source_ready =
                            if let (Some(archive), Some(source)) = (&self.archive, source) {
                                archive.source_object_exists(&source).await.unwrap_or(false)
                            } else {
                                // Legacy local-only fixtures/receipts keep their original
                                // full conversation path; a new source seed needs a store.
                                false
                            };
                        source_durable = source_ready;
                        if let Err(error) = identity_enqueued {
                            Err(error)
                        } else if source_ready {
                            Ok(())
                        } else {
                            Err(crate::error::AppError::storage(
                                "terminal_conversation_source_pending",
                            ))
                        }
                    }
                    (_, None) if projection.conversation_state != ConversationState::Pending => {
                        Ok(())
                    }
                    (_, None) => Err(crate::error::AppError::NotFound),
                    (None, Some(_)) => Err(crate::error::AppError::Conflict(
                        "conversation receipt has no settlement claim".into(),
                    )),
                }
            };
            if result.is_ok() {
                if let Some(entry) = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&receipt_id)
                {
                    entry.conversation_enqueued = true;
                    entry.conversation_pending = false;
                    entry.preparation = None;
                    Arc::make_mut(&mut entry.snapshot).conversation_state =
                        ConversationState::Enqueued;
                    entry.snapshot_revision += 1;
                    entry.projection_retained = None;
                }
            } else {
                tracing::error!(%request_id, %receipt_id, "conversation projection remains pending independently of financial settlement");
            }
        }
        let operational_acknowledged = snapshot.claim.is_none()
            || self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&receipt_id)
                .is_some_and(|entry| entry.operational_acknowledged);
        #[cfg(test)]
        if applied && operational_acknowledged {
            let _retirement = self.retirement_gate.acquire().await.unwrap();
        }
        let conversation_complete = self
            .pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&receipt_id)
            .is_some_and(|entry| !entry.conversation_pending && entry.conversation_enqueued);
        // A publication owns its PUT and local-cache write until completion.
        // Pin retirement under the same lock used to start publishers so no
        // late writer can recreate a receipt after deletion.
        let removed = if applied && operational_acknowledged && conversation_complete {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.get_mut(&receipt_id).is_some_and(|entry| {
                if entry.publication_running {
                    return false;
                }
                entry.retiring = true;
                true
            })
        } else {
            false
        };
        if removed {
            if let Some(archive) = &self.archive
                && archive
                    .delete(&self.receipt_location(receipt_id))
                    .await
                    .is_err()
            {
                // Keep the already acknowledged receipt for safe idempotent retry.
                if let Some(entry) = self
                    .pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .get_mut(&receipt_id)
                {
                    entry.running = false;
                    entry.applied = true;
                    entry.retry_after = tokio::time::Instant::now() + RETRY_INTERVAL;
                }
                return;
            }
            let _ = tokio::fs::remove_file(self.directory.join(format!("{receipt_id}.json"))).await;
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let preparation_failed = pending.get(&receipt_id).is_some_and(|entry| {
            !entry.conversation_pending
                && !entry.preparation_running
                && entry.snapshot.conversation_state == ConversationState::Pending
                && entry.snapshot.conversation.is_none()
                && entry.preparation.is_none()
        });
        let compact_published = pending.get(&receipt_id).is_some_and(|entry| {
            entry.durable
                && !entry.publication_running
                && entry.published_revision == Some(entry.snapshot_revision)
        });
        let evict_unknown = compact_published
            && (reconcile || applied)
            && operational_acknowledged
            && (conversation_complete
                || preparation_failed
                || (reconcile && source_durable && !snapshot.financial_evidence_unavailable()));
        if removed || evict_unknown {
            if evict_unknown
                && !removed
                && conversation_complete
                && pending.get(&receipt_id).is_some_and(|entry| {
                    entry.recovered
                        && entry.unknown_annotated
                        && entry.snapshot.claim.is_some()
                        && entry.snapshot.financial_ack.is_none()
                        && entry.snapshot.financial_evidence_unavailable()
                })
                && let Ok(bytes) = serde_json::to_vec(snapshot.as_ref())
            {
                self.awaiting_evidence
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remember(receipt_id, *blake3::hash(&bytes).as_bytes());
            }
            pending.remove(&receipt_id);
            if removed {
                tracing::info!(%request_id, %receipt_id, financial_state = "settled", "terminal settlement acknowledged and receipt retired");
            } else {
                tracing::info!(%request_id, %receipt_id, financial_state = "unknown", "terminal reconciliation retained in durable storage; memory released");
            }
        } else if let Some(entry) = pending.get_mut(&receipt_id) {
            entry.running = false;
            entry.retry_after = tokio::time::Instant::now() + RETRY_INTERVAL;
            if (entry.durable || applied) && conversation_complete {
                entry.retained = None;
            }
            if reconcile || applied {
                entry.activity = None;
            }
        }
    }
}

async fn write_compact_receipt(
    directory: &std::path::Path,
    bytes: &[u8],
    receipt_id: Uuid,
) -> std::io::Result<()> {
    use tokio::io::AsyncWriteExt;
    tokio::fs::create_dir_all(directory).await?;
    let final_path = directory.join(format!("{receipt_id}.json"));
    let temporary = directory.join(format!("{receipt_id}.json.tmp"));
    let mut file = tokio::fs::File::create(&temporary).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    drop(file);
    tokio::fs::rename(&temporary, final_path).await?;
    tokio::fs::File::open(directory).await?.sync_all().await
}

impl Drop for Journal {
    fn drop(&mut self) {
        if let Some(worker) = self
            .recovery_worker
            .get_mut()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            worker.abort();
        }
        if let Some(worker) = self
            .worker
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            worker.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaiting_evidence_keeps_a_bounded_stable_working_set_and_accepts_updates() {
        let mut cache = AwaitingEvidence::default();
        for index in 0..AWAITING_EVIDENCE_CAPACITY {
            cache.remember(Uuid::from_u128(index as u128 + 1), [1; 32]);
        }
        let first = Uuid::from_u128(1);
        cache.remember(Uuid::from_u128(u128::MAX), [2; 32]);
        assert_eq!(cache.entries.len(), AWAITING_EVIDENCE_CAPACITY);
        assert_eq!(cache.entries.get(&first).unwrap().0, [1; 32]);
        cache.remember(first, [3; 32]);
        assert_eq!(cache.entries.get(&first).unwrap().0, [3; 32]);
        assert!(!cache.entries.contains_key(&Uuid::from_u128(u128::MAX)));
    }

    #[tokio::test]
    async fn unavailable_journal_never_acknowledges_memory_as_durable_or_hot_loops() {
        let directory = tempfile::tempdir().unwrap();
        let database = Database::connect(&format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("retry.db").display()
        ))
        .await
        .unwrap();
        let unavailable = directory.path().join("journal-is-a-file");
        std::fs::write(&unavailable, b"not a directory").unwrap();
        let journal = Journal::new(unavailable, database.clone());
        journal.stop_for_test().await;
        let reservation = UsageReservation {
            id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            key_id: Uuid::now_v7(),
            enforcement_mode: crate::model::EnforcementMode::MeteredUnlimited,
            reserved_micros: 0,
            input_micros_per_million: 1,
            output_micros_per_million: 1,
            price_tiers: Vec::new(),
            rate_window_start: 0,
            reserved_tokens: 16,
        };
        let snapshot = Snapshot::capture(
            FinishProxyRequest {
                request_id: Uuid::now_v7(),
                tenant_id: Uuid::now_v7(),
                reservation: &reservation,
                input_token_ceiling: 8,
                output_token_ceiling: 8,
                requested_service_tier: None,
                status_code: 200,
                duration_ms: 1,
                first_output_ms: None,
                generation_duration_ms: None,
                usage: TokenUsage {
                    input_tokens: 3,
                    output_tokens: 2,
                    ..Default::default()
                },
                usage_basis: Some(crate::model::RequestUsageBasis::ProviderReported),
                terminal_cause: None,
                error_code: None,
                response_object: "gap://retry-test/response",
                routing_session_id: None,
                routing_terminal_observed_at: None,
                conversation: None,
            },
            ProxyRequestUpstreamAttribution::KeepSelected,
            None,
            false,
            "buffered_response_ready",
        );
        // Keep paused time fixed while SQLite/filesystem blocking I/O completes.
        // Otherwise auto-advance can legitimately expire an earlier publication
        // retry while the independent receipt attempt is still finishing.
        async fn finish_io_at(journal: &Journal, expected: tokio::time::Instant) {
            let tasks = std::mem::take(&mut *journal.tasks.lock().unwrap());
            for (_, task) in tasks {
                while !task.is_finished() {
                    tokio::task::yield_now().await;
                }
                task.await.unwrap();
                assert_eq!(tokio::time::Instant::now(), expected);
            }
        }

        fn assert_retries_are_future(journal: &Journal) {
            let now = tokio::time::Instant::now();
            for pending in journal.pending.lock().unwrap().values() {
                assert!(pending.retry_after > now);
                assert!(pending.publication_retry_after > now);
            }
        }

        tokio::time::pause();
        let frozen = tokio::time::Instant::now();
        journal.submit_with_preparation(snapshot.clone(), (), None, async { None });
        journal.start_pending(&database);
        finish_io_at(&journal, frozen).await;
        journal.start_pending(&database);
        finish_io_at(&journal, frozen).await;
        assert_eq!(journal.failed_publications_for_test(), 1);
        assert_retries_are_future(&journal);
        for _ in 0..100 {
            journal.start_pending(&database);
            assert!(journal.tasks.lock().unwrap().is_empty());
        }
        let mut second = snapshot.clone();
        second.request_id = Uuid::now_v7();
        second.receipt_id = Uuid::now_v7();
        journal.submit_with_preparation(second, (), None, async { None });
        journal.start_pending(&database);
        finish_io_at(&journal, frozen).await;
        journal.start_pending(&database);
        finish_io_at(&journal, frozen).await;
        assert_eq!(journal.failed_publications_for_test(), 2);
        assert_retries_are_future(&journal);
        tokio::time::advance(RETRY_INTERVAL).await;
        journal.start_pending(&database);
        finish_io_at(&journal, frozen + RETRY_INTERVAL).await;
        assert_eq!(journal.failed_publications_for_test(), 4);
        {
            let pending = journal.pending.lock().unwrap();
            assert_eq!(pending.len(), 2);
            let retained = pending.get(&snapshot.receipt_id).unwrap();
            assert!(!retained.durable);
            assert!(!retained.applied);
            assert!(retained.retained.is_some());
            assert_eq!(serde_json::to_value(&*retained.snapshot).unwrap(), {
                let mut snapshot = snapshot;
                snapshot.separate_conversation = false;
                serde_json::to_value(snapshot).unwrap()
            });
        }
        tokio::time::resume();
        journal.stop_for_test().await;
        database.close().await;
    }

    async fn finish_attempts(journal: &Journal) {
        let tasks = std::mem::take(&mut *journal.tasks.lock().unwrap());
        for (_, task) in tasks {
            task.await.unwrap();
        }
    }

    fn owned_conversation() -> OwnedConversation {
        let mut key = AuthenticatedKey {
            key_id: Uuid::now_v7(),
            tenant_id: Uuid::now_v7(),
            principal_id: Uuid::now_v7(),
            account_id: Uuid::now_v7(),
            alias: "original alias".into(),
            currency: "USD".into(),
            credential_generation: 17,
            policy: crate::model::KeyPolicy {
                allowed_models: vec!["legacy-snapshot-only".into()],
                requests_per_minute: Some(23),
                tokens_per_minute: Some(4567),
                max_concurrency: Some(8),
                enforcement_mode: crate::model::EnforcementMode::MeteredUnlimited,
                daily_budget: Some("12.34".into()),
                weekly_budget: Some("56.78".into()),
                lifetime_budget: Some("90.12".into()),
            },
        };
        let request_json = serde_json::json!({"input": "original input"});
        let hints = crate::conversation::ConversationHints {
            session_id: Some("original session".into()),
            ..Default::default()
        };
        let captured = OwnedConversation::capture(crate::db::ProxyConversationInput {
            key: &key,
            request_json: &request_json,
            hints: &hints,
            client_name: Some("original client"),
            upstream_response_id: Some("original response"),
        });
        key.alias = "current alias".into();
        key.currency = "EUR".into();
        key.credential_generation += 1;
        key.policy = crate::model::KeyPolicy::default();
        assert_ne!(captured.key.alias, key.alias);
        captured
    }

    #[test]
    fn owned_key_roundtrip_preserves_every_original_snapshot_field() {
        let captured = owned_conversation();
        let original = serde_json::to_value(&captured).unwrap();
        let restored: OwnedConversation = serde_json::from_value(original.clone()).unwrap();
        assert_eq!(serde_json::to_value(&restored).unwrap(), original);
        assert_eq!(restored.key.key_id, captured.key.key_id);
        assert_eq!(restored.key.tenant_id, captured.key.tenant_id);
        assert_eq!(restored.key.principal_id, captured.key.principal_id);
        assert_eq!(restored.key.account_id, captured.key.account_id);
        assert_eq!(restored.key.alias, "original alias");
        assert_eq!(restored.key.currency, "USD");
        assert_eq!(restored.key.credential_generation, 17);
        assert_eq!(restored.key.policy.allowed_models, ["legacy-snapshot-only"]);
        assert_eq!(restored.key.policy.requests_per_minute, Some(23));
        assert_eq!(restored.key.policy.tokens_per_minute, Some(4567));
        assert_eq!(restored.key.policy.max_concurrency, Some(8));
        assert_eq!(
            restored.key.policy.enforcement_mode,
            crate::model::EnforcementMode::MeteredUnlimited
        );
        assert_eq!(restored.key.policy.daily_budget.as_deref(), Some("12.34"));
        assert_eq!(restored.key.policy.weekly_budget.as_deref(), Some("56.78"));
        assert_eq!(
            restored.key.policy.lifetime_budget.as_deref(),
            Some("90.12")
        );
    }

    #[test]
    fn owned_key_roundtrip_retains_unconfigured_rates_as_null() {
        let mut captured = owned_conversation();
        captured.key.policy.requests_per_minute = None;
        captured.key.policy.tokens_per_minute = None;
        captured.key.policy.max_concurrency = None;
        let encoded = serde_json::to_value(&captured).unwrap();
        for field in [
            "requests_per_minute",
            "tokens_per_minute",
            "max_concurrency",
        ] {
            assert!(encoded["key"]["policy"][field].is_null());
        }
        let restored: OwnedConversation = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(restored).unwrap(), encoded);
    }

    #[test]
    fn incomplete_owned_key_does_not_reconstruct_identity_or_policy_defaults() {
        let original = serde_json::to_value(owned_conversation()).unwrap();
        for field in ["alias", "currency", "credential_generation", "policy"] {
            let mut incomplete = original.clone();
            incomplete["key"].as_object_mut().unwrap().remove(field);
            assert!(serde_json::from_value::<OwnedConversation>(incomplete).is_err());
        }
        for field in [
            "allowed_models",
            "requests_per_minute",
            "tokens_per_minute",
            "max_concurrency",
            "enforcement_mode",
            "daily_budget",
            "weekly_budget",
            "lifetime_budget",
        ] {
            let mut incomplete = original.clone();
            incomplete["key"]["policy"]
                .as_object_mut()
                .unwrap()
                .remove(field);
            assert!(serde_json::from_value::<OwnedConversation>(incomplete).is_err());
        }
    }

    #[test]
    fn owned_usage_roundtrip_retains_cache_evidence_and_service_tier() {
        let original = TokenUsage {
            input_tokens: 19,
            cached_input_tokens: 7,
            cache_write_tokens: 3,
            output_tokens: 11,
            service_tier: Some("priority".into()),
            cache_coverage: Some(crate::model::CacheUsageCoverage {
                read_tokens: 7,
                inclusive_input_tokens: Some(26),
            }),
        };
        let bytes = serde_json::to_vec(&UsageWire::from(original)).unwrap();
        let restored = serde_json::from_slice::<UsageWire>(&bytes).unwrap().usage();
        assert_eq!(restored.input_tokens, 19);
        assert_eq!(restored.cached_input_tokens, 7);
        assert_eq!(restored.cache_write_tokens, 3);
        assert_eq!(restored.output_tokens, 11);
        assert_eq!(restored.service_tier.as_deref(), Some("priority"));
        let coverage = restored.cache_coverage.unwrap();
        assert_eq!(coverage.read_tokens, 7);
        assert_eq!(coverage.inclusive_input_tokens, Some(26));
    }
}
