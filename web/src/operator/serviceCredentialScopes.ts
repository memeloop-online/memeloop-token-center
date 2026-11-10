export interface ServiceCredentialScopeCopy {
  value: string;
  label: string;
  labelEn: string;
  hint: string;
  hintEn: string;
}

export const serviceCredentialStatisticsGroupId = 'statistics-and-requests';

export const serviceCredentialScopeGroups = [
  { id: serviceCredentialStatisticsGroupId, title: '统计与请求', titleEn: 'Statistics and requests', scopes: [
    { value: 'metrics:read', label: '读取诊断指标', labelEn: 'Read diagnostic metrics', hint: '诊断指标与健康状态。', hintEn: 'Diagnostic metrics and health status.' },
    { value: 'requests:read', label: '读取请求与用量', labelEn: 'Read requests and usage', hint: '请求记录、监控快照与用量分析。', hintEn: 'Request records, monitoring snapshots, and usage analysis.' },
  ] },
  { id: 'credentials-and-credits', title: '凭据与额度', titleEn: 'Credentials and credits', scopes: [
    { value: 'keys:read', label: '读取客户端凭据', labelEn: 'Read client credentials', hint: '查看客户端凭据及其配置。', hintEn: 'View client credentials and their configuration.' },
    { value: 'keys:write', label: '管理客户端凭据', labelEn: 'Manage client credentials', hint: '创建、更新或撤销客户端凭据。', hintEn: 'Create, update, or revoke client credentials.' },
    { value: 'credits:read', label: '读取额度', labelEn: 'Read credits', hint: '查看额度账户与余额。', hintEn: 'View credit accounts and balances.' },
    { value: 'credits:write', label: '管理额度', labelEn: 'Manage credits', hint: '修改额度与账户余额。', hintEn: 'Modify credits and account balances.' },
    { value: 'entitlements:read', label: '读取授权权益', labelEn: 'Read entitlements', hint: '查看租户授权权益。', hintEn: 'View tenant entitlements.' },
    { value: 'entitlements:write', label: '管理授权权益', labelEn: 'Manage entitlements', hint: '修改租户授权权益。', hintEn: 'Modify tenant entitlements.' },
  ] },
  { id: 'platform-configuration', title: '平台配置', titleEn: 'Platform configuration', scopes: [
    { value: 'providers:read', label: '读取提供商', labelEn: 'Read providers', hint: '查看提供商配置。', hintEn: 'View provider configuration.' },
    { value: 'providers:write', label: '管理提供商', labelEn: 'Manage providers', hint: '创建或修改提供商配置。', hintEn: 'Create or modify provider configuration.' },
    { value: 'plugins:read', label: '读取插件', labelEn: 'Read plugins', hint: '查看插件与其配置。', hintEn: 'View plugins and their configuration.' },
    { value: 'plugins:write', label: '管理插件', labelEn: 'Manage plugins', hint: '修改插件配置或生命周期。', hintEn: 'Modify plugin configuration or lifecycle.' },
    { value: 'routes:read', label: '读取模型路由', labelEn: 'Read model routes', hint: '查看模型路由配置。', hintEn: 'View model route configuration.' },
    { value: 'routes:write', label: '管理模型路由', labelEn: 'Manage model routes', hint: '创建或修改模型路由。', hintEn: 'Create or modify model routes.' },
    { value: 'prices:read', label: '读取价格', labelEn: 'Read prices', hint: '查看模型价格配置。', hintEn: 'View model pricing configuration.' },
    { value: 'prices:write', label: '管理价格', labelEn: 'Manage prices', hint: '修改模型价格配置。', hintEn: 'Modify model pricing configuration.' },
    { value: 'schemas:read', label: '读取配置 Schema', labelEn: 'Read configuration schemas', hint: '读取平台公开的配置 Schema。', hintEn: 'Read configuration schemas exposed by the platform.' },
    { value: 'upstreams:import:write', label: '导入上游账号', labelEn: 'Import upstream accounts', hint: '导入上游账号配置。', hintEn: 'Import upstream account configuration.' },
  ] },
  { id: 'system-and-operations', title: '系统与运维', titleEn: 'System and operations', scopes: [
    { value: 'generations:write', label: '创建生成任务', labelEn: 'Create generation jobs', hint: '提交图片或视频生成任务。', hintEn: 'Submit image or video generation jobs.' },
    { value: 'generations:quarantine:read', label: '读取隔离任务', labelEn: 'Read quarantined jobs', hint: '查看被隔离的生成任务。', hintEn: 'View quarantined generation jobs.' },
    { value: 'generations:reconcile', label: '对账生成任务', labelEn: 'Reconcile generation jobs', hint: '执行生成任务状态对账。', hintEn: 'Reconcile generation job status.' },
    { value: 'filter_assistant:execute', label: '运行筛选助手', labelEn: 'Run filter assistant', hint: '调用模型执行筛选，可能产生计费用量。', hintEn: 'Invoke models for filtering; this may incur billable usage.' },
    { value: 'oauth:write', label: '管理 OAuth 授权', labelEn: 'Manage OAuth authorization', hint: '发起或管理 OAuth 授权流程。', hintEn: 'Start or manage OAuth authorization flows.' },
    { value: 'service_tokens:read', label: '读取服务凭据', labelEn: 'Read service credentials', hint: '列出服务凭据及其权限范围。', hintEn: 'List service credentials and their scopes.' },
    { value: 'service_tokens:write', label: '管理服务凭据', labelEn: 'Manage service credentials', hint: '创建、编辑、复制、轮换或暂停服务凭据。', hintEn: 'Create, edit, copy, rotate, or suspend service credentials.' },
    { value: 'tenants:read', label: '读取租户', labelEn: 'Read tenants', hint: '查看租户信息。', hintEn: 'View tenant information.' },
    { value: 'tenants:write', label: '管理租户', labelEn: 'Manage tenants', hint: '创建或修改租户。', hintEn: 'Create or modify tenants.' },
    { value: 'settlements:adjust', label: '调整结算记录', labelEn: 'Adjust settlements', hint: '执行结算记录调整。', hintEn: 'Adjust settlement records.' },
  ] },
] as const;

