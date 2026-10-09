import { createContext, useContext, useEffect, useId, useLayoutEffect, useRef, useState, type ReactNode, type RefObject } from 'react';
import { Button, Disclosure, Field, FormSection, Input, LoadingProgress, LoadingState, Select } from '../design-system';
import { SecretInput } from '../SecretInput';
import { useI18n } from '../i18n';
import type { UpstreamAccount } from '../types';
import { useConfirmDialog } from '../useConfirmDialog';
import { useOperatorResource } from './hooks/useOperatorResource';
import { transportProxyError, transportProxyFailureKind, transportProxyGroupsPath, transportProxyRequest, type TransportProxyBinding, type TransportProxyGroup } from './transportProxyGroups';
import { transportProxyGroupCopy } from './transportProxyGroupCopy';

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
}

const newMember = (): MemberDraft => ({ key: crypto.randomUUID(), label: '', proxyUrl: '' });
const ProxyGroupContext = createContext<{
  allowed: boolean;
  reason: string;
  retry: boolean;
  reload: () => void;
  open: (accountId: string) => void;
} | null>(null);

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

export function ProxyGroupWorkspace({ token, tenant, accounts, accountsState = 'ready', onChanged, closeRequest, initialAccountId }: Props & { initialAccountId?: string; closeRequest: RefObject<(() => Promise<boolean>) | null> }) {
  const { locale, t } = useI18n();
  const copy = transportProxyGroupCopy(locale);
  const eligibleAccounts = accounts.filter(account => account.driver === 'openai-codex' && account.auth_kind === 'oauth'
    && account.can_update_transport_proxy === true && (!account.tenant_external_id || account.tenant_external_id === tenant));
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
  const [members, setMembers] = useState<MemberDraft[]>([]);
  const [replacement, setReplacement] = useState('');
  const [accountId, setAccountId] = useState(() => eligibleAccounts.find(account => account.id === initialAccountId)?.id ?? '');
  const [binding, setBinding] = useState<TransportProxyBinding>();
  const [groupId, setGroupId] = useState('');
  const [initialMember, setInitialMember] = useState('');
  const [singleMember, setSingleMember] = useState('');
  const alive = useRef(false);
  const locked = useRef(false);
  const pendingWrite = useRef<AbortController | null>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const bindingRead = useRef(0);
  const query = `?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const selectedGroup = groups.find(group => group.id === groupId);
  const boundGroup = groups.find(group => group.id === binding?.group_id);
  const bindingDirty = Boolean(groupId || initialMember || singleMember);
  const requiresReplacement = Boolean(editing?.bound_account_count && editing.members.some(member => {
    const draft = members.find(value => value.id === member.id);
    return !draft || Boolean(draft.proxyUrl.trim());
  }));
  const validMembers = members.length >= 1 && members.length <= 4 && members.every(member => member.label.trim()
    && (member.id && !member.proxyUrl.trim() || member.proxyUrl.trim().startsWith('socks5h://')));
  const bindingPath = (id: string) => `/internal/v1/upstreams/${encodeURIComponent(id)}/transport-proxy-group`;
  const request = <Result,>(path: string, init: RequestInit = {}) => transportProxyRequest<Result>(path, token, {
    ...init, signal: pendingWrite.current?.signal,
  });

  async function requestClose() {
    if (pendingWrite.current) {
      if (!await confirm(copy.closePending)) return false;
      pendingWrite.current?.abort();
    } else if ((editing !== undefined || bindingDirty) && !await confirm(copy.closeDraft)) return false;
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
    return () => { alive.current = false; bindingRead.current += 1; pendingWrite.current?.abort(); };
  }, []);

  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      if (editing === undefined && !bindingDirty && !pendingWrite.current) return;
      event.preventDefault();
      event.returnValue = '';
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => window.removeEventListener('beforeunload', beforeUnload);
  }, [editing, bindingDirty]);

  async function refresh() {
    if (locked.current) return;
    locked.current = true;
    bindingRead.current += 1;
    setBusy(true); setReady(false); setError(''); setBinding(undefined); setEditing(undefined);
    setMembers([]); setName(''); setReplacement(''); setGroupId(''); setInitialMember(''); setSingleMember('');
    try {
      const result = await request<{ items: TransportProxyGroup[] }>(`${transportProxyGroupsPath}${query}`);
      const current = accountId ? await request<TransportProxyBinding>(`${bindingPath(accountId)}${query}`) : undefined;
      if (!alive.current) return;
      setGroups(result.items); setBinding(current); setReady(true);
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

  async function selectAccount(id: string) {
    if (bindingDirty && !await confirm(copy.switchAccountDraft)) return;
    const revision = ++bindingRead.current;
    setAccountId(id); setBinding(undefined); setGroupId(''); setInitialMember(''); setSingleMember(''); setError(''); setMessage('');
    if (!id) return;
    try {
      const result = await request<TransportProxyBinding>(`${bindingPath(id)}${query}`);
      if (alive.current && revision === bindingRead.current) setBinding(result);
    } catch (reason) { if (alive.current && revision === bindingRead.current) setError(transportProxyError(reason, locale, 'read')); }
  }

  async function mutate(action: () => Promise<void>, success: string) {
    if (locked.current || !ready) return;
    locked.current = true; bindingRead.current += 1;
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
    setGroups([]); setBinding(undefined); setEditing(undefined); setMembers([]); setName(''); setReplacement('');
    setGroupId(''); setInitialMember(''); setSingleMember(''); setAccountId('');
  }

  function editGroup(group: TransportProxyGroup | null) {
    setEditing(group); setName(group?.name ?? ''); setReplacement(''); setError(''); setMessage('');
    setMembers(group ? group.members.map(member => ({ key: member.id, id: member.id, label: member.label, proxyUrl: '' })) : [newMember()]);
  }

  function updateMember(key: string, change: Partial<MemberDraft>) {
    setMembers(current => current.map(member => member.key === key ? { ...member, ...change } : member));
  }

  async function saveGroup() {
    if (editing === undefined || !name.trim() || !validMembers || requiresReplacement && !members.some(member => member.id === replacement)) return;
    const body = {
      tenant_external_id: tenant, name: name.trim(),
      ...(editing ? { expected_version: editing.version } : {}),
      ...(requiresReplacement ? { replacement_member_id: replacement } : {}),
      members: members.map(member => ({ ...(member.id ? { id: member.id } : {}), label: member.label.trim(), ...(member.proxyUrl.trim() ? { proxy_url: member.proxyUrl.trim() } : {}) })),
    };
    await mutate(async () => {
      const saved = await request<TransportProxyGroup>(editing ? `${transportProxyGroupsPath}/${encodeURIComponent(editing.id)}` : transportProxyGroupsPath, {
        method: editing ? 'PUT' : 'POST', body: JSON.stringify(body),
      });
      if (!alive.current) return;
      setGroups(current => [...current.filter(group => group.id !== saved.id), saved]);
      setEditing(undefined); setMembers([]); setName(''); setBinding(undefined);
      if (accountId) {
        try {
          const latest = await request<TransportProxyBinding>(`${bindingPath(accountId)}${query}`);
          if (alive.current) setBinding(latest);
        } catch { if (alive.current) setError(copy.errors.load); }
      }
    }, copy.saved);
  }

  async function changeBinding(unbind: boolean) {
    if (!binding || binding.account_id !== accountId || !eligibleAccounts.some(account => account.id === accountId)) return;
    if (unbind ? !boundGroup || !boundGroup.members.some(member => member.id === singleMember)
      : !selectedGroup || !selectedGroup.members.some(member => member.id === initialMember)) return;
    if (unbind && !await confirm(copy.confirmUnlink)) return;
    const previousGroup = binding.group_id;
    await mutate(async () => {
      const saved = await request<TransportProxyBinding>(bindingPath(accountId), {
        method: unbind ? 'DELETE' : 'PUT',
        body: JSON.stringify({
          tenant_external_id: tenant, expected_binding_version: binding.binding_version,
          expected_credential_generation: binding.credential_generation, expected_updated_at: binding.updated_at,
          ...(unbind ? { expected_group_version: boundGroup!.version, single_proxy_member_id: singleMember }
            : { group_id: selectedGroup!.id, expected_group_version: selectedGroup!.version, initial_member_id: initialMember }),
        }),
      });
      if (!alive.current) return;
      setBinding(saved); setGroupId(''); setInitialMember(''); setSingleMember('');
      setGroups(current => current.map(group => ({ ...group, bound_account_count: group.bound_account_count
        - (group.id === previousGroup ? 1 : 0) + (group.id === saved.group_id ? 1 : 0) })));
    }, unbind ? copy.unbound : copy.bound);
  }

  return <div className="transport-proxy-workspace">
    {confirmationDialog}
    <p>{copy.purpose}</p>
    <div className="button-row">
      <Button type="button" disabled={busy} onClick={async () => {
        if ((editing !== undefined || bindingDirty) && !await confirm(copy.refreshDraft)) return;
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
          <Button type="button" disabled={editing !== undefined} onClick={() => editGroup(group)}>{copy.edit(group.name)}</Button>
          <Button type="button" disabled={editing !== undefined || group.bound_account_count > 0} onClick={async () => {
            if (!await confirm(copy.confirmDelete(group.name))) return;
            void mutate(async () => {
              await request(`${transportProxyGroupsPath}/${encodeURIComponent(group.id)}`, { method: 'DELETE', body: JSON.stringify({ tenant_external_id: tenant, expected_version: group.version }) });
              if (alive.current) { setGroups(current => current.filter(value => value.id !== group.id)); setGroupId(''); setInitialMember(''); }
            }, copy.deleted);
          }}>{t('common.remove')}</Button>
        </div>)}
        </div>
        <Button type="button" disabled={editing !== undefined} onClick={() => editGroup(null)}>{copy.create}</Button>
      </FormSection>
      {editing !== undefined && <form onSubmit={event => { event.preventDefault(); void saveGroup(); }}>
        <FormSection title={editing ? copy.edit(editing.name) : copy.create}>
          <Field label={copy.name} required><Input required maxLength={64} value={name} onChange={(_, data) => setName(data.value)} /></Field>
          {members.map((member, index) => <FormSection key={member.key} title={copy.candidate(index + 1)}>
            <Field label={copy.memberName} required><Input required maxLength={64} value={member.label} onChange={(_, data) => updateMember(member.key, { label: data.value })} /></Field>
            <label htmlFor={`${inputPrefix}-${member.key}`}>{member.id ? copy.replaceAddress : copy.requiredAddress}</label>
            <SecretInput fluent id={`${inputPrefix}-${member.key}`} label={copy.address} value={member.proxyUrl} disabled={busy || !ready} required={!member.id} onChange={event => updateMember(member.key, { proxyUrl: event.target.value })} placeholder="socks5h://mihomo.egress.svc:1080" />
            <div className="button-row">
              <Button type="button" disabled={index === 0} onClick={() => setMembers(current => { const next = [...current]; [next[index - 1], next[index]] = [next[index], next[index - 1]]; return next; })}>{t('common.moveUp')}</Button>
              <Button type="button" disabled={index === members.length - 1} onClick={() => setMembers(current => { const next = [...current]; [next[index], next[index + 1]] = [next[index + 1], next[index]]; return next; })}>{t('common.moveDown')}</Button>
              <Button type="button" disabled={members.length <= 1} onClick={() => { setMembers(current => current.filter(value => value.key !== member.key)); if (replacement === member.id) setReplacement(''); }}>{copy.removeMember}</Button>
            </div>
          </FormSection>)}
          <p>{copy.addressHint}</p>
          <Button type="button" disabled={members.length >= 4} onClick={() => setMembers(current => [...current, newMember()])}>{copy.addMember}</Button>
          {requiresReplacement && <Field label={copy.replacement} required>
            <Select value={replacement} onChange={event => setReplacement(event.target.value)} required>
              <option value="">{copy.selectSavedMember}</option>
              {members.filter(member => member.id).map(member => <option key={member.id} value={member.id}>{member.label || copy.unnamed}</option>)}
            </Select>
            <p>{copy.replacementHint}</p>
          </Field>}
          <div className="button-row">
            <Button type="submit" appearance="primary" disabled={!name.trim() || !validMembers || requiresReplacement && !replacement}>{copy.save}</Button>
            <Button type="button" onClick={async () => { if (await confirm(copy.discardEdit)) { setEditing(undefined); setMembers([]); setName(''); } }}>{copy.cancelEdit}</Button>
          </div>
        </FormSection>
      </form>}
      <fieldset disabled={editing !== undefined} style={{ border: 0, padding: 0, minWidth: 0 }}>
        <FormSection title={copy.bindingTitle} description={copy.bindingHint}>
          <Field label={copy.account}><Select value={accountId} disabled={accountsState !== 'ready'} onChange={event => void selectAccount(event.target.value)}>
            <option value="">{copy.selectAccount}</option>
            {eligibleAccounts.map(account => <option key={account.id} value={account.id}>{account.name}</option>)}
          </Select></Field>
          {accountsState === 'loading' && <LoadingState label={copy.loadingBinding} variant="compact" />}
          {ready && accountsState === 'ready' && eligibleAccounts.length === 0 && <p>{copy.noAccounts}</p>}
          {accountId && !binding && !error && <LoadingState label={copy.loadingBinding} variant="compact" />}
          {binding && <>
            <p>{copy.currentGroup}: {binding.group_id ? boundGroup?.name ?? copy.refreshRequired : copy.noGroup}</p>
            <Disclosure title={copy.connectionDetails}>
              <p>{copy.runtime[binding.runtime.configuration_state]}</p>
              <p>{copy.observedAt}: {new Date(binding.runtime.observed_at).toLocaleString(locale)}</p>
              <p>{copy.observedExit}: {binding.runtime.selected_member_id ? boundGroup?.members.find(member => member.id === binding.runtime.selected_member_id)?.label ?? copy.refreshRequired : copy.noSelection}</p>
              <p className="muted">{copy.runtimeHint}</p>
            </Disclosure>
            <Field label={copy.targetGroup}><Select value={groupId} onChange={event => { setGroupId(event.target.value); setInitialMember(''); }}>
              <option value="">{copy.selectGroup}</option>{groups.map(group => <option key={group.id} value={group.id}>{group.name}</option>)}
            </Select></Field>
            <Field label={copy.initialExit}><Select value={initialMember} disabled={!selectedGroup} onChange={event => setInitialMember(event.target.value)}>
              <option value="">{copy.selectExit}</option>{selectedGroup?.members.map(member => <option key={member.id} value={member.id}>{member.label}</option>)}
            </Select></Field>
            <Button type="button" appearance="primary" disabled={!selectedGroup || !initialMember} onClick={() => void changeBinding(false)}>{copy.saveBinding}</Button>
            {boundGroup && <>
              <Field label={copy.retainedExit}><Select value={singleMember} onChange={event => setSingleMember(event.target.value)}>
                <option value="">{copy.selectRetained}</option>{boundGroup.members.map(member => <option key={member.id} value={member.id}>{member.label}</option>)}
              </Select></Field>
              <Button type="button" disabled={!singleMember} onClick={() => void changeBinding(true)}>{copy.unlink}</Button>
            </>}
          </>}
        </FormSection>
      </fieldset>
    </fieldset>
  </div>;
}
