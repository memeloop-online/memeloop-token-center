use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use serde::{Deserialize, Serialize};
use tokio::sync::Notify;
use uuid::Uuid;

use super::{ProxyLifecycleActivity, finish_proxy_request_with_retry};
use crate::{
    db::{Database, FinishProxyRequest, FinishProxyRequestResult, ProxyRequestUpstreamAttribution},
    model::{AuthenticatedKey, TokenUsage, UsageReservation},
};

const RETRY_INTERVAL: Duration = Duration::from_secs(1);

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
    requests_per_minute: u32,
    tokens_per_minute: u64,
    max_concurrency: u32,
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
    routing_session_id: Option<String>,
    routing_terminal_observed_at: Option<i64>,
    conversation: Option<OwnedConversation>,
    keep_selected: bool,
    last_dispatched: Option<(Uuid, Uuid)>,
    pub(crate) response_id: Option<String>,
    pub(crate) queued_billable: bool,
    claim: Option<crate::db::PendingProxySettlementClaim>,
    #[serde(default)]
    operational_terminal: Option<crate::db::PendingProxyOperationalTerminal>,
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
            routing_session_id: input.routing_session_id.map(str::to_owned),
            routing_terminal_observed_at: input.routing_terminal_observed_at,
            conversation: input.conversation.map(OwnedConversation::capture),
            keep_selected,
            last_dispatched,
            response_id,
            queued_billable,
            claim: None,
            operational_terminal: Some(operational_terminal),
        }
    }

    fn attribution(&self) -> ProxyRequestUpstreamAttribution {
        if self.keep_selected {
            ProxyRequestUpstreamAttribution::KeepSelected
        } else {
            ProxyRequestUpstreamAttribution::LastDispatched(self.last_dispatched)
        }
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
        .map_err(|_| crate::error::AppError::Internal)?;
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
                .ok_or(crate::error::AppError::Internal)
            })
            .transpose()?;
        if self.version != 1
            || self
                .input_token_ceiling
                .checked_add(self.output_token_ceiling)
                != Some(self.reservation.reserved_tokens)
        {
            return Err(crate::error::AppError::Internal);
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
            conversation: self.conversation.as_ref().map(|conversation| {
                crate::db::ProxyConversationInput {
                    key: &conversation.key,
                    request_json: &conversation.request_json,
                    hints: &conversation.hints,
                    client_name: conversation.client_name.as_deref(),
                    upstream_response_id: conversation.upstream_response_id.as_deref(),
                }
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
}

type Preparation =
    std::pin::Pin<Box<dyn std::future::Future<Output = Option<PreparedConversation>> + Send>>;

pub(crate) struct Journal {
    directory: PathBuf,
    pending: Mutex<HashMap<Uuid, Pending>>,
    claims: Mutex<HashMap<Uuid, ClaimState>>,
    wake: Notify,
    worker: Mutex<Option<tokio::task::JoinHandle<()>>>,
    tasks: Mutex<Vec<(Work, tokio::task::JoinHandle<()>)>>,
    publication_failures: AtomicU64,
    #[cfg(test)]
    writer_gate: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    retirement_gate: Arc<tokio::sync::Semaphore>,
}

struct ClaimState {
    claim: crate::db::PendingProxySettlementClaim,
    unknown: bool,
    running: bool,
}

enum Work {
    Receipt(Uuid),
    Operational(Uuid),
    Unknown(Uuid),
}

impl Journal {
    pub(crate) fn new(directory: PathBuf, database: Database) -> Arc<Self> {
        let journal = Arc::new(Self {
            directory,
            pending: Mutex::new(HashMap::new()),
            claims: Mutex::new(HashMap::new()),
            wake: Notify::new(),
            worker: Mutex::new(None),
            tasks: Mutex::new(Vec::new()),
            publication_failures: AtomicU64::new(0),
            #[cfg(test)]
            writer_gate: Arc::new(tokio::sync::Semaphore::new(1)),
            #[cfg(test)]
            retirement_gate: Arc::new(tokio::sync::Semaphore::new(1)),
        });
        let weak = Arc::downgrade(&journal);
        let worker = tokio::spawn(async move {
            while let Some(journal) = weak.upgrade() {
                journal.reap().await;
                journal.recover().await;
                journal.start_operational(&database);
                journal.start_pending(&database);
                journal.start_unknown(&database);
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
        journal
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
                };
                tracing::error!(%request_id, financial_state = "unknown", "pending terminal worker interrupted; evidence retained for supervised retry");
            }
        }
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in pending.values_mut().filter(|entry| {
            entry.durable && entry.reconcile && entry.conversation_enqueued && !entry.running
        }) {
            if let Some(snapshot) = Arc::get_mut(&mut entry.snapshot) {
                snapshot.conversation = None;
                entry.projection_retained = None;
            }
        }
    }

    pub(crate) fn submit_with_preparation(
        &self,
        mut snapshot: Snapshot,
        retained: impl Send + 'static,
        preparation: impl std::future::Future<Output = Option<PreparedConversation>> + Send + 'static,
    ) {
        let request_id = snapshot.request_id;
        let receipt_id = snapshot.receipt_id;
        snapshot.claim = self
            .claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .remove(&request_id)
            .map(|entry| entry.claim);
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
            }
            tracing::error!(%request_id, %receipt_id, financial_state = "unknown", "multiple terminal receipts retained for database arbitration");
        }
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
            },
        );
        tracing::info!(%request_id, %receipt_id, financial_state = "pending_unacknowledged", "terminal responsibility accepted in memory; journal durability not acknowledged");
        self.wake.notify_one();
    }

    pub(crate) fn register_claim(&self, claim: crate::db::PendingProxySettlementClaim) {
        self.claims
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(
                claim.request_id,
                ClaimState {
                    claim,
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
        self.submit_with_preparation(snapshot, (), async { None });
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
    pub(crate) fn directory_for_test(&self) -> &std::path::Path {
        &self.directory
    }

    #[cfg(test)]
    pub(crate) async fn hold_retirement_for_test(&self) -> tokio::sync::OwnedSemaphorePermit {
        self.retirement_gate.clone().acquire_owned().await.unwrap()
    }

    #[cfg(test)]
    pub(crate) fn requires_arbitration_for_test(&self, receipt_id: Uuid) -> bool {
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&receipt_id)
            .is_some_and(|entry| entry.reconcile || entry.operational_reconcile)
    }

    #[cfg(test)]
    pub(crate) async fn retained_conversation_for_test(&self, receipt_id: Uuid) -> bool {
        self.reap().await;
        self.pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&receipt_id)
            .is_some_and(|entry| entry.snapshot.conversation.is_some())
    }

    #[cfg(test)]
    pub(crate) fn failed_publications_for_test(&self) -> u64 {
        self.publication_failures.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    pub(crate) async fn stop_for_test(&self) {
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

    async fn recover(&self) {
        let mut entries = match tokio::fs::read_dir(&self.directory).await {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return,
            Err(error) => {
                tracing::error!(financial_state = "unknown", io_error_kind = ?error.kind(), "pending terminal journal directory is unavailable; recovery deferred");
                return;
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
            let mut pending = self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if let std::collections::hash_map::Entry::Vacant(entry) =
                pending.entry(snapshot.receipt_id)
            {
                tracing::info!(request_id = %snapshot.request_id, %receipt_id, financial_state = "pending_durable", "pending terminal receipt recovered");
                entry.insert(Pending {
                    snapshot: Arc::new(snapshot),
                    activity: None,
                    retained: None,
                    running: false,
                    retry_after: tokio::time::Instant::now(),
                    durable: true,
                    applied: false,
                    reconcile: false,
                    operational_running: false,
                    operational_acknowledged: false,
                    unknown_annotated: false,
                    conversation_enqueued: false,
                    operational_reconcile: false,
                    preparation: None,
                    projection_retained: None,
                });
            }
        }
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
                    || (entry.snapshot.usage_basis
                        != Some(crate::model::RequestUsageBasis::ProviderReported)
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
                    let claim = snapshot
                        .claim
                        .as_ref()
                        .ok_or(crate::error::AppError::Internal)?;
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
                    if snapshot.usage_basis
                        != Some(crate::model::RequestUsageBasis::ProviderReported)
                    {
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
                            if snapshot.usage_basis
                                != Some(crate::model::RequestUsageBasis::ProviderReported)
                            {
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

    fn start_pending(self: &Arc<Self>, database: &Database) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for entry in pending.values_mut().filter(|entry| {
            !entry.running
                && (!entry.reconcile || (entry.unknown_annotated && !entry.conversation_enqueued))
                && tokio::time::Instant::now() >= entry.retry_after
        }) {
            entry.running = true;
            let journal = self.clone();
            let database = database.clone();
            let snapshot = entry.snapshot.clone();
            let durable = entry.durable;
            let applied = entry.applied;
            let preparation = entry.preparation.take();
            let receipt_id = snapshot.receipt_id;
            let task = tokio::spawn(async move {
                let snapshot = if let Some(preparation) = preparation {
                    let mut prepared = (*snapshot).clone();
                    let projection_retained = if let Some(conversation) = preparation.await {
                        prepared.conversation = Some(conversation.conversation);
                        Some(conversation.retained)
                    } else {
                        None
                    };
                    let prepared = Arc::new(prepared);
                    if let Some(entry) = journal
                        .pending
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .get_mut(&prepared.receipt_id)
                    {
                        entry.snapshot = prepared.clone();
                        entry.projection_retained = projection_retained;
                    }
                    prepared
                } else {
                    snapshot
                };
                journal.attempt(database, snapshot, durable, applied).await;
            });
            self.tasks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((Work::Receipt(receipt_id), task));
        }
    }

    async fn attempt(
        &self,
        database: Database,
        snapshot: Arc<Snapshot>,
        mut durable: bool,
        mut applied: bool,
    ) {
        let request_id = snapshot.request_id;
        let receipt_id = snapshot.receipt_id;
        if !durable {
            #[cfg(test)]
            let _writer = self.writer_gate.acquire().await.unwrap();
            let directory = self.directory.clone();
            let record = snapshot.clone();
            let publication = tokio::task::spawn_blocking(move || {
                let bytes = serde_json::to_vec(&*record).map_err(std::io::Error::other)?;
                crate::early_request::publish_record(&directory, receipt_id, &bytes, None)?;
                if std::fs::read(directory.join(format!("{receipt_id}.json")))? != bytes {
                    return Err(std::io::Error::other("terminal receipt content conflict"));
                }
                Ok::<_, std::io::Error>(())
            })
            .await;
            durable = matches!(&publication, Ok(Ok(())));
            if durable {
                tracing::info!(%request_id, %receipt_id, financial_state = "pending_durable", "terminal journal publication acknowledged");
            } else {
                let failure_kind = match publication {
                    Ok(Err(error)) => format!("{:?}", error.kind()),
                    Err(error) if error.is_panic() => "worker_panic".to_owned(),
                    Err(_) => "worker_cancelled".to_owned(),
                    Ok(Ok(())) => unreachable!(),
                };
                let failures = self.publication_failures.fetch_add(1, Ordering::Relaxed) + 1;
                tracing::error!(%request_id, %receipt_id, failures, %failure_kind, financial_state = "pending_unacknowledged", "terminal journal publication failed; in-memory evidence retained");
            }
        }
        let mut reconcile = false;
        if !applied && durable {
            let attribution = snapshot.attribution();
            let result = match snapshot.terminal() {
                Ok(input) => {
                    if let Some(claim) = snapshot.claim.as_ref() {
                        if input.usage_basis
                            == Some(crate::model::RequestUsageBasis::ProviderReported)
                        {
                            database
                                .finish_pending_proxy_settlement(
                                    claim,
                                    receipt_id,
                                    crate::db::PendingProxySettlementEvidence::ProviderReported,
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
                                        Ok(Some(FinishProxyRequestResult::Finished {
                                            cost_micros: ack.cost_micros,
                                            usage_invalid: false,
                                        }))
                                    } else {
                                        Err(crate::error::AppError::Internal)
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
                                    let projection =
                                        if let Some(conversation) = input.conversation.as_ref() {
                                            database
                                                .enqueue_operational_conversation_projection(
                                                    claim,
                                                    receipt_id,
                                                    conversation,
                                                )
                                                .await
                                        } else {
                                            Ok(())
                                        };
                                    projection.map(|()| {
                                        if let Some(entry) = self
                                            .pending
                                            .lock()
                                            .unwrap_or_else(|error| error.into_inner())
                                            .get_mut(&receipt_id)
                                        {
                                            entry.conversation_enqueued = true;
                                        }
                                        None
                                    })
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
                    tracing::error!(%request_id, %receipt_id, financial_state = "unknown", "terminal lacks authoritative provider usage; receipt retained without zero-usage settlement");
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
        let operational_acknowledged = snapshot.claim.is_none()
            || self
                .pending
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(&receipt_id)
                .is_some_and(|entry| entry.operational_acknowledged);
        let removed = if applied && operational_acknowledged {
            #[cfg(test)]
            let _retirement = self.retirement_gate.acquire().await.unwrap();
            let directory = self.directory.clone();
            matches!(
                tokio::task::spawn_blocking(move || {
                    let path = directory.join(format!("{receipt_id}.json"));
                    match std::fs::remove_file(path) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error),
                    }
                    std::fs::File::open(directory)?.sync_all()
                })
                .await,
                Ok(Ok(()))
            )
        } else {
            false
        };
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if removed {
            pending.remove(&receipt_id);
            tracing::info!(%request_id, %receipt_id, financial_state = "settled", "terminal settlement acknowledged and receipt retired");
        } else if let Some(entry) = pending.get_mut(&receipt_id) {
            entry.running = false;
            entry.retry_after = tokio::time::Instant::now() + RETRY_INTERVAL;
            entry.durable = durable;
            entry.applied = applied;
            entry.reconcile = reconcile;
            if durable {
                entry.retained = None;
            }
            if reconcile || applied {
                entry.activity = None;
            }
        }
    }
}

impl Drop for Journal {
    fn drop(&mut self) {
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

    #[tokio::test]
    async fn unrelated_wakes_do_not_accelerate_failed_publications_or_delay_new_receipts() {
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
        tokio::time::pause();
        journal.submit_with_preparation(snapshot.clone(), (), async { None });
        journal.start_pending(&database);
        finish_attempts(&journal).await;
        assert_eq!(journal.failed_publications_for_test(), 1);
        for _ in 0..100 {
            journal.start_pending(&database);
            assert!(journal.tasks.lock().unwrap().is_empty());
        }
        let mut second = snapshot.clone();
        second.request_id = Uuid::now_v7();
        second.receipt_id = Uuid::now_v7();
        journal.submit_with_preparation(second, (), async { None });
        journal.start_pending(&database);
        finish_attempts(&journal).await;
        assert_eq!(journal.failed_publications_for_test(), 2);
        tokio::time::advance(RETRY_INTERVAL).await;
        journal.start_pending(&database);
        finish_attempts(&journal).await;
        assert_eq!(journal.failed_publications_for_test(), 4);
        {
            let pending = journal.pending.lock().unwrap();
            assert_eq!(pending.len(), 2);
            let retained = pending.get(&snapshot.receipt_id).unwrap();
            assert!(!retained.durable);
            assert!(!retained.applied);
            assert!(retained.retained.is_some());
            assert_eq!(
                serde_json::to_value(&*retained.snapshot).unwrap(),
                serde_json::to_value(snapshot).unwrap()
            );
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
                requests_per_minute: 23,
                tokens_per_minute: 4567,
                max_concurrency: 8,
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
        assert_eq!(restored.key.policy.requests_per_minute, 23);
        assert_eq!(restored.key.policy.tokens_per_minute, 4567);
        assert_eq!(restored.key.policy.max_concurrency, 8);
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
