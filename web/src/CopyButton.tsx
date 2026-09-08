import { useId, useState } from 'react';
import { useI18n } from './i18n.js';

/**
 * Copies a value already held by the current UI. Callers must not pass an ID,
 * fingerprint, or hash in place of an unavailable credential secret.
 */
export function CopyButton({ value, label }: { value: string; label?: string }) {
  const { t } = useI18n();
  const feedbackId = useId();
  const [state, setState] = useState<'idle' | 'copied' | 'failed'>('idle');
  const feedback = state === 'copied'
    ? t('common.copied')
    : state === 'failed'
      ? t('common.requestFailed')
      : '';

  async function copy() {
    try {
      if (!navigator.clipboard?.writeText) throw new Error('Clipboard API unavailable');
      await navigator.clipboard.writeText(value);
      setState('copied');
    } catch {
      setState('failed');
    }
  }

  return <span className="copy-control">
    <button type="button" className="secondary" aria-describedby={feedback ? feedbackId : undefined} onClick={() => void copy()}>
      {state === 'copied' ? t('common.copied') : label ?? t('common.copy')}
    </button>
    <span id={feedbackId} className="visually-hidden" role="status" aria-live="polite">{feedback}</span>
  </span>;
}
