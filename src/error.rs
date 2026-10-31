use axum::{
    Json,
    http::{HeaderValue, StatusCode, header},
    response::IntoResponse,
};
use serde_json::json;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LimitReason {
    BalanceExhausted,
    DailyBudgetExhausted,
    WeeklyBudgetExhausted,
    LifetimeBudgetExhausted,
    RpmExhausted,
    TpmExhausted,
    ConcurrencyExhausted,
}

impl LimitReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BalanceExhausted => "balance_exhausted",
            Self::DailyBudgetExhausted => "daily_budget_exhausted",
            Self::WeeklyBudgetExhausted => "weekly_budget_exhausted",
            Self::LifetimeBudgetExhausted => "lifetime_budget_exhausted",
            Self::RpmExhausted => "rpm_exhausted",
            Self::TpmExhausted => "tpm_exhausted",
            Self::ConcurrencyExhausted => "concurrency_exhausted",
        }
    }

    const fn is_quota(self) -> bool {
        matches!(
            self,
            Self::BalanceExhausted
                | Self::DailyBudgetExhausted
                | Self::WeeklyBudgetExhausted
                | Self::LifetimeBudgetExhausted
        )
    }

    const fn retryable(self) -> bool {
        !matches!(self, Self::BalanceExhausted | Self::LifetimeBudgetExhausted)
    }
}

/// Server-generated facts that are safe to show: stage names, phases, byte
/// counts, limits, HTTP status digits, lock names, and `sqlx_error_kind`.
#[derive(Clone, Copy, Debug, Default)]
pub struct ErrorFacts {
    pub stage: Option<&'static str>,
    pub phase: Option<&'static str>,
    pub kind: Option<&'static str>,
    pub status: Option<u16>,
    pub bytes: Option<u64>,
    pub limit: Option<u64>,
    pub lock: Option<&'static str>,
}

impl ErrorFacts {
    pub const fn new() -> Self {
        Self {
            stage: None,
            phase: None,
            kind: None,
            status: None,
            bytes: None,
            limit: None,
            lock: None,
        }
    }

    pub const fn stage(mut self, stage: &'static str) -> Self {
        self.stage = Some(stage);
        self
    }

    pub const fn phase(mut self, phase: &'static str) -> Self {
        self.phase = Some(phase);
        self
    }

    pub const fn kind(mut self, kind: &'static str) -> Self {
        self.kind = Some(kind);
        self
    }

    pub const fn status(mut self, status: u16) -> Self {
        self.status = Some(status);
        self
    }

    pub const fn bytes(mut self, bytes: u64) -> Self {
        self.bytes = Some(bytes);
        self
    }

    pub const fn limit(mut self, limit: u64) -> Self {
        self.limit = Some(limit);
        self
    }

    #[allow(dead_code)]
    pub const fn lock(mut self, lock: &'static str) -> Self {
        self.lock = Some(lock);
        self
    }

    const fn merge(self, extra: Self) -> Self {
        Self {
            stage: match extra.stage {
                Some(value) => Some(value),
                None => self.stage,
            },
            phase: match extra.phase {
                Some(value) => Some(value),
                None => self.phase,
            },
            kind: match extra.kind {
                Some(value) => Some(value),
                None => self.kind,
            },
            status: match extra.status {
                Some(value) => Some(value),
                None => self.status,
            },
            bytes: match extra.bytes {
                Some(value) => Some(value),
                None => self.bytes,
            },
            limit: match extra.limit {
                Some(value) => Some(value),
                None => self.limit,
            },
            lock: match extra.lock {
                Some(value) => Some(value),
                None => self.lock,
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("authentication required")]
    Unauthorized,
    #[error("permission denied")]
    Forbidden,
    #[error("request exceeds traffic plugin input limit")]
    PluginInputTooLarge,
    #[error("model has no price in the key currency")]
    UnpricedModel,
    #[error("available balance or configured budget is insufficient")]
    QuotaExceeded,
    #[error("rate limit exceeded")]
    RateLimited,
    #[error("usage limit exceeded: {reason:?}")]
    LimitExceeded {
        reason: LimitReason,
        retry_after_seconds: Option<u64>,
    },
    #[error("resource not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("transport proxy configuration conflict: {0}")]
    ProxyGroupConflict(&'static str),
    #[error("invalid request: {0}")]
    BadRequest(String),
    #[error(
        "invalid request: no available authorized upstream supports Responses compaction production"
    )]
    CompactionProducerUnavailable,
    #[error("secret schema analysis exceeds the supported complexity limit")]
    SchemaSecretAnalysisTooComplex,
    #[error("{0}")]
    PluginReleaseDownload(PluginReleaseDownloadFailure),
    #[error("configured upstream is unavailable: {code}")]
    Upstream {
        code: &'static str,
        facts: ErrorFacts,
    },
    #[error(
        "Could not refresh authorization. Check the connection or authorize this account again."
    )]
    OAuthRefresh(crate::oauth::OAuthRefreshFailure),
    #[error("service is temporarily overloaded")]
    Overloaded,
    /// A retryable capacity failure. `code` names the resource; `facts` are joined onto both languages.
    #[error("capacity {code}")]
    Capacity {
        code: &'static str,
        facts: ErrorFacts,
    },
    #[error("no authorized upstream candidate is currently available")]
    NoAuthorizedUpstreamCandidate,
    #[error("storage error: {code}")]
    Storage {
        code: &'static str,
        facts: ErrorFacts,
    },
    #[error("{0}")]
    DatabaseUnavailable(DatabaseFailure),
    #[error("internal error")]
    Internal {
        code: &'static str,
        facts: ErrorFacts,
    },
}

