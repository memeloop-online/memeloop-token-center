import { createContext, useContext, useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from 'react';
import { Button, Field, FormSection, Input, Label, LoadingProgress, LoadingState, Select } from '../design-system';
import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { useConfirmDialog } from '../useConfirmDialog';
import { useOperatorResource } from './hooks/useOperatorResource';
import { transportProxyError, transportProxyFailureKind, transportProxyGroupsPath, transportProxyRequest, type TransportProxyGroup } from './transportProxyGroups';
import { transportProxyGroupCopy } from './transportProxyGroupCopy';
import { egressAddress } from './requestEgress';
import './requestEgress.css';

interface Props {
  token: string;
  tenant: string;
  accounts: UpstreamAccount[];
  accountsState?: 'loading' | 'failed' | 'ready';
  onChanged: () => Promise<void>;
}

interface MemberDraft {
  key: string;
  id?: string;
  label: string;
  proxyUrl: string;
  originalProxyUrl?: string;
}

export function ProxyAddressInput({ id, value, disabled, required, placeholder, onChange }: {
  id: string; value: string; disabled: boolean; required: boolean; placeholder: string;
  onChange: (value: string) => void;
}) {
  const input = useRef<HTMLInputElement>(null);
  useLayoutEffect(() => {
    if (input.current && input.current.value !== value) input.current.value = value;
  }, [value]);
  return <input id={id} ref={input} className="proxy-address-input" type="text" autoComplete="off" autoCapitalize="none" spellCheck={false}
    disabled={disabled} required={required} placeholder={placeholder}
    onChange={event => onChange(event.target.value)} />;
}

const newMember = (): MemberDraft => ({ key: crypto.randomUUID(), label: '', proxyUrl: '' });
const ProxyGroupContext = createContext<{
  allowed: boolean;
  reason: string;
  retry: boolean;
  reload: () => void;
  open: (accountId: string) => void;
} | null>(null);

export function useTransportProxyGroups() {
  return useContext(ProxyGroupContext);
}

export function TransportProxyGroups(props: Props & { children: ReactNode; onNavigate?: (accountId?: string) => void }) {
  const { locale } = useI18n();
  const copy = transportProxyGroupCopy(locale);
  const accessResource = useOperatorResource(Boolean(props.token), props.token, async signal => {
    try {
      const result = await transportProxyRequest<{ can_manage: boolean }>(`${transportProxyGroupsPath}/access`, props.token, { signal });
      if (typeof result?.can_manage !== 'boolean') throw new Error();
      return result;
    } catch {
      throw new Error(copy.unavailable);
    }
  }, copy.unavailable);
  const accessState = accessResource.state;
  const access = accessState.kind === 'failed' || accessState.kind === 'ready' && accessState.refreshError
    ? 'unavailable' : accessState.kind === 'ready' ? accessState.value.can_manage ? 'allowed' : 'denied' : 'checking';
  return <ProxyGroupContext.Provider value={{
    allowed: Boolean(props.tenant && props.onNavigate) && access === 'allowed',
    reason: !props.tenant ? copy.selectTenant : access === 'checking' ? copy.checking : access === 'denied' ? copy.denied : access === 'unavailable' ? copy.unavailable : '',
    retry: access === 'denied' || access === 'unavailable',
    reload: () => void accessResource.reload(),
    open: accountId => { if (props.tenant && access === 'allowed') props.onNavigate?.(accountId); },
  }}>
    {props.children}
  </ProxyGroupContext.Provider>;
}

export function TransportProxyGroupAction({ accountId, disabled = false }: { accountId?: string; disabled?: boolean }) {
  const context = useContext(ProxyGroupContext);
  const { locale } = useI18n();
  const copy = transportProxyGroupCopy(locale);
  const description = useId();
  if (!context || !accountId) return null;
  return <div className="row-actions transport-proxy-management-action">
    <Button appearance="secondary" type="button" disabled={disabled || !context.allowed} aria-describedby={context.reason ? description : undefined} onClick={() => context.open(accountId)}>{copy.chooseGroup}</Button>
    {context.reason && <span id={description} className="muted" role="status">{context.reason}</span>}
    {context.retry && <Button appearance="secondary" type="button" onClick={context.reload}>{copy.retry}</Button>}
  </div>;
}

export function TransportProxyGroupFields({ groups, groupId, initialMember, onGroupChange, onMemberChange, disabled = false, emptyLabel }: {
  groups: TransportProxyGroup[]; groupId: string; initialMember: string;
  onGroupChange: (value: string) => void; onMemberChange: (value: string) => void;
  disabled?: boolean; emptyLabel?: string;
}) {
  const { locale } = useI18n();
  const copy = transportProxyGroupCopy(locale);
  const selectedGroup = groups.find(group => group.id === groupId);
  return <>
    <Field label={copy.targetGroup}><Select value={groupId} disabled={disabled} onChange={event => onGroupChange(event.target.value)}>
      <option value="">{emptyLabel ?? copy.selectGroup}</option>{groups.map(group => <option key={group.id} value={group.id}>{group.name}</option>)}
    </Select></Field>
    <Field label={copy.initialExit}><Select value={initialMember} disabled={disabled || !selectedGroup} onChange={event => onMemberChange(event.target.value)}>
      <option value="">{copy.selectExit}</option>{selectedGroup?.members.map(member => <option key={member.id} value={member.id}>{member.label}</option>)}
    </Select></Field>
  </>;
}

export function ProxyGroupWorkspace({ token, tenant, onChanged, closeRequest }: Pick<Props, 'token' | 'tenant' | 'onChanged'> & { closeRequest: RefObject<(() => Promise<boolean>) | null> }) {
  const { locale, t } = useI18n();
  const copy = transportProxyGroupCopy(locale);
  const inputPrefix = useId();
  const [failureKind, setFailureKind] = useState<ReturnType<typeof transportProxyFailureKind>>();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, failureKind === 'denied']);
  const [groups, setGroups] = useState<TransportProxyGroup[]>([]);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [editing, setEditing] = useState<TransportProxyGroup | null>();
  const [name, setName] = useState('');
  const [probeUrl, setProbeUrl] = useState('');
  const [probeAttempts, setProbeAttempts] = useState('2');
  const [members, setMembers] = useState<MemberDraft[]>([]);
  const alive = useRef(false);
  const locked = useRef(false);
  const pendingWrite = useRef<AbortController | null>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const query = `?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const validMembers = members.length >= 1 && members.length <= 4 && members.every(member => member.label.trim()
    && (member.id && !member.proxyUrl.trim() || member.proxyUrl.trim().startsWith('socks5h://')));
  const probeAttemptCount = Number(probeAttempts);
  const validProbe = Number.isInteger(probeAttemptCount) && probeAttemptCount >= 1 && probeAttemptCount <= 8
    && (probeUrl.trim() === '' || /^https?:\/\//.test(probeUrl.trim()));
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
    return () => { alive.current = false; pendingWrite.current?.abort(); };
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

  async function refresh() {
    if (locked.current) return;
    locked.current = true;
    setBusy(true); setReady(false); setError(''); setEditing(undefined);
    setMembers([]); setName(''); setProbeUrl(''); setProbeAttempts('2');
    try {
      const result = await request<{ items: TransportProxyGroup[] }>(`${transportProxyGroupsPath}${query}`);
      if (!alive.current) return;
      setGroups(result.items); setReady(true);
      if (failureKind === 'unknown') setMessage(copy.refreshedUnknown);
      setFailureKind(undefined);
    } catch (reason) {
      if (alive.current) {
        setError(transportProxyError(reason, locale, 'read'));
        if (transportProxyFailureKind(reason) === 'denied') { clearRestrictedState(); setFailureKind('denied'); }
      }
    }
    finally { locked.current = false; if (alive.current) setBusy(false); }
  }

  async function mutate(action: () => Promise<void>, success: string) {
    if (locked.current || !ready) return;
    locked.current = true;
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
        setError(transportProxyError(reason, locale)); setFailureKind(kind);
        setReady(kind === 'validation');
        if (kind === 'denied') clearRestrictedState();
      }
    } finally { pendingWrite.current = null; locked.current = false; if (alive.current) setBusy(false); }
  }

  function clearRestrictedState() {
    setGroups([]); setEditing(undefined); setMembers([]); setName(''); setProbeUrl(''); setProbeAttempts('2');
  }

  function editGroup(group: TransportProxyGroup | null) {
    setEditing(group); setName(group?.name ?? ''); setProbeUrl(group?.probe_url ?? ''); setProbeAttempts(String(group?.probe_attempts ?? 2)); setError(''); setMessage('');
    setMembers(group ? group.members.map(member => {
      const address = member.has_auth ? '' : egressAddress(member, locale);
      return { key: member.id, id: member.id, label: member.label, proxyUrl: address, originalProxyUrl: address };
    }) : [newMember()]);
  }

  function updateMember(key: string, change: Partial<MemberDraft>) {
    setMembers(current => current.map(member => member.key === key ? { ...member, ...change } : member));
  }

  async function saveGroup() {
    if (editing === undefined || !name.trim() || !validMembers || !validProbe) return;
    const body = {
      tenant_external_id: tenant, name: name.trim(),
      ...(editing ? { expected_version: editing.version } : {}),
      probe_url: probeUrl.trim() || null,
      probe_attempts: Number(probeAttempts),
      members: members.map(member => ({ ...(member.id ? { id: member.id } : {}), label: member.label.trim(),
        ...(member.proxyUrl.trim() && member.proxyUrl.trim() !== member.originalProxyUrl ? { proxy_url: member.proxyUrl.trim() } : {}) })),
    };
    await mutate(async () => {
      const saved = await request<TransportProxyGroup>(editing ? `${transportProxyGroupsPath}/${encodeURIComponent(editing.id)}` : transportProxyGroupsPath, {
        method: editing ? 'PUT' : 'POST', body: JSON.stringify(body),
      });
      if (!alive.current) return;
      setGroups(current => [...current.filter(group => group.id !== saved.id), saved]);
      setEditing(undefined); setMembers([]); setName(''); setProbeUrl(''); setProbeAttempts('2');
    }, copy.saved);
  }

  return <div className="transport-proxy-workspace">
    {confirmationDialog}
    <p>{copy.purpose}</p>
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
      <FormSection title={copy.title} description={copy.groupsHint}>
        <div className="proxy-group-list">
        {!ready && busy && groups.length === 0 && <LoadingState label={t('common.loading')} variant="compact" />}
        {ready && groups.length === 0 && <p>{copy.empty}</p>}
        {groups.map(group => <div className="button-row" key={group.id}>
          <span>{copy.groupSummary(group.name, group.members.length, group.bound_account_count)}</span>
          <ul className="proxy-group-addresses">{group.members.map(member => <li key={member.id}>{member.label} · {egressAddress(member, locale)}</li>)}</ul>
          <Button type="button" disabled={editing !== undefined} onClick={() => editGroup(group)}>{copy.edit(group.name)}</Button>
          <Button type="button" disabled={editing !== undefined || group.bound_account_count > 0} onClick={async () => {
            if (!await confirm(copy.confirmDelete(group.name))) return;
            void mutate(async () => {
              await request(`${transportProxyGroupsPath}/${encodeURIComponent(group.id)}`, { method: 'DELETE', body: JSON.stringify({ tenant_external_id: tenant, expected_version: group.version }) });
              if (alive.current) setGroups(current => current.filter(value => value.id !== group.id));
            }, copy.deleted);
          }}>{t('common.remove')}</Button>
        </div>)}
        </div>
        <Button type="button" disabled={editing !== undefined} onClick={() => editGroup(null)}>{copy.create}</Button>
      </FormSection>
      {editing !== undefined && <form onSubmit={event => { event.preventDefault(); void saveGroup(); }}>
        <FormSection title={editing ? copy.edit(editing.name) : copy.create}>
          <Field label={copy.name} required><Input required maxLength={64} value={name} onChange={(_, data) => setName(data.value)} /></Field>
          {members.map((member, index) => {
            const savedMember = editing?.members.find(saved => saved.id === member.id);
            return <FormSection key={member.key} title={copy.candidate(index + 1)}>
            <Field label={copy.memberName} required><Input required maxLength={64} value={member.label} onChange={(_, data) => updateMember(member.key, { label: data.value })} /></Field>
            <div className="proxy-address-field">
              <Label htmlFor={`${inputPrefix}-${member.key}`}>{savedMember?.has_auth ? copy.replaceAddress : member.id ? copy.address : copy.requiredAddress}</Label>
              <ProxyAddressInput id={`${inputPrefix}-${member.key}`} value={member.proxyUrl} disabled={busy || !ready} required={!member.id}
                onChange={value => updateMember(member.key, { proxyUrl: value })}
                placeholder={savedMember ? egressAddress(savedMember, locale) : 'socks5h://mihomo.egress.svc:1080'} />
            </div>
            <div className="button-row">
              <Button type="button" disabled={index === 0} onClick={() => setMembers(current => { const next = [...current]; [next[index - 1], next[index]] = [next[index], next[index - 1]]; return next; })}>{t('common.moveUp')}</Button>
              <Button type="button" disabled={index === members.length - 1} onClick={() => setMembers(current => { const next = [...current]; [next[index], next[index + 1]] = [next[index + 1], next[index]]; return next; })}>{t('common.moveDown')}</Button>
              <Button type="button" disabled={members.length <= 1} onClick={() => setMembers(current => current.filter(value => value.key !== member.key))}>{copy.removeMember}</Button>
            </div>
          </FormSection>;
          })}
          <p>{copy.addressHint}</p>
          <Button type="button" disabled={members.length >= 4} onClick={() => setMembers(current => [...current, newMember()])}>{copy.addMember}</Button>
          <Field label={copy.probeUrl}><Input maxLength={200} value={probeUrl} onChange={(_, data) => setProbeUrl(data.value)} placeholder="https://example.com/" /></Field>
          <Field label={copy.probeAttempts}><Input type="number" required min={1} max={8} value={probeAttempts} onChange={(_, data) => setProbeAttempts(data.value)} /></Field>
          <p>{copy.probeHint}</p>
          {editing && editing.last_probe_checked_at ? <p>{copy.lastProbe(editing.last_probe_status ?? '', new Date(editing.last_probe_checked_at).toLocaleString(locale))}</p> : null}
          <div className="button-row">
            <Button type="submit" appearance="primary" disabled={!name.trim() || !validMembers || !validProbe}>{copy.save}</Button>
            {editing?.id ? <Button type="button" disabled={busy || !editing?.probe_url} onClick={() => { const groupId = editing?.id; if (!groupId) return; void mutate(async () => { await request(`${transportProxyGroupsPath}/${encodeURIComponent(groupId)}/probe${query}`, { method: 'POST' }); }, copy.probeQueued); }}>{copy.probeNow}</Button> : null}
            <Button type="button" onClick={async () => { if (await confirm(copy.discardEdit)) { setEditing(undefined); setMembers([]); setName(''); setProbeUrl(''); setProbeAttempts('2'); } }}>{copy.cancelEdit}</Button>
          </div>
        </FormSection>
      </form>}
    </fieldset>
  </div>;
}
