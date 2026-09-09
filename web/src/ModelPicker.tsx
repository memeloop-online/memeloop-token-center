import { useEffect, useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';
import { useI18n } from './i18n';
import { useAnchoredPopover } from './useAnchoredPopover';
import './modelPicker.css';

export interface ModelPickerOption {
  key: string;
  value: string;
  label: string;
  provider: string;
  upstream: string;
  description?: string;
}

function groupOptions(options: ModelPickerOption[], field: 'provider' | 'upstream') {
  const groups = new Map<string, ModelPickerOption[]>();
  for (const option of options) groups.set(option[field], [...(groups.get(option[field]) ?? []), option]);
  return groups;
}

export function ModelPicker({ label, value, onChange, options, disabled = false, editable = false, onQueryChange, onOpen, loading = false, error = '', popupLabel, invalid = false }: {
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
    [option.label, option.provider, option.upstream, option.description].some((text) => text?.toLocaleLowerCase().includes(query)))
    .sort((a, b) => a.provider.localeCompare(b.provider) || a.upstream.localeCompare(b.upstream) || a.label.localeCompare(b.label)), [options, query]);
  const groups = groupOptions(matching, 'provider');
  const selected = options.find((option) => option.value === value);
  const close = (restore = false) => { setOpen(false); setActive(-1); if (restore) anchor.current?.focus(); };
  const show = () => { if (!open) { setSearch(''); setActive(-1); onOpen?.(); setOpen(true); } };
  const choose = (option: ModelPickerOption) => { onChange(option.value); close(true); };
  const keyboard = (event: KeyboardEvent<HTMLInputElement | HTMLButtonElement>) => {
    if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
      event.preventDefault(); show();
      setActive((current) => matching.length ? (current < 0 ? event.key === 'ArrowDown' ? 0 : matching.length - 1 : (current + (event.key === 'ArrowDown' ? 1 : -1) + matching.length) % matching.length) : -1);
    } else if (event.key === 'Enter' && open && matching[active >= 0 ? active : 0]) {
      event.preventDefault(); choose(matching[active >= 0 ? active : 0]);
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
    {editable ? <input ref={anchor} id={`${id}-input`} role="combobox" aria-autocomplete="list" aria-expanded={open} aria-controls={`${id}-list`} aria-activedescendant={activeId} aria-invalid={invalid} disabled={disabled} autoComplete="off" value={value}
      onClick={show} onFocus={show} onKeyDown={keyboard} onChange={(event) => { onChange(event.target.value); onQueryChange?.(event.target.value); setActive(-1); show(); }} />
      : <button ref={anchor} id={`${id}-input`} type="button" className="secondary model-picker-trigger" aria-labelledby={`${id}-label ${id}-value`} aria-haspopup="listbox" aria-expanded={open} aria-controls={`${id}-list`} disabled={disabled} onClick={() => open ? close() : show()} onKeyDown={keyboard}><span id={`${id}-value`}>{selected?.label || value || t('common.select')}</span><span aria-hidden="true">⌄</span></button>}
    {open && <section ref={panel} className="shared-model-popover" popover="auto" style={position} role="dialog" aria-label={popupLabel || t('filter.catalogModels')} onToggle={(event) => { if (event.newState === 'closed') close(); }}>
      {!editable && <input ref={searchInput} autoFocus role="combobox" aria-label={t('filter.searchCatalog')} aria-autocomplete="list" aria-expanded="true" aria-controls={`${id}-list`} aria-activedescendant={activeId} placeholder={t('filter.searchCatalog')} value={search} onKeyDown={keyboard} onChange={(event) => { setSearch(event.target.value); onQueryChange?.(event.target.value); setActive(-1); }} />}
      {loading && <small role="status">{t('common.loading')}</small>}
      {error && <small role="alert" className="error-text">{error}</small>}
      <div id={`${id}-list`} role="listbox" aria-label={label}>
        {[...groups].map(([provider, models]) => <div role="group" aria-label={provider} key={provider}><h4>{provider}</h4>
          {[...groupOptions(models, 'upstream')].map(([upstream, entries]) => <div role="group" aria-label={upstream} key={upstream}><h5>{upstream}</h5>
            {entries.map((option) => { const index = matching.indexOf(option); return <button type="button" role="option" tabIndex={-1} id={`${id}-option-${index}`} key={option.key} aria-selected={option.value === value} className={index === active ? 'active' : ''} onMouseDown={(event) => event.preventDefault()} onMouseEnter={() => setActive(index)} onClick={() => choose(option)}><b>{option.label}</b>{option.description && <small>{option.description}</small>}</button>; })}
          </div>)}
        </div>)}
      </div>
      {!loading && !error && matching.length === 0 && <small>{t('filter.catalogEmpty')}</small>}
    </section>}
  </div>;
}