export const serviceCredentialStatisticsScopes = ['metrics:read', 'requests:read'];

export const supportedServiceCredentialScopes = new Set<string>(serviceCredentialScopeGroups.flatMap((group) => group.scopes.map((scope) => scope.value)));

export function serviceCredentialCreatePayload(name: string, scopes: string[], tenant: string) {
  const normalizedName = name.trim();
  const uniqueScopes = [...new Set(scopes)];
  if (!tenant || !normalizedName || new TextEncoder().encode(normalizedName).length > 120 || uniqueScopes.length === 0 || uniqueScopes.some((scope) => !supportedServiceCredentialScopes.has(scope))) return undefined;
  return { name: normalizedName, scopes: uniqueScopes, tenant_external_id: tenant };
}

export interface ServiceCredentialUpdateRequest {
  name?: string;
  scopes?: string[];
  tenant_external_id?: string | null;
}

export type ServiceCredentialUpdatePlan =
  | { status: 'ready'; payload: ServiceCredentialUpdateRequest }
  | { status: 'unchanged' }
  | { status: 'invalid' };

/**
 * The edit panel shares the create form's fields but saves through PATCH:
 * only sections that actually changed are sent, and an omitted section is
 * preserved by the server. The draft tenant '' is the explicit global
 * option; it is never inferred from a missing or unloaded tenant list.
 */
export function serviceCredentialUpdatePlan(
  current: { name: string; scopes: string[]; tenant_external_id: string | null },
  draft: { name: string; scopes: string[]; tenant: string },
): ServiceCredentialUpdatePlan {
  const payload: ServiceCredentialUpdateRequest = {};
  const normalizedName = draft.name.trim();
  if (normalizedName !== current.name) {
    if (!normalizedName || new TextEncoder().encode(normalizedName).length > 120) return { status: 'invalid' };
    payload.name = normalizedName;
  }
  const uniqueScopes = [...new Set(draft.scopes)];
  if (uniqueScopes.some((scope) => !supportedServiceCredentialScopes.has(scope))) return { status: 'invalid' };
  const currentScopes = new Set(current.scopes);
  const scopesChanged = uniqueScopes.length !== currentScopes.size || uniqueScopes.some((scope) => !currentScopes.has(scope));
  if (scopesChanged) {
    if (uniqueScopes.length === 0) return { status: 'invalid' };
    payload.scopes = uniqueScopes;
  }
  if (draft.tenant !== (current.tenant_external_id ?? '')) {
    if (draft.tenant && new TextEncoder().encode(draft.tenant).length > 200) return { status: 'invalid' };
    payload.tenant_external_id = draft.tenant || null;
  }
  return Object.keys(payload).length === 0 ? { status: 'unchanged' } : { status: 'ready', payload };
}

const scopeCopy = new Map<string, ServiceCredentialScopeCopy>(serviceCredentialScopeGroups.flatMap((group) => group.scopes.map((scope) => [scope.value, scope] as const)));

/** Ordinary labels identify permissions by name; the technical scope value stays available in labeled details. */
export function serviceCredentialScopeLabel(value: string, zh: boolean): string {
  const copy = scopeCopy.get(value);
  return copy ? (zh ? copy.label : copy.labelEn) : value;
}

export function serviceCredentialScopeSummary(scopes: string[], zh: boolean): string {
  return scopes.map((scope) => serviceCredentialScopeLabel(scope, zh)).join(' · ');
}