impl AppError {
    /// Low-cardinality diagnostic label safe for structured logs.
    ///
    /// Several variants intentionally carry operator-, provider-, or
    /// guest-controlled detail for local control flow.  Their `Display`
    /// representation must not be copied into logs because it can contain
    /// configuration fragments, URLs, or plugin failure text.
    pub(crate) const fn diagnostic_category(&self) -> &'static str {
        match self {
            Self::Unauthorized => "unauthorized",
            Self::Forbidden => "forbidden",
            Self::PluginInputTooLarge => "plugin_input_too_large",
            Self::UnpricedModel => "unpriced_model",
            Self::QuotaExceeded => "quota_exceeded",
            Self::RateLimited => "rate_limited",
            Self::LimitExceeded { .. } => "limit_exceeded",
            Self::NotFound => "not_found",
            Self::Conflict(_) => "conflict",
            Self::ProxyGroupConflict(code) => code,
            Self::BadRequest(_) => "invalid_request",
            Self::CompactionProducerUnavailable => "compaction_producer_unavailable",
            Self::SchemaSecretAnalysisTooComplex => "schema_secret_analysis_too_complex",
            Self::PluginReleaseDownload(failure) => failure.code(),
            Self::Upstream { code, .. } => code,
            Self::OAuthRefresh(_) => "oauth_refresh",
            Self::Overloaded => "service_overloaded",
            Self::Capacity { code, .. } => code,
            Self::NoAuthorizedUpstreamCandidate => "no_authorized_upstream_candidate",
            Self::Storage { code, .. } => code,
            Self::DatabaseUnavailable(failure) => failure.code(),
            Self::Internal { code, .. } => code,
        }
    }

    pub(crate) fn capacity(code: &'static str, facts: ErrorFacts) -> Self {
        Self::Capacity { code, facts }
    }

    pub(crate) fn capacity_error(code: &'static str) -> Self {
        Self::capacity(code, ErrorFacts::new())
    }

    pub(crate) fn upstream(code: &'static str) -> Self {
        Self::upstream_with(code, ErrorFacts::new())
    }

    pub(crate) fn upstream_with(code: &'static str, facts: ErrorFacts) -> Self {
        Self::Upstream { code, facts }
    }

    pub(crate) fn upstream_status(code: &'static str, status: u16) -> Self {
        Self::upstream_with(code, ErrorFacts::new().status(status))
    }

    pub fn storage(code: &'static str) -> Self {
        Self::storage_with(code, ErrorFacts::new())
    }

    pub(crate) fn storage_with(code: &'static str, facts: ErrorFacts) -> Self {
        Self::Storage { code, facts }
    }

    pub(crate) fn internal(code: &'static str) -> Self {
        Self::Internal {
            code,
            facts: ErrorFacts::new(),
        }
    }

    pub(crate) fn internal_kind(code: &'static str, kind: &'static str) -> Self {
        Self::Internal {
            code,
            facts: ErrorFacts::new().kind(kind),
        }
    }

    /// Keep any sqlx kind or byte count already recorded, then name the stage.
    pub(crate) fn retarget_capacity(self, code: &'static str, extra: ErrorFacts) -> Self {
        let prior = match self {
            Self::Capacity { facts, .. }
            | Self::Internal { facts, .. }
            | Self::Upstream { facts, .. }
            | Self::Storage { facts, .. } => facts,
            _ => ErrorFacts::new(),
        };
        Self::capacity(code, prior.merge(extra))
    }

    pub(crate) fn name_if_overloaded(self, stage: &'static str) -> Self {
        match self {
            Self::Overloaded => Self::capacity_error(stage),
            Self::Capacity { code, mut facts } => {
                if facts.stage.is_none() && stage != code {
                    facts.stage = Some(stage);
                }
                Self::Capacity { code, facts }
            }
            other => other,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, code, facts) = match &self {
            Self::Unauthorized => (StatusCode::UNAUTHORIZED, "unauthorized", ErrorFacts::new()),
            Self::Forbidden => (StatusCode::FORBIDDEN, "forbidden", ErrorFacts::new()),
            Self::PluginInputTooLarge => (
                StatusCode::PAYLOAD_TOO_LARGE,
                "plugin_input_too_large",
                ErrorFacts::new(),
            ),
            Self::UnpricedModel => (
                StatusCode::FAILED_DEPENDENCY,
                "unpriced_model",
                ErrorFacts::new(),
            ),
            Self::QuotaExceeded => (
                StatusCode::TOO_MANY_REQUESTS,
                "insufficient_quota",
                ErrorFacts::new(),
            ),
            Self::RateLimited => (
                StatusCode::TOO_MANY_REQUESTS,
                "rate_limit_exceeded",
                ErrorFacts::new(),
            ),
            Self::LimitExceeded { reason, .. } => (
                StatusCode::TOO_MANY_REQUESTS,
                if reason.is_quota() {
                    "insufficient_quota"
                } else {
                    "rate_limit_exceeded"
                },
                ErrorFacts::new().stage(reason.as_str()),
            ),
            Self::NotFound => (StatusCode::NOT_FOUND, "not_found", ErrorFacts::new()),
            Self::Conflict(_) => (StatusCode::CONFLICT, "conflict", ErrorFacts::new()),
            Self::ProxyGroupConflict(group_code) => {
                (StatusCode::CONFLICT, *group_code, ErrorFacts::new())
            }
            Self::BadRequest(_) => (
                StatusCode::BAD_REQUEST,
                "invalid_request",
                ErrorFacts::new(),
            ),
            Self::CompactionProducerUnavailable => (
                StatusCode::BAD_REQUEST,
                "compaction_producer_unavailable",
                ErrorFacts::new(),
            ),
            Self::SchemaSecretAnalysisTooComplex => (
                StatusCode::BAD_REQUEST,
                "schema_secret_analysis_too_complex",
                ErrorFacts::new(),
            ),
            Self::PluginReleaseDownload(failure) => (
                if *failure == PluginReleaseDownloadFailure::Timeout {
                    StatusCode::GATEWAY_TIMEOUT
                } else {
                    StatusCode::BAD_GATEWAY
                },
                failure.code(),
                ErrorFacts::new().stage("plugin_release_download"),
            ),
            Self::Upstream { code, facts } => (StatusCode::BAD_GATEWAY, *code, *facts),
            Self::OAuthRefresh(_) => (
                StatusCode::BAD_GATEWAY,
                "oauth_refresh_failed",
                ErrorFacts::new().stage("oauth_refresh"),
            ),
            Self::Overloaded => (
                StatusCode::SERVICE_UNAVAILABLE,
                "service_overloaded",
                ErrorFacts::new().stage("service_overloaded"),
            ),
            Self::Capacity { code, facts } => (StatusCode::SERVICE_UNAVAILABLE, *code, *facts),
            Self::NoAuthorizedUpstreamCandidate => (
                StatusCode::SERVICE_UNAVAILABLE,
                "no_authorized_upstream_candidate",
                ErrorFacts::new(),
            ),
            Self::DatabaseUnavailable(failure) => (
                StatusCode::SERVICE_UNAVAILABLE,
                failure.code(),
                ErrorFacts::new().stage("database"),
            ),
            Self::Storage { code, facts } => (StatusCode::INTERNAL_SERVER_ERROR, *code, *facts),
            Self::Internal { code, facts } => (StatusCode::INTERNAL_SERVER_ERROR, *code, *facts),
        };
        let (mut message, mut message_en) = render_bilingual(code, facts);
        match &self {
            Self::BadRequest(detail) => {
                if let Some(field) = request_field_fact(detail) {
                    message.push_str("字段 ");
                    message.push_str(field);
                    message.push('。');
                }
                if let Some(english) = specific_public_sentence("invalid request", detail) {
                    message_en = english;
                }
            }
            Self::Conflict(detail) => {
                if let Some(english) = specific_public_sentence("conflict", detail) {
                    message_en = english;
                }
            }
            Self::CompactionProducerUnavailable => {
                message_en = "invalid request: no available authorized upstream supports Responses compaction production".to_owned();
            }
            _ => {}
        }
        let mut response = match &self {
            Self::LimitExceeded { reason, .. } => (
                status,
                Json(json!({"error": {
                    "code": code,
                    "message": message,
                    "message_en": message_en,
                    "reason": reason.as_str(),
                    "retryable": reason.retryable(),
                }})),
            )
                .into_response(),
            _ => (
                status,
                Json(json!({"error": {
                    "code": code,
                    "message": message,
                    "message_en": message_en,
                }})),
            )
                .into_response(),
        };
        if let Self::LimitExceeded {
            retry_after_seconds: Some(seconds),
            ..
        } = &self
            && let Ok(value) = HeaderValue::from_str(&(*seconds).max(1).to_string())
        {
            response.headers_mut().insert(header::RETRY_AFTER, value);
        }
        response.extensions_mut().insert(PublicErrorCode(code));
        if matches!(
            &self,
            Self::Overloaded | Self::Capacity { .. } | Self::NoAuthorizedUpstreamCandidate
        ) {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

#[derive(Clone, Copy)]
pub(crate) struct PublicErrorCode(pub(crate) &'static str);

/// Server-generated public codes only. Rejects `http_NNN` and any token that
/// could carry a credential, header, or supplier sentence.
pub(crate) fn is_public_error_code(code: &str) -> bool {
    let mut bytes = code.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() || code.len() > 64 {
        return false;
    }
    if !bytes.all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_') {
        return false;
    }
    !code
        .strip_prefix("http_")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

/// 服务端自己写的短句可以原样放进 `message_en`。用户输入、地址和超长内容不放。
fn specific_public_sentence(prefix: &str, detail: &str) -> Option<String> {
    if detail.is_empty() || detail.len() > 240 {
        return None;
    }
    if !detail.is_ascii() || detail.chars().any(char::is_control) || detail.contains("://") {
        return None;
    }
    Some(format!("{prefix}: {detail}"))
}

fn request_field_fact(detail: &str) -> Option<&'static str> {
    const FIELDS: &[&str] = &[
        "frequency_penalty",
        "presence_penalty",
        "max_completion_tokens",
        "max_output_tokens",
        "stream_options",
        "service_tier",
        "input_audio",
        "max_tokens",
        "temperature",
        "top_p",
    ];
    FIELDS.iter().copied().find(|field| detail.contains(field))
}

pub(crate) fn render_bilingual(code: &str, facts: ErrorFacts) -> (String, String) {
    let known = public_error_sentences(code);
    let (zh_base, en_base) = known.unwrap_or(("内部错误", "internal error"));
    let mut zh = vec![zh_base.to_owned()];
    let mut en = vec![en_base.to_owned()];
    if known.is_none() && facts.stage.is_none_or(|stage| stage != code) {
        zh.push(format!("失败阶段 {code}"));
        en.push(format!("failure stage {code}"));
    }
    let names_stage = facts.stage.is_some_and(|stage| {
        is_public_error_code(stage) && stage != code && !zh_base.contains(stage)
    });
    if known.is_some()
        && matches!(
            en_base,
            "configured upstream is unavailable" | "internal error"
        )
        && !names_stage
        && !en_base.contains(code)
    {
        en.push(format!("failure stage {code}"));
    }
    if let Some(stage) = facts
        .stage
        .filter(|stage| is_public_error_code(stage) && *stage != code && !zh_base.contains(*stage))
    {
        zh.push(format!("失败阶段 {stage}"));
        en.push(format!("failure stage {stage}"));
    }
    if let Some(phase) = facts.phase.filter(|phase| is_public_error_code(phase)) {
        zh.push(format!("阶段 {phase}"));
        en.push(format!("phase {phase}"));
    }
    if let Some(kind) = facts.kind.filter(|kind| is_public_error_code(kind)) {
        zh.push(format!("错误类型 {kind}"));
        en.push(format!("error kind {kind}"));
    }
    if let Some(status) = facts.status {
        zh.push(format!("上游 HTTP 状态 {status}"));
        en.push(format!("upstream HTTP status {status}"));
    }
    if let Some(bytes) = facts.bytes {
        zh.push(format!("需要 {bytes} 字节"));
        en.push(format!("requested {bytes} bytes"));
    }
    if let Some(limit) = facts.limit {
        zh.push(format!("上限 {limit} 字节"));
        en.push(format!("limit {limit} bytes"));
    }
    if let Some(lock) = facts.lock.filter(|lock| is_public_error_code(lock)) {
        zh.push(format!("锁 {lock}"));
        en.push(format!("lock {lock}"));
    }
    (
        format!("{}。", zh.join("。")),
        format!("{}.", en.join(". ")),
    )
}

pub(crate) fn public_error_sentences(code: &str) -> Option<(&'static str, &'static str)> {
    Some(match code {
        "proxy_memory_ingress" => (
            "网关内存装不下这次请求体，也没有写入对象存储",
            "gateway memory budget could not hold the request body, and the body was not written to object storage",
        ),
        "proxy_memory_json" => (
            "网关内存装不下解析后的 JSON 请求体",
            "gateway memory budget could not hold the parsed JSON body",
        ),
        "proxy_memory_projection" => (
            "网关内存装不下这次会话正文归档",
            "gateway memory could not hold this conversation archive",
        ),
        "proxy_memory_capacity" => (
            "保留请求的内存分区已满",
            "the retained request partition was full",
        ),
        "proxy_memory_plugin" => (
            "网关内存装不下流量插件的工作副本",
            "gateway memory budget could not hold the traffic plugin working set",
        ),
        "proxy_memory_route" => (
            "网关内存装不下路由准备",
            "gateway memory budget could not hold route preparation",
        ),
        "upstream_response_memory_capacity" => (
            "网关内存装不下上游响应，这次请求没有再次发送",
            "the gateway memory budget could not hold the upstream response, and the request was not sent again",
        ),
        "database_pool_exhausted" => ("数据库连接池等待超时", "database connection pool timed out"),
        "request_spool_read_failed" => (
            "已暂存的请求体无法读回",
            "the stored request body could not be read back",
        ),
        "request_spool_unavailable" => (
            "请求体暂存当时不可用",
            "request spool is temporarily unavailable",
        ),
        "media_concurrency_permit" => (
            "媒体并发许可当时没有空位",
            "the media concurrency permit could not be acquired",
        ),
        "plugin_installation_lock" => (
            "插件安装锁当时被占用",
            "the plugin installation lock could not be acquired",
        ),
        "plugin_installation_capacity" => (
            "插件安装容量当时已满",
            "plugin installation capacity is temporarily unavailable",
        ),
        "plugin_subset_capacity" => (
            "插件子集发布容量当时已满",
            "plugin subset publication capacity is temporarily unavailable",
        ),
        "plugin_retained_copy_capacity" => (
            "插件安装副本的复制容量当时已满",
            "plugin installation copy capacity is temporarily unavailable",
        ),
        "plugin_publish_lock" => (
            "插件发布锁当时被占用",
            "the plugin publication lock could not be acquired",
        ),
        "plugin_compilation_capacity" => (
            "插件编译容量当时已满",
            "plugin compilation capacity is temporarily unavailable",
        ),
        "plugin_snapshot_unavailable" => (
            "应用插件快照当时还没准备好",
            "the application plugin snapshot is not ready",
        ),
        "plugin_pin_timeout" => (
            "应用插件快照没有在时限内准备好",
            "the application plugin snapshot was not ready before the deadline",
        ),
        "plugin_snapshot_refresh_timeout" => (
            "应用插件快照刷新没有在时限内完成",
            "the application plugin snapshot refresh did not finish before the deadline",
        ),
        "plugin_export_capacity" => (
            "插件导出容量当时已满",
            "plugin export capacity is temporarily unavailable",
        ),
        "plugin_release_fetch_capacity" => (
            "插件发行包下载容量当时已满",
            "plugin release fetch capacity is temporarily unavailable",
        ),
        "control_request_body_capacity" => (
            "控制面请求体当时无法接入",
            "the control-plane request body could not be admitted",
        ),
        "cloud_webhook_body_capacity" => (
            "云端回调请求体当时无法接入",
            "the cloud webhook request body could not be admitted",
        ),
        "operator_inference_memory" => (
            "网关内存装不下这次操作台推理请求体",
            "gateway memory budget could not hold the operator inference request body",
        ),
        "transport_proxy_storage_unavailable" => (
            "传输代理组的存储当时不可用",
            "transport proxy group storage is unavailable",
        ),
        "upstream_candidate_preparation_exhausted" => (
            "已授权的上游候选在发出请求前就用尽了",
            "authorized upstream candidate preparation was exhausted before dispatch",
        ),
        "component_target" => (
            "组件提供商的目标地址被拒绝",
            "the component provider target was rejected",
        ),
        "component_network_client" => (
            "组件提供商的网络客户端没有建起来",
            "the component provider network client could not be created",
        ),
        "component_credential" => (
            "组件提供商的凭据没有套用上",
            "the component provider credential could not be applied",
        ),
        "component_response" => (
            "组件提供商的响应没有读完",
            "the component provider response could not be read",
        ),
        "component_normalize" => (
            "组件提供商的响应没有整理成网关格式",
            "the component provider response could not be normalized",
        ),
        "component_request_header" => (
            "组件提供商的请求头无效",
            "the component provider request headers were invalid",
        ),
        "quota_reset_permit" => (
            "上游额度重置的并发许可当时没有空位",
            "the upstream quota reset permit could not be acquired",
        ),
        "quota_reset_budget" => (
            "上游额度重置的预算没有读出来",
            "the upstream quota reset budget could not be read",
        ),
        "quota_reset_transport" => (
            "上游额度重置的传输没有准备好",
            "the upstream quota reset transport could not be prepared",
        ),
        "quota_reset_read_failed" => (
            "上游额度积分没有读到",
            "the upstream quota credit read failed",
        ),
        "quota_reset_timeout" => (
            "读取上游额度积分超时",
            "the upstream quota credit read timed out",
        ),
        "quota_reset_normalize" => (
            "上游额度积分的响应没有读懂",
            "the upstream quota credit response could not be read",
        ),
        "quota_reset_credits_unavailable" => (
            "当时没有可用的上游额度重置积分",
            "fresh supplier quota reset credits are unavailable",
        ),
        "quota_reset_transport_timeout" => (
            "上游额度重置的传输准备超时",
            "the upstream quota reset transport timed out",
        ),
        "proxy_connect_failed" => ("出网代理连接失败", "the egress proxy connection failed"),
        "dns_resolution_failed" => ("上游域名解析失败", "upstream DNS resolution failed"),
        "tls_handshake_failed" => ("上游 TLS 握手失败", "the upstream TLS handshake failed"),
        "upstream_connect_failed" => ("上游 TCP 连接失败", "the upstream TCP connection failed"),
        "upstream_connection" => ("上游连接失败", "the upstream connection failed"),
        "upstream_transport_timeout" => ("等待上游响应超时", "the upstream request timed out"),
        "upstream_transport_connection_reset" => {
            ("上游连接被重置", "the upstream connection was reset")
        }
        "upstream_transport_http2_reset" => (
            "上游 HTTP/2 流被重置",
            "the upstream HTTP/2 stream was reset",
        ),
        "upstream_transport_http2_goaway" => (
            "上游 HTTP/2 连接收到 GOAWAY",
            "the upstream HTTP/2 connection received GOAWAY",
        ),
        "upstream_transport_body" => (
            "发送上游请求内容失败",
            "sending the upstream request body failed",
        ),
        "upstream_transport_decode" => {
            ("解析上游响应失败", "decoding the upstream response failed")
        }
        "upstream_transport_request" => ("发送上游请求失败", "sending the upstream request failed"),
        "upstream_transport_other" => ("上游网络请求失败", "the upstream network request failed"),
        "upstream_http2_reset" => (
            "上游 HTTP/2 流被重置",
            "the upstream HTTP/2 stream was reset",
        ),
        "upstream_http2_goaway" => (
            "上游 HTTP/2 连接收到 GOAWAY",
            "the upstream HTTP/2 connection received GOAWAY",
        ),
        "upstream_read_timeout" => (
            "读取上游响应超时",
            "reading the upstream response timed out",
        ),
        "upstream_request_timeout" => ("等待上游请求响应超时", "the upstream request timed out"),
        "upstream_stream" => (
            "上游响应流中断",
            "the upstream response stream was interrupted",
        ),
        "upstream_stream_read_error" => (
            "读取上游响应时连接中断",
            "the upstream response stream was interrupted",
        ),
        "upstream_invalid_usage" => (
            "上游响应里的用量无效",
            "the upstream response reported invalid usage",
        ),
        "upstream_invalid_response" => ("上游响应无效", "the upstream response was invalid"),
        "upstream_failed_response" => ("上游响应失败", "the upstream response failed"),
        "upstream_credential_invalid" => ("上游凭据无效", "the upstream credential is invalid"),
        "upstream_credential_unavailable" => {
            ("上游凭据不可用", "the upstream credential is unavailable")
        }
        "upstream_candidate_invalid" => (
            "选中的上游候选无效",
            "the selected upstream candidate is invalid",
        ),
        "provider_candidate_invalid" => ("提供商候选无效", "the provider candidate is invalid"),
        "provider_credential" => (
            "提供商凭据没有套用上",
            "the provider credential could not be applied",
        ),
        "provider_unsafe_target" => (
            "组件提供商的目标地址被拒绝",
            "the component provider target was rejected",
        ),
        "provider_invalid_request" => (
            "组件提供商的请求无效",
            "the component provider request was invalid",
        ),
        "provider_normalize" => (
            "组件提供商的响应没有整理成网关格式",
            "the component provider response could not be normalized",
        ),
        "provider_configuration" => (
            "组件提供商的配置不完整",
            "the component provider configuration is incomplete",
        ),
        "provider_invalid_response" => (
            "组件提供商的响应无效",
            "the component provider response was invalid",
        ),
        "upstream_invalid_headers" => (
            "上游响应头无效",
            "the upstream response headers were invalid",
        ),
        "upstream_invalid_content_encoding" => (
            "上游响应的内容编码不受支持",
            "the upstream response content encoding is unsupported",
        ),
        "upstream_response_too_large" => (
            "上游响应超过大小上限",
            "the upstream response was too large",
        ),
        "upstream_attempts_exhausted" => (
            "上游尝试次数已经用尽",
            "the upstream attempt budget was exhausted",
        ),
        "upstream_unavailable" => ("配置的上游不可用", "the configured upstream is unavailable"),
        "upstream_rejected" => ("上游拒绝了这次请求", "the upstream rejected the request"),
        "idempotency_claim_lost" => (
            "图片幂等声明在发给上游之前丢失了",
            "the image idempotency claim was lost before the upstream request was sent",
        ),
        "image_submission_failed" => (
            "图片提交没有完成",
            "the image submission could not be completed",
        ),
        "image_deadline_exceeded" => (
            "图片请求超过了时限",
            "the image request exceeded its deadline",
        ),
        "upstream_image_invalid_json" => (
            "上游图片响应不是有效的 JSON",
            "the upstream image response was not valid JSON",
        ),
        "upstream_image_invalid_payload" => (
            "上游图片响应的内容无效",
            "the upstream image response payload was invalid",
        ),
        "upstream_image_response_too_large" => (
            "上游图片响应超过大小上限",
            "the upstream image response was too large",
        ),
        "upstream_image_too_large" => ("上游图片超过大小上限", "the upstream image was too large"),
        "upstream_image_asset" => (
            "上游图片资源没有存下来",
            "the upstream image asset could not be stored",
        ),
        "upstream_image_asset_unavailable" => (
            "上游图片资源不可用",
            "the upstream image asset is unavailable",
        ),
        "upstream_http_429" => (
            "上游图片请求被限流",
            "the upstream image request was rate limited",
        ),
        "generation_upstream_snapshot_is_unavailable" => (
            "请求失败。失败阶段 generation_upstream_snapshot_is_unavailable",
            "generation upstream snapshot is unavailable",
        ),
        "unsupported_generation_driver" => (
            "请求失败。失败阶段 unsupported_generation_driver",
            "unsupported generation driver",
        ),
        "generation_cancellation_returned_http" => (
            "取消生成时上游返回了失败状态",
            "the upstream returned an unsuccessful status while cancelling generation",
        ),
        "generation_submit_returned_http" => (
            "提交生成时上游返回了失败状态",
            "the upstream returned an unsuccessful status while submitting generation",
        ),
        "generation_submit_response_has_no_job_id" => (
            "请求失败。失败阶段 generation_submit_response_has_no_job_id",
            "generation submit response has no job id",
        ),
        "siliconflow_video_poll_returned_http" => (
            "轮询视频生成时上游返回了失败状态",
            "the upstream returned an unsuccessful status while polling video generation",
        ),
        "seedance_poll_returned_http" => (
            "轮询生成任务时上游返回了失败状态",
            "the upstream returned an unsuccessful status while polling the generation job",
        ),
        "generation_poll_returned_http" => (
            "轮询生成任务时上游返回了失败状态",
            "the upstream returned an unsuccessful status while polling generation",
        ),
        "generation_upstream_http_failed" => (
            "生成任务的上游 HTTP 操作失败",
            "the generation upstream HTTP operation failed",
        ),
        "generation_control_response_is_not_json" => (
            "生成控制响应不是有效的 JSON",
            "the generation control response was not valid JSON",
        ),
        "generation_control_response_too_large" => (
            "生成控制响应超过大小上限",
            "the generation control response exceeded the size limit",
        ),
        "generation_asset_probe_returned_http" => (
            "探测生成资源时上游返回了失败状态",
            "the upstream returned an unsuccessful status while probing the generation asset",
        ),
        "generation_asset_range_mismatch" => (
            "生成资源的范围响应与请求不一致",
            "the generation asset range response did not match the request",
        ),
        "generation_cancel_unconfirmed" => (
            "上游没有确认生成任务已取消",
            "the upstream did not confirm that the generation job was cancelled",
        ),
        "generation_api_prefix_rejected" => (
            "生成路由的 API 前缀不被接受",
            "the generation route API prefix was rejected",
        ),
        "generation_job_id_rejected" => (
            "生成任务标识不符合允许的格式",
            "the generation job id did not match the allowed format",
        ),
        "wasu_wan_resubmit_rejected" => (
            "这个生成任务不能再次提交",
            "this generation job cannot be submitted again",
        ),
        "generation_workflow_mismatch" => (
            "生成路由的工作流与任务不一致",
            "the generation route workflow did not match the job",
        ),
        "filter_assistant_upstream_status" => (
            "筛选助手的上游返回了失败状态",
            "the filter assistant upstream returned an unsuccessful status",
        ),
        "filter_assistant_response_unreadable" => (
            "筛选助手的上游响应没有读完",
            "the filter assistant upstream response could not be read",
        ),
        "generation_asset_url_is_invalid" => (
            "请求失败。失败阶段 generation_asset_url_is_invalid",
            "generation asset URL is invalid",
        ),
        "failed" => (
            "生成任务的上游操作失败",
            "the generation upstream operation failed",
        ),
        "generation_asset_origin_is_not_allowed" => (
            "请求失败。失败阶段 generation_asset_origin_is_not_allowed",
            "configured upstream is unavailable",
        ),
        "unknown_generation_status" => (
            "请求失败。失败阶段 unknown_generation_status",
            "configured upstream is unavailable",
        ),
        "generation_upstream_url_cannot_be_a_base_url" => (
            "请求失败。失败阶段 generation_upstream_url_cannot_be_a_base_url",
            "generation upstream URL cannot be a base URL",
        ),
        "local_storage_failed" => ("本地存储操作失败", "storage operation failed"),
        "generation_request_archive_is_invalid" => (
            "请求失败。失败阶段 generation_request_archive_is_invalid",
            "generation request archive is invalid",
        ),
        "generation_input_archive_is_invalid" => (
            "请求失败。失败阶段 generation_input_archive_is_invalid",
            "generation input archive is invalid",
        ),
        "generation_pixel_contract_is_invalid" => (
            "请求失败。失败阶段 generation_pixel_contract_is_invalid",
            "generation pixel contract is invalid",
        ),
        "comfyui_workflow_id_is_missing" => (
            "请求失败。失败阶段 comfyui_workflow_id_is_missing",
            "ComfyUI workflow id is missing",
        ),
        "comfyui_workflow_template_is_missing" => (
            "请求失败。失败阶段 comfyui_workflow_template_is_missing",
            "ComfyUI workflow template is missing",
        ),
        "oauth_response_is_too_large" => (
            "请求失败。失败阶段 oauth_response_is_too_large",
            "OAuth response is too large",
        ),
        "plugin_returned_invalid_models_json" => (
            "请求失败。失败阶段 plugin_returned_invalid_models_json",
            "configured upstream is unavailable",
        ),
        "plugin_models_response_must_be_a_json_array" => (
            "请求失败。失败阶段 plugin_models_response_must_be_a_json_array",
            "configured upstream is unavailable",
        ),
        "plugin_prepared_request_exceeds_the_4_mib_abi_limit" => (
            "插件准备的请求超过 4MiB ABI 上限",
            "the plugin prepared request exceeded the 4MiB ABI limit",
        ),
        "plugin_returned_an_invalid_prepared_request" => (
            "请求失败。失败阶段 plugin_returned_an_invalid_prepared_request",
            "configured upstream is unavailable",
        ),
        "plugin_requested_unsupported_streaming_transport" => (
            "请求失败。失败阶段 plugin_requested_unsupported_streaming_transport",
            "configured upstream is unavailable",
        ),
        "plugin_returned_invalid_request_body_encoding" => (
            "请求失败。失败阶段 plugin_returned_invalid_request_body_encoding",
            "configured upstream is unavailable",
        ),
        "plugin_prepared_request_body_exceeds_4_mib" => (
            "插件准备的请求正文超过 4MiB 上限",
            "the plugin prepared request body exceeded the 4MiB limit",
        ),
        "plugin_upstream_response_exceeds_its_declared_limit" => (
            "插件上游响应超过声明的大小上限",
            "the plugin upstream response exceeded its declared size limit",
        ),
        "plugin_upstream_response_exceeds_its_declared_abi_limit" => (
            "插件上游响应超过声明的 ABI 大小上限",
            "the plugin upstream response exceeded its declared ABI size limit",
        ),
        "plugin_normalized_response_exceeds_its_declared_limit" => (
            "插件整理后的响应超过声明的大小上限",
            "the plugin normalized response exceeded its declared size limit",
        ),
        "plugin_returned_an_invalid_normalized_response" => (
            "请求失败。失败阶段 plugin_returned_an_invalid_normalized_response",
            "configured upstream is unavailable",
        ),
        "plugin_returned_an_invalid_downstream_status" => (
            "插件返回的下游状态码无效",
            "the plugin returned an invalid downstream status",
        ),
        "plugin_returned_invalid_normalized_body_encoding" => (
            "请求失败。失败阶段 plugin_returned_invalid_normalized_body_encoding",
            "configured upstream is unavailable",
        ),
        "plugin_normalized_response_body_exceeds_its_declared_limit" => (
            "插件整理后的响应正文超过声明的大小上限",
            "the plugin normalized response body exceeded its declared size limit",
        ),
        "component_provider_returned_an_invalid_method" => (
            "请求失败。失败阶段 component_provider_returned_an_invalid_method",
            "component provider returned an invalid method",
        ),
        "component_provider_header_name_is_invalid" => (
            "请求失败。失败阶段 component_provider_header_name_is_invalid",
            "component provider header name is invalid",
        ),
        "component_provider_header_value_is_invalid" => (
            "请求失败。失败阶段 component_provider_header_value_is_invalid",
            "component provider header value is invalid",
        ),
        "plugin_contributes_a_traffic_policy_without_a_component" => (
            "请求失败。失败阶段 plugin_contributes_a_traffic_policy_without_a_component",
            "storage operation failed",
        ),
        "plugin_component_provider_is_unavailable" => (
            "请求失败。失败阶段 plugin_component_provider_is_unavailable",
            "storage operation failed",
        ),
        "plugin_declared_a_component_provider_without_a_component" => (
            "请求失败。失败阶段 plugin_declared_a_component_provider_without_a_component",
            "storage operation failed",
        ),
        "audio_upstream_transport_failed" => (
            "请求失败。失败阶段 audio_upstream_transport_failed",
            "audio upstream transport failed",
        ),
        "audio_upstream_timed_out" => (
            "请求失败。失败阶段 audio_upstream_timed_out",
            "audio upstream timed out",
        ),
        "audio_upstream_rate_limited" => (
            "请求失败。失败阶段 audio_upstream_rate_limited",
            "audio upstream rate limited",
        ),
        "audio_upstream_response_must_be_json" => (
            "请求失败。失败阶段 audio_upstream_response_must_be_json",
            "audio upstream response must be JSON",
        ),
        "audio_upstream_response_must_be_an_object" => (
            "请求失败。失败阶段 audio_upstream_response_must_be_an_object",
            "audio upstream response must be an object",
        ),
        "audio_upstream_response_text_is_missing" => (
            "请求失败。失败阶段 audio_upstream_response_text_is_missing",
            "audio upstream response text is missing",
        ),
        "audio_upstream_must_be_a_string" => (
            "请求失败。失败阶段 audio_upstream_must_be_a_string",
            "configured upstream is unavailable",
        ),
        "audio_upstream_segments_must_be_an_array" => (
            "请求失败。失败阶段 audio_upstream_segments_must_be_an_array",
            "audio upstream segments must be an array",
        ),
        "audio_segment_text_is_missing" => (
            "请求失败。失败阶段 audio_segment_text_is_missing",
            "audio segment text is missing",
        ),
        "audio_upstream_segment_must_be_an_object" => (
            "请求失败。失败阶段 audio_upstream_segment_must_be_an_object",
            "audio upstream segment must be an object",
        ),
        "audio_segment_is_invalid" => (
            "请求失败。失败阶段 audio_segment_is_invalid",
            "configured upstream is unavailable",
        ),
        "audio_segment_tokens_are_invalid" => (
            "请求失败。失败阶段 audio_segment_tokens_are_invalid",
            "audio segment tokens are invalid",
        ),
        "audio_upstream_words_are_invalid" => (
            "请求失败。失败阶段 audio_upstream_words_are_invalid",
            "audio upstream words are invalid",
        ),
        "audio_upstream_word_is_invalid" => (
            "请求失败。失败阶段 audio_upstream_word_is_invalid",
            "audio upstream word is invalid",
        ),
        "audio_upstream_word_text_is_missing" => (
            "请求失败。失败阶段 audio_upstream_word_text_is_missing",
            "audio upstream word text is missing",
        ),
        "openai_codex_transport_is_unavailable" => (
            "请求失败。失败阶段 openai_codex_transport_is_unavailable",
            "OpenAI Codex transport is unavailable",
        ),
        "plugin_execution_timed_out" => (
            "请求失败。失败阶段 plugin_execution_timed_out",
            "plugin execution timed out",
        ),
        "plugin_task_failed" => (
            "请求失败。失败阶段 plugin_task_failed",
            "plugin task failed",
        ),
        "private_fixture_payload" => (
            "请求失败。失败阶段 private_fixture_payload",
            "private-fixture-payload",
        ),
        "session_title_generation_failed" => (
            "请求失败。失败阶段 session_title_generation_failed",
            "session title generation failed",
        ),
        "plugin_returned_an_invalid_upstream_account_id" => (
            "请求失败。失败阶段 plugin_returned_an_invalid_upstream_account_id",
            "plugin returned an invalid upstream account id",
        ),
        "component_provider_adapter_is_declared_but_unavailable" => (
            "请求失败。失败阶段 component_provider_adapter_is_declared_but_unavailable",
            "component provider adapter is declared but unavailable",
        ),
        "archive_filesystem_initialization_failed" => (
            "请求失败。失败阶段 archive_filesystem_initialization_failed",
            "archive filesystem initialization failed",
        ),
        "archive_s3_configuration_is_invalid" => (
            "请求失败。失败阶段 archive_s3_configuration_is_invalid",
            "archive S3 configuration is invalid",
        ),
        "archive_cas_object_integrity_failure" => (
            "请求失败。失败阶段 archive_cas_object_integrity_failure",
            "archive CAS object integrity failure",
        ),
        "archive_compressed_object_integrity_failure" => (
            "请求失败。失败阶段 archive_compressed_object_integrity_failure",
            "archive compressed object integrity failure",
        ),
        "archive_object_size_overflow" => (
            "请求失败。失败阶段 archive_object_size_overflow",
            "archive object size overflow",
        ),
        "archive_writer_is_already_closed" => (
            "请求失败。失败阶段 archive_writer_is_already_closed",
            "archive writer is already closed",
        ),
        "archive_compressed_read_failed" => (
            "请求失败。失败阶段 archive_compressed_read_failed",
            "archive compressed read failed",
        ),
        "archive_object_exceeds_byte_read_limit" => (
            "请求失败。失败阶段 archive_object_exceeds_byte_read_limit",
            "storage operation failed",
        ),
        "archive_object_size_exceeds_this_platform" => (
            "请求失败。失败阶段 archive_object_size_exceeds_this_platform",
            "archive object size exceeds this platform",
        ),
        "archive_readiness_canary_failed" => {
            ("归档就绪检查失败", "archive readiness canary failed")
        }
        "route_is_not_configured" => (
            "请求失败。失败阶段 route_is_not_configured",
            "configured upstream is unavailable",
        ),
        "candidates_are_unavailable" => (
            "请求失败。失败阶段 candidates_are_unavailable",
            "configured upstream is unavailable",
        ),
        "generation_driver_cannot_execute_asynchronous_jobs" => (
            "请求失败。失败阶段 generation_driver_cannot_execute_asynchronous_jobs",
            "configured upstream is unavailable",
        ),
        "wasu_wan_submit_response_has_no_output" => (
            "请求失败。失败阶段 wasu_wan_submit_response_has_no_output",
            "Wasu Wan submit response has no output",
        ),
        "wasu_wan_submit_response_has_no_task_id" => (
            "请求失败。失败阶段 wasu_wan_submit_response_has_no_task_id",
            "Wasu Wan submit response has no task id",
        ),
        "wasu_wan_query_response_has_no_output" => (
            "请求失败。失败阶段 wasu_wan_query_response_has_no_output",
            "Wasu Wan query response has no output",
        ),
        "wasu_wan_query_response_has_no_usage" => (
            "请求失败。失败阶段 wasu_wan_query_response_has_no_usage",
            "Wasu Wan query response has no usage",
        ),
        "google_account_identity_request_failed" => (
            "请求失败。失败阶段 google_account_identity_request_failed",
            "Google account identity request failed",
        ),
        "google_account_identity_returned_http" => (
            "读取 Google 账号身份时上游返回了失败状态",
            "the upstream returned an unsuccessful status while reading the Google account identity",
        ),
        "google_account_identity_response_is_invalid" => (
            "请求失败。失败阶段 google_account_identity_response_is_invalid",
            "Google account identity response is invalid",
        ),
        "google_account_identity_is_missing" => (
            "请求失败。失败阶段 google_account_identity_is_missing",
            "Google account identity is missing",
        ),
        "account_identity_request_failed" => (
            "请求失败。失败阶段 account_identity_request_failed",
            "account identity request failed",
        ),
        "account_identity_returned_http" => (
            "读取账号身份时上游返回了失败状态",
            "the upstream returned an unsuccessful status while reading the account identity",
        ),
        "account_identity_response_is_invalid" => (
            "请求失败。失败阶段 account_identity_response_is_invalid",
            "account identity response is invalid",
        ),
        "account_identity_is_missing" => (
            "请求失败。失败阶段 account_identity_is_missing",
            "account identity is missing",
        ),
        "invalid_oauth_token_response" => (
            "请求失败。失败阶段 invalid_oauth_token_response",
            "invalid OAuth token response",
        ),
        "oauth_token_request_failed" => (
            "请求失败。失败阶段 oauth_token_request_failed",
            "OAuth token request failed",
        ),
        "oauth_token_endpoint_returned_http" => (
            "换取 OAuth 令牌时上游返回了失败状态",
            "the upstream returned an unsuccessful status from the OAuth token endpoint",
        ),
        "claude_oauth_authorization_failed" => (
            "请求失败。失败阶段 claude_oauth_authorization_failed",
            "Claude OAuth authorization failed",
        ),
        "openai_codex_authorization_failed" => (
            "请求失败。失败阶段 openai_codex_authorization_failed",
            "OpenAI Codex authorization failed",
        ),
        "github_copilot_authorization_failed" => (
            "请求失败。失败阶段 github_copilot_authorization_failed",
            "GitHub Copilot authorization failed",
        ),
        "cursor_oauth_poll_returned" => (
            "Cursor 登录轮询返回了失败状态",
            "the Cursor login poll returned an unsuccessful status",
        ),
        "cursor_oauth_returned_invalid_json" => (
            "请求失败。失败阶段 cursor_oauth_returned_invalid_json",
            "Cursor OAuth returned invalid JSON",
        ),
        "cursor_oauth_refresh_failed" => (
            "请求失败。失败阶段 cursor_oauth_refresh_failed",
            "Cursor OAuth refresh failed",
        ),
        "cursor_oauth_refresh_returned" => (
            "Cursor 登录刷新返回了失败状态",
            "the Cursor login refresh returned an unsuccessful status",
        ),
        "cursor_oauth_refresh_returned_invalid_json" => (
            "请求失败。失败阶段 cursor_oauth_refresh_returned_invalid_json",
            "Cursor OAuth refresh returned invalid JSON",
        ),
        "cursor_oauth_did_not_return_a_stable_account_identity" => (
            "请求失败。失败阶段 cursor_oauth_did_not_return_a_stable_account_identity",
            "Cursor OAuth did not return a stable account identity",
        ),
        "kimi_oauth_request_failed" => (
            "请求失败。失败阶段 kimi_oauth_request_failed",
            "Kimi OAuth request failed",
        ),
        "plugin_configuration_read_timed_out" => (
            "请求失败。失败阶段 plugin_configuration_read_timed_out",
            "plugin configuration read timed out",
        ),
        "plugin_service_data_collection_failed" => (
            "请求失败。失败阶段 plugin_service_data_collection_failed",
            "plugin service data collection failed",
        ),
        "plugin_ui_projection_is_invalid" => (
            "请求失败。失败阶段 plugin_ui_projection_is_invalid",
            "plugin UI projection is invalid",
        ),
        "antigravity_request_failed" => (
            "请求失败。失败阶段 antigravity_request_failed",
            "Antigravity request failed",
        ),
        "antigravity_returned_http" => (
            "Antigravity 上游返回了失败状态",
            "the Antigravity upstream returned an unsuccessful status",
        ),
        "invalid_antigravity_response" => (
            "请求失败。失败阶段 invalid_antigravity_response",
            "invalid Antigravity response",
        ),
        "antigravity_model_catalog_is_invalid" => (
            "请求失败。失败阶段 antigravity_model_catalog_is_invalid",
            "Antigravity model catalog is invalid",
        ),
        "antigravity_image_response_has_no_candidates" => (
            "请求失败。失败阶段 antigravity_image_response_has_no_candidates",
            "Antigravity image response has no candidates",
        ),
        "antigravity_image_mime_type_is_unsupported" => (
            "请求失败。失败阶段 antigravity_image_mime_type_is_unsupported",
            "Antigravity image MIME type is unsupported",
        ),
        "antigravity_image_data_is_invalid" => (
            "请求失败。失败阶段 antigravity_image_data_is_invalid",
            "Antigravity image data is invalid",
        ),
        "antigravity_image_encoding_is_invalid" => (
            "请求失败。失败阶段 antigravity_image_encoding_is_invalid",
            "Antigravity image encoding is invalid",
        ),
        "antigravity_returned_no_image" => (
            "请求失败。失败阶段 antigravity_returned_no_image",
            "Antigravity returned no image",
        ),
        "antigravity_response_interrupted" => (
            "请求失败。失败阶段 antigravity_response_interrupted",
            "Antigravity response interrupted",
        ),
        "private_untrusted_marker" => (
            "请求失败。失败阶段 private_untrusted_marker",
            "/private/untrusted-marker",
        ),
        "openai_codex_oauth_refresh_failed" => (
            "请求失败。失败阶段 openai_codex_oauth_refresh_failed",
            "OpenAI Codex OAuth refresh failed",
        ),
        "upstream_returned_invalid_token_usage" => (
            "请求失败。失败阶段 upstream_returned_invalid_token_usage",
            "upstream returned invalid token usage",
        ),
        "egress_attempt_snapshot_conflict" => (
            "请求失败。失败阶段 egress_attempt_snapshot_conflict",
            "egress attempt snapshot conflict",
        ),
        "egress_transport_snapshot_is_malformed" => (
            "请求失败。失败阶段 egress_transport_snapshot_is_malformed",
            "egress transport snapshot is malformed",
        ),
        "egress_connection_binding_is_malformed" => (
            "请求失败。失败阶段 egress_connection_binding_is_malformed",
            "egress connection binding is malformed",
        ),
        "egress_snapshot_is_malformed" => (
            "请求失败。失败阶段 egress_snapshot_is_malformed",
            "egress snapshot is malformed",
        ),
        "transport_diagnostic_route_is_malformed" => (
            "请求失败。失败阶段 transport_diagnostic_route_is_malformed",
            "transport diagnostic route is malformed",
        ),
        "transport_diagnostic_account_is_malformed" => (
            "请求失败。失败阶段 transport_diagnostic_account_is_malformed",
            "transport diagnostic account is malformed",
        ),
        "healthy_admission_epoch_is_malformed" => (
            "请求失败。失败阶段 healthy_admission_epoch_is_malformed",
            "healthy admission epoch is malformed",
        ),
        "upstream_probe_lease_token_is_malformed" => (
            "请求失败。失败阶段 upstream_probe_lease_token_is_malformed",
            "upstream probe lease token is malformed",
        ),
        "generation_asset_upstream_is_unavailable" => (
            "请求失败。失败阶段 generation_asset_upstream_is_unavailable",
            "generation asset upstream is unavailable",
        ),
        "generation_asset_fetch_failed" => (
            "请求失败。失败阶段 generation_asset_fetch_failed",
            "generation asset fetch failed",
        ),
        "generation_asset_fetch_returned_http" => (
            "读取生成资源时上游返回了失败状态",
            "the upstream returned an unsuccessful status while fetching the generation asset",
        ),
        "generation_route_for_does_not_support_generation" => (
            "请求失败。失败阶段 generation_route_for_does_not_support_generation",
            "configured upstream is unavailable",
        ),
        "generation_driver_does_not_implement_the_openai_images_api" => (
            "请求失败。失败阶段 generation_driver_does_not_implement_the_openai_images_api",
            "configured upstream is unavailable",
        ),
        "image_upstream_response_has_no_data_array" => (
            "请求失败。失败阶段 image_upstream_response_has_no_data_array",
            "image upstream response has no data array",
        ),
        "image_upstream_asset_metadata_is_incomplete" => (
            "请求失败。失败阶段 image_upstream_asset_metadata_is_incomplete",
            "image upstream asset metadata is incomplete",
        ),
        "provider_driver_is_not_loaded" => (
            "请求失败。失败阶段 provider_driver_is_not_loaded",
            "configured upstream is unavailable",
        ),
        "compatible_upstream_reservation_metadata_must_be_an_object" => (
            "请求失败。失败阶段 compatible_upstream_reservation_metadata_must_be_an_object",
            "compatible upstream reservation metadata must be an object",
        ),
        "fixture_transport_failure" => (
            "请求失败。失败阶段 fixture_transport_failure",
            "fixture transport failure",
        ),
        "invalid_response" => ("请求失败。失败阶段 invalid_response", "invalid_response"),
        "unauthorized" => ("需要先通过身份验证", "authentication required"),
        "forbidden" => ("没有访问权限", "permission denied"),
        "plugin_input_too_large" => (
            "请求超过流量插件的输入上限",
            "request exceeds traffic plugin input limit",
        ),
        "unpriced_model" => (
            "这个模型在密钥币种下没有价格",
            "model has no price in the key currency",
        ),
        "insufficient_quota" => (
            "可用余额或预算不足",
            "available balance or configured budget is insufficient",
        ),
        "rate_limit_exceeded" => ("请求频率已到上限", "rate limit exceeded"),
        "not_found" => ("没有找到这项资源", "resource not found"),
        "conflict" => ("当前状态和这次修改冲突", "conflict"),
        "invalid_request" => ("请求内容不符合接口要求", "invalid request"),
        "compaction_producer_unavailable" => (
            "没有已授权的上游可以生成 Responses 压缩结果",
            "no available authorized upstream supports Responses compaction production",
        ),
        "schema_secret_analysis_too_complex" => (
            "密钥结构分析超过了支持的复杂度",
            "secret schema analysis exceeds the supported complexity limit",
        ),
        "oauth_refresh_failed" => (
            "授权刷新失败。请检查连接，或重新授权这个账号",
            "Could not refresh authorization. Check the connection or authorize this account again",
        ),
        "service_overloaded" => (
            "网关当时接不住新的请求",
            "service is temporarily overloaded",
        ),
        "database_storage_full" => (
            "数据库存储已满，暂时不能写入。请联系服务操作员。服务凭据不需要重新填写",
            "Database storage is full. Database writes are unavailable; contact the service operator. Service credentials do not need to be re-entered",
        ),
        "database_unavailable" => (
            "数据库暂时连不上，数据和授权都无法核对。服务恢复后请再试。服务凭据不需要重新填写",
            "The database cannot be reached. Data and authorization cannot be verified; try again after service recovery. Service credentials do not need to be re-entered",
        ),
        "database_operation_failed" => ("数据库操作失败", "the database operation failed"),
        "object_storage_failed" => ("对象存储操作失败", "object storage operation failed"),
        "upstream_http_operation_failed" => {
            ("上游 HTTP 操作失败", "the upstream HTTP operation failed")
        }
        "upstream_error" => ("上游请求失败", "upstream request failed"),
        "proxy_memory_temporary" => (
            "网关临时内存装不下这次工作副本",
            "the gateway temporary memory budget could not hold this working set",
        ),
        "request_archive_prepare_failed" => (
            "请求归档在准备阶段失败",
            "preparing the request archive failed",
        ),
        "request_archive_reservation_failed" => (
            "请求归档的容量预留失败",
            "the request archive capacity reservation failed",
        ),
        "request_archive_capture_failed" => (
            "请求归档在写入阶段失败",
            "capturing the request archive failed",
        ),
        "request_archive_commit_failed" => (
            "请求归档在提交阶段失败",
            "committing the request archive failed",
        ),
        "request_record_persist_failed" => (
            "请求记录没有写入数据库",
            "the request record could not be persisted",
        ),
        "response_archive_too_large" => (
            "已保存的原始返回 JSON 超过读取上限，无法读回",
            "the stored raw return document could not be read back because it is larger than the reader limit",
        ),
        "plugin_runtime_failed" => ("插件运行时失败", "the plugin runtime failed"),
        "plugin_invalid_result" => (
            "插件返回了无效结果",
            "the plugin returned an invalid result",
        ),
        "plugin_reported_error" => (
            "插件报告了执行错误",
            "the plugin reported an execution error",
        ),
        "plugin_reported_invalid_error" => (
            "插件报告的错误内容无效",
            "the plugin reported an invalid error",
        ),
        "plugin_execution_failed" => ("插件执行失败", "plugin execution failed"),
        "plugin_snapshot_load_failed" => (
            "应用插件快照没有加载成功",
            "the application plugin snapshot did not load",
        ),
        "price_source_http_status" => (
            "价格源返回了失败的 HTTP 状态",
            "the price source returned an unsuccessful HTTP status",
        ),
        "price_source_response_too_large" => (
            "价格源响应超过大小上限",
            "the price source response exceeded the size limit",
        ),
        "price_source_invalid_json" => (
            "价格源返回的内容不是有效的 JSON",
            "the price source returned invalid JSON",
        ),
        "price_source_timeout" => (
            "价格源同步超时",
            "the price source synchronization timed out",
        ),
        "price_source_catalog_invalid" => {
            ("价格源目录无效", "the price source catalog was invalid")
        }
        "price_source_model_limit" => (
            "价格源目录超过模型数量上限",
            "the price source catalog exceeded the model limit",
        ),
        "price_source_no_prices" => (
            "价格源目录里没有可用的词元价格",
            "the price source catalog contained no usable token prices",
        ),
        "all_configured_model_price_sources_are_unavailable" => (
            "配置的模型价格源都不可用",
            "all configured model price sources are unavailable",
        ),
        "plugin_release_dns_failed" => (
            "插件发行包的域名解析失败",
            "GitHub plugin release DNS resolution failed",
        ),
        "plugin_release_connection_failed" => (
            "插件发行包的连接失败",
            "GitHub plugin release connection failed",
        ),
        "plugin_release_request_failed" => (
            "插件发行包的请求失败",
            "GitHub plugin release request failed",
        ),
        "plugin_release_body_failed" => (
            "插件发行包的正文传输失败",
            "GitHub plugin release response body transfer failed",
        ),
        "plugin_release_timeout" => (
            "插件发行包的下载超时",
            "GitHub plugin release download timed out",
        ),
        "plugin_release_http_failed" => (
            "插件发行包的服务器返回了失败状态",
            "GitHub plugin release server returned an unsuccessful HTTP status",
        ),
        "plugin_release_client_failed" => (
            "插件发行包的 HTTP 客户端没有建起来",
            "GitHub plugin release HTTP client setup failed",
        ),
        _ => return None,
    })
}

#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn public_error_meaning(code: &str) -> Option<&'static str> {
    public_error_sentences(code).map(|(_zh, en)| en)
}

pub(crate) fn explained_failure_body(
    lead_zh: &str,
    lead_en: &str,
    error_code: &str,
    facts: ErrorFacts,
) -> Vec<u8> {
    let code = if is_public_error_code(error_code) {
        error_code
    } else {
        "upstream_error"
    };
    let (detail_zh, detail_en) = render_bilingual(code, facts);
    let message = format!("{lead_zh}。{detail_zh}");
    let message_en = format!("{lead_en}. {detail_en}");
    serde_json::to_vec(&json!({
        "error": {
            "code": code,
            "message": message,
            "message_en": message_en,
            "type": "upstream_error"
        }
    }))
    .unwrap_or_else(|_| {
        r#"{"error":{"code":"upstream_error","message":"上游请求失败。","message_en":"upstream request failed.","type":"upstream_error"}}"#
            .as_bytes()
            .to_vec()
    })
}

impl From<sqlx::Error> for AppError {
    fn from(error: sqlx::Error) -> Self {
        #[cfg(test)]
        if let sqlx::Error::Database(database) = &error
            && let Some(code) = database.code()
            && code.len() == 5
            && code.bytes().all(|byte| byte.is_ascii_alphanumeric())
        {
            eprintln!("database operation failed: SQLSTATE={code}");
        }
        // Driver messages can echo bound values (including identifiers or
        // encrypted credential envelopes). Keep a stable diagnostic class,
        // never the driver-provided message, in application logs.
        tracing::error!(
            error_kind = sqlx_error_kind(&error),
            "database operation failed"
        );
        if let Some(failure) = DatabaseFailure::from_sqlx(&error) {
            failure.observe();
            return Self::DatabaseUnavailable(failure);
        }
        match error {
            // Pool exhaustion is a bounded, retryable capacity failure. Keeping
            // it distinct from corrupt data and programmer errors prevents a
            // saturated pool from surfacing as a misleading HTTP 500.
            sqlx::Error::PoolTimedOut => Self::capacity(
                "database_pool_exhausted",
                ErrorFacts::new().kind("pool_timeout"),
            ),
            _ => Self::internal_kind("database_operation_failed", sqlx_error_kind(&error)),
        }
    }
}

impl From<object_store::Error> for AppError {
    fn from(_error: object_store::Error) -> Self {
        // S3 errors may include the endpoint, access-key identifier, object
        // locator, or a signed URL. None of those belong in logs or in an
        // AppError that a higher layer could later log with Display.
        tracing::error!(
            error_kind = "object_store",
            "object storage operation failed"
        );
        Self::storage_with(
            "object_storage_failed",
            ErrorFacts::new().kind("object_store"),
        )
    }
}

impl From<reqwest::Error> for AppError {
    fn from(error: reqwest::Error) -> Self {
        tracing::warn!(
            is_timeout = error.is_timeout(),
            is_connect = error.is_connect(),
            "upstream HTTP operation failed"
        );
        // reqwest's Display includes the request URL, which may carry a
        // provider-supplied signature or token in its query string.
        let facts = if error.is_timeout() {
            ErrorFacts::new().stage("timeout")
        } else if error.is_connect() {
            ErrorFacts::new().stage("connect")
        } else {
            ErrorFacts::new().stage("http_operation")
        };
        Self::upstream_with("upstream_http_operation_failed", facts)
    }
}

pub(crate) fn sqlx_error_kind(error: &sqlx::Error) -> &'static str {
    match error {
        sqlx::Error::Configuration(_) => "configuration",
        sqlx::Error::Database(error) if error.code().as_deref() == Some("57014") => {
            "query_cancelled"
        }
        sqlx::Error::Database(error) if error.code().as_deref() == Some("55P03") => {
            "lock_unavailable"
        }
        sqlx::Error::Database(error)
            if error.code().as_deref().is_some_and(|code| {
                matches!(code, "5" | "6" | "SQLITE_BUSY" | "SQLITE_LOCKED")
            }) =>
        {
            "database_busy"
        }
        sqlx::Error::Database(_) => "database",
        sqlx::Error::Io(_) => "io",
        sqlx::Error::Tls(_) => "tls",
        sqlx::Error::Protocol(_) => "protocol",
        sqlx::Error::RowNotFound => "row_not_found",
        sqlx::Error::TypeNotFound { .. } => "type_not_found",
        sqlx::Error::ColumnIndexOutOfBounds { .. } => "column_index",
        sqlx::Error::ColumnNotFound(_) => "column_not_found",
        sqlx::Error::ColumnDecode { .. } => "column_decode",
        sqlx::Error::Encode(_) => "encode",
        sqlx::Error::Decode(_) => "decode",
        sqlx::Error::AnyDriverError(_) => "driver",
        sqlx::Error::PoolTimedOut => "pool_timeout",
        sqlx::Error::PoolClosed => "pool_closed",
        sqlx::Error::WorkerCrashed => "worker_crashed",
        sqlx::Error::Migrate(_) => "migration",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use axum::body::to_bytes;

    use super::*;

    #[test]
    fn diagnostic_categories_do_not_echo_error_payloads() {
        const CANARY: &str = "panic: config_secret=should-never-enter-a-log";
        for (error, expected) in [
            (AppError::Conflict(CANARY.to_owned()), "conflict"),
            (AppError::BadRequest(CANARY.to_owned()), "invalid_request"),
            (
                AppError::CompactionProducerUnavailable,
                "compaction_producer_unavailable",
            ),
            (
                AppError::SchemaSecretAnalysisTooComplex,
                "schema_secret_analysis_too_complex",
            ),
            (
                AppError::upstream_with(
                    "upstream_unavailable",
                    crate::error::ErrorFacts::new()
                        .stage("error_diagnostic_categories_do_not_echo_error_payloads"),
                ),
                "upstream_unavailable",
            ),
            (
                AppError::storage("object_storage_failed"),
                "object_storage_failed",
            ),
        ] {
            let category = error.diagnostic_category();
            assert_eq!(category, expected);
            assert!(!category.contains(CANARY));
        }
    }

    #[tokio::test]
    async fn secret_schema_complexity_has_a_distinct_safe_error_code() {
        let response = AppError::SchemaSecretAnalysisTooComplex.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body["error"]["code"], "schema_secret_analysis_too_complex");
    }

    #[tokio::test]
    async fn retryable_limit_response_has_fixed_reason_and_retry_after() {
        let response = AppError::LimitExceeded {
            reason: LimitReason::RpmExhausted,
            retry_after_seconds: Some(17),
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "17");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "rate_limit_exceeded");
        assert_eq!(body["error"]["reason"], "rpm_exhausted");
        assert_eq!(body["error"]["retryable"], true);
    }

    #[tokio::test]
    async fn database_pool_timeout_is_retryable_overload() {
        let error = AppError::from(sqlx::Error::PoolTimedOut);
        assert_eq!(error.diagnostic_category(), "database_pool_exhausted");

        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
        assert_eq!(
            response
                .extensions()
                .get::<PublicErrorCode>()
                .map(|code| code.0),
            Some("database_pool_exhausted")
        );
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "database_pool_exhausted");
        assert_eq!(
            body["error"]["message"],
            "数据库连接池等待超时。错误类型 pool_timeout。"
        );
        assert_eq!(
            body["error"]["message_en"],
            "database connection pool timed out. error kind pool_timeout."
        );
    }

    #[tokio::test]
    async fn permanent_limit_response_has_no_retry_after() {
        let response = AppError::LimitExceeded {
            reason: LimitReason::LifetimeBudgetExhausted,
            retry_after_seconds: None,
        }
        .into_response();

        assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(!response.headers().contains_key(header::RETRY_AFTER));
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "insufficient_quota");
        assert_eq!(body["error"]["reason"], "lifetime_budget_exhausted");
        assert_eq!(body["error"]["retryable"], false);
    }

    #[tokio::test]
    async fn service_capacity_is_not_reported_as_a_key_rate_limit() {
        let response = AppError::Overloaded.into_response();

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers().get(header::RETRY_AFTER).unwrap(), "1");
        let body: serde_json::Value =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(body["error"]["code"], "service_overloaded");
        assert!(body["error"].get("reason").is_none());
    }

    #[test]
    fn dependency_errors_do_not_retain_secret_bearing_driver_messages() {
        let object_error = object_store::Error::Generic {
            store: "signed-url?token=object-secret",
            source: Box::new(std::io::Error::other("access-key-secret")),
        };
        let rendered = AppError::from(object_error).to_string();
        assert_eq!(rendered, "storage error: object_storage_failed");
        assert!(!rendered.contains("object-secret"));
        assert!(!rendered.contains("access-key-secret"));

        let request_error = reqwest::Client::builder()
            .build()
            .unwrap()
            .get("http://[::1?token=request-secret")
            .build()
            .expect_err("invalid secret-bearing URL");
        let rendered = AppError::from(request_error).to_string();
        assert_eq!(
            rendered,
            "configured upstream is unavailable: upstream_http_operation_failed"
        );
        assert!(!rendered.contains("request-secret"));
    }
}

