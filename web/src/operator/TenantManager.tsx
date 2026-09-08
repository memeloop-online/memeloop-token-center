import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { DrawerFrame } from '../components';
import { useI18n } from '../i18n';
import type { TenantManagementView } from '../types';
import { ResourceListStatusEmpty, ResourceListStatusFilterControl, useResourceListStatusFilter } from './ResourceListStatusFilter';
import { messageOf } from './scope/operatorShared';

interface Props {
  token: string;
  onChanged: () => Promise<void>;
}

const managementPath = '/internal/v1/tenant-management';

type TenantDialog = {
  kind: 'rename' | 'archive' | 'restore' | 'delete';
  tenant: TenantManagementView;
};

/**
 * Tenant lifecycle is intentionally separate from client-credential
 * management: callers create a tenant first, then place credentials and
 * routing resources inside it. Archiving preserves history; deletion is
 * rejected while the tenant still owns data.
 */
export function TenantManager({ token, onChanged }: Props) {
  const { t } = useI18n();
  const [values, setValues] = useState<TenantManagementView[]>();
  const [name, setName] = useState('');
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const [dialog, setDialog] = useState<TenantDialog>();
  const [renameDraft, setRenameDraft] = useState('');
  const loadedFor = useRef('');
  const loadSequence = useRef(0);
  const tokenRef = useRef(token);
  tokenRef.current = token;

  async function load(force = false) {
    if (!token || (!force && loadedFor.current === token)) return;
    const request = ++loadSequence.current;
    const loadToken = token;
    try {
      const next = await api<TenantManagementView[]>(managementPath, loadToken);
      if (request !== loadSequence.current || tokenRef.current !== loadToken) return;
      loadedFor.current = loadToken;
      setValues(next);
      setError('');
    } catch (reason) {
      if (request !== loadSequence.current || tokenRef.current !== loadToken) return;
      setError(messageOf(reason, t('tenants.loadFailed')));
    }
  }

  async function refresh() {
    loadedFor.current = '';
    loadSequence.current += 1;
    await Promise.all([load(true), onChanged()]);
  }

  async function create() {
    const externalId = name.trim();
    if (!externalId) return;
    setBusy('create'); setMessage(''); setError('');
    try {
      const created = await api<TenantManagementView>(managementPath, token, {
        method: 'POST',
        headers: { 'Idempotency-Key': crypto.randomUUID() },
        body: JSON.stringify({ external_id: externalId }),
      });
      setName('');
      setMessage(t('tenants.created', { tenant: created.external_id }));
      await refresh();
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  function openDialog(kind: TenantDialog['kind'], tenant: TenantManagementView) {
    setError('');
    setMessage('');
    if (kind === 'rename') setRenameDraft(tenant.external_id);
    setDialog({ kind, tenant });
  }

  function closeDialog() {
    if (!busy) setDialog(undefined);
  }

  async function rename(value: TenantManagementView, nextExternalId: string) {
    const externalId = nextExternalId.trim();
    if (!externalId || externalId === value.external_id) return;
    setBusy(`rename-${value.external_id}`); setMessage(''); setError('');
    try {
      const updated = await api<TenantManagementView>(`${managementPath}/${encodeURIComponent(value.external_id)}`, token, {
        method: 'PATCH', body: JSON.stringify({ external_id: externalId }),
      });
      setMessage(t('tenants.renamed', { tenant: updated.external_id }));
      await refresh();
      setDialog(undefined);
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  async function archive(value: TenantManagementView, status: 'active' | 'archived') {
    const action = status === 'archived' ? 'archive' : 'restore';
    setBusy(`${action}-${value.external_id}`); setMessage(''); setError('');
    try {
      const updated = await api<TenantManagementView>(`${managementPath}/${encodeURIComponent(value.external_id)}/${action}`, token, { method: 'POST' });
      setMessage(t(status === 'archived' ? 'tenants.archivedMessage' : 'tenants.restored', { tenant: updated.external_id }));
      await refresh();
      setDialog(undefined);
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  async function remove(value: TenantManagementView) {
    setBusy(`delete-${value.external_id}`); setMessage(''); setError('');
    try {
      await api<void>(`${managementPath}/${encodeURIComponent(value.external_id)}`, token, { method: 'DELETE' });
      setMessage(t('tenants.deleted', { tenant: value.external_id }));
      await refresh();
      setDialog(undefined);
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  useEffect(() => { void load(); }, [token]);

  const statusFilter = useResourceListStatusFilter('tenants', '', values ?? [], (value) => value.status === 'active');

  function submitDialog() {
    if (!dialog || busy) return;
    if (dialog.kind === 'rename') { void rename(dialog.tenant, renameDraft); return; }
    if (dialog.kind === 'archive') { void archive(dialog.tenant, 'archived'); return; }
    if (dialog.kind === 'restore') { void archive(dialog.tenant, 'active'); return; }
    void remove(dialog.tenant);
  }

  const dialogTitle = dialog?.kind === 'rename'
    ? t('tenants.renameTitle')
    : dialog?.kind === 'archive'
      ? t('tenants.archiveTitle')
      : dialog?.kind === 'restore'
        ? t('tenants.restoreTitle')
        : t('tenants.deleteTitle');
  const dialogImpact = dialog?.kind === 'rename'
    ? t('tenants.renameImpact')
    : dialog?.kind === 'archive'
      ? t('tenants.archiveImpact')
      : dialog?.kind === 'restore'
        ? t('tenants.restoreImpact')
        : t('tenants.deleteImpact');
  const dialogAction = dialog?.kind === 'rename'
    ? t('tenants.rename')
    : dialog?.kind === 'archive'
      ? t('tenants.archive')
      : dialog?.kind === 'restore'
        ? t('tenants.restore')
        : t('tenants.delete');

  return <section className="panel tenant-manager">
    <div className="panel-title tenant-manager-title"><div><h2>{t('tenants.title')}</h2><p className="muted">{t('tenants.description')}</p></div>{values && <ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('tenants.archived')} />}</div>
    <div className="tenant-manager-body">
      {!dialog && error && <div className="notice error" role="alert">{error}</div>}
      {message && <div className="notice success" role="status">{message}</div>}
      <div className="tenant-create-row">
        <label>{t('tenants.name')}<input value={name} maxLength={200} onChange={(event) => setName(event.target.value)} /></label>
        <button type="button" disabled={busy === 'create' || !name.trim()} onClick={() => void create()}>{t('tenants.create')}</button>
      </div>
      {!values ? <div className="empty">{t('common.loading')}</div> : <div className="account-list tenant-list">
        {statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('tenants.active')} empty={t('tenants.empty')} />}
        {statusFilter.values.map((value) => {
          const isDefault = value.external_id === 'default';
          return <div className="managed-resource" key={value.external_id}>
            <div className="managed-resource-header"><div><b>{value.external_id}</b><span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{t(`tenants.${value.status}`)}</span></div></div>
            {!isDefault && <div className="row-actions">
                <button type="button" className="secondary" disabled={Boolean(busy)} onClick={() => openDialog('rename', value)}>{t('tenants.rename')}</button>
                {value.status === 'active'
                  ? <button type="button" className="secondary" disabled={Boolean(busy)} onClick={() => openDialog('archive', value)}>{t('tenants.archive')}</button>
                  : <><button type="button" className="secondary" disabled={Boolean(busy)} onClick={() => openDialog('restore', value)}>{t('tenants.restore')}</button><button type="button" className="danger" disabled={Boolean(busy)} onClick={() => openDialog('delete', value)}>{t('tenants.delete')}</button></>}
              </div>}
          </div>;
        })}
      </div>}
    </div>
    {dialog && <DrawerFrame title={dialogTitle} eyebrow={t('tenants.title')} onClose={closeDialog}>
      <p className="tenant-dialog-object"><code>{dialog.tenant.external_id}</code></p>
      <p className="tenant-dialog-impact">{dialogImpact}</p>
      {dialog.kind === 'rename' && <label className="tenant-dialog-input">{t('tenants.name')}<input autoFocus value={renameDraft} maxLength={200} onChange={(event) => setRenameDraft(event.target.value)} /></label>}
      {error && <div className="notice error" role="alert">{error}</div>}
      <div className="button-row tenant-dialog-actions"><button type="button" className="secondary" disabled={Boolean(busy)} onClick={closeDialog}>{t('common.cancel')}</button><button type="button" className={dialog.kind === 'delete' ? 'danger' : ''} disabled={Boolean(busy) || (dialog.kind === 'rename' && !renameDraft.trim())} onClick={submitDialog}>{busy ? t('common.loading') : dialogAction}</button></div>
    </DrawerFrame>}
  </section>;
}
