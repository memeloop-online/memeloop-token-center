import { useEffect, useState } from 'react';
import { api } from '../api';
import { Button, Combobox, Option } from '../design-system';
import { useI18n } from '../i18n';
import { appendDistinctKeys, keyListPage, keyListPath } from './keyPagination';
import type { KeyListCursor, KeyView } from '../types';

/**
 * Client-credential alias editor shared by the request list and usage
 * analysis filter rows.  Aliases are read one page at a time from the keys
 * API (never a full preload), the typed text itself drives the server-side
 * search, and any free-form or historical alias stays valid: the current
 * value is always offered as an option and typed text commits on blur even
 * when it matches no listed credential.
 */
export function FilterCredentialAliasPicker({ disabled = false, onChange, tenant, token, value }: {
  disabled?: boolean;
  onChange: (value: string) => void;
  tenant: string;
  token: string;
  value: string;
}) {
  const { t } = useI18n();
  const [credentials, setCredentials] = useState<KeyView[]>([]);
  const [query, setQuery] = useState<string>();
  const [opened, setOpened] = useState(false);
  const [cursor, setCursor] = useState<KeyListCursor>();
  const [nextCursor, setNextCursor] = useState<KeyListCursor>();
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const search = (query ?? '').trim();
  useEffect(() => {
    setCredentials([]); setCursor(undefined); setNextCursor(undefined);
  }, [tenant, token, search]);
  useEffect(() => {
    if (!opened || !token.trim()) { setCredentials([]); return; }
    const controller = new AbortController();
    setLoading(true); setError('');
    void api<KeyView[]>(keyListPath(tenant, cursor, { search }), token.trim(), { signal: controller.signal })
      .then((rows) => {
        if (controller.signal.aborted) return;
        const page = keyListPage(rows);
        setCredentials((previous) => cursor ? appendDistinctKeys(previous, page.values) : page.values);
        setNextCursor(page.nextCursor);
      })
      .catch((reason: unknown) => { if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('common.requestFailed')); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [tenant, token, search, cursor, opened, t]);
  const aliases = [...new Set([...credentials.map((credential) => credential.alias.trim()), value].filter(Boolean))].sort((left, right) => left.localeCompare(right));
  return <span className="typed-filter-alias-picker">
    <Combobox aria-label={t('usage.clientCredential')} disabled={disabled} inlinePopup style={{ flexGrow: 1, minWidth: 0 }} value={query ?? value} selectedOptions={value ? [value] : []} onOpenChange={(_, data) => { if (data.open) setOpened(true); }} placeholder={t('routes.searchCredentials')}
      input={{
        onBlur: () => {
          if (query === undefined) return;
          const typed = query.trim();
          setQuery(undefined);
          if (typed !== value) onChange(typed);
        },
        onChange: (event) => setQuery(event.target.value),
      }}
      onOptionSelect={(_, data) => { if (data.optionValue === undefined) return; setQuery(undefined); onChange(data.optionValue); }}>
      <Option value="" text={t('common.all')}>{t('common.all')}</Option>
      {aliases.map((alias) => <Option key={alias} value={alias} text={alias}>{alias}</Option>)}
    </Combobox>
    {nextCursor && <Button type="button" appearance="subtle" disabled={disabled || loading} onClick={() => setCursor(nextCursor)}>{t('credentials.loadMore')}</Button>}
    {error && <span className="typed-filter-alias-error" role="alert">{error}</span>}
  </span>;
}
