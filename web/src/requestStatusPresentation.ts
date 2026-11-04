import { egressAddress } from './operator/requestEgress.js';
import { requestEgressCopy } from './operator/requestEgressCopy.js';
import type { RequestEgressObservation, RequestView, SupplierError } from './types.js';

type RequestLocale = 'zh-CN' | 'en';

export type RequestOutcome = 'running' | 'waiting_upstream' | 'reading_body' | 'delivering' | 'completed' | 'cancelled' | 'interrupted' | 'failed' | 'unknown';

const recordedFailureCopy: Record<string, [string, string]> = {
  upstream_eof_without_terminal: ['上游响应在结束事件前中断', 'Upstream response ended before a terminal event'],
  upstream_stream_read_error: ['读取上游响应时连接中断', 'The upstream response stream was interrupted'],
  upstream_stream: ['上游响应流中断', 'The upstream response stream was interrupted'],
  upstream_incomplete_response: ['上游响应未完整结束', 'The upstream response did not finish'],
  upstream_response_terminal_conflict: ['上游响应包含冲突的结束信息', 'The upstream response contained conflicting terminal information'],
  upstream_transport_timeout: ['等待上游响应超时', 'The upstream request timed out'],
  upstream_transport_outer_deadline: ['等待上游响应超过请求时限', 'The upstream request exceeded its overall deadline'],
  upstream_transport_connection_reset: ['上游连接被重置', 'The upstream connection was reset'],
  upstream_transport_http2_reset: ['上游 HTTP/2 流被重置', 'The upstream HTTP/2 stream was reset'],
  upstream_transport_http2_goaway: ['上游 HTTP/2 连接收到 GOAWAY', 'The upstream HTTP/2 connection received GOAWAY'],
  upstream_http2_reset: ['上游 HTTP/2 流被重置', 'The upstream HTTP/2 stream was reset'],
  upstream_http2_goaway: ['上游 HTTP/2 连接收到 GOAWAY', 'The upstream HTTP/2 connection received GOAWAY'],
  upstream_transport_body: ['发送上游请求内容失败', 'Sending the upstream request body failed'],
  upstream_transport_decode: ['解析上游响应失败', 'Decoding the upstream response failed'],
  upstream_transport_request: ['发送上游请求失败', 'Sending the upstream request failed'],
  upstream_transport_other: ['上游网络请求失败', 'The upstream network request failed'],
  upstream_connection: ['上游连接失败', 'The upstream connection failed'],
  proxy_connect_failed: ['出网代理连接失败', 'The egress proxy connection failed'],
  dns_resolution_failed: ['上游域名解析失败', 'Upstream DNS resolution failed'],
  tls_handshake_failed: ['上游 TLS 握手失败', 'The upstream TLS handshake failed'],
  upstream_connect_failed: ['上游 TCP 连接失败', 'The upstream TCP connection failed'],
  service_overloaded: ['网关当时接不住新的请求', 'The gateway could not take another request'],
  proxy_memory_ingress: ['网关内存装不下这次请求体，也没有写入对象存储', 'The gateway memory budget could not hold the request body, and the body was not written to object storage'],
  proxy_memory_json: ['网关内存装不下解析后的 JSON 请求体', 'The gateway memory budget could not hold the parsed JSON body'],
  proxy_memory_projection: ['网关内存装不下这次会话正文归档', 'The gateway memory could not hold this conversation archive'],
  proxy_memory_capacity: ['保留请求的内存分区已满', 'The retained request partition was full'],
  proxy_memory_plugin: ['网关内存装不下流量插件的工作副本', 'The gateway memory budget could not hold the traffic plugin working set'],
  proxy_memory_route: ['网关内存装不下路由准备', 'The gateway memory budget could not hold route preparation'],
  upstream_response_memory_capacity: ['网关内存装不下上游响应，这次请求没有再次发送', 'The gateway memory budget could not hold the upstream response, and the request was not sent again'],
  database_pool_exhausted: ['数据库连接池等待超时', 'The database connection pool timed out'],
  request_spool_read_failed: ['已暂存的请求体无法读回', 'The stored request body could not be read back'],
  media_concurrency_permit: ['媒体并发许可当时没有空位', 'The media concurrency permit could not be acquired'],
  plugin_installation_lock: ['插件安装锁当时被占用', 'The plugin installation lock could not be acquired'],
  plugin_installation_capacity: ['插件安装容量当时已满', 'Plugin installation capacity is temporarily unavailable'],
  plugin_subset_capacity: ['插件子集发布容量当时已满', 'Plugin subset publication capacity is temporarily unavailable'],
  plugin_retained_copy_capacity: ['插件安装副本的复制容量当时已满', 'Plugin installation copy capacity is temporarily unavailable'],
  plugin_publish_lock: ['插件发布锁当时被占用', 'The plugin publication lock could not be acquired'],
  plugin_compilation_capacity: ['插件编译容量当时已满', 'Plugin compilation capacity is temporarily unavailable'],
  plugin_snapshot_unavailable: ['应用插件快照当时还没准备好', 'The application plugin snapshot is not ready'],
  plugin_pin_timeout: ['应用插件快照没有在时限内准备好', 'The application plugin snapshot was not ready before the deadline'],
  plugin_snapshot_refresh_timeout: ['应用插件快照刷新没有在时限内完成', 'The application plugin snapshot refresh did not finish before the deadline'],
  plugin_export_capacity: ['插件导出容量当时已满', 'Plugin export capacity is temporarily unavailable'],
  plugin_release_fetch_capacity: ['插件发行包下载容量当时已满', 'Plugin release fetch capacity is temporarily unavailable'],
  control_request_body_capacity: ['控制面请求体当时无法接入', 'The control-plane request body could not be admitted'],
  cloud_webhook_body_capacity: ['云端回调请求体当时无法接入', 'The cloud webhook request body could not be admitted'],
  operator_inference_memory: ['网关内存装不下这次操作台推理请求体', 'The gateway memory budget could not hold the operator inference request body'],
  transport_proxy_storage_unavailable: ['传输代理组的存储当时不可用', 'Transport proxy group storage is unavailable'],
  upstream_candidate_preparation_exhausted: ['已授权的上游候选在发出请求前就用尽了', 'Authorized upstream candidate preparation was exhausted before dispatch'],
  component_target: ['组件提供商的目标地址被拒绝', 'The component provider target was rejected'],
  component_network_client: ['组件提供商的网络客户端没有建起来', 'The component provider network client could not be created'],
  component_credential: ['组件提供商的凭据没有套用上', 'The component provider credential could not be applied'],
  component_response: ['组件提供商的响应没有读完', 'The component provider response could not be read'],
  component_normalize: ['组件提供商的响应没有整理成网关格式', 'The component provider response could not be normalized'],
  component_request_header: ['组件提供商的请求头无效', 'The component provider request headers were invalid'],
  quota_reset_permit: ['上游额度重置的并发许可当时没有空位', 'The upstream quota reset permit could not be acquired'],
  quota_reset_budget: ['上游额度重置的预算没有读出来', 'The upstream quota reset budget could not be read'],
  quota_reset_transport: ['上游额度重置的传输没有准备好', 'The upstream quota reset transport could not be prepared'],
  quota_reset_read_failed: ['上游额度积分没有读到', 'The upstream quota credit read failed'],
  quota_reset_timeout: ['读取上游额度积分超时', 'The upstream quota credit read timed out'],
  quota_reset_normalize: ['上游额度积分的响应没有读懂', 'The upstream quota credit response could not be read'],
  quota_reset_credits_unavailable: ['当时没有可用的上游额度重置积分', 'Fresh supplier quota reset credits are unavailable'],
  quota_reset_transport_timeout: ['上游额度重置的传输准备超时', 'The upstream quota reset transport timed out'],
  no_authorized_upstream_candidate: ['这条密钥没有可发送的上游账号', 'This key had no upstream account to send to'],
  request_spool_unavailable: ['请求体暂存当时不可用', 'Request-body storage was unavailable'],
  request_body_too_large: ['请求体超过大小限制', 'The request body exceeded the size limit'],
  request_body_timeout: ['请求体没有在时限内收完', 'The request body was not received in time'],
  request_body_read_failed: ['请求体没有读完', 'The request body could not be read'],
  rate_limit_exceeded: ['请求频率已到上限', 'The request rate limit was reached'],
  invalid_request: ['请求内容不符合接口要求', 'The request did not match the API'],
  forbidden: ['这条密钥没有访问权限', 'This key has no access to the route'],
  upstream_read_timeout: ['读取上游响应超时', 'Reading the upstream response timed out'],
  upstream_request_timeout: ['等待上游请求响应超时', 'The upstream request timed out'],
  gateway_owner_lost: ['网关副本已结束，这条连接不再处理', 'The gateway replica ended and is no longer handling this connection'],

  proxy_memory_temporary: ['网关临时内存装不下这次工作副本', 'The gateway temporary memory budget could not hold this working set'],
  request_archive_prepare_failed: ['请求归档在准备阶段失败', 'Preparing the request archive failed'],
  request_archive_reservation_failed: ['请求归档的容量预留失败', 'The request archive capacity reservation failed'],
  request_archive_capture_failed: ['请求归档在写入阶段失败', 'Capturing the request archive failed'],
  request_archive_commit_failed: ['请求归档在提交阶段失败', 'Committing the request archive failed'],
  request_record_persist_failed: ['请求记录没有写入数据库', 'The request record could not be persisted'],
  response_archive_too_large: ['已保存的原始返回 JSON 超过读取上限，无法读回', 'The stored raw return document could not be read back because it is larger than the reader limit'],
  database_operation_failed: ['数据库操作失败', 'The database operation failed'],
  object_storage_failed: ['对象存储操作失败', 'Object storage operation failed'],
  local_storage_failed: ['本地存储操作失败', 'Local storage operation failed'],
  upstream_rejected: ['上游拒绝了这次请求', 'The upstream rejected the request'],
  upstream_http_operation_failed: ['上游 HTTP 操作失败', 'The upstream HTTP operation failed'],
  plugin_runtime_failed: ['插件运行时失败', 'The plugin runtime failed'],
  plugin_snapshot_load_failed: ['应用插件快照没有加载成功', 'The application plugin snapshot did not load'],
  generation_upstream_http_failed: ['生成任务的上游 HTTP 操作失败', 'The generation upstream HTTP operation failed'],
  generation_cancellation_returned_http: ['取消生成时上游返回了失败状态', 'The upstream returned an unsuccessful status while cancelling generation'],
  generation_submit_returned_http: ['提交生成时上游返回了失败状态', 'The upstream returned an unsuccessful status while submitting generation'],
  generation_poll_returned_http: ['轮询生成任务时上游返回了失败状态', 'The upstream returned an unsuccessful status while polling generation'],
  siliconflow_video_poll_returned_http: ['轮询视频生成时上游返回了失败状态', 'The upstream returned an unsuccessful status while polling video generation'],
  seedance_poll_returned_http: ['轮询生成任务时上游返回了失败状态', 'The upstream returned an unsuccessful status while polling the generation job'],
  generation_asset_fetch_returned_http: ['读取生成资源时上游返回了失败状态', 'The upstream returned an unsuccessful status while fetching the generation asset'],
  generation_asset_probe_returned_http: ['探测生成资源时上游返回了失败状态', 'The upstream returned an unsuccessful status while probing the generation asset'],
  generation_asset_range_mismatch: ['生成资源的范围响应与请求不一致', 'The generation asset range response did not match the request'],
  generation_control_response_too_large: ['生成控制响应超过大小上限', 'The generation control response exceeded the size limit'],
  generation_cancel_unconfirmed: ['上游没有确认生成任务已取消', 'The upstream did not confirm that the generation job was cancelled'],
  generation_api_prefix_rejected: ['生成路由的 API 前缀不被接受', 'The generation route API prefix was rejected'],
  generation_job_id_rejected: ['生成任务标识不符合允许的格式', 'The generation job id did not match the allowed format'],
  generation_workflow_mismatch: ['生成路由的工作流与任务不一致', 'The generation route workflow did not match the job'],
  wasu_wan_resubmit_rejected: ['这个生成任务不能再次提交', 'This generation job cannot be submitted again'],
  filter_assistant_upstream_status: ['筛选助手的上游返回了失败状态', 'The filter assistant upstream returned an unsuccessful status'],
  filter_assistant_response_unreadable: ['筛选助手的上游响应没有读完', 'The filter assistant upstream response could not be read'],
  cursor_oauth_poll_returned: ['Cursor 登录轮询返回了失败状态', 'The Cursor login poll returned an unsuccessful status'],
  cursor_oauth_refresh_returned: ['Cursor 登录刷新返回了失败状态', 'The Cursor login refresh returned an unsuccessful status'],
  google_account_identity_returned_http: ['读取 Google 账号身份时上游返回了失败状态', 'The upstream returned an unsuccessful status while reading the Google account identity'],
  account_identity_returned_http: ['读取账号身份时上游返回了失败状态', 'The upstream returned an unsuccessful status while reading the account identity'],
  oauth_token_endpoint_returned_http: ['换取 OAuth 令牌时上游返回了失败状态', 'The upstream returned an unsuccessful status from the OAuth token endpoint'],
  antigravity_returned_http: ['Antigravity 上游返回了失败状态', 'The Antigravity upstream returned an unsuccessful status'],
  plugin_prepared_request_exceeds_the_4_mib_abi_limit: ['插件准备的请求超过 4MiB ABI 上限', 'The plugin prepared request exceeded the 4MiB ABI limit'],
  plugin_prepared_request_body_exceeds_4_mib: ['插件准备的请求正文超过 4MiB 上限', 'The plugin prepared request body exceeded the 4MiB limit'],
  plugin_upstream_response_exceeds_its_declared_limit: ['插件上游响应超过声明的大小上限', 'The plugin upstream response exceeded its declared size limit'],
  plugin_upstream_response_exceeds_its_declared_abi_limit: ['插件上游响应超过声明的 ABI 大小上限', 'The plugin upstream response exceeded its declared ABI size limit'],
  plugin_normalized_response_exceeds_its_declared_limit: ['插件整理后的响应超过声明的大小上限', 'The plugin normalized response exceeded its declared size limit'],
  plugin_normalized_response_body_exceeds_its_declared_limit: ['插件整理后的响应正文超过声明的大小上限', 'The plugin normalized response body exceeded its declared size limit'],
  plugin_returned_an_invalid_downstream_status: ['插件返回的下游状态码无效', 'The plugin returned an invalid downstream status'],
  price_source_http_status: ['价格源返回了失败的 HTTP 状态', 'The price source returned an unsuccessful HTTP status'],
};

