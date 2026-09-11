import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { useI18n } from './i18n';
import { useAnchoredPopover } from './useAnchoredPopover';
import './modelPicker.css';

export interface ModelPickerOption {
  key: string;
  value: string;
  label: string;
  /** An optional organizational grouping above the provider. */
  providerGroup?: string;
  provider: string;
  upstream: string;
  description?: string;
  /** Configuration-derived availability. It is deliberately not a probe result. */
  availability?: 'available' | 'unavailable' | 'unknown';
  /** The last known health signal, when the caller has one. */
  health?: 'healthy' | 'degraded' | 'unhealthy' | 'unknown';
  /** Short, human-readable capability facets such as the protocol. */
  capabilities?: string[];
  /** Keep an unavailable candidate visible but prevent selecting it. */
  disabled?: boolean;
}

function groupOptions(options: ModelPickerOption[], field: 'providerGroup' | 'provider' | 'upstream') {
  const groups = new Map<string, ModelPickerOption[]>();
  for (const option of options) {
    const group = option[field] ?? '';
    groups.set(group, [...(groups.get(group) ?? []), option]);
  }
  return groups;
}

function healthLabel(health: ModelPickerOption['health'], t: (key: string) => string) {
  return health ? t(`modelPicker.health.${health}`) : undefined;
}

function availabilityLabel(availability: ModelPickerOption['availability'], t: (key: string) => string) {
  return availability ? t(`modelPicker.availability.${availability}`) : undefined;
}

