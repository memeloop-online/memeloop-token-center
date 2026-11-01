import { useId, useRef, useState } from 'react';
import type { FieldProps, RJSFSchema, UiSchema } from '@rjsf/utils';
import { Button, Combobox, Option } from './design-system';
import { useI18n } from './i18n';
import type { ComboboxOption } from './operator/MultiCombobox';

export interface ResourceSelectorContext { resources: Record<string, ComboboxOption[]> }

/** Plugin-owned UI hints select a core resource picker; core never interprets plugin IDs or property names. */
export function resourceSelectorSource(schema: RJSFSchema): string | undefined {
  const hint = schema['x-resource-selector'];
  return schema.type === 'array' && schema.items && !Array.isArray(schema.items) && typeof schema.items === 'object' && schema.items.type === 'string'
    && hint && typeof hint === 'object' && hint.ordered === true && typeof hint.source === 'string' ? hint.source : undefined;
}

export function resourceSelectorUiSchema(schema: RJSFSchema): UiSchema {
  return Object.fromEntries(Object.entries(schema.properties ?? {}).flatMap(([name, property]) =>
    typeof property === 'object' && resourceSelectorSource(property) ? [[name, { 'ui:field': 'OrderedResources', 'ui:options': { label: false } }]] : []));
}

export function hasInvalidResources(schema: RJSFSchema, data: Record<string, unknown>, context: ResourceSelectorContext) {
  return Object.entries(schema.properties ?? {}).some(([name, property]) => {
    const source = typeof property === 'object' ? resourceSelectorSource(property) : undefined;
    if (!source) return false;
    const value = data[name];
    return value !== undefined && (!Array.isArray(value) || value.some(id => typeof id !== 'string' || !context.resources[source]?.some(option => option.value === id)));
  });
}

export function SchemaResourceSelector({ schema, formData, onChange, fieldPathId, disabled, readonly, registry }: FieldProps<string[], RJSFSchema, ResourceSelectorContext>) {
  const { t } = useI18n();
  const id = useId();
  const picker = useRef<HTMLInputElement>(null);
  const rowsRef = useRef<HTMLOListElement>(null);
  const [query, setQuery] = useState('');
  const [announcement, setAnnouncement] = useState('');
  const options = registry.formContext.resources[resourceSelectorSource(schema) ?? ''] ?? [];
  const values = Array.isArray(formData) ? formData : [];
  const selected = new Set(values);
  const available = options.filter(option => !selected.has(option.value) && !option.disabled);
  const matches = available.filter(option => `${option.label} ${option.description ?? ''} ${option.value}`.toLocaleLowerCase().includes(query.toLocaleLowerCase()));
  const locked = disabled || readonly;
  const full = typeof schema.maxItems === 'number' && values.length >= schema.maxItems;
  const missing = values.some(value => !options.some(option => option.value === value));
  function update(next: string[]) { onChange(next, fieldPathId.path, undefined, fieldPathId.$id); }
  function move(index: number, offset: number) {
    const next = [...values];
    [next[index], next[index + offset]] = [next[index + offset], next[index]];
    update(next);
    setAnnouncement(t('resourceSelector.moved', { name: options.find(option => option.value === values[index])?.label ?? t('resourceSelector.missing'), position: index + offset + 1 }));
    // The moved row retains a stable key. Focus its always-enabled label, even at a boundary.
    requestAnimationFrame(() => rowsRef.current?.querySelector<HTMLElement>(`[data-position="${index + offset}"] .resource-selector-name`)?.focus());
  }
  return <div className="schema-resource-selector">
    <label id={`${id}-label`} htmlFor={id}>{schema.title ?? t('resourceSelector.title')}</label>
    {schema.description && <p className="field-hint" id={`${id}-help`}>{schema.description}</p>}
    <Combobox id={id} ref={picker} aria-labelledby={`${id}-label`} aria-describedby={schema.description ? `${id}-help` : undefined}
      placeholder={t('resourceSelector.search')} value={query} selectedOptions={[]} disabled={locked || full || options.length === 0}
      onChange={event => setQuery(event.target.value)} onOptionSelect={(_, data) => {
        const choice = available.find(option => option.value === data.optionValue);
        if (!choice || locked || full) return;
        update([...values, choice.value]); setQuery(''); setAnnouncement(t('resourceSelector.added', { name: choice.label }));
      }}>
      {matches.map(option => <Option key={option.value} value={option.value} text={option.label}><span className="resource-selector-option"><strong>{option.label}</strong><small>{option.description}</small><small>{t('resourceSelector.accountId')}: {option.value}</small></span></Option>)}
      {matches.length === 0 && <Option disabled value="__empty">{available.length ? t('groups.noMatches') : t('resourceSelector.allAdded')}</Option>}
    </Combobox>
    {options.length === 0 && <p className="field-hint">{t('resourceSelector.noMembers')}</p>}
    {values.length === 0 && options.length > 0 && <p className="field-hint">{t('resourceSelector.empty')}</p>}
    {missing && <p className="notice error" role="alert">{t('resourceSelector.resolveMissing')}</p>}
    <ol ref={rowsRef} className="resource-selector-order" aria-label={schema.title ?? t('resourceSelector.title')}>
      {values.map((value, index) => {
        const option = options.find(item => item.value === value);
        const name = option?.label ?? t('resourceSelector.missing');
        return <li key={`${value}-${values.indexOf(value) === index ? '' : index}`} data-position={index} className={option ? '' : 'resource-selector-missing'}>
          <span className="resource-selector-rank" aria-hidden="true">{index + 1}</span>
          <span className="resource-selector-name" tabIndex={-1}><strong>{name}</strong><small>{option?.description}</small><small>{t('resourceSelector.accountId')}: {value}</small></span>
          <div className="resource-selector-actions">
            <Button type="button" appearance="subtle" disabled={locked || index === 0} aria-label={t('resourceSelector.moveUp', { name })} onClick={() => move(index, -1)}>↑</Button>
            <Button type="button" appearance="subtle" disabled={locked || index === values.length - 1} aria-label={t('resourceSelector.moveDown', { name })} onClick={() => move(index, 1)}>↓</Button>
            <Button type="button" appearance="subtle" disabled={locked} aria-label={t('groups.removeMember', { name })} onClick={() => { update(values.filter((_, position) => position !== index)); picker.current?.focus(); }}>{t('common.remove')}</Button>
          </div>
        </li>;
      })}
    </ol>
    <span className="resource-selector-announcement" role="status" aria-live="polite">{announcement}</span>
  </div>;
}