// Operational terminal stage recorded for a buffered request. For a failure this
// marks where the gateway finalized the outcome, not necessarily where the
// failure physically happened, so each stage needs its own copy instead of
// being shown as a raw token.
const recordedStageCopy: Record<string, [string, string]> = {
  upstream_response_headers: ['上游刚开始返回响应头', 'The upstream had just started returning response headers'],
  upstream_response_body: ['上游正在返回响应体', 'The upstream was returning the response body'],
  response_delivery: ['网关正在把响应交给客户端', 'The gateway was delivering the response to the client'],
  buffered_response_ready: [
    '请求收尾阶段',
    'Request finalization stage',
  ],
};

// Reason-only copy mirroring the backend SafeReason catalog (src/supplier_error/reason.rs):
// every stable code, alias-free, with its local fallback action so records
// without the optional action field still carry a useful next step.
const supplierReasonCopy: Record<string, { copy: [string, string]; action: string }> = {
  no_active_plan: { copy: ['当前上游账号没有可用套餐', 'The upstream account has no active plan'], action: 'check_plan' },
  insufficient_quota: { copy: ['当前上游账号额度不足', 'The upstream account has insufficient quota'], action: 'check_quota' },
  authentication_invalid: { copy: ['上游账号凭据无效', 'The upstream account credentials are invalid'], action: 'check_credentials' },
  authentication_expired: { copy: ['上游账号凭据已过期', 'The upstream account credentials have expired'], action: 'check_credentials' },
  rate_limited: { copy: ['请求触发了上游速率限制', 'The upstream request rate limit was exceeded'], action: 'retry_later' },
  model_unavailable: { copy: ['所请求的模型对该上游账号不可用', 'The requested model is unavailable to the upstream account'], action: 'check_model' },
  context_length_exceeded: { copy: ['请求超过模型的上下文窗口', 'The input exceeds the model context window'], action: 'shorten_context' },
  invalid_encrypted_content: { copy: ['上游无法验证加密上下文', 'The upstream could not validate the encrypted context'], action: 'rebuild_context' },
  invalid_prompt: { copy: ['上游判定请求内容无效', 'The upstream rejected the prompt as invalid'], action: 'revise_request' },
  misalignment_policy_violation: { copy: ['上游安全系统因潜在的非预期活动拒绝了请求', 'The upstream safety system blocked the request for potentially unintended activity'], action: 'review_request' },
  cyber_policy: { copy: ['上游安全系统因网络安全风险拒绝了请求', 'The upstream safety system blocked the request for cybersecurity risk'], action: 'review_request' },
  bio_policy: { copy: ['上游安全系统因生物安全风险拒绝了请求', 'The upstream safety system blocked the request for biological safety risk'], action: 'review_request' },
  usage_not_included: { copy: ['当前上游套餐不包含此次用量', 'The upstream plan does not include this usage'], action: 'check_plan' },
  server_is_overloaded: { copy: ['上游服务当前过载', 'The upstream service is overloaded'], action: 'retry_later' },
  invalid_value: { copy: ['请求参数的值无效', 'A request parameter has an invalid value'], action: 'revise_request' },
  invalid_type: { copy: ['请求参数的类型无效', 'A request parameter has an invalid type'], action: 'revise_request' },
  missing_required_parameter: { copy: ['请求缺少必填参数', 'A required request parameter is missing'], action: 'revise_request' },
  unsupported_parameter: { copy: ['上游不支持请求中的参数', 'The upstream does not support a request parameter'], action: 'revise_request' },
  unsupported_value: { copy: ['上游不支持请求中的参数值', 'The upstream does not support a request parameter value'], action: 'revise_request' },
  invalid_request_error: { copy: ['上游拒绝了请求格式', 'The upstream rejected the request format'], action: 'revise_request' },
};

