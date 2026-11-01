import { useId } from 'react';
import { Button, DetailTooltip, Select } from './design-system';
import { useI18n } from './i18n';
import type { ResourceChoiceOption } from './resourceChoice';

export function SchemaResourceChoice({ options, value, description, busy, error, disabled, onRetry, onChange }: {
  options: ResourceChoiceOption[]; value?: string; description?: string; busy: boolean; error: string; disabled: boolean;
  onRetry: () => void; onChange: (option: ResourceChoiceOption) => void;
}) {
  const { t } = useI18n();
  const id = useId();
  return <div className="schema-resource-choice">
    <div className="resource-choice-heading"><label htmlFor={id}>{t('resourceChoice.window')}</label>
      {description && <DetailTooltip content={description}><Button type="button" appearance="subtle" size="small" aria-label={t('resourceChoice.explanation')}>ⓘ</Button></DetailTooltip>}
    </div>
    <Select id={id} value={value ?? ''} disabled={disabled || (busy && !options.length)} onChange={event => {
      const option = options.find(item => item.value === event.target.value); if (option) onChange(option);
    }}>
      {!value && <option value="">{t(busy ? 'common.loading' : 'resourceChoice.choose')}</option>}
      {options.map(option => <option key={option.value} value={option.value} title={option.details}>{option.label}</option>)}
    </Select>
    {busy && <small role="status">{t('resourceChoice.loading')}</small>}
    {error && <div role="alert">{error}<Button type="button" appearance="subtle" disabled={disabled || busy} onClick={onRetry}>{t('common.retry')}</Button></div>}
    {!busy && !error && options.length === 0 && <div className="field-hint">{t('resourceChoice.empty')}<Button type="button" appearance="subtle" disabled={disabled} onClick={onRetry}>{t('common.retry')}</Button></div>}
  </div>;
}
