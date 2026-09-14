import { useEffect, useId, useState } from 'react';
import { Combobox, Option } from '../design-system';
import { useI18n } from '../i18n.js';
import type { LogicalSessionSummary } from '../types.js';

/** Alias suggestions are explicitly limited to the loaded session projection.
 * A pasted diagnostic ID remains accepted, including deep-linked credentials. */
export function SessionCredentialFilter({ value, sessions, onChange }: {
  value: string; sessions: LogicalSessionSummary[]; onChange: (value: string) => void;
}) {
  const { t } = useI18n();
  const id = useId();
  const options = [...new Map(sessions.map(session => [session.key_id, { id: session.key_id, label: session.key_alias || session.key_id }])).values()];
  const selected = options.find(option => option.id === value);
  const [query, setQuery] = useState(selected?.label ?? value);
  useEffect(() => setQuery(selected?.label ?? value), [value, selected?.label]);
  const matches = options.filter(option => `${option.label} ${option.id}`.toLocaleLowerCase().includes(query.toLocaleLowerCase()));
  return <label htmlFor={id}>{t('sessions.credential')}
    <Combobox id={id} freeform value={query} selectedOptions={value ? [value] : []}
      placeholder={t('sessions.searchPlaceholder')}
      onChange={event => { const next = event.target.value; setQuery(next); if (!next || /^[0-9a-f]{8}-[0-9a-f-]{27}$/i.test(next)) onChange(next); }}
      onOptionSelect={(_, data) => { onChange(data.optionValue ?? ''); setQuery(data.optionText ?? ''); }}>
      <Option value="" text={t('common.all')}>{t('common.all')}</Option>
      {matches.map(option => <Option key={option.id} value={option.id} text={option.label}>{option.label}</Option>)}
    </Combobox>
  </label>;
}