// Generic transport wrappers whose message only restates "the upstream said
// no". A semantic supplier reason beats these; an explicit physical failure
// (timeout, reset, EOF before terminal) still wins over supplier copy.
const genericTransportWrapperCodes = new Set(['upstream_rejected', 'upstream_http_operation_failed', 'upstream_stream_read_error', 'upstream_eof_without_terminal', 'upstream_incomplete_response']);

// Coarse category recorded alongside the fixed reason. Each one is a sentence
// about the kind of problem; the suggestion carries the next step.
const supplierCategoryCopy: Record<string, [string, string]> = {
  upstream: ['上游返回了未归类的错误', 'The upstream returned an unclassified error'],
  request: ['请求内容不符合上游接口要求', 'The request did not match the upstream API'],
  policy: ['上游拒绝了这类内容', 'The upstream refused this content'],
  authentication: ['上游账号凭据无法通过验证', 'The upstream account credentials did not authenticate'],
  quota: ['上游账号额度不足或没有可用套餐', 'The upstream account lacks quota or an active plan'],
  rate_limit: ['上游服务已达到请求频率上限', 'The upstream service reached its request rate limit'],
  availability: ['上游服务或所请求的模型暂时不可用', 'The upstream service or requested model is temporarily unavailable'],
};

// Stable suggested next step. Copy is the action to take, not the token name.
const supplierActionCopy: Record<string, [string, string]> = {
  inspect_upstream_error: ['查看下方的上游错误详情；需要时带着错误码和说明联系上游提供商', 'Inspect the upstream error details below; contact the upstream provider with the code and message if needed'],
  check_plan: ['在提供商处开通套餐或更换上游账号', 'Activate a plan with the provider or switch the upstream account'],
  check_quota: ['在提供商处补充额度或更换上游账号', 'Add quota with the provider or switch the upstream account'],
  rebuild_context: ['重新开始会话或重建上下文后再试', 'Start a fresh session or rebuild the context, then try again'],
  review_request: ['检查请求内容，移除可能触发安全策略的部分后再试', 'Review the request content, remove anything that may trip safety policy, then try again'],
  shorten_context: ['缩短请求内容或上下文后再试', 'Shorten the request content or context, then try again'],
  revise_request: ['修改请求内容后再试', 'Revise the request content, then try again'],
  contact_provider: ['联系上游提供商处理该账号问题', 'Contact the upstream provider about this account'],
  retry_later: ['稍后重试；仍然失败时更换上游账号', 'Retry later; switch the upstream account if it keeps failing'],
  check_credentials: ['检查或更换该上游账号的凭据', 'Check or replace the upstream account credentials'],
  check_model: ['检查该账号的模型权限，或更换模型', 'Check the account’s model access, or use another model'],
};