/// Fixed, public dependency diagnostics. Never retain driver messages or URLs.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum DatabaseFailure {
    #[error(
        "Database storage is full. Database writes are unavailable; contact the service operator. Service credentials do not need to be re-entered."
    )]
    StorageFull,
    #[error(
        "The database cannot be reached. Data and authorization cannot be verified; try again after service recovery. Service credentials do not need to be re-entered."
    )]
    Unavailable,
}

static LAST_DATABASE_FAILURE: std::sync::Mutex<Option<(DatabaseFailure, u64)>> =
    std::sync::Mutex::new(None);

impl DatabaseFailure {
    pub const fn code(self) -> &'static str {
        match self {
            Self::StorageFull => "database_storage_full",
            Self::Unavailable => "database_unavailable",
        }
    }

    pub(crate) fn from_sqlx(error: &sqlx::Error) -> Option<Self> {
        match error {
            sqlx::Error::Database(error) => Self::from_sqlstate(error.code().as_deref()?),
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed => {
                Some(Self::Unavailable)
            }
            _ => None,
        }
    }

    fn from_sqlstate(code: &str) -> Option<Self> {
        match code {
            "53100" | "13" | "SQLITE_FULL" => Some(Self::StorageFull),
            "57P01" | "57P02" | "57P03" => Some(Self::Unavailable),
            code if code.starts_with("08") => Some(Self::Unavailable),
            _ => None,
        }
    }

    fn observe(self) {
        let observed_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        if let Ok(mut last) = LAST_DATABASE_FAILURE.lock() {
            *last = Some((self, observed_at));
        }
    }

    pub(crate) fn last_observed() -> Option<serde_json::Value> {
        let last = LAST_DATABASE_FAILURE.lock().ok()?;
        let (failure, observed_at) = (*last)?;
        Some(json!({"code": failure.code(), "observed_at": observed_at}))
    }
}

