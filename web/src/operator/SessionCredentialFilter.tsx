import { useEffect, useId, useState } from 'react';
import { api } from '../api.js';
import { Combobox, Option } from '../design-system';
import { useI18n } from '../i18n.js';
import type { KeyView, LogicalSessionSummary } from '../types.js';

/** Paged credential metadata supplements aliases from the session projection.
 * A pasted diagnostic ID remains accepted, including deep-linked credentials. */
export function SessionCredentialFilter({ value, sessions, token, tenant, onChange }: {
  value: string; sessions: LogicalSessionSummary[]; token: string; tenant: string; onChange: (value: string) => void;
}) {
  const { t } = useI18n();
  const id = useId();
  const scope = `${tenant}\0${token}`;
  const [page, setPage] = useState<{ scope: string; values: KeyView[]; more: boolean }>({ scope: '', values: [], more: false });
  const [pageNumber, setPageNumber] = useState(0);
  const [loading, setLoading] = useState(false);
  const [failed, setFailed] = useState(false);
  const currentKeys = page.scope === scope ? page.values : [];
  useEffect(() => {
    if (!token.trim()) return;
    const controller = new AbortController();
    const params = new URLSearchParams({ limit: '100' });
    if (tenant) params.set('tenant_external_id', tenant);
    const last = page.scope === scope ? page.values.at(-1) : undefined;
    if (last && pageNumber > 0) { params.set('before_created_at', String(last.created_at)); params.set('before_id', last.key_id); }
    setLoading(true); setFailed(false);
    void api<KeyView[]>(`/internal/v1/keys?${params}`, token.trim(), { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(15_000)]) })
      .then(values => { if (!Array.isArray(values)) throw new Error('Invalid credential page'); if (!controller.signal.aborted) setPage(previous => ({ scope, values: last ? [...previous.values, ...values] : values, more: values.length === 100 })); })
      .catch(() => { if (!controller.signal.aborted) setFailed(true); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [scope, pageNumber]);
  const options = [...new Map([
    ...sessions.map(session => [session.key_id, { id: session.key_id, label: session.key_alias || session.key_id }] as const),
    ...currentKeys.map(key => [key.key_id, { id: key.key_id, label: key.alias || key.key_id }] as const),
  ]).values()];
  const selected = options.find(option => option.id === value);
  const [query, setQuery] = useState(selected?.label ?? value);
  useEffect(() => {
    setQuery(selected?.label ?? value);
    (document.getElementById(id) as HTMLInputElement | null)?.setCustomValidity('');
  }, [value, selected?.label, scope, id]);
  const matches = options.filter(option => `${option.label} ${option.id}`.toLocaleLowerCase().includes(query.toLocaleLowerCase()));
  return <label htmlFor={id}>{t('sessions.credential')}
    <Combobox id={id} freeform value={query} selectedOptions={value ? [value] : []}
      placeholder={t('sessions.searchPlaceholder')}
      input={{ onChange: event => { const next = event.target.value; setQuery(next); const valid = !next || /^[0-9a-f]{8}-[0-9a-f-]{27}$/i.test(next); event.target.setCustomValidity(valid ? '' : t('groups.noMatches')); if (valid) onChange(next); } }}
      onOptionSelect={(_, data) => { if (data.optionValue === undefined) return; (document.getElementById(id) as HTMLInputElement | null)?.setCustomValidity(''); onChange(data.optionValue); setQuery(data.optionText ?? ''); }}>
      <Option value="" text={t('common.all')}>{t('common.all')}</Option>
      {matches.map(option => <Option key={option.id} value={option.id} text={option.label}>{option.label}</Option>)}
    </Combobox>
    {(loading || failed || (page.scope === scope && page.more)) && <button type="button" className="secondary" disabled={loading} onClick={() => setPageNumber(number => number + 1)}>{loading ? t('common.loading') : failed ? t('sessions.retryLoad') : t('sessions.loadOlder')}</button>}
  </label>;
}