/** One labeled technical fact; rendered by React, so raw upstream text is
    escaped and wraps instead of being dropped by an allowlist. */
export interface SupplierDetailEntry { label: string; value: string }

/**
 * Structured provider error presentation. Known codes map to a fixed
 * bilingual reason; an unknown code keeps the raw upstream message as a
 * labeled upstream note instead of being hidden. The next step comes from
 * the recorded action or the local mapping for the fixed code. Raw provider
 * identifiers stay as labeled details and are never interpolated into copy;
 * every optional field may be absent on older records.
 */
export function supplierErrorPresentation(supplier: SupplierError | null | undefined, locale: RequestLocale): {
  reason: string | null;
  action: string | null;
  details: SupplierDetailEntry[];
} | null {
  if (!supplier) return null;
  const zh = locale === 'zh-CN';
  const known = Object.hasOwn(supplierReasonCopy, supplier.code) ? supplierReasonCopy[supplier.code] : null;
  const rawMessage = typeof supplier.message === 'string' ? supplier.message.trim() : '';
  // Mapped copy explains the known code; an unknown code falls back to its
  // category sentence, then to the raw upstream message so nothing hides.
  const reason = known ? known.copy[zh ? 0 : 1]
    : supplier.category && Object.hasOwn(supplierCategoryCopy, supplier.category)
      ? supplierCategoryCopy[supplier.category][zh ? 0 : 1]
      : rawMessage || null;
  // Prefer the server-recorded action; older records without the field fall
  // back to the local action mapped from the fixed reason code.
  const actionToken = supplier.action && Object.hasOwn(supplierActionCopy, supplier.action) ? supplier.action : (known?.action ?? 'inspect_upstream_error');
  const action = actionToken ? supplierActionCopy[actionToken][zh ? 0 : 1] : null;
  const details: SupplierDetailEntry[] = [];
  const push = (label: string, value: unknown) => {
    if (typeof value !== 'string') return;
    const trimmed = value.trim();
    if (trimmed && !details.some((entry) => entry.value === trimmed)) details.push({ label, value: trimmed });
  };
  // Every recorded provider fact stays viewable: structured fields first,
  // then the original code/message so older records without the structured
  // fields lose nothing. Values deduplicate across labels; raw text is never
  // dropped for being unrecognized, long, or URL-bearing.
  push(zh ? '供应商错误码' : 'Provider error code', supplier.provider_code ?? supplier.code);
  push(zh ? '供应商说明' : 'Provider message', supplier.provider_message ?? rawMessage);
  if (supplier.retryable !== undefined) details.push({ label: zh ? '可重试' : 'Retryable', value: supplier.retryable ? (zh ? '是' : 'yes') : (zh ? '否' : 'no') });
  if (!reason && !action && details.length === 0) return null;
  return { reason, action, details };
}

