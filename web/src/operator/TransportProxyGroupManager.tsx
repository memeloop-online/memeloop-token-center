import { useEffect, useId, useLayoutEffect, useRef, useState, type RefObject } from 'react';
import { Button, Dialog, DialogBody, DialogContent, DialogSurface, DialogTitle, DialogTrigger, Field, FormSection, Input, Select } from '../design-system';
import { SecretInput } from '../SecretInput';
import type { UpstreamAccount } from '../types';
import { useConfirmDialog } from '../useConfirmDialog';
import { transportProxyError, transportProxyFailureKind, transportProxyGroupsPath, transportProxyRequest, type TransportProxyBinding, type TransportProxyGroup } from './transportProxyGroups';

interface Props {
  token: string;
  tenant: string;
  accounts: UpstreamAccount[];
  onChanged: () => Promise<void>;
}

interface MemberDraft {
  key: string;
  id?: string;
  label: string;
  proxyUrl: string;
}

const newMember = (): MemberDraft => ({ key: crypto.randomUUID(), label: '', proxyUrl: '' });
const runtimeLabels = { unbound: '未绑定', pending: '等待当前进程加载', applied: '当前进程已加载配置', unavailable: '当前进程状态不可用' };

export function TransportProxyGroupManager(props: Props) {
  const [open, setOpen] = useState(false);
  const [access, setAccess] = useState<'checking' | 'allowed' | 'denied' | 'unavailable'>('checking');
  const [accessRevision, setAccessRevision] = useState(0);
  const accessDescription = useId();
  const closeRequest = useRef<(() => Promise<void>) | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    setAccess('checking');
    if (!props.tenant || !props.token) return () => controller.abort();
    void transportProxyRequest<{ items: TransportProxyGroup[] }>(`${transportProxyGroupsPath}?${new URLSearchParams({ tenant_external_id: props.tenant })}`, props.token, { signal: controller.signal })
      .then(result => { if (!controller.signal.aborted) setAccess(Array.isArray(result.items) ? 'allowed' : 'unavailable'); })
      .catch(reason => { if (!controller.signal.aborted) setAccess(transportProxyFailureKind(reason) === 'denied' ? 'denied' : 'unavailable'); });
    return () => controller.abort();
  }, [props.token, props.tenant, accessRevision]);
  return <Dialog open={open} onOpenChange={(_, data) => { if (data.open) setOpen(true); else void closeRequest.current?.(); }}>
    <div className="button-row">
      <DialogTrigger disableButtonEnhancement><Button appearance="secondary" type="button" disabled={!props.tenant || access !== 'allowed'} aria-describedby={accessDescription}>代理组与账号绑定</Button></DialogTrigger>
      <span id={accessDescription} role="status">{!props.tenant ? '请先选择租户。' : access === 'checking' ? '正在确认全局操作员及 providers:write 管理权限…' : access === 'denied' ? '无管理权限：需要具有 providers:write 权限的全局操作员。' : access === 'unavailable' ? '暂时无法确认管理权限，入口已禁用，请稍后重试。' : '已确认当前租户的全局操作员及 providers:write 管理权限。'}</span>
      {(access === 'denied' || access === 'unavailable') && <Button type="button" onClick={() => setAccessRevision(current => current + 1)}>重新检查管理权限</Button>}
    </div>
    <DialogSurface style={{ width: 'min(920px, 96vw)', maxWidth: '96vw' }}>
      <DialogBody>
        <DialogTitle>代理组与账号绑定</DialogTitle>
        <DialogContent>{open && <ProxyGroupWorkspace {...props} key={`${props.token}\0${props.tenant}`} closeRequest={closeRequest} onClose={() => setOpen(false)} />}</DialogContent>
      </DialogBody>
    </DialogSurface>
  </Dialog>;
}

