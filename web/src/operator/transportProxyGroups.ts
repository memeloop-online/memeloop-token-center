import { ApiError, api } from '../api';

export interface TransportProxyMember {
  id: string;
  label: string;
  scheme: string;
  remote_dns: boolean;
  has_auth: boolean;
}

export interface TransportProxyGroup {
  id: string;
  tenant_external_id: string;
  name: string;
  version: number;
  members: TransportProxyMember[];
  bound_account_count: number;
}

export interface TransportProxyBinding {
  account_id: string;
  tenant_external_id: string;
  binding_version: number;
  group_id: string | null;
  group_version: number | null;
  initial_member_id: string | null;
  credential_generation: number;
  updated_at: number;
  runtime: {
    scope: 'this_process';
    configuration_state: 'unbound' | 'pending' | 'applied' | 'unavailable';
    selected_member_id: string | null;
    observed_at: number;
  };
}

export const transportProxyGroupsPath = '/internal/v1/transport-proxy-groups';
export const transportProxyRequestPolicy = { cache: 'no-store', credentials: 'omit', referrerPolicy: 'no-referrer' } as const;
export const transportProxyWaitMilliseconds = 15_000;

export function transportProxyFailureKind(reason: unknown): 'validation' | 'conflict' | 'denied' | 'unknown' {
  if (!(reason instanceof ApiError)) return 'unknown';
  if (reason.status === 400 || reason.status === 422) return 'validation';
  if (reason.status === 401 || reason.status === 403) return 'denied';
  if (reason.status === 404 || reason.status === 409) return 'conflict';
  return 'unknown';
}

export async function transportProxyRequest<Result>(path: string, token: string, init: RequestInit = {}): Promise<Result> {
  const controller = new AbortController();
  let timer: ReturnType<typeof setTimeout> | undefined;
  let stopWaiting: (() => void) | undefined;
  const interrupted = new Promise<never>((_, reject) => {
    stopWaiting = () => {
      controller.abort();
      reject(new Error('transport proxy request result unknown'));
    };
    timer = setTimeout(stopWaiting, transportProxyWaitMilliseconds);
    init.signal?.addEventListener('abort', stopWaiting, { once: true });
    if (init.signal?.aborted) stopWaiting();
  });
  try {
    return await Promise.race([
      api<Result>(path, token, { ...transportProxyRequestPolicy, ...init, signal: controller.signal }),
      interrupted,
    ]);
  } finally {
    clearTimeout(timer);
    if (stopWaiting) init.signal?.removeEventListener('abort', stopWaiting);
  }
}

export function transportProxyError(reason: unknown): string {
  if (transportProxyFailureKind(reason) === 'unknown') return '等待已结束，写入结果未知；这不表示服务端已取消。请先刷新读回配置，核对后再决定是否重试，系统不会自动重放写入。';
  if (!(reason instanceof ApiError)) return '请求未完成，请刷新配置。';
  switch (reason.code) {
    case 'proxy_group_version_conflict': return '代理组已被其他操作更新。请刷新配置并重新确认，未覆盖他人的修改。';
    case 'proxy_group_binding_conflict': return '账号绑定或凭证版本已变化。请刷新配置并重新选择，未覆盖他人的修改。';
    case 'proxy_group_in_use': return '代理组仍有账号绑定，不能整体删除；请先为这些账号更换组或解绑。';
    case 'proxy_group_capacity_exceeded': return '代理组或绑定数量已达到配置容量上限，请联系管理员整理配置。';
    case 'invalid_request': return '配置无效。请检查名称、出口数量、私网 socks5h 地址和替代出口；新出口作为替代项前需先保存。';
    case 'service_overloaded': return '服务暂时无法写入，尚未确认保存成功。请稍后刷新再操作。';
  }
  if (reason.status === 401 || reason.status === 403) return '需要具有 providers:write 权限的全局操作员，并且能够管理当前租户。';
  if (reason.status === 400 || reason.status === 422) return '配置校验失败，草稿已保留。请修正输入后重新保存。';
  if (reason.status === 404) return '当前租户下的账号或代理组已不存在，请刷新配置。';
  if (reason.status === 409) return '配置发生冲突，请刷新并重新确认后提交。';
  return '请求未完成，结果尚未确认。请刷新配置后再操作。';
}