/** Raw upstream note for an unclassified reason: shown as the upstream's own
    message, labeled as such, and omitted when the same raw text already sits
    in the technical provider message. */
export function supplierUnknownNote(supplier: SupplierError | null | undefined, locale: RequestLocale): string | null {
  if (!supplier || Object.hasOwn(supplierReasonCopy, supplier.code)) return null;
  const rawMessage = typeof supplier.message === 'string' ? supplier.message.trim() : '';
  if (!rawMessage || supplier.provider_message == null || rawMessage === supplier.provider_message.trim()) return null;
  return locale === 'zh-CN' ? `上游说明: ${rawMessage}` : `Upstream message: ${rawMessage}`;
}

/**
 * Labeled technical detail entries for a failed request: deduplicated status,
 * error and terminal-cause codes, the typed diagnostic, and all recorded
 * provider identifiers. Identifiers stay out of primary copy; anything
 * already explained by the cause sentence is omitted instead of repeated.
 * Raw recorded values are kept viewable (React escapes them at render).
 */
export function requestFailureDetails(request: RequestView, locale: RequestLocale): SupplierDetailEntry[] {
  const zh = locale === 'zh-CN';
  const details: SupplierDetailEntry[] = [];
  const push = (label: string, value: string | null | undefined) => {
    if (typeof value !== 'string') return;
    const trimmed = value.trim();
    if (trimmed && !details.some((entry) => entry.value === trimmed)) details.push({ label, value: trimmed });
  };
  push(zh ? '记录状态码' : 'Recorded status', request.status_code === null ? null : String(request.status_code));
  // An http_NNN wrapper only restates the recorded status above it.
  push(zh ? '错误码' : 'Error code', request.error_code && request.error_code !== `http_${request.status_code}` ? request.error_code : null);
  push(zh ? '终态原因码' : 'Terminal cause code', request.terminal_cause_code && request.terminal_cause_code !== request.error_code ? request.terminal_cause_code : null);
  const diagnostic = request.failure_diagnostic;
  if (diagnostic) {
    const stage = typeof diagnostic.stage === 'string' ? diagnostic.stage.trim() : '';
    const reason = typeof diagnostic.reason === 'string' ? diagnostic.reason.trim() : '';
    const parts = [stage, reason].filter(Boolean).join(' / ');
    if (parts && reason !== request.error_code) push(zh ? '失败阶段' : 'Failure stage', parts);
  }
  return details;
}

/** Provider technical entries: the full recorded detail set (structured
    fields, or the canonical legacy fields) with nothing dropped. */
export function requestProviderDetails(request: RequestView, locale: RequestLocale): SupplierDetailEntry[] {
  return supplierErrorPresentation(request.supplier_error, locale)?.details ?? [];
}

export function requestSupplierDetail(request: RequestView, locale: RequestLocale): string | null {
  if (!request.supplier_error) return null;
  const entries = requestProviderDetails(request, locale).filter((entry) => entry.label !== (locale === 'zh-CN' ? '可重试' : 'Retryable'));
  return entries.map((entry) => `${entry.label}: ${entry.value}`).join(' · ') || null;
}

function recordedCause(errorCode: string, locale: RequestLocale): string | null {
  const cause = recordedFailureCopy[errorCode];
  return cause ? cause[locale === 'zh-CN' ? 0 : 1] : null;
}

function serverErrorCode(code: string | null | undefined): code is string {
  return typeof code === 'string' && /^[a-z][a-z0-9_]{0,63}$/.test(code) && !/^http_\d+$/.test(code);
}

