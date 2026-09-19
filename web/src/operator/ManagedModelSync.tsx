import { useLayoutEffect, useRef, useState } from 'react';
import { api } from '../api';
import { Button } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { ManagedModelSyncResponse } from '../types';
import { ManagedSyncResponseError, managedSyncTone, parseManagedModelSync } from './managedModelSync';

interface SyncState {
  scope: string;
  phase: 'idle' | 'syncing' | 'done' | 'failed';
  result?: ManagedModelSyncResponse;
  message?: string;
}

/** First-screen per-account sync action: refreshes the complete catalog and reconciles managed routes. */
export function ManagedModelSync({ accountId, tenant, token, disabled = false, onReconciled }: {
  accountId: string;
  tenant: string;
  token: string;
  disabled?: boolean;
  onReconciled?: () => void;
}) {
  const { locale, t } = useI18n();
  const scope = JSON.stringify([accountId, tenant, token]);
  const [state, setState] = useState<SyncState>({ scope, phase: 'idle' });
  const request = useRef<AbortController | undefined>(undefined);
  useLayoutEffect(() => {
    if (state.scope === scope) return;
    request.current?.abort();
    setState({ scope, phase: 'idle' });
  }, [scope, state.scope]);
  useLayoutEffect(() => () => { request.current?.abort(); }, []);

  const visible = state.scope === scope ? state : { scope, phase: 'idle' } as SyncState;
  const syncing = visible.phase === 'syncing';
  const result = visible.phase === 'done' ? visible.result : undefined;
  const tone = result ? managedSyncTone(result) : 'success';
  const warningText = (code: string) => {
    const key = `managedSync.warning.${code}`;
    const translated = t(key);
    if (translated !== key) return translated;
    const catalogKey = `providerCatalog.error.${code}`;
    const catalogTranslated = t(catalogKey);
    return catalogTranslated === catalogKey ? code : catalogTranslated;
  };

  async function sync() {
    if (disabled || syncing || !token || !tenant) return;
    request.current?.abort();
    const controller = new AbortController();
    request.current = controller;
    setState({ scope, phase: 'syncing' });
    try {
      const query = new URLSearchParams({ tenant_external_id: tenant });
      const synced = parseManagedModelSync(await api<unknown>(`/internal/v1/upstreams/${accountId}/models/sync-routes?${query}`, token, { method: 'POST', signal: controller.signal }));
      if (controller.signal.aborted) return;
      setState({ scope, phase: 'done', result: synced });
      onReconciled?.();
    } catch (reason) {
      if (controller.signal.aborted) return;
      const message = reason instanceof ManagedSyncResponseError
        ? t('providerCatalog.error.invalid_response')
        : reason instanceof Error ? reason.message : t('managedSync.failed');
      setState({ scope, phase: 'failed', message });
    }
  }

  return <div className="managed-model-sync" aria-busy={syncing}>
    <div className="managed-model-sync-row">
      <Button appearance="primary" type="button" disabled={disabled || syncing || !token || !tenant} onClick={() => void sync()}>{t('managedSync.sync')}</Button>
      <span className="field-hint">{t('managedSync.hint')}</span>
    </div>
    {syncing && <p className="muted" role="status">{t('managedSync.syncing')}</p>}
    {result && <div className="managed-model-sync-result">
      <p role="status"><span className={`status ${tone === 'success' ? 'ok' : 'pending'}`}>{t(tone === 'success' ? 'managedSync.done' : 'managedSync.partial')}</span></p>
      <p>{t('managedSync.summary', {
        count: formatNumber(result.catalog.models.length, locale),
        added: formatNumber(result.routes.added, locale),
        disabled: formatNumber(result.routes.disabled, locale),
        restored: formatNumber(result.routes.restored, locale),
        unchanged: formatNumber(result.routes.unchanged, locale),
        skipped: formatNumber(result.routes.skipped, locale),
      })}</p>
      <p className="muted">{t('managedSync.pricePending')}</p>
      {result.routes.warnings.length > 0 && <ul className="managed-model-sync-warnings">
        {result.routes.warnings.map((warning) => <li key={warning}>{warningText(warning)}</li>)}
      </ul>}
    </div>}
    {visible.phase === 'failed' && <p className="error" role="alert">{visible.message}</p>}
  </div>;
}