#[cfg(test)]
mod database_failure_tests {
    use super::*;
    use axum::body::to_bytes;

    #[test]
    fn disk_full_requires_explicit_driver_evidence_and_is_never_inferred_from_capacity() {
        for code in ["53100", "13", "SQLITE_FULL"] {
            assert_eq!(
                DatabaseFailure::from_sqlstate(code),
                Some(DatabaseFailure::StorageFull)
            );
        }
        for code in ["53200", "53300", "57014", "55P03", "23505"] {
            assert_eq!(DatabaseFailure::from_sqlstate(code), None, "{code}");
        }
        assert_eq!(DatabaseFailure::from_sqlx(&sqlx::Error::PoolTimedOut), None);
        assert_eq!(
            DatabaseFailure::from_sqlstate("08006"),
            Some(DatabaseFailure::Unavailable)
        );
    }

    #[tokio::test]
    async fn dependency_failure_response_has_fixed_code_without_raw_driver_message() {
        for failure in [DatabaseFailure::StorageFull, DatabaseFailure::Unavailable] {
            let response = AppError::DatabaseUnavailable(failure).into_response();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
            let body = to_bytes(response.into_body(), 8192).await.unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["error"]["code"], failure.code());
            assert!(
                body["error"]["message_en"]
                    .as_str()
                    .unwrap()
                    .contains("credentials do not need")
            );
            assert!(body["error"]["message"].as_str().unwrap().contains("凭据"));
        }
    }
}

/// Fixed release-fetch stages; never retain transport errors, URLs or credentials.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PluginReleaseDownloadFailure {
    #[error("GitHub plugin release DNS resolution failed")]
    Dns,
    #[error("GitHub plugin release connection failed")]
    Connection,
    #[error("GitHub plugin release request failed")]
    Request,
    #[error("GitHub plugin release response body transfer failed")]
    Body,
    #[error("GitHub plugin release download timed out")]
    Timeout,
    #[error("GitHub plugin release server returned an unsuccessful HTTP status")]
    RemoteHttp,
    #[error("GitHub plugin release HTTP client setup failed")]
    Client,
}

impl PluginReleaseDownloadFailure {
    pub const fn code(self) -> &'static str {
        match self {
            Self::Dns => "plugin_release_dns_failed",
            Self::Connection => "plugin_release_connection_failed",
            Self::Request => "plugin_release_request_failed",
            Self::Body => "plugin_release_body_failed",
            Self::Timeout => "plugin_release_timeout",
            Self::RemoteHttp => "plugin_release_http_failed",
            Self::Client => "plugin_release_client_failed",
        }
    }
}