/** A gateway body already joined stage, status, or byte facts. Do not replace it with only the canned sentence. */
function storedGatewaySentence(request: RequestView): string | null {
  const message = request.supplier_error?.message;
  if (!message || message.length > 400 || /https?:|[\r\n]/.test(message)) return null;
  if (!/失败阶段|需要 \d+ 字节|上限 \d+ 字节|上游 HTTP 状态|错误类型 |阶段 |failure stage |requested \d+ bytes|limit \d+ bytes|upstream HTTP status |error kind |phase /.test(message)) return null;
  return message.replace(/[。.\s]+$/, '');
}

function withDiagnostic(base: string | null, request: RequestView, locale: RequestLocale): string | null {
  if (!base) return null;
  const diagnostic = request.failure_diagnostic;
  if (!diagnostic || (diagnostic.source !== 'upstream' && diagnostic.source !== 'gateway')) return base;
  let sentence = base;
  for (const token of [diagnostic.stage, diagnostic.reason]) {
    if (!serverErrorCode(token)) continue;
    const extra = recordedCause(token, locale) ?? recordedStageCopy[token]?.[locale === 'zh-CN' ? 0 : 1];
    const fact = extra ?? (locale === 'zh-CN' ? `失败阶段 ${token}` : `failure stage ${token}`);
    if (sentence.includes(fact)) continue;
    sentence = locale === 'zh-CN' ? `${sentence}。${fact}` : `${sentence}. ${fact}`;
  }
  return sentence;
}

function clientDisconnectCopy(request: RequestView, locale: RequestLocale): string {
  const zh = locale === 'zh-CN';
  const stage = request.failure_diagnostic?.stage ?? request.operational_terminal?.stage ?? null;
  const sent = Boolean(request.upstream_account_id);
  const first = typeof request.first_output_ms === 'number' && Number.isFinite(request.first_output_ms) && request.first_output_ms >= 0;
  const where = first || stage === 'upstream_response_body' || stage === 'response_delivery'
    ? (zh ? '上游已经开始返回，客户端在收完之前断开' : 'The upstream had started returning, and the client disconnected before the response finished')
    : sent || stage === 'upstream_response_headers'
      ? (zh ? '请求已经发到上游，上游还没有返回第一个字节，客户端断开' : 'The request was sent upstream. The upstream had not returned the first byte, and the client disconnected')
      : (zh ? '请求还没有发到上游，客户端断开' : 'The client disconnected before the request was sent upstream');
  const output = typeof request.output_tokens === 'number' && request.output_tokens > 0
    ? (zh ? `已记录输出 ${request.output_tokens} 词元` : `${request.output_tokens} output tokens were recorded`)
    : '';
  const waited = first
    ? (zh ? `等到第一个输出用了 ${request.first_output_ms} ms` : `First output arrived at ${request.first_output_ms} ms`)
    : '';
  return [where, waited, output].filter(Boolean).join(zh ? '。' : '. ');
}

// Localized explanations keyed by the server's typed stage/source/reason triple.
// Unknown typed evidence renders nothing; a null stage never adds a phase claim.
function diagnosticCause(request: RequestView, locale: RequestLocale): string | null {
  const diagnostic = request.failure_diagnostic;
  const zh = locale === 'zh-CN';
  if (request.terminal_cause_code === 'gateway_pre_admission'
    || (diagnostic?.source === 'gateway' && diagnostic.stage === 'pre_admission' && diagnostic.reason === 'gateway_pre_admission')) {
    const detail = request.error_code ? recordedCause(request.error_code, locale) : null;
    return zh
      ? `请求没有发到上游。${detail ?? '记录里没有更细的错误码'}。`
      : `The request was not sent upstream. ${detail ?? 'No finer error code was stored'}.`;
  }
  if (request.terminal_cause_code === 'client_disconnected' && diagnostic?.stage !== 'response_delivery') {
    return clientDisconnectCopy(request, locale);
  }
  const stored = storedGatewaySentence(request);
  if (stored) return withDiagnostic(stored, request, locale);
  if (!diagnostic) return null;
  if (diagnostic.source === 'downstream_connection' && diagnostic.reason === 'client_disconnected') {
    return clientDisconnectCopy(request, locale);
  }
  if (diagnostic.source === 'upstream_http_response' && diagnostic.reason === 'upstream_http_status') {
    if (diagnostic.stage === 'upstream_response_headers') {
      return zh ? '上游服务在响应开始时返回了错误状态' : 'The upstream service returned an error status at the start of its response';
    }
    return zh ? '上游服务返回了错误状态' : 'The upstream service returned an error status';
  }
  if (diagnostic.source === 'upstream') return recordedCause(diagnostic.reason, locale);
  return null;
}

export function requestFailureCause(request: RequestView, locale: RequestLocale): string | null {
  const diagnostic = request.failure_diagnostic;
  if (request.terminal_cause_code === 'gateway_pre_admission'
    || (diagnostic?.source === 'gateway' && diagnostic.stage === 'pre_admission' && diagnostic.reason === 'gateway_pre_admission')) {
    return diagnosticCause(request, locale);
  }
  const stored = storedGatewaySentence(request);
  if (stored) return withDiagnostic(stored, request, locale);
  const supplierReason = supplierErrorPresentation(request.supplier_error, locale)?.reason ?? null;
  // A semantic supplier reason explains a generic "the upstream said no"
  // wrapper; an explicit physical transport failure still wins.
  const wrapper = (code: string | null | undefined) => Boolean(code && genericTransportWrapperCodes.has(code));
  if (supplierReason && (wrapper(request.terminal_cause_code) || (!request.terminal_cause_code && wrapper(request.error_code)))) {
    return supplierReason;
  }
  const terminal = request.terminal_cause_code ? recordedCause(request.terminal_cause_code, locale) : null;
  const transport = terminal ?? (request.error_code ? recordedCause(request.error_code, locale) : null);
  if (transport) return withDiagnostic(transport, request, locale);
  if (!['failed', 'interrupted', 'cancelled'].includes(requestOutcome(request))) return null;
  if (supplierReason) return supplierReason;
  return diagnosticCause(request, locale)
    ?? (['client_cancelled', 'downstream_disconnected'].includes(request.error_code ?? '')
      ? clientDisconnectCopy(request, locale)
      : serverErrorCode(request.error_code) ? requestErrorCopy(request.error_code, locale) : null);
}

