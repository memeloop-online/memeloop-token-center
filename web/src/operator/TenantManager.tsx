import { useRef, useState } from 'react';
import { api } from '../api';
import { useI18n } from '../i18n';
import type { TenantManagementView } from '../types';
import { messageOf } from './scope/operatorShared';

interface Props {
  token: string;
  onChanged: () => Promise<void>;
}

const managementPath = '/internal/v1/tenant-management';

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

  async function rename(value: TenantManagementView) {
    const externalId = window.prompt(t('tenants.renamePrompt'), value.external_id)?.trim();
    if (!externalId || externalId === value.external_id) return;
    setBusy(`rename-${value.external_id}`); setMessage(''); setError('');
    try {
      const updated = await api<TenantManagementView>(`${managementPath}/${encodeURIComponent(value.external_id)}`, token, {
        method: 'PATCH', body: JSON.stringify({ external_id: externalId }),
      });
      setMessage(t('tenants.renamed', { tenant: updated.external_id }));
      await refresh();
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  async function archive(value: TenantManagementView, status: 'active' | 'archived') {
    const action = status === 'archived' ? 'archive' : 'restore';
    const confirmation = status === 'archived'
      ? t('tenants.confirmArchive', { tenant: value.external_id })
      : t('tenants.confirmRestore', { tenant: value.external_id });
    if (!window.confirm(confirmation)) return;
    setBusy(`${action}-${value.external_id}`); setMessage(''); setError('');
    try {
      const updated = await api<TenantManagementView>(`${managementPath}/${encodeURIComponent(value.external_id)}/${action}`, token, { method: 'POST' });
      setMessage(t(status === 'archived' ? 'tenants.archivedMessage' : 'tenants.restored', { tenant: updated.external_id }));
      await refresh();
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  async function remove(value: TenantManagementView) {
    if (!window.confirm(t('tenants.confirmDelete', { tenant: value.external_id }))) return;
    setBusy(`delete-${value.external_id}`); setMessage(''); setError('');
    try {
      await api<void>(`${managementPath}/${encodeURIComponent(value.external_id)}`, token, { method: 'DELETE' });
      setMessage(t('tenants.deleted', { tenant: value.external_id }));
      await refresh();
    } catch (reason) {
      setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      setBusy('');
    }
  }

  return <details className="panel tenant-manager" onToggle={(event) => {
    if ((event.currentTarget as HTMLDetailsElement).open) void load();
  }}>
    <summary><span><b>{t('tenants.title')}</b><small>{t('tenants.description')}</small></span><span aria-hidden="true">＋</span></summary>
    <div className="create-resource-body">
      {error && <div className="notice error" role="alert">{error}</div>}
      {message && <div className="notice success" role="status">{message}</div>}
      <div className="tenant-create-row">
        <label>{t('tenants.name')}<input value={name} maxLength={200} onChange={(event) => setName(event.target.value)} /></label>
        <button type="button" disabled={busy === 'create' || !name.trim()} onClick={() => void create()}>{t('tenants.create')}</button>
      </div>
      {!values ? <div className="empty">{t('common.loading')}</div> : <div className="account-list tenant-list">
        {values.length === 0 && <div className="empty">{t('tenants.empty')}</div>}
        {values.map((value) => {
          const isDefault = value.external_id === 'default';
          return <div className="managed-resource" key={value.external_id}>
            <div className="managed-resource-header"><div><b>{value.external_id}</b><span className={`status ${value.status === 'active' ? 'ok' : 'pending'}`}>{t(`tenants.${value.status}`)}</span></div></div>
            {isDefault
              ? <small className="tenant-default-note">{t('tenants.default')}</small>
              : <div className="row-actions">
                <button type="button" className="secondary" disabled={Boolean(busy)} onClick={() => void rename(value)}>{t('tenants.rename')}</button>
                {value.status === 'active'
                  ? <button type="button" className="secondary" disabled={Boolean(busy)} onClick={() => void archive(value, 'archived')}>{t('tenants.archive')}</button>
                  : <><button type="button" className="secondary" disabled={Boolean(busy)} onClick={() => void archive(value, 'active')}>{t('tenants.restore')}</button><button type="button" className="danger" disabled={Boolean(busy)} onClick={() => void remove(value)}>{t('tenants.delete')}</button></>}
              </div>}
          </div>;
        })}
      </div>}
    </div>
  </details>;
}
