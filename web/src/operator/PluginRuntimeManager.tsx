import { useCallback, useEffect, useRef, useState } from 'react';
import { ApiError, api } from '../api';
import { Button, Checkbox, Disclosure } from '../design-system';
import { useI18n } from '../i18n';
import type { PluginManifest } from '../types';
import { useConfirmDialog } from '../useConfirmDialog';
import { CreateJourney } from './CreateJourney';
import { pluginManagementCopy as copy } from './pluginManagementCopy';

interface Revision { revision: number; inventory_id: string; reason: string; created_at?: number }
interface Candidate { inventory_id: string; staged: boolean; plugins: Record<string, string[]> }
interface RuntimeStatus { current: Revision | null; candidates: Candidate[] }
interface Installation {
  id: string; inventory_id: string; actor: string; status: string; packages: string[]; base_revision?: number | null;
  review_digest: string | null; review: { plugins: PluginManifest[] } | null;
  failure_category: string | null; created_at: number; updated_at: number; completed_packages: number;
}
interface Audit { id: string; actor: string; action: string; inventory_id: string | null; revision: number | null; outcome: string; created_at: number }
interface History { runtime_enabled?: boolean; installation_enabled: boolean; installations: Installation[]; revisions: Revision[]; audit: Audit[] }


interface ReleaseResolution {
  id: string; base_revision: number; release_url: string; descriptor_sha256: string; source: string;
  descriptor: { reference: string; signer: { issuer: string; identity: string } };
}
type NetworkView = { mode: 'default' | 'direct' } | { mode: 'custom'; url: string; username?: string | null; has_password?: boolean } | { mode: 'group'; tenant_external_id: string; group_id: string; group_version: number; member_id: string };
interface ProxyGroup { id: string; name: string; version: number; members: { id: string; label: string; scheme: string; host: string; port: number }[] }

interface PluginRuntimeManagerProps { token: string; tenant: string; canManage: boolean; onPublished: () => Promise<void> }

export function PluginRuntimeManager(props: PluginRuntimeManagerProps) {
  // Credentials own all drafts, confirmations and pending operation identities.
  return <PluginRuntimeManagerSession key={props.token} {...props} />;
}