export function requestErrorCopy(errorCode: string, locale: RequestLocale) {
  const cause = recordedCause(errorCode, locale);
  if (cause) return cause;
  if (!serverErrorCode(errorCode)) return locale === 'zh-CN' ? '请求失败' : 'The request failed';
  const prefix = errorCode.startsWith('upstream_')
    ? (locale === 'zh-CN' ? '上游服务返回错误' : 'The upstream service returned an error')
    : errorCode.startsWith('downstream_')
      ? (locale === 'zh-CN' ? '响应交付中断' : 'Response delivery was interrupted')
      : errorCode.startsWith('client_')
        ? (locale === 'zh-CN' ? '客户端未完成请求' : 'The client did not complete the request')
        : null;
  // Unknown codes never appear in primary copy; they stay in labeled details.
  const unmapped = locale === 'zh-CN' ? '记录了未识别的错误码' : 'An unrecognized error code was recorded';
  return prefix ?? unmapped;
}

/** Recorded terminal evidence, never an inference from initial HTTP headers. */
export function requestCompletedAt(request: RequestView): number | null {
  const completedAt = request.operational_terminal?.completed_at ?? request.completed_at;
  return typeof completedAt === 'number' && Number.isFinite(completedAt) && completedAt >= request.created_at ? completedAt : null;
}

const recordedInFlightStages = ['upstream_response_headers', 'upstream_response_body', 'response_delivery'] as const;

function recordedInFlightStage(request: RequestView): typeof recordedInFlightStages[number] | null {
  const stage = request.in_flight_stage;
  return stage && (recordedInFlightStages as readonly string[]).includes(stage) ? stage : null;
}

/** Open-request label from a stage that was stored, never from a stage that was not reached. */
function openRequestOutcome(request: RequestView): RequestOutcome {
  const stage = recordedInFlightStage(request);
  if (request.error_code === 'delivery_started' || stage === 'response_delivery') return 'delivering';
  if (stage === 'upstream_response_headers') return 'waiting_upstream';
  if (stage === 'upstream_response_body') return 'reading_body';
  return 'running';
}

export function requestOutcome(request: RequestView): RequestOutcome {
  if (request.status_code === null) return openRequestOutcome(request);
  if (request.error_code === 'client_cancelled' || request.error_code === 'downstream_disconnected'
    || request.terminal_cause_code === 'client_disconnected') return 'cancelled';
  if (['upstream_incomplete_response', 'upstream_stream', 'downstream_backpressure'].includes(request.error_code ?? '')) return 'interrupted';
  if (request.error_code || request.status_code >= 400) return 'failed';
  if (request.status_code < 200) return 'unknown';
  return requestCompletedAt(request) !== null ? 'completed' : 'unknown';
}

