import { useId, useMemo, useRef, useState, type KeyboardEvent } from 'react';

export interface ComboboxOption {
  value: string;
  label: string;
  description?: string;
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
}

function normalized(value: string) {
  return value.trim().toLowerCase();
}

export function MultiCombobox({
  label, options, value, onChange, placeholder, emptyText, removeLabel, allowCreate = false,
  createLabel, disabled = false, hint,
}: MultiComboboxProps) {
  const id = useId();
  const inputRef = useRef<HTMLInputElement>(null);
  const [query, setQuery] = useState('');
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);
  const selected = useMemo(() => new Set(value.map((item) => item.value)), [value]);
  const available = useMemo(() => options.filter((item) => !selected.has(item.value)
    && (!query.trim() || `${item.label} ${item.description ?? ''}`.toLowerCase().includes(normalized(query)))), [options, query, selected]);
  const canCreate = allowCreate && Boolean(query.trim())
    && !options.some((item) => normalized(item.label) === normalized(query))
    && !value.some((item) => normalized(item.label) === normalized(query));
  const rows = canCreate
    ? [...available, { value: `new:${query.trim()}`, label: query.trim(), created: true }]
    : available;

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
    } else if (event.key === 'Enter' && open && rows[activeIndex >= 0 ? activeIndex : 0]) {
      event.preventDefault(); choose(rows[activeIndex >= 0 ? activeIndex : 0]);
    } else if (event.key === 'Escape') {
      event.preventDefault(); setOpen(false);
    } else if (event.key === 'Backspace' && !query && value.length > 0) {
      onChange(value.slice(0, -1));
    }
  };

  return <div className={`multi-combobox${disabled ? ' disabled' : ''}`}>
    <label id={`${id}-label`} htmlFor={`${id}-input`}>{label}</label>
    {hint && <small className="field-hint" id={`${id}-hint`}>{hint}</small>}
    <div className="multi-combobox-control" onClick={() => inputRef.current?.focus()}>
      {value.map((item) => <span className={`selection-chip${item.created ? ' pending' : ''}`} key={item.value}>
        {item.label}
        <button type="button" disabled={disabled} aria-label={removeLabel(item.label)} onClick={(event) => {
          event.stopPropagation(); onChange(value.filter((selectedItem) => selectedItem.value !== item.value));
        }}>×</button>
      </span>)}
      <input
        id={`${id}-input`}
        ref={inputRef}
        role="combobox"
        aria-autocomplete="list"
        aria-expanded={open}
        aria-controls={`${id}-listbox`}
        aria-activedescendant={open && activeIndex >= 0 && rows[activeIndex] ? `${id}-option-${activeIndex}` : undefined}
        aria-describedby={hint ? `${id}-hint` : undefined}
        autoComplete="off"
        disabled={disabled}
        placeholder={value.length === 0 ? placeholder : ''}
        value={query}
        onFocus={() => setOpen(true)}
        onBlur={() => window.setTimeout(() => setOpen(false), 100)}
        onChange={(event) => { setQuery(event.target.value); setActiveIndex(-1); setOpen(true); }}
        onKeyDown={onKeyDown}
      />
    </div>
    {open && !disabled && <div className="combobox-popover" id={`${id}-listbox`} role="listbox" aria-labelledby={`${id}-label`}>
      {rows.map((item, index) => <button
        type="button"
        role="option"
        aria-selected={index === activeIndex}
        className={index === activeIndex ? 'active' : ''}
        id={`${id}-option-${index}`}
        key={item.value}
        onMouseDown={(event) => event.preventDefault()}
        onMouseEnter={() => setActiveIndex(index)}
        onClick={() => choose(item)}
      ><span>{item.created ? createLabel?.(item.label) ?? item.label : item.label}</span>{item.description && <small>{item.description}</small>}</button>)}
      {rows.length === 0 && <div className="combobox-empty">{emptyText}</div>}
    </div>}
  </div>;
}
