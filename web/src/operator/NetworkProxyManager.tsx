import { useEffect, useLayoutEffect, useRef, useState, type RefObject } from 'react';
import { Button, Checkbox, Disclosure, Field, FormSection, Input, Label, LoadingProgress, LoadingState, Select, Textarea } from '../design-system';
import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { useConfirmDialog } from '../useConfirmDialog';
import { formatPercent } from '../format';
import { egressAddress, egressBytes } from './requestEgress';
import { ProxyAddressInput } from './TransportProxyGroupManager';
import {
  networkProxyAddressValid, networkProxyError, networkProxyLiveConcurrency, networkProxyObservationsPath,
  networkProxiesPath, networkProxySelectionPath, networkProxyStrategiesPath, upstreamConcurrencyPath,
  type NetworkProxy, type NetworkProxyObservations, type NetworkProxySelection, type NetworkProxyStrategy,
  type UpstreamConcurrencySnapshot,
} from './NetworkProxies';
import { networkProxyCopy } from './NetworkProxyCopy';
import { transportProxyFailureKind, transportProxyGroupsPath, transportProxyRequest, type TransportProxyGroup } from './transportProxyGroups';
import './requestEgress.css';

interface WorkspaceProps {
  token: string;
  tenant: string;
  onChanged: () => Promise<void>;
  closeRequest: RefObject<(() => Promise<boolean>) | null>;
  initialAccountId?: string;
}

interface DetailsState {
  open: boolean;
  loading: boolean;
  error: string;
  observations?: NetworkProxyObservations;
}

const detailsWindowMs = 24 * 60 * 60 * 1000;

