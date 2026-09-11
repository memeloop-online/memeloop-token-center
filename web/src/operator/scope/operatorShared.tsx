import { useState } from 'react';
import { useI18n } from '../../i18n';

export type Translate = (key: string, variables?: Record<string, string | number>) => string;

export function queryForTenant(tenant: string, existing = '') {
  const params = new URLSearchParams(existing);
  if (tenant) params.set('tenant_external_id', tenant);
  const query = params.toString();
  return query ? `?${query}` : '';
}

export function messageOf(reason: unknown, fallback: string) {
  return reason instanceof Error ? reason.message : fallback;
}

export function enumLabel(t: Translate, prefix: string, value: string) {
  const key = `${prefix}.${value}`;
  const translated = t(key);
  return translated === key ? value : translated;
}

export function WriteScopeNotice({ tenant }: { tenant: string }) {
  const { t } = useI18n();
  if (tenant) return null;
  return <div className="scope-context"><span aria-hidden="true">◎</span><p>{t('operator.selectTenantToWrite')}</p></div>;
}

export function OneTimeSecret({ value, message, filename = 'token-center-credential.txt', onDismiss }: {
  value: string;
  message: string;
  filename?: string;
  onDismiss?: () => void;
}) {
  const { t } = useI18n();
  const [copyState, setCopyState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const [saved, setSaved] = useState(false);

  async function copySecret() {
    try {
      if (navigator.clipboard?.writeText) {
        await navigator.clipboard.writeText(value);
      } else {
        // Clipboard API is unavailable in some embedded or non-secure browser
        // contexts. Keep the fallback entirely local; the server is never
        // queried for a previous secret.
        const textarea = document.createElement('textarea');
        textarea.value = value;
        textarea.setAttribute('readonly', '');
        textarea.style.position = 'fixed';
        textarea.style.opacity = '0';
        document.body.appendChild(textarea);
        textarea.select();
        const copied = document.execCommand('copy');
        textarea.remove();
        if (!copied) throw new Error('clipboard unavailable');
      }
      setCopyState('copied');
    } catch {
      setCopyState('failed');
    }
  }

  function downloadSecret() {
    const blob = new Blob([`${value}\n`], { type: 'text/plain;charset=utf-8' });
    const url = URL.createObjectURL(blob);
    const link = document.createElement('a');
    link.href = url;
    link.download = filename;
    link.click();
    window.setTimeout(() => URL.revokeObjectURL(url), 0);
    setSaved(true);
  }

  function dismiss() {
    if (onDismiss && window.confirm(t('common.confirmDismissSecret'))) onDismiss();
  }

  return <aside className="one-time" role="status" aria-live="polite">
    <div className="one-time-heading">
      <div><b>{message}</b><p>{t('common.secretShownOnce')}</p></div>
      {onDismiss && <button type="button" className="secondary one-time-close" aria-label={t('common.close')} onClick={dismiss}>×</button>}
    </div>
    <code aria-label={t('common.secretValue')}>{value}</code>
    <div className="button-row">
      <button type="button" onClick={() => void copySecret()}>
        {copyState === 'copied' ? t('common.copied') : t('common.copySecret')}
      </button>
      <button type="button" className="secondary" onClick={downloadSecret}>{t('common.downloadSecret')}</button>
    </div>
    {copyState === 'failed' && <small className="one-time-error" role="alert">{t('common.copySecretFailed')}</small>}
    {saved && <small className="one-time-saved" role="status">{t('common.secretSaved')}</small>}
    <small className="one-time-hint">{t('common.secretCloseHint')}</small>
  </aside>;
}
