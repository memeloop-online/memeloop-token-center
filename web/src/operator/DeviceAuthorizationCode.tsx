import { useId, useState } from 'react';
import { Button } from '../design-system';
import { useI18n } from '../i18n';
import { authorizationJourneyCopy } from './authorizationJourneyCopy';

export function DeviceAuthorizationCode({ value }: { value: string }) {
  const { locale, t } = useI18n();
  const copy = authorizationJourneyCopy(locale);
  const id = useId();
  const [status, setStatus] = useState<'copied' | 'copyFailed'>();
  return <div className="device-code-actions">
    <b id={id}>{t('providers.deviceCode')}</b><code tabIndex={0} aria-labelledby={id}>{value}</code>
    <Button type="button" appearance="secondary" onClick={async () => {
      try { await navigator.clipboard.writeText(value); setStatus('copied'); }
      catch { setStatus('copyFailed'); }
    }}>{copy.copyCode}</Button>
    {status && <span role="status">{copy[status]}</span>}
  </div>;
}