export function NetworkProxyWorkspace({ token, tenant, onChanged, closeRequest }: WorkspaceProps) {
  const { locale, t } = useI18n();
  const copy = networkProxyCopy(locale);
  const inputPrefix = useRef(`proxy-${Math.random().toString(36).slice(2)}`).current;
  const [failureKind, setFailureKind] = useState<ReturnType<typeof transportProxyFailureKind>>();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, failureKind === 'denied']);
  const [proxies, setProxies] = useState<NetworkProxy[]>([]);
  const [snapshot, setSnapshot] = useState<UpstreamConcurrencySnapshot>();
  const [concurrencyFailed, setConcurrencyFailed] = useState(false);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [editing, setEditing] = useState<NetworkProxy | null>();
  const [name, setName] = useState('');
  const [address, setAddress] = useState('');
  const [originalAddress, setOriginalAddress] = useState('');
  const [probeEnabled, setProbeEnabled] = useState(true);
  const [probeTarget, setProbeTarget] = useState('https://openai.com/');
  const [probeInterval, setProbeInterval] = useState('60000');
  const [probeAttempts, setProbeAttempts] = useState('2');
  const [details, setDetails] = useState<Record<string, DetailsState>>({});
  const alive = useRef(false);
  const locked = useRef(false);
  const pendingWrite = useRef<AbortController | null>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const detailsRead = useRef(0);
  const query = `?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const intervalValue = Number(probeInterval);
  const attemptsValue = Number(probeAttempts);
  const validProbe = /^https?:\/\//.test(probeTarget.trim()) && probeTarget.trim().length <= 2048
    && Number.isInteger(intervalValue) && intervalValue >= 10_000 && intervalValue <= 3_600_000
    && Number.isInteger(attemptsValue) && attemptsValue >= 1 && attemptsValue <= 8;
  const validForm = name.trim().length >= 1 && name.trim().length <= 64 && networkProxyAddressValid(address) && validProbe;
  const request = <Result,>(path: string, init: RequestInit = {}) => transportProxyRequest<Result>(path, token, {
    ...init, signal: pendingWrite.current?.signal,
  });

  async function requestClose() {
    if (pendingWrite.current) {
      if (!await confirm(copy.closePending)) return false;
      pendingWrite.current?.abort();
    } else if (editing !== undefined && !await confirm(copy.closeDraft)) return false;
    return true;
  }

  useLayoutEffect(() => {
    closeRequest.current = requestClose;
    return () => { closeRequest.current = null; };
  });

  useLayoutEffect(() => {
    if (busy || error || message) feedback.current?.focus({ preventScroll: true });
  }, [busy, error, message]);

  useEffect(() => {
    alive.current = true;
    void refresh();
    return () => { alive.current = false; detailsRead.current += 1; pendingWrite.current?.abort(); };
  }, []);

  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (editing === undefined && !pendingWrite.current) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => window.removeEventListener('beforeunload', beforeUnload);
  }, [editing]);

  function clearForm() {
    setEditing(undefined); setName(''); setAddress(''); setOriginalAddress('');
    setProbeEnabled(true); setProbeTarget('https://openai.com/'); setProbeInterval('60000'); setProbeAttempts('2');
  }

  function clearRestrictedState() {
    setProxies([]); setSnapshot(undefined); setDetails({}); clearForm();
  }

  async function refresh() {
    if (locked.current) return;
    locked.current = true;
    detailsRead.current += 1;
    setBusy(true); setReady(false); setError(''); clearForm(); setDetails({});
    try {
      const [list, concurrency] = await Promise.all([
        request<{ items: NetworkProxy[] }>(`${networkProxiesPath}${query}`),
        request<UpstreamConcurrencySnapshot>(`${upstreamConcurrencyPath}${query}`).catch(() => undefined),
      ]);
      if (!alive.current) return;
      setProxies(list.items); setSnapshot(concurrency); setConcurrencyFailed(concurrency === undefined);
      setReady(true);
      if (failureKind === 'unknown') setMessage(copy.refreshedUnknown);
      setFailureKind(undefined);
    } catch (reason) {
      if (alive.current) {
        setError(networkProxyError(reason, locale, 'read'));
        if (transportProxyFailureKind(reason) === 'denied') { clearRestrictedState(); setFailureKind('denied'); }
      }
    } finally { locked.current = false; if (alive.current) setBusy(false); }
  }

  async function mutate(action: () => Promise<void>, success: string) {
    if (locked.current || !ready) return;
    locked.current = true; detailsRead.current += 1;
    pendingWrite.current = new AbortController();
    setBusy(true); setError(''); setMessage(''); setFailureKind(undefined);
    try {
      await action();
      if (!alive.current) return;
      setMessage(success);
      void onChanged().catch(() => { if (alive.current) setError(copy.accountRefreshFailed); });
    } catch (reason) {
      if (alive.current) {
        const kind = transportProxyFailureKind(reason);
        setError(networkProxyError(reason, locale)); setFailureKind(kind);
        setReady(kind === 'validation');
        if (kind === 'denied') clearRestrictedState();
      }
    } finally { pendingWrite.current = null; locked.current = false; if (alive.current) setBusy(false); }
  }

  function editProxy(proxy: NetworkProxy | null) {
    setEditing(proxy); setError(''); setMessage('');
    setName(proxy?.name ?? '');
    const displayAddress = proxy ? egressAddress(proxy.address, locale) : '';
    setAddress(displayAddress); setOriginalAddress(displayAddress);
    setProbeEnabled(proxy?.probe.enabled ?? true);
    setProbeTarget(proxy?.probe.target_url ?? 'https://openai.com/');
    setProbeInterval(String(proxy?.probe.interval_ms ?? 60_000));
    setProbeAttempts(String(proxy?.probe.attempts ?? 2));
  }

  async function saveProxy() {
    if (editing === undefined || !validForm) return;
    const probe = {
      enabled: probeEnabled, target_url: probeTarget.trim(),
      interval_ms: intervalValue, attempts: attemptsValue,
    };
    const changedAddress = address.trim();
    const body = {
      tenant_external_id: tenant, name: name.trim(), probe,
      ...(editing ? { expected_version: editing.version } : { proxy_url: changedAddress }),
      ...(editing && changedAddress !== originalAddress ? { proxy_url: changedAddress } : {}),
    };
    await mutate(async () => {
      const saved = await request<NetworkProxy>(editing ? `${networkProxiesPath}/${encodeURIComponent(editing.id)}` : networkProxiesPath, {
        method: editing ? 'PUT' : 'POST', body: JSON.stringify(body),
      });
      if (!alive.current) return;
      setProxies(current => [...current.filter(proxy => proxy.id !== saved.id), saved]);
      setDetails(current => { const next = { ...current }; delete next[saved.id]; return next; });
      clearForm();
    }, copy.saved);
  }

  async function removeProxy(proxy: NetworkProxy) {
    if (!await confirm(copy.confirmDelete(proxy.name))) return;
    await mutate(async () => {
      await request(`${networkProxiesPath}/${encodeURIComponent(proxy.id)}`, {
        method: 'DELETE', body: JSON.stringify({ tenant_external_id: tenant, expected_version: proxy.version }),
      });
      if (!alive.current) return;
      setProxies(current => current.filter(value => value.id !== proxy.id));
      setDetails(current => { const next = { ...current }; delete next[proxy.id]; return next; });
      if (editing?.id === proxy.id) clearForm();
    }, copy.deleted);
  }

  async function probeNow(proxy: NetworkProxy) {
    await mutate(async () => {
      await request(`${networkProxiesPath}/${encodeURIComponent(proxy.id)}/probe`, {
        method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, expected_version: proxy.version }),
      });
    }, copy.probeQueued);
  }

  async function loadDetails(proxy: NetworkProxy) {
    const revision = ++detailsRead.current;
    setDetails(current => ({ ...current, [proxy.id]: { open: true, loading: true, error: '', observations: current[proxy.id]?.observations } }));
    try {
      const end = Date.now();
      const observations = await request<NetworkProxyObservations>(networkProxyObservationsPath(proxy.id, tenant, end - detailsWindowMs, end));
      if (alive.current && revision === detailsRead.current) {
        setDetails(current => current[proxy.id]?.open ? { ...current, [proxy.id]: { open: true, loading: false, error: '', observations } } : current);
      }
    } catch (reason) {
      if (alive.current && revision === detailsRead.current) {
        setDetails(current => current[proxy.id]?.open ? { ...current, [proxy.id]: { open: true, loading: false, error: networkProxyError(reason, locale, 'read') } } : current);
      }
    }
  }

  function toggleDetails(proxy: NetworkProxy, open: boolean) {
    setDetails(current => ({ ...current, [proxy.id]: { open, loading: false, error: '', observations: current[proxy.id]?.observations } }));
    if (open) void loadDetails(proxy);
  }

  return <div className="transport-proxy-workspace network-proxy-workspace">
    {confirmationDialog}
    <p>{copy.purpose}</p>
    <p className="muted">{copy.hint}</p>
    <div className="button-row">
      <Button type="button" disabled={busy} onClick={async () => {
        if (editing !== undefined && !await confirm(copy.refreshDraft)) return;
        setMessage(''); void refresh();
      }}>{busy ? t('common.loading') : copy.refresh}</Button>
    </div>
    <div ref={feedback} tabIndex={-1} aria-label={copy.operationStatus}>
      <LoadingProgress active={busy} label={t('common.loading')} level={ready ? 'section' : 'page'} />
      {error && <p className="notice error" role="alert">{error}</p>}
      {message && <p className="notice success" role="status">{message}</p>}
    </div>
    <fieldset disabled={!ready || busy} style={{ border: 0, padding: 0, minWidth: 0 }}>
      <FormSection title={copy.title}>
        <div className="proxy-group-list">
          {!ready && busy && proxies.length === 0 && <LoadingState label={t('common.loading')} variant="compact" />}
          {ready && proxies.length === 0 && <p>{copy.empty}</p>}
          {proxies.map(proxy => {
            const detail = details[proxy.id];
            const references = proxy.group_ids === undefined || proxy.bound_account_count === undefined ? undefined
              : proxy.group_ids.length + proxy.bound_account_count;
            const health = copy.health[proxy.active_health.status] ?? proxy.active_health.status;
            return <div className="network-proxy-item" key={proxy.id}>
              <div className="button-row">
                <span>{copy.summary(proxy.name, proxy.group_ids?.length, proxy.bound_account_count)}</span>
                <span className="muted">{egressAddress(proxy.address, locale)}</span>
                <span>{copy.healthTitle}: {health}{proxy.active_health.latency_ms !== null ? ` · ${proxy.active_health.latency_ms}ms` : ''}</span>
                <Button type="button" disabled={editing !== undefined} onClick={() => editProxy(proxy)}>{copy.edit(proxy.name)}</Button>
                <Button type="button" disabled={editing !== undefined || (references ?? 0) > 0} aria-describedby={(references ?? 0) > 0 ? `${inputPrefix}-in-use-${proxy.id}` : undefined}
                  onClick={() => void removeProxy(proxy)}>{t('common.remove')}</Button>
                <Button type="button" disabled={editing !== undefined} onClick={() => void probeNow(proxy)}>{copy.probeNow}</Button>
              </div>
              {(references ?? 0) > 0 && <p id={`${inputPrefix}-in-use-${proxy.id}`} className="muted">{copy.inUseHint}</p>}
              <Disclosure title={copy.details} open={detail?.open ?? false} onOpenChange={open => toggleDetails(proxy, open)}>
                {detail?.loading && <LoadingState label={t('common.loading')} variant="compact" />}
                {detail?.error && <p className="notice error" role="alert">{detail.error}</p>}
                <p>{copy.healthTitle}: {health}{proxy.active_health.checked_at ? ` · ${copy.checkedAt}: ${new Date(proxy.active_health.checked_at).toLocaleString(locale)}` : ''}{proxy.active_health.latency_ms !== null ? ` · ${copy.latency}: ${proxy.active_health.latency_ms}ms` : ''}</p>
                <p>{copy.concurrency}: {concurrencyFailed || !snapshot ? copy.concurrencyUnknown
                  : networkProxyLiveConcurrency(snapshot, proxy.address) ?? copy.concurrencyUnknown}</p>
                <p className="muted">{copy.concurrencyHint}</p>
                {detail?.observations && <>
                  <p>{copy.trafficTitle} · {copy.window(new Date(detail.observations.window_start).toLocaleString(locale), new Date(detail.observations.window_end).toLocaleString(locale))}</p>
                  {detail.observations.stale && <p className="muted">{copy.healthStale}</p>}
                  {(detail.observations.traffic.recorded_attempts ?? 0) === 0 ? <p>{copy.noTraffic}</p> : <>
                    <ul className="network-proxy-outcomes">
                      {(['succeeded', 'provider_failed', 'transport_failed', 'pending', 'cancelled', 'unknown'] as const)
                        .filter(key => detail.observations!.request_outcomes[key] > 0)
                        .map(key => <li key={key}>{copy.outcomes[key] ?? key}: {detail.observations!.request_outcomes[key]}</li>)}
                    </ul>
                    <p>{copy.requestSuccessRate}: {formatPercent(detail.observations.request_outcomes.request_success_rate, locale)} · {copy.transportSuccessRate}: {formatPercent(detail.observations.request_outcomes.transport_success_rate, locale)}</p>
                    <p>{copy.recordedAttempts}: {detail.observations.traffic.recorded_attempts ?? 0} · {copy.requestBytes}: {egressBytes(detail.observations.traffic.request_payload_bytes, locale)} · {copy.responseBytes}: {egressBytes(detail.observations.traffic.response_payload_bytes, locale)}</p>
                  </>}
                  <p className="muted">{copy.trafficBasis}</p>
                  <Button type="button" disabled={detail.loading} onClick={() => void loadDetails(proxy)}>{copy.refreshDetails}</Button>
                </>}
              </Disclosure>
            </div>;
          })}
        </div>
        <Button type="button" disabled={editing !== undefined} onClick={() => editProxy(null)}>{copy.create}</Button>
      </FormSection>
      {editing !== undefined && <form onSubmit={event => { event.preventDefault(); void saveProxy(); }}>
        <FormSection title={editing ? copy.edit(editing.name) : copy.create}>
          <Field label={copy.name} required><Input required maxLength={64} value={name} onChange={(_, data) => setName(data.value)} /></Field>
          <div className="proxy-address-field">
            <Label htmlFor={`${inputPrefix}-address`} required>{editing ? copy.address : copy.requiredAddress}</Label>
            <ProxyAddressInput id={`${inputPrefix}-address`} value={address} disabled={busy || !ready} required
              onChange={setAddress} placeholder="socks5h://100.64.0.16:1080" />
          </div>
          <p>{copy.addressHint}</p>
          {editing?.address.has_auth && <p>{copy.authHint}</p>}
          <Checkbox label={copy.probeEnabled} checked={probeEnabled} onChange={(_, data) => setProbeEnabled(data.checked === true)} />
          <Field label={copy.probeTarget} required><Input required maxLength={2048} value={probeTarget} onChange={(_, data) => setProbeTarget(data.value)} placeholder="https://openai.com/" /></Field>
          <Field label={copy.probeInterval} required><Input type="number" required min={10_000} max={3_600_000} step={1000} value={probeInterval} onChange={(_, data) => setProbeInterval(data.value)} /></Field>
          <Field label={copy.probeAttempts} required><Input type="number" required min={1} max={8} value={probeAttempts} onChange={(_, data) => setProbeAttempts(data.value)} /></Field>
          <p>{copy.probeHint}</p>
          <div className="button-row">
            <Button type="submit" appearance="primary" disabled={!validForm}>{copy.save}</Button>
            <Button type="button" onClick={async () => { if (await confirm(copy.discardEdit)) clearForm(); }}>{copy.cancelEdit}</Button>
          </div>
        </FormSection>
      </form>}
    </fieldset>
  </div>;
}

interface PolicyDraft {
  mode: string;
  proxyId: string;
  groupId: string;
  pluginKey: string;
  fields: Record<string, string | boolean>;
  configText: string;
}

interface StrategyProperty {
  type?: string;
  minimum?: number;
  maximum?: number;
  maxLength?: number;
}

function strategyProperties(strategy?: NetworkProxyStrategy): Record<string, StrategyProperty> {
  const properties = strategy?.schema.properties;
  if (!properties || typeof properties !== 'object' || Array.isArray(properties)) return {};
  return Object.fromEntries(Object.entries(properties).flatMap(([key, value]) =>
    value && typeof value === 'object' && !Array.isArray(value) ? [[key, value as StrategyProperty]] : []));
}

function strategyFor(strategies: NetworkProxyStrategy[], mode: string, pluginKey: string): NetworkProxyStrategy | undefined {
  if (mode === 'manual' || mode === 'auto') return strategies.find(strategy => strategy.id === mode);
  const separator = pluginKey.indexOf('\0');
  return separator < 0 ? undefined : strategies.find(strategy => strategy.id === pluginKey.slice(0, separator)
    && strategy.version === pluginKey.slice(separator + 1));
}

function scalarStrategy(strategy?: NetworkProxyStrategy): boolean {
  return Object.values(strategyProperties(strategy)).every(property =>
    property.type === 'integer' || property.type === 'number' || property.type === 'boolean' || property.type === 'string');
}

function strategyFields(strategy: NetworkProxyStrategy | undefined, config: Record<string, unknown>): {
  fields: Record<string, string | boolean>; configText: string;
} {
  const values = { ...(strategy?.default ?? {}), ...config };
  return {
    fields: Object.fromEntries(Object.entries(strategyProperties(strategy)).map(([key, property]) => [key,
      property.type === 'boolean' ? values[key] === true : values[key] === undefined ? '' : String(values[key])])),
    configText: JSON.stringify(config, null, 2),
  };
}

function emptyPolicyDraft(): PolicyDraft {
  return { mode: 'manual', proxyId: '', groupId: '', pluginKey: '', fields: {}, configText: '{}' };
}

function policyDraft(selection: NetworkProxySelection, strategies: NetworkProxyStrategy[]): PolicyDraft {
  const desired = selection.desired;
  const mode = desired.mode === 'manual' || desired.mode === 'auto' || desired.mode === 'plugin' ? desired.mode : 'manual';
  const pluginKey = desired.plugin_id && desired.plugin_version ? `${desired.plugin_id}\0${desired.plugin_version}` : '';
  const config = desired.config && typeof desired.config === 'object' && !Array.isArray(desired.config) ? desired.config : {};
  return {
    mode, proxyId: desired.proxy_id ?? '', groupId: desired.group_id ?? '', pluginKey,
    ...strategyFields(strategyFor(strategies, mode, pluginKey), config),
  };
}

export function NetworkProxyPolicyWorkspace({ token, tenant, accounts, accountsState = 'ready', initialAccountId, onChanged, closeRequest }: WorkspaceProps & {
  accounts: UpstreamAccount[];
  accountsState?: 'loading' | 'failed' | 'ready';
}) {
  const { locale, t } = useI18n();
  const copy = networkProxyCopy(locale);
  const eligibleAccounts = accounts.filter(account => account.driver === 'openai-codex' && account.auth_kind === 'oauth'
    && account.can_update_transport_proxy === true && (!account.tenant_external_id || account.tenant_external_id === tenant));
  const [failureKind, setFailureKind] = useState<ReturnType<typeof transportProxyFailureKind>>();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, failureKind === 'denied']);
  const [proxies, setProxies] = useState<NetworkProxy[]>([]);
  const [groups, setGroups] = useState<TransportProxyGroup[]>([]);
  const [strategies, setStrategies] = useState<NetworkProxyStrategy[]>([]);
  const [accountId, setAccountId] = useState(() => eligibleAccounts.find(account => account.id === initialAccountId)?.id ?? '');
  const [selection, setSelection] = useState<NetworkProxySelection>();
  const [draft, setDraft] = useState<PolicyDraft>(emptyPolicyDraft);
  const [dirty, setDirty] = useState(false);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const alive = useRef(false);
  const locked = useRef(false);
  const readRevision = useRef(0);
  const pendingWrite = useRef<AbortController | null>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const query = `?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const strategy = strategyFor(strategies, draft.mode, draft.pluginKey);
  const properties = strategyProperties(strategy);
  const useJson = !scalarStrategy(strategy);
  const selectedGroup = groups.find(group => group.id === draft.groupId);
  const plugins = strategies.filter(item => item.id !== 'manual' && item.id !== 'auto');
  const validDraft = draft.mode === 'manual' ? Boolean(draft.proxyId)
    : Boolean(selectedGroup) && (draft.mode === 'auto' || draft.mode === 'plugin' && Boolean(strategy));
  const request = <Result,>(path: string, init: RequestInit = {}) => transportProxyRequest<Result>(path, token, {
    ...init, signal: pendingWrite.current?.signal,
  });

  function changeDraft(change: Partial<PolicyDraft>) {
    setDraft(current => ({ ...current, ...change }));
    setDirty(true); setError(''); setMessage('');
  }

  function changeStrategy(mode: string, pluginKey = draft.pluginKey) {
    const nextStrategy = strategyFor(strategies, mode, pluginKey);
    changeDraft({ mode, pluginKey, ...strategyFields(nextStrategy, nextStrategy?.default ?? {}) });
  }

  async function requestClose() {
    if (pendingWrite.current) {
      if (!await confirm(copy.closePending)) return false;
      pendingWrite.current?.abort();
    } else if (dirty && !await confirm(copy.closeDraft)) return false;
    return true;
  }

  useLayoutEffect(() => {
    closeRequest.current = requestClose;
    return () => { closeRequest.current = null; };
  });

  useLayoutEffect(() => {
    if (busy || error || message) feedback.current?.focus({ preventScroll: true });
  }, [busy, error, message]);

  useEffect(() => {
    alive.current = true;
    void refresh();
    return () => { alive.current = false; readRevision.current += 1; pendingWrite.current?.abort(); };
  }, []);

  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (!dirty && !pendingWrite.current) return;
      event.preventDefault(); event.returnValue = '';
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => window.removeEventListener('beforeunload', beforeUnload);
  }, [dirty]);

  function clearRestrictedState() {
    setProxies([]); setGroups([]); setStrategies([]); setSelection(undefined); setDraft(emptyPolicyDraft()); setDirty(false); setAccountId('');
  }

  async function readSelection(id: string): Promise<NetworkProxySelection | undefined> {
    if (!id) return undefined;
    return request<NetworkProxySelection>(`${networkProxySelectionPath(id)}${query}`);
  }

  async function refresh() {
    if (locked.current) return;
    locked.current = true;
    const revision = ++readRevision.current;
    setBusy(true); setReady(false); setError(''); setMessage(''); setSelection(undefined); setDirty(false);
    try {
      const [proxyList, groupList, strategyList] = await Promise.all([
        request<{ items: NetworkProxy[] }>(`${networkProxiesPath}${query}`),
        request<{ items: TransportProxyGroup[] }>(`${transportProxyGroupsPath}${query}`),
        request<{ items: NetworkProxyStrategy[] }>(`${networkProxyStrategiesPath}${query}`),
      ]);
      const current = await readSelection(accountId);
      if (!alive.current || revision !== readRevision.current) return;
      setProxies(proxyList.items); setGroups(groupList.items); setStrategies(strategyList.items);
      setSelection(current); setDraft(current ? policyDraft(current, strategyList.items) : emptyPolicyDraft());
      setReady(true);
      if (failureKind === 'unknown') setMessage(copy.refreshedUnknown);
      setFailureKind(undefined);
    } catch (reason) {
      if (alive.current && revision === readRevision.current) {
        setError(networkProxyError(reason, locale, 'read'));
        if (transportProxyFailureKind(reason) === 'denied') { clearRestrictedState(); setFailureKind('denied'); }
      }
    } finally { locked.current = false; if (alive.current) setBusy(false); }
  }

  async function selectAccount(id: string) {
    if (locked.current || dirty && !await confirm(copy.switchAccountDraft)) return;
    locked.current = true;
    const revision = ++readRevision.current;
    setAccountId(id); setSelection(undefined); setDraft(emptyPolicyDraft()); setDirty(false); setBusy(true); setError(''); setMessage('');
    try {
      const current = await readSelection(id);
      if (!alive.current || revision !== readRevision.current) return;
      setSelection(current); setDraft(current ? policyDraft(current, strategies) : emptyPolicyDraft()); setReady(true);
    } catch (reason) {
      if (alive.current && revision === readRevision.current) {
        setError(networkProxyError(reason, locale, 'read')); setReady(false);
        if (transportProxyFailureKind(reason) === 'denied') { clearRestrictedState(); setFailureKind('denied'); }
      }
    } finally { locked.current = false; if (alive.current) setBusy(false); }
  }

  function draftConfig(): Record<string, unknown> | undefined {
    if (draft.mode === 'manual') return {};
    if (useJson) {
      try {
        const value: unknown = JSON.parse(draft.configText);
        return value && typeof value === 'object' && !Array.isArray(value) ? value as Record<string, unknown> : undefined;
      } catch { return undefined; }
    }
    const config: Record<string, unknown> = {};
    for (const [key, property] of Object.entries(properties)) {
      const value = draft.fields[key];
      if (property.type === 'boolean') { config[key] = value === true; continue; }
      if (property.type === 'string') {
        const text = String(value ?? '');
        if (property.maxLength !== undefined && text.length > property.maxLength) return undefined;
        config[key] = text; continue;
      }
      const number = Number(value);
      if (String(value ?? '').trim() === '' || !Number.isFinite(number) || property.type === 'integer' && !Number.isInteger(number)
        || property.minimum !== undefined && number < property.minimum || property.maximum !== undefined && number > property.maximum) return undefined;
      config[key] = number;
    }
    return config;
  }

  async function savePolicy() {
    if (!selection || locked.current || !ready || !validDraft) return;
    const config = draftConfig();
    if (!config) { setError(copy.configInvalid); return; }
    const base = {
      tenant_external_id: tenant,
      expected_selection_version: selection.selection_version,
      expected_binding_version: selection.binding_version,
      expected_credential_generation: selection.credential_generation,
      expected_updated_at: selection.account_updated_at,
    };
    const body = draft.mode === 'manual' ? { ...base, mode: 'manual', proxy_id: draft.proxyId, config }
      : {
        ...base, mode: draft.mode, group_id: draft.groupId, expected_group_version: selectedGroup!.version, config,
        ...(draft.mode === 'plugin' ? { plugin_id: strategy!.id, plugin_version: strategy!.version } : {}),
      };
    locked.current = true; readRevision.current += 1;
    pendingWrite.current = new AbortController();
    setBusy(true); setError(''); setMessage(''); setFailureKind(undefined);
    try {
      const saved = await request<NetworkProxySelection>(networkProxySelectionPath(selection.account_id), { method: 'PUT', body: JSON.stringify(body) });
      if (!alive.current) return;
      setSelection(saved); setDraft(policyDraft(saved, strategies)); setDirty(false); setMessage(copy.policySaved);
      void onChanged().catch(() => { if (alive.current) setError(copy.accountRefreshFailed); });
    } catch (reason) {
      if (alive.current) {
        const kind = transportProxyFailureKind(reason);
        setError(networkProxyError(reason, locale)); setFailureKind(kind); setReady(kind === 'validation');
        if (kind === 'denied') clearRestrictedState();
      }
    } finally { pendingWrite.current = null; locked.current = false; if (alive.current) setBusy(false); }
  }

  return <div className="transport-proxy-workspace network-proxy-policy-workspace">
    {confirmationDialog}
    <p>{copy.policyHint}</p>
    <div className="button-row"><Button type="button" disabled={busy} onClick={async () => {
      if (dirty && !await confirm(copy.refreshDraft)) return;
      void refresh();
    }}>{busy ? t('common.loading') : copy.refresh}</Button></div>
    <div ref={feedback} tabIndex={-1} aria-label={copy.operationStatus}>
      <LoadingProgress active={busy} label={t('common.loading')} level={ready ? 'section' : 'page'} />
      {error && <p className="notice error" role="alert">{error}</p>}
      {message && <p className="notice success" role="status">{message}</p>}
    </div>
    <FormSection title={copy.policyTitle}>
      {accountsState === 'loading' ? <LoadingState label={t('common.loading')} variant="compact" />
        : accountsState === 'failed' ? null
        : eligibleAccounts.length === 0 ? <p>{copy.noAccounts}</p> : <>
          <Field label={copy.account} required><Select value={accountId} disabled={busy || !ready} onChange={event => void selectAccount(event.target.value)}>
            <option value="">{copy.selectAccount}</option>
            {eligibleAccounts.map(account => <option key={account.id} value={account.id}>{account.name}</option>)}
          </Select></Field>
          {accountId && busy && !selection && <LoadingState label={copy.loadingPolicy} variant="compact" />}
          {selection && <>
            <p>{copy.currentPolicy}: {copy.modes[selection.desired.mode] ?? copy.policyLegacy}{selection.applied?.pending ? ` · ${copy.appliedPending}` : ''}</p>
            <Field label={copy.mode} required><Select value={draft.mode} disabled={busy || !ready} onChange={event => changeStrategy(event.target.value)}>
              <option value="manual">{copy.modes.manual}</option><option value="auto">{copy.modes.auto}</option>
              {(plugins.length > 0 || selection.desired.mode === 'plugin') && <option value="plugin">{copy.modes.plugin}</option>}
            </Select></Field>
            {draft.mode === 'manual' ? <Field label={copy.targetProxy} required><Select value={draft.proxyId} disabled={busy || !ready}
              onChange={event => changeDraft({ proxyId: event.target.value })}>
              <option value="">{copy.selectProxy}</option>{proxies.map(proxy => <option key={proxy.id} value={proxy.id}>{proxy.name}</option>)}
            </Select></Field> : <>
              <Field label={copy.targetGroup} required><Select value={draft.groupId} disabled={busy || !ready}
                onChange={event => changeDraft({ groupId: event.target.value })}>
                <option value="">{copy.selectGroup}</option>{groups.map(group => <option key={group.id} value={group.id}>{group.name}</option>)}
              </Select></Field>
              {draft.mode === 'plugin' && <Field label={copy.strategy} required><Select value={draft.pluginKey} disabled={busy || !ready}
                onChange={event => changeStrategy('plugin', event.target.value)}>
                <option value="">{copy.selectStrategy}</option>{plugins.map(item => <option key={`${item.id}\0${item.version}`} value={`${item.id}\0${item.version}`}>{item.id} · {item.version}</option>)}
              </Select></Field>}
              {(draft.mode === 'auto' || draft.mode === 'plugin' && strategy) && <FormSection title={copy.strategyConfig}>
                {useJson ? <>
                  <p className="muted">{copy.configUnsupported}</p>
                  <Textarea rows={10} value={draft.configText} disabled={busy || !ready} onChange={(_, data) => changeDraft({ configText: data.value })} />
                </> : Object.entries(properties).map(([key, property]) => property.type === 'boolean'
                  ? <Checkbox key={key} label={key} checked={draft.fields[key] === true} disabled={busy || !ready}
                    onChange={(_, data) => changeDraft({ fields: { ...draft.fields, [key]: data.checked === true } })} />
                  : <Field key={key} label={key} required><Input type={property.type === 'string' ? 'text' : 'number'} value={String(draft.fields[key] ?? '')}
                    min={property.minimum} max={property.maximum} maxLength={property.maxLength} step={property.type === 'integer' ? 1 : 'any'} disabled={busy || !ready}
                    onChange={(_, data) => changeDraft({ fields: { ...draft.fields, [key]: data.value } })} /></Field>)}
              </FormSection>}
            </>}
            <div className="button-row"><Button appearance="primary" type="button" disabled={busy || !ready || !validDraft} onClick={() => void savePolicy()}>{copy.savePolicy}</Button></div>
          </>}
        </>}
    </FormSection>
  </div>;
}