function PluginRuntimeManagerSession({ token, canManage, onPublished, tenant }: PluginRuntimeManagerProps) {
  const { locale } = useI18n();
  const text = locale.startsWith('zh') ? copy.zh : copy.en;
  const [status, setStatus] = useState<RuntimeStatus>();
  const [history, setHistory] = useState<History>();
  const [unavailable, setUnavailable] = useState(false);
  const [statusError, setStatusError] = useState('');
  const [historyError, setHistoryError] = useState('');
  const [error, setError] = useState('');
  const [message, setMessage] = useState('');
  const [busy, setBusy] = useState(false);
  const [inventory, setInventory] = useState('');
  const [packages, setPackages] = useState('');
  const [installOpen, setInstallOpen] = useState(false);
  const [activityOpen, setActivityOpen] = useState(false);
  const [releaseUrl, setReleaseUrl] = useState('');
  const [resolution, setResolution] = useState<ReleaseResolution>();
  const [trusted, setTrusted] = useState(false);
  const [network, setNetwork] = useState<NetworkView>({ mode: 'default' });
  const [networkReady, setNetworkReady] = useState(false);
  const [networkDirty, setNetworkDirty] = useState(false);
  const [networkError, setNetworkError] = useState('');
  const [password, setPassword] = useState('');
  const [clearPassword, setClearPassword] = useState(false);
  const [groups, setGroups] = useState<ProxyGroup[]>([]);
  const [groupsError, setGroupsError] = useState('');
  const [groupsLoading, setGroupsLoading] = useState(false);
  const resolveKey = useRef(crypto.randomUUID());
  const mutationLock = useRef(false);
  const networkEdited = useRef(false);
  const { confirm, confirmationDialog } = useConfirmDialog([token, canManage, status?.current?.revision]);
  const [approvals, setApprovals] = useState<Record<string, boolean>>({});
  const [reviews, setReviews] = useState<Record<string, Installation>>({});
  const generation = useRef(0);
  const alive = useRef(true);
  const keys = useRef(new Map<string, string>());

  useEffect(() => { alive.current = true; return () => { alive.current = false; generation.current++; }; }, []);
  const load = useCallback(async () => {
    const request = ++generation.current;
    setStatusError(''); setHistoryError('');
    const current = () => alive.current && request === generation.current;
    let statusLoaded = false;
    let historyLoaded = false;
    const statusRead = api<RuntimeStatus>('/internal/v1/plugin-runtime', token).then((nextStatus) => {
      if (!current()) return;
      statusLoaded = true; setStatus(nextStatus); setUnavailable(false);
    }).catch((reason) => {
      if (!current()) return;
      if (reason instanceof ApiError && [403, 404].includes(reason.status)) setUnavailable(true);
      else setStatusError(reason instanceof Error ? reason.message : 'Request failed');
    });
    const historyRead = api<History>('/internal/v1/plugin-runtime/history', token).then((nextHistory) => {
      if (current()) { historyLoaded = true; setHistory(nextHistory); }
    }).catch((reason) => {
      if (current()) setHistoryError(reason instanceof Error ? reason.message : 'Request failed');
    });
    await Promise.allSettled([statusRead, historyRead]);
    return current() && statusLoaded && historyLoaded;
  }, [token]);
  useEffect(() => { void load(); }, [load]);
  const installing = history?.installations.some((job) => job.status === 'installing') ?? false;
  useEffect(() => {
    if (!installing) return;
    const timer = window.setInterval(() => { void load(); }, 3000);
    return () => window.clearInterval(timer);
  }, [installing, load]);

  useEffect(() => {
    if (!canManage) return;
    let current = true;
    setNetworkReady(false);
    void api<NetworkView>('/internal/v1/plugin-runtime/installer-network', token).then((value) => {
      if (current) { if (!networkEdited.current) setNetwork(value); setNetworkReady(true); setNetworkError(''); }
    }).catch(() => { if (current) setNetworkError(text.networkFailed); });
    return () => { current = false; };
  }, [token, canManage, text.networkFailed]);
  useEffect(() => {
    setGroups([]); setGroupsError(''); setGroupsLoading(false);
    if (!canManage || !tenant || network.mode !== 'group') return;
    let current = true; setGroupsLoading(true);
    void api<{ items: ProxyGroup[] }>(`/internal/v1/plugin-runtime/installer-network/groups?tenant_external_id=${encodeURIComponent(tenant)}`, token).then((value) => {
      if (current) setGroups(value.items);
    }).catch(() => { if (current) setGroupsError(text.groupsFailed); }).finally(() => { if (current) setGroupsLoading(false); });
    return () => { current = false; };
  }, [token, tenant, canManage, network.mode, text.groupsFailed]);

  function editNetwork(value: NetworkView) {
    networkEdited.current = true;
    setNetwork(value); setNetworkDirty(true); setResolution(undefined); setTrusted(false);
    resolveKey.current = crypto.randomUUID();
  }
  const selectedGroup = network.mode === 'group' && network.tenant_external_id === tenant ? groups.find((group) => group.id === network.group_id) : undefined;
  const networkValid = network.mode === 'custom' ? Boolean(network.url.trim())
    : network.mode !== 'group' || Boolean(selectedGroup && selectedGroup.version === network.group_version && selectedGroup.members.some((member) => member.id === network.member_id));

  async function persistNetwork() {
    const input = network.mode === 'custom'
      ? { mode: network.mode, url: network.url.trim(), username: network.username || null, ...(clearPassword ? { password: '' } : password ? { password } : {}) }
      : network;
    const saved = await api<NetworkView>('/internal/v1/plugin-runtime/installer-network', token, { method: 'PUT', body: JSON.stringify(input) });
    if (alive.current) { setNetwork(saved); setNetworkDirty(false); setNetworkReady(true); setNetworkError(''); setPassword(''); setClearPassword(false); }
  }
  async function saveNetwork() {
    if (mutationLock.current || !canManage || !networkValid) return;
    mutationLock.current = true; setBusy(true); setMessage(''); setNetworkError('');
    try { await persistNetwork(); if (alive.current) setMessage(text.networkSaved); }
    catch { if (alive.current) setNetworkError(text.saveFailed); }
    finally { mutationLock.current = false; if (alive.current) setBusy(false); }
  }
  async function resolveRelease() {
    if (mutationLock.current || !canManage || !networkValid || (!networkReady && !networkDirty)) return;
    mutationLock.current = true; setBusy(true); setError(''); setMessage(''); setResolution(undefined); setTrusted(false);
    try {
      if (networkDirty) await persistNetwork();
      if (!alive.current) return;
      const value = await api<ReleaseResolution>('/internal/v1/plugin-runtime/release-resolutions', token, {
        method: 'POST', headers: { 'Idempotency-Key': resolveKey.current }, body: JSON.stringify({ url: releaseUrl.trim() }),
      });
      if (alive.current) setResolution(value);
    } catch { if (alive.current) setError(text.readFailed); }
    finally { mutationLock.current = false; if (alive.current) setBusy(false); }
  }
  async function installOci() {
    if (mutationLock.current || !canManage || !networkValid || (!networkReady && !networkDirty)) return;
    mutationLock.current = true; setBusy(true); setError('');
    try {
      if (networkDirty) await persistNetwork();
    } catch {
      if (alive.current) { setNetworkError(text.saveFailed); setBusy(false); }
      mutationLock.current = false;
      return;
    }
    mutationLock.current = false;
    if (!alive.current) return;
    await mutate('/internal/v1/plugin-runtime/installations', { inventory_id: inventory.trim(), packages: packages.split('\n').map((line) => line.trim()).filter(Boolean) });
  }
  async function uninstall(pluginId: string) {
    const revision = status?.current?.revision;
    if (revision === undefined || busy || !canManage || !await confirm(text.uninstallConfirm(pluginId))) return;
    await mutate('/internal/v1/plugin-runtime/uninstall', { plugin_id: pluginId, expected_revision: revision }, true);
  }
  function publicationRevision(candidate: Candidate): number | undefined {
    if (!history || historyError || !status || statusError) return undefined;
    const job = history.installations.find((item) => item.inventory_id === candidate.inventory_id);
    // URL installations must keep the revision frozen when their release was read.
    // An absent record/field is not permission to rebase a generated plugin set.
    if ((job && job.base_revision === undefined) || (!job && candidate.inventory_id.startsWith('install-'))) return undefined;
    return job?.base_revision ?? status.current?.revision ?? 0;
  }
  async function activate(candidate: Candidate) {
    const revision = publicationRevision(candidate);
    if (revision === undefined || busy || !canManage || !await confirm(text.activateConfirm(candidate.inventory_id))) return;
    await mutate('/internal/v1/plugin-runtime/publish', { inventory_id: candidate.inventory_id, expected_revision: revision }, true);
  }
  async function rollback(revision: Revision) {
    if (busy || !canManage || !await confirm(text.rollbackConfirm(revision.revision))) return;
    await mutate('/internal/v1/plugin-runtime/rollback', { target_revision: revision.revision, expected_revision: status?.current?.revision }, true);
  }

  async function mutate(path: string, body: unknown, activates = false) {
    if (mutationLock.current || !canManage) return;
    mutationLock.current = true; setBusy(true); setError(''); setMessage('');
    const encoded = JSON.stringify(body);
    const identity = `${path}\0${encoded}`;
    const key = keys.current.get(identity) ?? crypto.randomUUID();
    keys.current.set(identity, key);
    try {
      await api(path, token, { method: 'POST', body: encoded, headers: { 'Idempotency-Key': key } });
      if (!alive.current) return;
      setApprovals({});
      if (path === '/internal/v1/plugin-runtime/installations') {
        setResolution(undefined); setTrusted(false); setInstallOpen(false); setActivityOpen(true); resolveKey.current = crypto.randomUUID();
      }
      const refreshed = await load();
      if (!alive.current) return;
      const catalog = activates ? await Promise.allSettled([onPublished()]) : [];
      if (alive.current) setMessage(refreshed && catalog.every((result) => result.status === 'fulfilled') ? text.done : text.refreshPending);
    } catch (reason) {
      if (alive.current) {
        const stale = reason instanceof ApiError && [404, 409].includes(reason.status);
        if (stale) { keys.current.delete(identity); setResolution(undefined); setTrusted(false); resolveKey.current = crypto.randomUUID(); }
        setError(stale ? text.changed : `${text.failed} ${reason instanceof Error ? reason.message : ''}`); await load();
      }
    } finally { mutationLock.current = false; if (alive.current) setBusy(false); }
  }

  async function older() {
    const last = history?.revisions.at(-1);
    if (!last || busy) return;
    setBusy(true);
    try {
      const page = await api<History>(`/internal/v1/plugin-runtime/history?before_revision=${last.revision}`, token);
      if (alive.current) setHistory((current) => current ? { ...current, revisions: [...current.revisions, ...page.revisions.filter((item) => !current.revisions.some((old) => old.revision === item.revision))] } : page);
    } catch (reason) { if (alive.current) setError(reason instanceof Error ? reason.message : text.failed); }
    finally { if (alive.current) setBusy(false); }
  }

  async function olderAudit() {
    const last = history?.audit.at(-1);
    if (!last || busy) return;
    setBusy(true);
    try {
      const page = await api<History>(`/internal/v1/plugin-runtime/history?before_audit_id=${encodeURIComponent(last.id)}`, token);
      if (alive.current) setHistory((current) => current ? { ...current, audit: [...current.audit, ...page.audit.filter((item) => !current.audit.some((old) => old.id === item.id))] } : page);
    } catch (reason) { if (alive.current) setError(reason instanceof Error ? reason.message : text.failed); }
    finally { if (alive.current) setBusy(false); }
  }

  async function review(job: Installation) {
    try {
      const record = await api<Installation>(`/internal/v1/plugin-runtime/installations/${encodeURIComponent(job.id)}`, token);
      if (alive.current) { setReviews((old) => ({ ...old, [job.id]: record })); setApprovals((old) => ({ ...old, [job.id]: false })); }
    } catch (reason) { if (alive.current) setError(reason instanceof Error ? reason.message : text.failed); }
  }

  const activeSet = status?.current ? status.candidates.find((candidate) => candidate.inventory_id === status.current?.inventory_id) : undefined;
  const activePlugins = Object.entries(activeSet?.plugins ?? {});
  return <article className="panel" aria-label={text.title}>
    {confirmationDialog}
    <div className="panel-title"><div><h2>{text.title}</h2><p className="muted">{text.global}</p></div><Button appearance="secondary" disabled={busy} onClick={() => void load()}>{text.refresh}</Button></div>
    {error && <p className="notice error" role="alert">{error}</p>}
    {message && <p className="notice" role="status">{message}</p>}
    {unavailable ? <p className="muted">{text.unavailable}</p> : <>
      {statusError && <p className="notice error" role="alert">{statusError}</p>}
      {!status && !statusError && <p role="status">{text.loading}</p>}
      {status && <section aria-label={text.active}>
        <h3>{text.active}</h3>
        <p className="muted">{text.current}: {status.current ? status.current.revision : text.baseline}</p>
        {!activeSet ? <p className="notice">{!status.current && status.candidates.length === 0 ? text.noActive : text.missingActive}</p> : activePlugins.length === 0 ? <p className="empty">{text.noActive}</p> : <div className="plugin-runtime-list">{activePlugins.map(([id, versions]) => <div className="managed-resource" key={id}>
          <div className="managed-resource-header"><div><b>{id}</b><span>{versions.join(', ')}</span></div><Button appearance="secondary" disabled={busy || !canManage || Boolean(statusError)} onClick={() => void uninstall(id)}>{text.uninstall}</Button></div>
        </div>)}</div>}
      </section>}
      {historyError && <p className="notice error" role="alert">{historyError}</p>}
      {!history && !historyError && <p role="status">{text.loading}</p>}
      {history && (!history.installation_enabled ? <p className="notice">{text.disabled}</p> : canManage && <CreateJourney title={text.add} description={text.addHint} open={installOpen} onOpenChange={setInstallOpen} busy={busy}>
        <form className="plugin-runtime-install-form" onSubmit={(event) => { event.preventDefault(); void resolveRelease(); }}>
          <label>{text.releaseUrl}<input type="url" required value={releaseUrl} disabled={busy} placeholder="https://github.com/owner/repo/releases/download/v1/plugin.json" onChange={(event) => { setReleaseUrl(event.target.value); setResolution(undefined); setTrusted(false); resolveKey.current = crypto.randomUUID(); }} /></label>
          <p className="muted">{text.network}: {network.mode === 'default' ? text.defaultNetwork : network.mode === 'direct' ? text.direct : network.mode === 'group' ? text.group : text.custom}</p>
          {!networkValid && <p className="notice">{text.chooseNetwork}</p>}
          <Disclosure title={text.network}>
            <div className="plugin-runtime-install-form">
              <p className="muted">{text.networkHint}</p>
              {networkError && <p className="notice error" role="alert">{networkError}</p>}
              <label>{text.network}<select value={network.mode} disabled={busy} onChange={(event) => {
                setPassword(''); setClearPassword(false);
                const mode = event.target.value;
                editNetwork(mode === 'custom' ? { mode, url: '', username: '' } : mode === 'group' ? { mode, tenant_external_id: tenant, group_id: '', group_version: 0, member_id: '' } : { mode: mode === 'direct' ? 'direct' : 'default' });
              }}><option value="default">{text.defaultNetwork}</option><option value="direct">{text.direct}</option><option value="group">{text.group}</option><option value="custom">{text.custom}</option></select></label>
              {network.mode === 'group' && <>
                {!tenant && <p className="notice">{text.needTenant}</p>}
                {groupsError && <p className="notice error" role="alert">{groupsError}</p>}
                {groupsLoading && <p role="status">{text.loading}</p>}
                {tenant && !groupsLoading && !groupsError && groups.length === 0 && <p className="muted">{text.noGroups}</p>}
                <label>{text.groupLabel}<select disabled={busy || !tenant} value={selectedGroup?.id ?? ''} onChange={(event) => {
                  const group = groups.find((item) => item.id === event.target.value);
                  editNetwork({ mode: 'group', tenant_external_id: tenant, group_id: group?.id ?? '', group_version: group?.version ?? 0, member_id: group?.members[0]?.id ?? '' });
                }}><option value="">{text.select}</option>{groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</select></label>
                <label>{text.member}<select disabled={busy || !selectedGroup} value={selectedGroup ? network.member_id : ''} onChange={(event) => editNetwork({ ...network, member_id: event.target.value })}><option value="">{text.select}</option>{selectedGroup?.members.map((member) => <option key={member.id} value={member.id}>{member.label || `${member.scheme}://${member.host}:${member.port}`}</option>)}</select></label>
              </>}
              {network.mode === 'custom' && <>
                <label>{text.proxyUrl}<input type="url" disabled={busy} value={network.url} placeholder="http://proxy.example:8080" onChange={(event) => editNetwork({ ...network, url: event.target.value, has_password: false })} /></label><p className="muted">{text.proxyHint}</p>
                <label>{text.username}<input autoComplete="off" disabled={busy} value={network.username ?? ''} onChange={(event) => editNetwork({ ...network, username: event.target.value, has_password: false })} /></label>
                <label>{text.password}<input type="password" autoComplete="new-password" disabled={busy || clearPassword} value={password} onChange={(event) => { setPassword(event.target.value); editNetwork(network); }} /></label>
                {network.has_password && <p className="muted">{text.passwordKept}</p>}
                <Checkbox label={text.clearPassword} disabled={busy} checked={clearPassword} onChange={(_, data) => { setClearPassword(data.checked === true); setPassword(''); editNetwork(network); }} />
              </>}
              <Button type="button" appearance="secondary" disabled={busy || !networkValid} onClick={() => void saveNetwork()}>{text.saveNetwork}</Button>
            </div>
          </Disclosure>
          <Button appearance="primary" type="submit" disabled={busy || !networkValid || (!networkReady && !networkDirty)}>{busy ? text.pending : text.resolve}</Button>
          {networkError && <p className="notice error" role="alert">{networkError}</p>}
        </form>
        {resolution && <section className="plugin-runtime-section" aria-label={text.review}>
          <dl className="plugin-release-details"><dt>{text.source}</dt><dd>{resolution.source}</dd><dt>{text.releaseUrl}</dt><dd>{resolution.release_url}</dd><dt>{text.reference}</dt><dd>{resolution.descriptor.reference}</dd><dt>{text.issuer}</dt><dd>{resolution.descriptor.signer.issuer}</dd><dt>{text.identity}</dt><dd>{resolution.descriptor.signer.identity}</dd><dt>{text.digest}</dt><dd>{resolution.descriptor_sha256}</dd></dl>
          <p className="muted">{text.trustHint}</p><p>{text.scope}</p>
          <Checkbox label={text.trust} checked={trusted} disabled={busy} onChange={(_, data) => setTrusted(data.checked === true)} />
          <Button appearance="primary" disabled={busy || !trusted} onClick={() => void mutate('/internal/v1/plugin-runtime/installations', { release_resolution_id: resolution.id, release_descriptor_sha256: resolution.descriptor_sha256, packages: [] })}>{text.install}</Button>
        </section>}
        <Disclosure title={text.advanced}><p className="muted">{text.advancedHint}</p><form className="plugin-runtime-install-form" onSubmit={(event) => { event.preventDefault(); void installOci(); }}>
          <label>{text.inventory}<input required pattern="[A-Za-z0-9_-]{1,64}" value={inventory} onChange={(event) => setInventory(event.target.value)} disabled={busy} /></label>
          <label>{text.packages}<textarea required rows={3} value={packages} onChange={(event) => setPackages(event.target.value)} disabled={busy} placeholder="ghcr.io/example/plugin@sha256:…" /></label>
          <Button disabled={busy || !networkValid || (!networkReady && !networkDirty)} type="submit">{busy ? text.pending : text.install}</Button>
        </form></Disclosure>
      </CreateJourney>)}
      <div className="plugin-runtime-disclosures">
        <Disclosure title={text.inventoryFlow}><div className="plugin-runtime-section">
          {status && <><h3>{text.candidates}</h3>
            {status.candidates.length === 0 && <p className="empty">{text.empty}</p>}
            <div className="plugin-runtime-list">{status.candidates.map((candidate) => <div className="managed-resource" key={candidate.inventory_id}>
              <div className="plugin-runtime-resource-heading"><b>{candidate.inventory_id}</b><span className="pill">{candidate.staged ? text.staged : text.unstaged}</span></div>
              <p>{Object.entries(candidate.plugins).map(([id, versions]) => `${id}: ${versions.join(', ')}`).join(' · ') || text.noActive}</p>
              {publicationRevision(candidate) === undefined && candidate.inventory_id !== status.current?.inventory_id && <p className="notice">{text.missingBase}</p>}
              <div className="plugin-runtime-actions"><Button appearance="secondary" disabled={busy || !canManage} onClick={() => void mutate('/internal/v1/plugin-runtime/candidates', { inventory_id: candidate.inventory_id })}>{text.stage}</Button>
              <Button appearance="primary" disabled={busy || !canManage || publicationRevision(candidate) === undefined || candidate.inventory_id === status.current?.inventory_id} onClick={() => void activate(candidate)}>{text.publish}</Button></div>
            </div>)}</div>
          </>}
        </div></Disclosure>
        <Disclosure title={text.activity} open={activityOpen} onOpenChange={setActivityOpen}><div className="plugin-runtime-section">
          {!history && !historyError && <p role="status">{text.history}: {text.loading}</p>}
          {history && <>
            <section><h3>{text.tasks}</h3>
            {history.installations.length === 0 && <p className="muted">{text.empty}</p>}
            <div className="plugin-runtime-list">{history.installations.map((job) => <section className="managed-resource" key={job.id}>
              <div className="plugin-runtime-resource-heading"><b>{job.inventory_id}</b><span className="pill">{text.statusLabel(job.status)}</span></div><p className="muted">{job.completed_packages ?? 0}/{job.packages.length} · {new Date(job.updated_at).toLocaleString(locale)} · {job.actor}</p>
              {job.failure_category && <p role="alert">{job.failure_category}</p>}
              {job.review_digest && <button type="button" className="secondary" onClick={() => void review(job)}>{text.review}</button>}
              {reviews[job.id]?.review && <div className="plugin-runtime-section">
                {reviews[job.id].review.plugins.map((plugin) => <section key={plugin.id}>
                  <h4>{plugin.id} · {plugin.version}</h4>
                  <ul>{plugin.capabilities.map((capability, index) => <li key={index}>{capability.kind === 'http' ? `${text.httpAccess}: ${capability.allowed_origins.join(', ')}` : capability.kind === 'log' ? text.logAccess : capability.kind === 'kv' ? text.storageAccess : text.quotaAccess}</li>)}
                    {plugin.contributions.traffic_policy && <li>{text.trafficPolicy}</li>}
                    {plugin.contributions.request_rewrite && <li>{text.requestRewrite}</li>}
                    {plugin.contributions.configuration && <li>{text.configurationAccess}</li>}
                    {(plugin.contributions.providers ?? []).map((provider) => <li key={provider.id}>{text.providerAccess}: {provider.display_name || provider.id}</li>)}
                    {(plugin.contributions.operator_ui ?? []).map((contribution) => <li key={contribution.id}>{text.interfaceAccess}: {contribution.label || contribution.id}</li>)}
                    {(plugin.contributions.service_data ?? []).map((endpoint) => <li key={endpoint.id}>{text.dataAccess}: {endpoint.url || endpoint.id} · {endpoint.required_scope}</li>)}
                  </ul>
                  {plugin.capabilities.length === 0 && Object.keys(plugin.contributions).length === 0 && <p>{text.noPermissions}</p>}
                </section>)}
                <Disclosure title={text.technicalDetails}><pre className="plugin-manifest-details">{JSON.stringify(reviews[job.id].review, null, 2)}</pre></Disclosure>
              </div>}
              {canManage && job.status === 'review' && job.review_digest && <><p>{text.warning}</p><Checkbox label={text.approve} disabled={!reviews[job.id]?.review || reviews[job.id].review_digest !== job.review_digest} checked={approvals[job.id] === true} onChange={(_, data) => setApprovals((old) => ({ ...old, [job.id]: data.checked === true }))} /><button type="button" disabled={busy || !approvals[job.id] || reviews[job.id]?.review_digest !== job.review_digest} onClick={() => void mutate(`/internal/v1/plugin-runtime/installations/${encodeURIComponent(job.id)}/approve`, { review_digest: reviews[job.id].review_digest })}>{text.approve}</button></>}
              {canManage && job.status === 'registered' && status?.candidates.filter((candidate) => candidate.inventory_id === job.inventory_id).map((candidate) => <Button key={candidate.inventory_id} appearance="primary" disabled={busy || publicationRevision(candidate) === undefined || candidate.inventory_id === status.current?.inventory_id} onClick={() => void activate(candidate)}>{text.publish}</Button>)}
              {canManage && ['failed', 'interrupted'].includes(job.status) && <button type="button" disabled={busy} onClick={() => void mutate(`/internal/v1/plugin-runtime/installations/${encodeURIComponent(job.id)}/retry`, {})}>{text.retry}</button>}
            </section>)}</div></section>
            <section><h3>{text.history}</h3>
            {history.revisions.length === 0 && <p className="muted">{text.empty}</p>}
            <div className="plugin-runtime-list">{history.revisions.map((revision) => <div className="managed-resource" key={revision.revision}>
              <b>{revision.revision} · {revision.inventory_id}</b> <span>{revision.reason}</span>
              <button type="button" className="secondary" disabled={busy || !canManage || Boolean(statusError) || !status?.current || revision.revision >= status.current.revision || revision.inventory_id === status.current.inventory_id} onClick={() => void rollback(revision)}>{text.rollback}</button>
            </div>)}</div>
            {history.revisions.length > 0 && <button type="button" className="secondary" disabled={busy} onClick={() => void older()}>{text.older}</button>}</section>
            <section><h3>{text.audit}</h3>
            {history.audit.length === 0 ? <p className="muted">{text.empty}</p> : <><ul>{history.audit.map((entry) => <li key={entry.id}>{new Date(entry.created_at).toLocaleString(locale)} · {entry.actor} · {entry.action} · {entry.inventory_id ?? '—'} · {entry.revision ?? '—'} · {entry.outcome}</li>)}</ul><button type="button" className="secondary" disabled={busy} onClick={() => void olderAudit()}>{text.olderAudit}</button></>}</section>
          </>}
        </div></Disclosure>
      </div>
    </>}
  </article>;
}
