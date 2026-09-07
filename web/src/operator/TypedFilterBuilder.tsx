import { useEffect, useState } from 'react';
import { api } from '../api';
import { useI18n } from '../i18n';
import type {
  FilterAssistantPlan, FilterAssistantSettings, FilterPresetState, RequestListCursor,
  ModelRouteView, TypedFilterAst, TypedFilterCondition, TypedFilterField, TypedFilterOperator, TypedFilterValue,
  UpstreamAccount,
} from '../types';

type BuilderScope = 'requests' | 'usage';
type ValueType = TypedFilterValue['type'];

interface FieldDefinition {
  id: TypedFilterField;
  type: ValueType;
  usage: boolean;
}

const fields: FieldDefinition[] = [
  { id: 'created_at', type: 'timestamp', usage: true },
  { id: 'model', type: 'model', usage: true },
  { id: 'key_id', type: 'uuid', usage: true },
  { id: 'upstream_account_id', type: 'uuid', usage: true },
  { id: 'protocol', type: 'protocol', usage: true },
  { id: 'status', type: 'status', usage: true },
  { id: 'error_code', type: 'text', usage: true },
  { id: 'route_id', type: 'uuid', usage: false },
  { id: 'duration_ms', type: 'integer', usage: false },
  { id: 'cost_micros', type: 'money_micros', usage: false },
  { id: 'key_alias', type: 'text', usage: false },
  { id: 'principal', type: 'text', usage: false },
];

const numericOperators: TypedFilterOperator[] = [
  'equals', 'not_equals', 'greater_than', 'greater_than_or_equal', 'less_than', 'less_than_or_equal', 'between',
];
const textOperators: TypedFilterOperator[] = ['equals', 'not_equals', 'contains'];
const exactOperators: TypedFilterOperator[] = ['equals', 'not_equals'];

export const emptyTypedFilterAst: TypedFilterAst = { logical_operator: 'and', conditions: [] };

export function typedFilterActive(ast: TypedFilterAst) {
  return ast.conditions.length > 0;
}

export function typedFilterRequestBody(tenant: string, ast: TypedFilterAst, before?: RequestListCursor) {
  return {
    tenant_external_id: tenant || undefined,
    limit: 100,
    paged: true,
    before_created_at: before?.before_created_at,
    before_id: before?.before_id,
    ast,
  };
}

function fieldDefinition(id: TypedFilterField) {
  return fields.find((field) => field.id === id) ?? fields[0];
}

function blankValue(type: ValueType): TypedFilterValue {
  if (type === 'protocol') return { type, value: 'openai' };
  if (type === 'status') return { type, value: 'error' };
  if (type === 'integer' || type === 'timestamp' || type === 'money_micros') return { type, value: 0 };
  return { type, value: '' } as TypedFilterValue;
}

function blankCondition(scope: BuilderScope): TypedFilterCondition {
  const field = scope === 'usage' ? fields[0] : fields[0];
  return {
    field: field.id,
    operator: 'between',
    value: { type: 'timestamp', value: Date.now() - 86_400_000 },
    upper: { type: 'timestamp', value: Date.now() },
  };
}

function operatorsFor(field: FieldDefinition, scope: BuilderScope): TypedFilterOperator[] {
  if (scope === 'usage') return field.id === 'created_at' ? ['between'] : ['equals'];
  if (field.type === 'timestamp' || field.type === 'integer' || field.type === 'money_micros') return numericOperators;
  if (field.type === 'text' || field.type === 'model') return textOperators;
  return exactOperators;
}

function localDateTime(epoch: number) {
  const date = new Date(epoch);
  return new Date(epoch - date.getTimezoneOffset() * 60_000).toISOString().slice(0, 16);
}

