/// One catalog owns classification, public copy and recovery semantics.
/// Protocol adapters may change envelope shape, never the meaning of a reason.
macro_rules! reasons {
    ($( $variant:ident => ($code:literal, $message:literal, $category:literal,
        $retryable:literal, $action:literal, [$($alias:literal),*]) ),+ $(,)?) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum UpstreamReason { $($variant),+ }

        impl UpstreamReason {
            pub(crate) fn from_marker(code: &str) -> Option<Self> {
                match code { $($code => Some(Self::$variant),)+ _ => None }
            }
            pub(crate) fn from_code(code: &str) -> Option<Self> {
                match code { $($code $(| $alias)* => Some(Self::$variant),)+ _ => None }
            }
            pub(crate) const fn code(self) -> &'static str {
                match self { $(Self::$variant => $code,)+ }
            }
            pub(crate) const fn message(self) -> &'static str {
                match self { $(Self::$variant => $message,)+ }
            }
            pub(crate) const fn category(self) -> &'static str {
                match self { $(Self::$variant => $category,)+ }
            }
            pub(crate) const fn retryable(self) -> bool {
                match self { $(Self::$variant => $retryable,)+ }
            }
            pub(crate) const fn action(self) -> &'static str {
                match self { $(Self::$variant => $action,)+ }
            }
        }
    };
}

reasons! {
    NoActivePlan => ("no_active_plan", "当前账号没有可用套餐", "quota", false, "check_plan", ["NoAvailablePlan", "NoActivePlan", "NoPlan"]),
    InsufficientQuota => ("insufficient_quota", "The upstream account has insufficient quota", "quota", false, "check_quota", ["insufficient_user_quota"]),
    AuthenticationInvalid => ("authentication_invalid", "The upstream authentication credentials are invalid", "authentication", false, "check_credentials", ["invalid_api_key"]),
    AuthenticationExpired => ("authentication_expired", "The upstream authentication credentials have expired", "authentication", false, "check_credentials", ["api_key_expired", "token_expired"]),
    RateLimited => ("rate_limited", "The upstream request rate limit was exceeded", "rate_limit", true, "retry_later", ["rate_limit_exceeded", "ModelAccountTpmRateLimitExceeded"]),
    ModelUnavailable => ("model_unavailable", "The requested upstream model is unavailable", "availability", false, "check_model", ["model_not_found", "unsupported_model"]),
    ContextLengthExceeded => ("context_length_exceeded", "The input exceeds the model context window", "request", false, "shorten_context", []),
    InvalidEncryptedContent => ("invalid_encrypted_content", "The upstream could not validate the encrypted context", "request", false, "rebuild_context", []),
    InvalidPrompt => ("invalid_prompt", "The upstream rejected the prompt as invalid", "request", false, "revise_request", []),
    MisalignmentPolicyViolation => ("misalignment_policy_violation", "The upstream safety system blocked the request for potentially unintended activity", "policy", false, "review_request", []),
    CyberPolicy => ("cyber_policy", "The upstream safety system blocked the request for cybersecurity risk", "policy", false, "review_request", []),
    BioPolicy => ("bio_policy", "The upstream safety system blocked the request for biological safety risk", "policy", false, "review_request", []),
    UsageNotIncluded => ("usage_not_included", "The upstream plan does not include this usage", "quota", false, "check_plan", []),
    ServerOverloaded => ("server_is_overloaded", "The upstream service is overloaded", "availability", true, "retry_later", ["slow_down"]),
    InvalidValue => ("invalid_value", "A request parameter has an invalid value", "request", false, "revise_request", []),
    InvalidType => ("invalid_type", "A request parameter has an invalid type", "request", false, "revise_request", []),
    MissingParameter => ("missing_required_parameter", "A required request parameter is missing", "request", false, "revise_request", []),
    UnsupportedParameter => ("unsupported_parameter", "The upstream does not support a request parameter", "request", false, "revise_request", ["unknown_parameter"]),
    UnsupportedValue => ("unsupported_value", "The upstream does not support a request parameter value", "request", false, "revise_request", []),
    InvalidRequest => ("invalid_request_error", "The upstream rejected the request format", "request", false, "revise_request", []),
}

impl UpstreamReason {
    pub(crate) const fn message_zh(self) -> &'static str {
        match self {
            Self::NoActivePlan => "当前上游账号没有可用套餐",
            Self::InsufficientQuota => "当前上游账号额度不足",
            Self::AuthenticationInvalid => "上游账号凭证无效",
            Self::AuthenticationExpired => "上游账号凭证已过期",
            Self::RateLimited => "请求触发了上游速率限制",
            Self::ModelUnavailable => "请求的上游模型不可用",
            Self::ContextLengthExceeded => "请求超过模型的上下文窗口",
            Self::InvalidEncryptedContent => "上游无法验证加密上下文",
            Self::InvalidPrompt => "上游判定请求内容无效",
            Self::MisalignmentPolicyViolation => "上游安全系统因潜在的非预期活动拒绝了请求",
            Self::CyberPolicy => "上游安全系统因网络安全风险拒绝了请求",
            Self::BioPolicy => "上游安全系统因生物安全风险拒绝了请求",
            Self::UsageNotIncluded => "当前上游套餐不包含此次用量",
            Self::ServerOverloaded => "上游服务当前过载",
            Self::InvalidValue => "请求参数的值无效",
            Self::InvalidType => "请求参数的类型无效",
            Self::MissingParameter => "请求缺少必填参数",
            Self::UnsupportedParameter => "上游不支持请求中的参数",
            Self::UnsupportedValue => "上游不支持请求中的参数值",
            Self::InvalidRequest => "上游拒绝了请求格式",
        }
    }

    pub(crate) const fn client_code(self) -> &'static str {
        match self {
            Self::RateLimited => "rate_limit_exceeded",
            _ => self.code(),
        }
    }
}
