import { useEffect, useId, useMemo, useState, type KeyboardEvent } from 'react';
import { useAnchoredPopover } from '../useAnchoredPopover';

export interface ComboboxOption {
  value: string;
  label: string;
  description?: string;
  group?: string;
  created?: boolean;
}

interface MultiComboboxProps {
  label: string;
  options: ComboboxOption[];
  value: ComboboxOption[];
  onChange: (value: ComboboxOption[]) => void;
  placeholder: string;
  emptyText: string;
  removeLabel: (label: string) => string;
  allowCreate?: boolean;
  createLabel?: (value: string) => string;
  disabled?: boolean;
  hint?: string;
  onQueryChange?: (query: string) => void;
  inputId?: string;
  required?: boolean;
  invalid?: boolean;
}

function normalized(value: string) {
  return value.trim().toLowerCase();
}

function rowsForQuery(options: ComboboxOption[], value: ComboboxOption[], query: string, allowCreate: boolean) {
  const selected = new Set(value.map((item) => item.value));
  const available = options.filter((item) => !selected.has(item.value)
    && (!query.trim() || `${item.label} ${item.description ?? ''} ${item.group ?? ''}`.toLowerCase().includes(normalized(query))));
  const canCreate = allowCreate && Boolean(query.trim())
    && !options.some((item) => normalized(item.label) === normalized(query))
    && !value.some((item) => normalized(item.label) === normalized(query));
  return canCreate
    ? [...available, { value: `new:${query.trim()}`, label: query.trim(), created: true }]
    : available;
}

export function MultiCombobox({
  label, options, value, onChange, placeholder, emptyText, removeLabel, allowCreate = false,
  createLabel, disabled = false, hint, onQueryChange, inputId, required = false, invalid = false,
}: MultiComboboxProps) {
  const id = useId();
  const [query, setQuery] = useState('');
  const [open, setOpen] = useState(false);
  const { anchor: inputRef, panel, position } = useAnchoredPopover(open && !disabled);
  const [activeIndex, setActiveIndex] = useState(-1);
  const rows = useMemo(() => rowsForQuery(options, value, query, allowCreate), [options, value, query, allowCreate]);
  const groups = new Map<string, ComboboxOption[]>();
  rows.forEach(item => groups.set(item.group ?? '', [...(groups.get(item.group ?? '') ?? []), item]));
  useEffect(() => {
    if (open && activeIndex >= 0) document.getElementById(`${id}-option-${activeIndex}`)?.scrollIntoView({ block: 'nearest' });
  }, [open, activeIndex, id]);

  const choose = (item: ComboboxOption) => {
    onChange([...value, item]);
    setQuery('');
    setActiveIndex(-1);
    setOpen(true);
    requestAnimationFrame(() => inputRef.current?.focus());
  };

  const onKeyDown = (event: KeyboardEvent<HTMLInputElement>) => {
    if (event.key === 'ArrowDown') {
      event.preventDefault(); setOpen(true); setActiveIndex((current) => Math.min(current + 1, Math.max(rows.length - 1, 0)));
    } else if (event.key === 'ArrowUp') {
      event.preventDefault(); setOpen(true); setActiveIndex((current) => Math.max(current - 1, 0));
    } else if (event.key === 'Enter') {
      // Enter belongs to the autocomplete, even for an empty result. It must
      // never submit the surrounding create/edit form accidentally.
      event.preventDefault();
      // React may not have committed the input/open state before a fast keyboard user presses Enter.
      const currentRows = rowsForQuery(options, value, event.currentTarget.value, allowCreate);
      const item = currentRows[activeIndex >= 0 ? activeIndex : 0];
      if (open && item) choose(item);
    } else if (event.key === 'Escape') {
      if (open) { event.preventDefault(); event.stopPropagation(); setOpen(false); }
    } else if (event.key === 'Backspace' && !query && value.length > 0) {
      onChange(value.slice(0, -1));
    }
  };

  return <div className={`multi-combobox${disabled ? ' disabled' : ''}`} onBlur={(event) => {
    if (!event.currentTarget.contains(event.relatedTarget as Node | null)) setOpen(false);
  }}>
    <label id={`${id}-label`} htmlFor={inputId ?? `${id}-input`}>{label}{required ? ' *' : ''}</label>
    {hint && <small className="field-hint" id={`${id}-hint`}>{hint}</small>}
    <div className="multi-combobox-control" onClick={() => inputRef.current?.focus()}>
      {value.map((item) => <span className={`selection-chip${item.created ? ' pending' : ''}`} key={item.value}>
        {item.label}
        <button type="button" disabled={disabled} aria-label={removeLabel(item.label)} onClick={(event) => {
          event.stopPropagation(); onChange(value.filter((selectedItem) => selectedItem.value !== item.value));
        }}>×</button>
      </span>)}
      <input
        id={inputId ?? `${id}-input`}
        ref={inputRef}
        role="combobox"
        aria-autocomplete="list"
        aria-required={required}
        aria-invalid={invalid}
        aria-expanded={open}
        aria-controls={`${id}-listbox`}
        aria-activedescendant={open && activeIndex >= 0 && rows[activeIndex] ? `${id}-option-${activeIndex}` : undefined}
        aria-describedby={hint ? `${id}-hint` : undefined}
        autoComplete="off"
        disabled={disabled}
        placeholder={value.length === 0 ? placeholder : ''}
        value={query}
        onFocus={() => setOpen(true)}
        onChange={(event) => {
          setQuery(event.target.value); setActiveIndex(-1); setOpen(true);
          onQueryChange?.(event.target.value);
        }}
        onKeyDown={onKeyDown}
      />
    </div>
    {open && !disabled && <section ref={panel} popover="auto" style={position} className="combobox-popover" id={`${id}-listbox`} role="listbox" aria-labelledby={`${id}-label`}
      onToggle={event => { if (event.target === event.currentTarget && event.newState === 'closed') setOpen(false); }}>
      {[...groups].map(([group, entries]) => <div key={group} role={group ? 'group' : undefined} aria-label={group || undefined}>
      {group && <div className="combobox-group-title">{group}</div>}
      {entries.map((item) => { const index = rows.indexOf(item); return <button
        type="button"
        tabIndex={-1}
        role="option"
        aria-selected={index === activeIndex}
        className={index === activeIndex ? 'active' : ''}
        id={`${id}-option-${index}`}
        key={item.value}
        onMouseDown={(event) => event.preventDefault()}
        onMouseEnter={() => setActiveIndex(index)}
        onClick={() => choose(item)}
      ><span>{item.created ? createLabel?.(item.label) ?? item.label : item.label}</span>{item.description && <small>{item.description}</small>}</button>; })}
      </div>)}
      {rows.length === 0 && <div className="combobox-empty">{emptyText}</div>}
    </section>}
  </div>;
}