function conditionLabel(condition: TypedFilterCondition, t: (key: string) => string) {
  const value = typeof condition.value.value === 'number'
    ? (condition.value.type === 'timestamp' ? new Date(condition.value.value).toLocaleString() : String(condition.value.value))
    : condition.value.value;
  const upper = condition.upper && typeof condition.upper.value === 'number'
    ? (condition.upper.type === 'timestamp' ? new Date(condition.upper.value).toLocaleString() : String(condition.upper.value))
    : condition.upper?.value;
  return `${t(`filter.field.${condition.field}`)} ${t(`filter.operator.${condition.operator}`)} ${value}${upper === undefined ? '' : ` – ${upper}`}`;
}

interface CatalogModel {
  id: string;
  protocols: string[];
}

/**
 * Request and usage filters operate on the public model name recorded in an
 * activity fact, not the provider-native model name.  The route catalog is
 * therefore the authoritative autocomplete source.  Reusing the upstream
 * catalog here made a valid public model impossible to select whenever its
 * provider used a different native name.
 */
function CatalogModelPicker({ disabled, onSelect, tenant, token, value }: {
  disabled: boolean; onSelect: (model: string) => void; tenant: string; token: string; value: string;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [search, setSearch] = useState('');
  const [models, setModels] = useState<CatalogModel[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => {
    if (!open || !token) return;
    const controller = new AbortController();
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    setLoading(true); setError('');
    void api<ModelRouteView[]>(`/internal/v1/model-routes${query}`, token, { signal: controller.signal })
      .then((routes) => {
        if (controller.signal.aborted) return;
        const byPublicModel = new Map<string, Set<string>>();
        for (const route of routes) {
          const name = route.public_model.trim();
          if (!name) continue;
          const protocols = byPublicModel.get(name) ?? new Set<string>();
          protocols.add(route.protocol);
          byPublicModel.set(name, protocols);
        }
        setModels([...byPublicModel].map(([id, protocols]) => ({ id, protocols: [...protocols].sort() })).sort((left, right) => left.id.localeCompare(right.id)));
      })
      .catch((reason: unknown) => { if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('filter.catalogUnavailable')); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [open, tenant, token, t]);

  const matchingModels = models.filter((model) => model.id.toLocaleLowerCase().includes(search.trim().toLocaleLowerCase()));
  return <div className="typed-filter-model-picker">
    <button type="button" className="secondary" disabled={disabled || !token} aria-haspopup="listbox" aria-expanded={open} onClick={() => setOpen((value) => !value)}>{value || t('filter.selectCatalogModel')}</button>
    {open && <div className="typed-filter-catalog" role="dialog" aria-label={t('filter.catalogModels')}>
      <input autoFocus value={search} onChange={(event) => setSearch(event.target.value)} placeholder={t('filter.searchCatalog')} aria-label={t('filter.searchCatalog')} />
      {loading && <small>{t('common.loading')}</small>}{error && <small className="error-text">{error}</small>}
      <div role="listbox">{matchingModels.map((model) => <button type="button" role="option" key={model.id} aria-selected={model.id === value} onClick={() => { onSelect(model.id); setOpen(false); setSearch(''); }}>{model.id}<small>{model.protocols.join(', ')}</small></button>)}</div>
      {!loading && !error && matchingModels.length === 0 && <small>{t('filter.catalogEmpty')}</small>}
    </div>}
  </div>;
}

export function TypedFilterBuilder({ ast, onApply, onClear, scope, token, tenant, upstreams, externalChips = [], disabled = false }: {
  ast: TypedFilterAst;
  onApply: (ast: TypedFilterAst) => void;
  onClear: () => void;
  scope: BuilderScope;
  token: string;
  tenant: string;
  upstreams: UpstreamAccount[];
  /** Read-only API filters that cannot be represented by the UUID-only AST. */
  externalChips?: Array<{ id: string; label: string }>;
  disabled?: boolean;
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [draft, setDraft] = useState<TypedFilterAst>(ast);
  const [presets, setPresets] = useState<FilterPresetState>({ named: [], recent: [] });
  const [presetName, setPresetName] = useState('');
  const [assistantPrompt, setAssistantPrompt] = useState('');
  const [assistantPlan, setAssistantPlan] = useState<FilterAssistantPlan>();
  const [assistantSettings, setAssistantSettings] = useState<FilterAssistantSettings | null>();
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const visibleFields = scope === 'usage' ? fields.filter((field) => field.usage) : fields;
  const hasActiveFilters = ast.conditions.length > 0 || externalChips.length > 0;

  // The editor is a draft.  Synchronizing it in a passive effect while it is
  // closed races a clear followed immediately by opening and adding a row: a
  // queued clear can erase that new row.  Initialize from the applied AST at
  // the explicit open boundary instead, so recents and in-dialog edits remain
  // local to the current editor session.
  const openEditor = () => { setDraft(ast); setError(''); setOpen(true); };
  useEffect(() => {
    if (!open || !token) return;
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    void api<FilterPresetState>(`/internal/v1/filter-presets${query}`, token)
      .then(setPresets).catch((reason: unknown) => setError(reason instanceof Error ? reason.message : t('common.requestFailed')));
  }, [open, tenant, token]);
  useEffect(() => {
    if (!open || !token || !tenant) { setAssistantSettings(undefined); return; }
    void api<FilterAssistantSettings | null>(`/internal/v1/filter-assistant/settings?tenant_external_id=${encodeURIComponent(tenant)}`, token)
      .then(setAssistantSettings).catch(() => setAssistantSettings(null));
  }, [open, tenant, token]);

  const updateCondition = (index: number, patch: Partial<TypedFilterCondition>) => {
    setDraft((current) => ({ ...current, conditions: current.conditions.map((condition, itemIndex) => itemIndex === index ? { ...condition, ...patch } : condition) }));
  };
  const selectField = (index: number, fieldId: TypedFilterField) => {
    const field = fieldDefinition(fieldId); const options = operatorsFor(field, scope); const operator = options[0];
    const value = blankValue(field.type);
    updateCondition(index, { field: field.id, operator, value, upper: operator === 'between' ? blankValue(field.type) : undefined });
  };
  const setValue = (index: number, side: 'value' | 'upper', value: TypedFilterValue) => {
    setDraft((current) => ({ ...current, conditions: current.conditions.map((condition, itemIndex) => itemIndex !== index ? condition : side === 'value' ? { ...condition, value } : { ...condition, upper: value }) }));
  };
  const apply = () => {
    setError(''); onApply(draft); setOpen(false); setAssistantPlan(undefined);
    const body = { tenant_external_id: tenant || undefined, ast: draft };
    void api<FilterPresetState>('/internal/v1/filter-presets', token, { method: 'POST', body: JSON.stringify(body) }).then(setPresets).catch(() => undefined);
  };
  const saveNamed = async () => {
    if (!presetName.trim()) return;
    setBusy(true); setError('');
    try { setPresets(await api<FilterPresetState>('/internal/v1/filter-presets', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant || undefined, name: presetName, ast: draft }) })); setPresetName(''); }
    catch (reason) { setError(reason instanceof Error ? reason.message : t('common.requestFailed')); }
    finally { setBusy(false); }
  };
  const planWithAssistant = async () => {
    if (!assistantPrompt.trim() || !tenant) return;
    setBusy(true); setError(''); setAssistantPlan(undefined);
    try { setAssistantPlan(await api<FilterAssistantPlan>('/internal/v1/filter-assistant/plan', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, prompt: assistantPrompt }) })); }
    catch (reason) { setError(reason instanceof Error ? reason.message : t('common.requestFailed')); }
    finally { setBusy(false); }
  };

  const renderValue = (condition: TypedFilterCondition, index: number, side: 'value' | 'upper') => {
    const value = side === 'value' ? condition.value : condition.upper;
    if (!value) return null;
    if (value.type === 'model') return <CatalogModelPicker disabled={disabled} onSelect={(model) => setValue(index, side, { type: 'model', value: model })} tenant={tenant} token={token} value={value.value} />;
    if (value.type === 'protocol') return <select value={value.value} disabled={disabled} onChange={(event) => setValue(index, side, { type: 'protocol', value: event.target.value as 'openai' | 'anthropic' | 'openai-image' | 'generation' })}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="openai-image">OpenAI Images</option><option value="generation">{t('routes.generation')}</option></select>;
    if (value.type === 'status') return <select value={value.value} disabled={disabled} onChange={(event) => setValue(index, side, { type: 'status', value: event.target.value as 'success' | 'error' | 'pending' })}><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></select>;
    if (condition.field === 'upstream_account_id') return <select value={value.value} disabled={disabled} onChange={(event) => setValue(index, side, { type: 'uuid', value: event.target.value })}><option value="">{t('common.select')}</option>{upstreams.filter((account) => account.status === 'active').map((account) => <option value={account.id} key={account.id}>{account.name}</option>)}</select>;
    if (value.type === 'timestamp') return <input type="datetime-local" value={localDateTime(value.value)} disabled={disabled} onChange={(event) => { const next = Date.parse(event.target.value); if (Number.isFinite(next)) setValue(index, side, { type: 'timestamp', value: next }); }} />;
    if (value.type === 'integer' || value.type === 'money_micros') return <input type="number" step="1" value={value.value} disabled={disabled} onChange={(event) => setValue(index, side, { type: value.type, value: Number(event.target.value) })} />;
    return <input value={value.value} disabled={disabled} onChange={(event) => setValue(index, side, { type: value.type, value: event.target.value } as TypedFilterValue)} placeholder={value.type === 'uuid' ? '019f…' : undefined} />;
  };

  return <div className="typed-filter-builder">
    <div className="typed-filter-chips" aria-label={t('filter.applied')}>
      {ast.conditions.map((condition, index) => <span className="filter-chip" key={`${condition.field}-${index}`}>{conditionLabel(condition, t)}</span>)}
      {externalChips.map((chip) => <span className="filter-chip" key={chip.id}>{chip.label}</span>)}
      {!hasActiveFilters && <span className="muted">{t('filter.noneApplied')}</span>}
    </div>
    <div className="typed-filter-actions"><button type="button" className="secondary" disabled={disabled} onClick={openEditor}>{t('filter.open')}</button>{hasActiveFilters && <button type="button" className="secondary" disabled={disabled} onClick={onClear}>{t('filter.clear')}</button>}</div>
    {open && <div className="typed-filter-overlay" role="presentation"><section className="typed-filter-dialog" role="dialog" aria-modal="true" aria-label={t('filter.title')}>
      <div className="panel-title"><div><h2>{t('filter.title')}</h2><p className="muted">{t('filter.description')}</p></div><button type="button" className="secondary" onClick={() => setOpen(false)}>{t('common.close')}</button></div>
      {error && <div className="notice error" role="alert">{error}</div>}
      <div className="typed-filter-rows">{draft.conditions.map((condition, index) => {
        const field = fieldDefinition(condition.field); const operators = operatorsFor(field, scope);
        // Field selection replaces only the value editor. Keeping the row
        // mounted preserves the freshly selected field while React swaps that
        // editor from a scalar input to the catalog picker.
        return <div className="typed-filter-row" key={index}>
          <label><span>{t('filter.field')}</span><select value={condition.field} disabled={disabled} onChange={(event) => selectField(index, event.target.value as TypedFilterField)}>{visibleFields.map((option) => <option key={option.id} value={option.id}>{t(`filter.field.${option.id}`)}</option>)}</select></label>
          <label><span>{t('filter.operator')}</span><select value={condition.operator} disabled={disabled} onChange={(event) => { const operator = event.target.value as TypedFilterOperator; updateCondition(index, { operator, upper: operator === 'between' ? blankValue(field.type) : undefined }); }}>{operators.map((operator) => <option key={operator} value={operator}>{t(`filter.operator.${operator}`)}</option>)}</select></label>
          <label className="typed-filter-value" data-filter-field={condition.field}><span>{t('filter.value')}</span>{renderValue(condition, index, 'value')}</label>
          {condition.operator === 'between' && <label className="typed-filter-value" data-filter-field={condition.field}><span>{t('filter.upper')}</span>{renderValue(condition, index, 'upper')}</label>}
          <div className="typed-filter-row-actions"><button type="button" className="secondary" disabled={disabled || index === 0} aria-label={t('common.moveUp')} onClick={() => setDraft((current) => { const conditions = [...current.conditions]; [conditions[index - 1], conditions[index]] = [conditions[index], conditions[index - 1]]; return { ...current, conditions }; })}>↑</button><button type="button" className="secondary" disabled={disabled || index + 1 === draft.conditions.length} aria-label={t('common.moveDown')} onClick={() => setDraft((current) => { const conditions = [...current.conditions]; [conditions[index], conditions[index + 1]] = [conditions[index + 1], conditions[index]]; return { ...current, conditions }; })}>↓</button><button type="button" className="secondary" disabled={disabled} aria-label={t('common.remove')} onClick={() => setDraft((current) => ({ ...current, conditions: current.conditions.filter((_, itemIndex) => itemIndex !== index) }))}>×</button></div>
        </div>;
      })}</div>
      <div className="typed-filter-footer"><button type="button" className="secondary" disabled={disabled || draft.conditions.length >= 12} onClick={() => setDraft((current) => ({ ...current, conditions: [...current.conditions, blankCondition(scope)] }))}>{t('filter.addCondition')}</button><button type="button" disabled={disabled} onClick={apply}>{t('filter.apply')}</button></div>
      <section className="typed-filter-presets"><h3>{t('filter.saved')}</h3><div className="typed-filter-preset-list">{presets.named.map((preset) => <button type="button" className="secondary" key={preset.name} onClick={() => setDraft(preset.ast)}>{preset.name}</button>)}{presets.recent.map((recent, index) => <button type="button" className="secondary" key={`recent-${index}`} onClick={() => setDraft(recent)}>{t('filter.recent')} {index + 1}</button>)}</div><div className="typed-filter-save"><input value={presetName} maxLength={80} onChange={(event) => setPresetName(event.target.value)} placeholder={t('filter.namePlaceholder')} /><button type="button" className="secondary" disabled={busy || !presetName.trim()} onClick={() => void saveNamed()}>{t('filter.save')}</button></div></section>
      {scope === 'requests' && <section className="typed-filter-assistant"><h3>{t('filter.assistant')}</h3>{!tenant ? <div className="empty">{t('filter.assistantTenantRequired')}</div> : assistantSettings === undefined ? <div className="muted">{t('common.loading')}</div> : assistantSettings === null ? <div className="empty">{t('filter.assistantNotConfigured')}</div> : <><label>{t('filter.assistantPrompt')}<input value={assistantPrompt} maxLength={2000} onChange={(event) => setAssistantPrompt(event.target.value)} placeholder={t('filter.assistantPlaceholder')} /></label><button type="button" className="secondary" disabled={busy || !assistantPrompt.trim()} onClick={() => void planWithAssistant()}>{t('filter.createPreview')}</button>{assistantPlan && <div className="typed-filter-preview"><b>{t('filter.preview')}</b><div className="typed-filter-chips">{assistantPlan.ast.conditions.map((condition, index) => <span className="filter-chip" key={`${condition.field}-${index}`}>{conditionLabel(condition, t)}</span>)}</div><button type="button" onClick={() => { setDraft(assistantPlan.ast); setAssistantPlan(undefined); }}>{t('filter.usePreview')}</button></div>}</>}</section>}
    </section></div>}
  </div>;
}