function ProxyGroupWorkspace({ token, tenant, accounts, onChanged, onClose, closeRequest }: Props & { onClose: () => void; closeRequest: RefObject<(() => Promise<void>) | null> }) {
  const inputPrefix = useId();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant]);
  const [groups, setGroups] = useState<TransportProxyGroup[]>([]);
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [editing, setEditing] = useState<TransportProxyGroup | null>();
  const [name, setName] = useState('');
  const [members, setMembers] = useState<MemberDraft[]>([]);
  const [replacement, setReplacement] = useState('');
  const [accountId, setAccountId] = useState('');
  const [binding, setBinding] = useState<TransportProxyBinding>();
  const [groupId, setGroupId] = useState('');
  const [initialMember, setInitialMember] = useState('');
  const [singleMember, setSingleMember] = useState('');
  const alive = useRef(false);
  const locked = useRef(false);
  const pendingWrite = useRef<AbortController | null>(null);
  const feedback = useRef<HTMLDivElement>(null);
  const [failureKind, setFailureKind] = useState<ReturnType<typeof transportProxyFailureKind>>();
  const bindingRead = useRef(0);
  const query = `?${new URLSearchParams({ tenant_external_id: tenant })}`;
  const eligibleAccounts = accounts.filter(account => account.driver === 'openai-codex' && account.auth_kind === 'oauth'
    && account.can_update_transport_proxy === true && (!account.tenant_external_id || account.tenant_external_id === tenant));
  const selectedGroup = groups.find(group => group.id === groupId);
  const boundGroup = groups.find(group => group.id === binding?.group_id);
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
      if (!await confirm('写入仍在等待响应。退出仅停止浏览器等待，不表示服务端取消；结果未知，重新进入后须读回配置核对，不能直接重试。是否返回上层？')) return;
      pendingWrite.current?.abort();
    } else if (editing !== undefined && !await confirm('关闭将丢弃未保存的代理组修改，是否返回上层？')) return;
    onClose();
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
      if (failureKind === 'unknown') setMessage('已读回当前配置，请核对是否包含上次操作；读回不代表先前写入已取消，确认后再决定是否重试。');
      setFailureKind(undefined);
    } catch (reason) { if (alive.current) setError(transportProxyError(reason)); }
    finally { locked.current = false; if (alive.current) setBusy(false); }
  }

  async function selectAccount(id: string) {
    const revision = ++bindingRead.current;
    setAccountId(id); setBinding(undefined); setGroupId(''); setInitialMember(''); setSingleMember(''); setError(''); setMessage('');
    if (!id) return;
    try {
      const result = await request<TransportProxyBinding>(`${bindingPath(id)}${query}`);
      if (alive.current && revision === bindingRead.current) setBinding(result);
    } catch (reason) { if (alive.current && revision === bindingRead.current) setError(transportProxyError(reason)); }
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
      void onChanged().catch(() => { if (alive.current) setError('配置已提交，但账号列表刷新失败。关闭后请刷新上层页面。'); });
    } catch (reason) {
      if (alive.current) {
        const kind = transportProxyFailureKind(reason);
        setError(transportProxyError(reason)); setFailureKind(kind);
        setReady(kind === 'validation');
      }
    } finally { pendingWrite.current = null; locked.current = false; if (alive.current) setBusy(false); }
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
      setEditing(undefined); setMembers([]); setName(''); setBinding(undefined); setAccountId('');
    }, '代理组配置已保存。已有健康出口保持粘性；运行时配置由各进程异步加载。');
  }

  async function changeBinding(unbind: boolean) {
    if (!binding || binding.account_id !== accountId || !eligibleAccounts.some(account => account.id === accountId)) return;
    if (unbind ? !boundGroup || !boundGroup.members.some(member => member.id === singleMember)
      : !selectedGroup || !selectedGroup.members.some(member => member.id === initialMember)) return;
    if (unbind && !await confirm('确认解绑？账号将保留所选出口作为单代理，不会改为直连。')) return;
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
    }, unbind ? '解绑配置已受理，将异步生效；账号保留所选单代理，不会直连。' : '绑定配置已受理，将异步生效；不表示所有进程已应用或代理健康。');
  }

  return <div className="transport-proxy-workspace">
    {confirmationDialog}
    <p>当前出口保持粘性，仅连接失败时尝试备用出口；健康出口不轮询、不自动切回。请求送达不明或已开始输出时不会因此重放。</p>
    <p className="muted">仅全局操作员的 providers:write 权限可管理当前租户；账号绑定仅支持原生 OpenAI Codex OAuth 账号。</p>
    <div className="button-row">
      <Button type="button" disabled={busy} onClick={async () => {
        if (editing !== undefined && !await confirm('刷新将丢弃当前未保存的代理组修改，是否继续？')) return;
        setMessage(''); void refresh();
      }}>{busy ? '处理中…' : '刷新配置'}</Button>
      <Button type="button" appearance="secondary" onClick={() => void requestClose()}>关闭并返回供应商</Button>
    </div>
    <div ref={feedback} tabIndex={-1} aria-label="代理组操作状态">
      {busy && <p role="status">正在处理请求…</p>}
      {error && <p className="notice error" role="alert">{error}</p>}
      {message && <p className="notice success" role="status">{message}</p>}
    </div>
    <fieldset disabled={!ready || busy} style={{ border: 0, padding: 0, minWidth: 0 }}>
      <FormSection title="代理组" description="每组 1–4 个私网 socks5h 出口，按列表顺序尝试候选；配置绑定数不代表活跃连接数。有账号绑定的组可维护成员，但不能整体删除。">
        {ready && groups.length === 0 && <p>尚无代理组，请先新建并添加出口。</p>}
        {groups.map(group => <div className="button-row" key={group.id}>
          <span>{group.name} · {group.members.length} 个出口 · {group.bound_account_count} 个账号绑定</span>
          <Button type="button" disabled={editing !== undefined} onClick={() => editGroup(group)}>编辑 {group.name}</Button>
          <Button type="button" disabled={editing !== undefined || group.bound_account_count > 0} onClick={async () => {
            if (!await confirm(`确认删除代理组「${group.name}」？`)) return;
            void mutate(async () => {
              await request(`${transportProxyGroupsPath}/${encodeURIComponent(group.id)}`, { method: 'DELETE', body: JSON.stringify({ tenant_external_id: tenant, expected_version: group.version }) });
              if (alive.current) { setGroups(current => current.filter(value => value.id !== group.id)); setGroupId(''); setInitialMember(''); }
            }, '代理组已删除。');
          }}>删除</Button>
        </div>)}
        <Button type="button" disabled={editing !== undefined} onClick={() => editGroup(null)}>新建代理组</Button>
      </FormSection>
      {editing !== undefined && <form onSubmit={event => { event.preventDefault(); void saveGroup(); }}>
        <FormSection title={editing ? `编辑「${editing.name}」` : '新建代理组'}>
          <Field label="代理组名称" required><Input required maxLength={64} value={name} onChange={(_, data) => setName(data.value)} /></Field>
          {members.map((member, index) => <FormSection key={member.key} title={`候选出口 ${index + 1}`}>
            <Field label="出口名称" required><Input required maxLength={64} value={member.label} onChange={(_, data) => updateMember(member.key, { label: data.value })} /></Field>
            <label htmlFor={`${inputPrefix}-${member.key}`}>{member.id ? '替换代理地址（留空保留原值）' : '私网代理地址（必填）'}</label>
            <SecretInput fluent id={`${inputPrefix}-${member.key}`} label="私网代理地址" value={member.proxyUrl} disabled={busy || !ready} required={!member.id} onChange={event => updateMember(member.key, { proxyUrl: event.target.value })} placeholder="socks5h://mihomo.egress.svc:1080" />
            <div className="button-row">
              <Button type="button" disabled={index === 0} onClick={() => setMembers(current => { const next = [...current]; [next[index - 1], next[index]] = [next[index], next[index - 1]]; return next; })}>上移</Button>
              <Button type="button" disabled={index === members.length - 1} onClick={() => setMembers(current => { const next = [...current]; [next[index], next[index + 1]] = [next[index + 1], next[index]]; return next; })}>下移</Button>
              <Button type="button" disabled={members.length <= 1} onClick={() => { setMembers(current => current.filter(value => value.key !== member.key)); if (replacement === member.id) setReplacement(''); }}>移除出口</Button>
            </div>
          </FormSection>)}
          <p>代理地址只写不回显，支持私网 IP 和 Kubernetes Service DNS。不会进行健康探测；相同地址不可重复添加。</p>
          <Button type="button" disabled={members.length >= 4} onClick={() => setMembers(current => [...current, newMember()])}>添加出口</Button>
          {requiresReplacement && <Field label="无法保留当前选择时使用的替代出口" required>
            <Select value={replacement} onChange={event => setReplacement(event.target.value)} required>
              <option value="">请选择已保存且仍保留的出口</option>
              {members.filter(member => member.id).map(member => <option key={member.id} value={member.id}>{member.label || '未命名出口'}</option>)}
            </Select>
            <p>仅当当前进程的选择无法保留时使用。若需使用全新出口，请先添加并保存，再移除旧出口。</p>
          </Field>}
          <div className="button-row">
            <Button type="submit" appearance="primary" disabled={!name.trim() || !validMembers || requiresReplacement && !replacement}>保存代理组</Button>
            <Button type="button" onClick={async () => { if (await confirm('放弃当前未保存的代理组修改？')) { setEditing(undefined); setMembers([]); setName(''); } }}>取消编辑</Button>
          </div>
        </FormSection>
      </form>}
      <fieldset disabled={editing !== undefined} style={{ border: 0, padding: 0, minWidth: 0 }}>
        <FormSection title="账号绑定" description="按名称选择账号与代理组；首次绑定或更换组须显式选择起始出口。重复配置同一组不会重置已经健康切换的出口。">
          <Field label="账号"><Select value={accountId} onChange={event => void selectAccount(event.target.value)}>
            <option value="">请选择账号</option>
            {eligibleAccounts.map(account => <option key={account.id} value={account.id}>{account.name}</option>)}
          </Select></Field>
          {ready && eligibleAccounts.length === 0 && <p>当前租户没有可管理的原生 Codex OAuth 账号。</p>}
          {accountId && !binding && <p role="status">绑定信息尚未就绪，请等待；加载失败时可刷新配置。</p>}
          {binding && <>
            <p>当前绑定：{binding.group_id ? boundGroup?.name ?? '代理组信息已变化，请刷新' : '未绑定代理组'}</p>
            <p role="status">{runtimeLabels[binding.runtime.configuration_state]} · 当前进程观察时间：{new Date(binding.runtime.observed_at).toLocaleString('zh-CN')}</p>
            <p>当前进程选择：{binding.runtime.selected_member_id ? boundGroup?.members.find(member => member.id === binding.runtime.selected_member_id)?.label ?? '出口信息已变化，请刷新' : '暂无本地选择'}</p>
            <p className="muted">仅反映处理本次请求的进程，不代表所有网关或 OAuth 进程；已加载不代表出口健康。可刷新查看新的观察结果。</p>
            <Field label="目标代理组"><Select value={groupId} onChange={event => { setGroupId(event.target.value); setInitialMember(''); }}>
              <option value="">请选择代理组</option>{groups.map(group => <option key={group.id} value={group.id}>{group.name}</option>)}
            </Select></Field>
            <Field label="起始出口"><Select value={initialMember} disabled={!selectedGroup} onChange={event => setInitialMember(event.target.value)}>
              <option value="">请选择起始出口</option>{selectedGroup?.members.map(member => <option key={member.id} value={member.id}>{member.label}</option>)}
            </Select></Field>
            <Button type="button" appearance="primary" disabled={!selectedGroup || !initialMember} onClick={() => void changeBinding(false)}>保存账号绑定</Button>
            {boundGroup && <>
              <Field label="解绑后保留的单代理出口"><Select value={singleMember} onChange={event => setSingleMember(event.target.value)}>
                <option value="">请选择保留出口</option>{boundGroup.members.map(member => <option key={member.id} value={member.id}>{member.label}</option>)}
              </Select></Field>
              <Button type="button" disabled={!singleMember} onClick={() => void changeBinding(true)}>解绑并保留所选代理</Button>
            </>}
          </>}
        </FormSection>
      </fieldset>
    </fieldset>
  </div>;
}