export function ModelPicker({ label, value, onChange, options, disabled = false, editable = false, onQueryChange, onOpen, loading = false, error = '', popupLabel, invalid = false, describedBy }: {
  label: string;
  value: string;
  onChange: (value: string) => void;
  options: ModelPickerOption[];
  disabled?: boolean;
  editable?: boolean;
  onQueryChange?: (query: string) => void;
  onOpen?: () => void;
  loading?: boolean;
  error?: string;
  popupLabel?: string;
  invalid?: boolean;
  describedBy?: string;
}) {
  const { t } = useI18n();
  const id = useId();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');
  const [active, setActive] = useState(-1);
  const searchInput = useRef<HTMLInputElement>(null);
  const { anchor, panel, position } = useAnchoredPopover(open);
  const query = (editable ? value : search).trim().toLocaleLowerCase();
  const matching = useMemo(() => options.filter((option) =>
    [option.label, option.providerGroup, option.provider, option.upstream, option.description, ...(option.capabilities ?? [])]
      .some((text) => text?.toLocaleLowerCase().includes(query)))
    .sort((a, b) => (a.providerGroup ?? '').localeCompare(b.providerGroup ?? '') || a.provider.localeCompare(b.provider) || a.upstream.localeCompare(b.upstream) || a.label.localeCompare(b.label)), [options, query]);
  const groups = groupOptions(matching, 'provider');
  const selected = options.find((option) => option.value === value);
  const close = (restore = false) => { setOpen(false); setActive(-1); if (restore) anchor.current?.focus(); };
  const show = () => { if (!open) { setSearch(''); setActive(-1); onOpen?.(); setOpen(true); } };
  const selectable = matching.filter((option) => !option.disabled);
  const activeOption = active >= 0 && !matching[active]?.disabled ? matching[active] : selectable[0];
  const choose = (option: ModelPickerOption) => { if (option.disabled) return; onChange(option.value); close(true); };
  const keyboard = (event: KeyboardEvent<HTMLInputElement | HTMLButtonElement>) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault(); show();
      setActive((current) => selectable.length ? (current < 0 || matching[current]?.disabled ? event.key === 'ArrowDown' ? matching.indexOf(selectable[0]) : matching.lastIndexOf(selectable.at(-1)!) : (() => {
        const selectedIndex = selectable.indexOf(matching[current]);
        const next = (selectedIndex + (event.key === 'ArrowDown' ? 1 : -1) + selectable.length) % selectable.length;
        return matching.indexOf(selectable[next]);
      })()) : -1);
    } else if (event.key === 'Enter' && open) {
      // A disabled result must never become the implicit first choice. This
      // matters most in editable pickers, where Enter would otherwise submit
      // the surrounding form instead of selecting the first usable result.
      event.preventDefault();
      if (activeOption) choose(activeOption);
    } else if (event.key === 'Escape' && open) {
      event.preventDefault(); event.stopPropagation(); close(true);
    }
  };
  const activeId = open && matching[active] ? `${id}-option-${active}` : undefined;
  useEffect(() => { if (activeId) document.getElementById(activeId)?.scrollIntoView({ block: 'nearest' }); }, [activeId]);
  return <div className="shared-model-picker" onBlur={(event) => {
    if (event.relatedTarget && !event.currentTarget.contains(event.relatedTarget as Node)) close();
  }}>
    <label id={`${id}-label`} htmlFor={`${id}-input`}>{label}</label>
    {editable ? <input ref={anchor} id={`${id}-input`} role="combobox" aria-autocomplete="list" aria-expanded={open} aria-controls={`${id}-list`} aria-activedescendant={activeId} aria-describedby={describedBy} aria-invalid={invalid} disabled={disabled} autoComplete="off" value={value}
      onClick={show} onFocus={show} onKeyDown={keyboard} onChange={(event) => { onChange(event.target.value); onQueryChange?.(event.target.value); setActive(-1); show(); }} />
      : <button ref={anchor} id={`${id}-input`} type="button" className="secondary model-picker-trigger" aria-labelledby={`${id}-label ${id}-value`} aria-describedby={describedBy} aria-haspopup="dialog" aria-expanded={open} aria-controls={`${id}-popover`} disabled={disabled} onClick={() => open ? close() : show()} onKeyDown={keyboard}><span id={`${id}-value`}>{selected?.label || value || t('common.select')}</span><span aria-hidden="true">⌄</span></button>}
    {open && <section ref={panel} id={`${id}-popover`} className="shared-model-popover" popover="auto" style={position} role="dialog" aria-modal="false" aria-label={popupLabel || t('filter.catalogModels')} onToggle={(event) => { if (event.target === event.currentTarget && event.newState === 'closed') close(); }}>
      {!editable && <input ref={searchInput} autoFocus role="combobox" aria-label={t('filter.searchCatalog')} aria-autocomplete="list" aria-expanded="true" aria-controls={`${id}-list`} aria-activedescendant={activeId} placeholder={t('filter.searchCatalog')} value={search} onKeyDown={keyboard} onChange={(event) => { setSearch(event.target.value); onQueryChange?.(event.target.value); setActive(-1); }} />}
      {loading && <small role="status">{t('common.loading')}</small>}
      {error && <small role="alert" className="error-text">{error}</small>}
      <div id={`${id}-list`} role="listbox" aria-label={label} aria-busy={loading}>
        {(matching.some((option) => option.providerGroup) ? [...groupOptions(matching, 'providerGroup')] : [[undefined, matching] as const]).map(([providerGroup, groupModels]) => <div role="group" aria-label={providerGroup || undefined} key={providerGroup || 'all-providers'}>
          {providerGroup && <h3>{providerGroup}</h3>}
          {[...groupOptions(groupModels, 'provider')].map(([provider, models]) => <div role="group" aria-label={provider} key={provider}><h4>{provider}</h4>
            {[...groupOptions(models, 'upstream')].map(([upstream, entries]) => <div role="group" aria-label={upstream} key={upstream}><h5>{upstream}</h5>
              {entries.map((option) => { const index = matching.indexOf(option); const availability = availabilityLabel(option.availability, t); const health = healthLabel(option.health, t); return <button type="button" role="option" tabIndex={-1} id={`${id}-option-${index}`} key={option.key} aria-selected={option.value === value} aria-disabled={option.disabled || undefined} disabled={option.disabled} className={`${index === active ? 'active' : ''}${option.disabled ? ' unavailable' : ''}`} onMouseDown={(event) => event.preventDefault()} onMouseEnter={() => !option.disabled && setActive(index)} onClick={() => choose(option)}><b>{option.label}</b>{option.description && <small>{option.description}</small>}<span className="model-picker-metadata">{availability && <small className={`model-picker-availability ${option.availability}`}>{availability}</small>}{health && <small className={`model-picker-health ${option.health}`}>{health}</small>}{option.capabilities?.map((capability) => <small className="model-picker-capability" key={capability}>{capability}</small>)}</span></button>; })}
            </div>)}
          </div>)}
        </div>)}
      </div>
      {!loading && !error && matching.length === 0 && <small>{t('filter.catalogEmpty')}</small>}
    </section>}
  </div>;
}