export function requestStatusCopy(request: RequestView, locale: RequestLocale) {
  const outcome = requestOutcome(request);
  const copy = locale === 'zh-CN' ? {
    running: ['运行中', '尚未记录完成状态，用量和费用仍待结算。'],
    waiting_upstream: ['等待上游', '请求已经发到上游，还没有收到响应开头。'],
    reading_body: ['正在读取响应', '已收到上游响应开头，响应内容还在读取。'],
    delivering: ['交付中', '已开始发送响应，交付完成状态待记录。'],
    completed: ['已完成', '服务端已记录正常结束。'],
    cancelled: ['客户端断开', '客户端连接已断开，响应没有交付完。'],
    interrupted: ['响应中断', '响应没有正常结束。已记录的用量是当时采到的部分。'],
    failed: ['失败', ''],
    unknown: ['记录待补充', '此历史记录缺少完成时间，无法确认完整交付。'],
  } : {
    running: ['Running', 'No terminal outcome is recorded yet. Usage and cost await settlement.'],
    waiting_upstream: ['Waiting on upstream', 'The request was sent upstream. The response has not started.'],
    reading_body: ['Reading the response', 'The upstream response has started and its body is still being read.'],
    delivering: ['Delivering', 'Response delivery has started, but its terminal outcome is not recorded yet.'],
    completed: ['Completed', 'The server recorded a normal finish.'],
    cancelled: ['Client disconnected', 'The client connection closed before the response finished.'],
    interrupted: ['Interrupted', 'The response ended early. Recorded usage is the portion captured at that time.'],
    failed: ['Failed', ''],
    unknown: ['Record incomplete', 'Completion time is missing; complete delivery is not confirmed.'],
  };
  const [label, explanation] = copy[outcome];
  const code = request.status_code === null ? '' : `${locale === 'zh-CN' ? '记录状态码' : 'Recorded status'}: ${request.status_code}`;
  const hasFailure = ['failed', 'interrupted', 'cancelled'].includes(outcome);
  const cause = hasFailure ? requestFailureCause(request, locale) : null;
  const unexplained = request.error_code && /^http_\d+$/.test(request.error_code)
    ? (locale === 'zh-CN' ? '只记下了 HTTP 状态，没有记下失败发生在哪一段' : 'Only the HTTP status was stored, not the phase where it failed')
    : (locale === 'zh-CN' ? '这条记录没有写下原因' : 'This record has no stored cause');
  const failureDetail = hasFailure
    ? `${locale === 'zh-CN' ? '已记录原因' : 'Recorded cause'}: ${cause ?? unexplained}` : '';
  const supplier = hasFailure ? supplierErrorPresentation(request.supplier_error, locale) : null;
  // Reason copy never embeds the next step; the action is always a separate
  // line so a failure shows both what happened and what to do next.
  const nextAction = supplier?.action ?? null;
  // An unknown supplier code keeps the raw upstream message visible as a
  // labeled note; the unmapped code itself stays in technical details.
  const upstreamNote = hasFailure ? supplierUnknownNote(request.supplier_error, locale) : null;
  // One deduplicated technical block: recorded identifiers first, then every
  // provider detail; nothing is dropped for being unrecognized.
  const details = hasFailure ? (() => {
    // One value renders once across labels; provider entries keep precedence
    // so a raw upstream fact is never lost to a gateway-side duplicate.
    const merged = [...requestFailureDetails(request, locale), ...requestProviderDetails(request, locale)];
    const seen = new Set<string>();
    return merged.filter((entry) => {
      const key = entry.value;
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
  })() : [];
  const supplierDetail = requestSupplierDetail(request, locale);
  const zh = locale === 'zh-CN';
  const labels = {
    nextStep: zh ? '下一步' : 'Next step',
    technicalDetails: zh ? '技术细节' : 'Technical details',
  };
  return { outcome, label, tone: outcome === 'completed' ? 'ok' : ['cancelled', 'interrupted', 'failed'].includes(outcome) ? 'bad' : outcome === 'unknown' ? 'unknown' : 'pending',
    cause: failureDetail,
    supplierDetail,
    nextAction,
    upstreamNote,
    details,
    labels,
    // Concise trigger: the outcome plus the recorded status. Reason, next
    // step and identifiers arrive through the tooltip's description.
    triggerLabel: `${label}${request.status_code === null ? '' : ` ${request.status_code}`}`,
    hint: hasFailure
      ? [failureDetail, upstreamNote, nextAction, supplierDetail].filter(Boolean).join(' · ')
      : [explanation, code].filter(Boolean).join(' · ') };
}

export type RequestEgressLineKind = 'proxy' | 'direct';

export interface RequestEgressLine {
  kind: RequestEgressLineKind;
  text: string;
  hover: string;
}

function egressObservedTime(observation: RequestEgressObservation, locale: RequestLocale): string | null {
  const at = observation.observed_at;
  return typeof at === 'number' && Number.isFinite(at) ? new Date(at).toLocaleString(locale) : null;
}

/**
 * The network exit recorded for this request: the attempt/selection snapshot
 * when the list source carries one, never the current proxy configuration and
 * never the upstream account. Anything without a real recorded exit — a
 * pre-admission terminal record, a missing or unknown snapshot, or a proxy
 * snapshot with neither a name nor a usable address — returns null so the
 * list omits the line instead of guessing or showing a placeholder.
 */
export function requestEgressLine(request: RequestView, locale: RequestLocale): RequestEgressLine | null {
  const zh = locale === 'zh-CN';
  const copy = requestEgressCopy(locale);
  if (request.terminal_cause_code === 'gateway_pre_admission') {
    return null;
  }
  const observation = request.egress;
  const selected = observation?.selected;
  if (!observation || !selected || selected.kind === 'unknown') {
    return null;
  }
  const recorded = `${zh ? '记录时间' : 'Recorded'}: ${egressObservedTime(observation, locale) ?? copy.unknown}`;
  // The probe cadence lives in the gateway and can change, so the copy names
  // the mechanism rather than hardcoding an interval.
  const connectivity = zh
    ? '连通性：代理出口到上游主机的隧道，按代理组周期探测判断。'
    : 'Connectivity: the tunnel from the proxy exit to the upstream host, judged by the proxy group’s periodic probe.';
  if (selected.kind === 'direct') {
    return {
      kind: 'direct',
      text: copy.direct,
      hover: [
        zh ? '这次请求走直连。' : 'This request used a direct connection.',
        recorded,
        zh ? '连通性：网关到上游主机的直连通道。' : 'Connectivity: the gateway’s direct channel to the upstream host.',
      ].join(' · '),
    };
  }
  const address = egressAddress(selected, locale);
  const member = selected.member_name?.trim();
  const group = selected.group_name?.trim();
  const name = member || group || '';
  const summary = name || (address !== copy.unknown ? address : '');
  if (!summary) return null;
  const groupDisplay = group || (selected.group_id ? copy.unnamedGroup : copy.ungrouped);
  const version = typeof selected.group_version === 'number' && Number.isFinite(selected.group_version)
    ? copy.version(selected.group_version) : '';
  return {
    kind: 'proxy',
    text: summary,
    hover: [
      zh ? '这次请求走代理出口。' : 'This request used a proxy exit.',
      `${zh ? '代理组' : 'Proxy group'}: ${[groupDisplay, version].filter(Boolean).join(' · ')}`,
      `${zh ? '出口' : 'Exit'}: ${member || copy.unnamedMember}`,
      `${copy.address}: ${address}`,
      recorded,
      connectivity,
    ].join(' · '),
  };
}
