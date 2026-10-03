import { useEffect, useId, useState, type ReactNode } from 'react';
import { api } from '../api';
import { Button, Input, Select, Textarea } from '../design-system';
import { useI18n } from '../i18n';
import { ModelPicker } from '../ModelPicker';
import { useAnchoredPopover } from '../useAnchoredPopover';
import { routeModelOptions } from './modelCatalog';
import { defaultUsageSelection, localDateTimeInput, usageAstFromSelection, usageSelectionFromAst, type UsageSelection } from './usageState';
import type {
  FilterAssistantPlan, FilterAssistantSettings, FilterPresetState, RequestListCursor,
  ModelRouteView, TypedFilterAst, TypedFilterCondition, TypedFilterField, TypedFilterOperator, TypedFilterValue,
  UpstreamAccount, GroupView,
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
  const field = scope === 'usage' ? fields[1] : fields[0];
  if (scope === 'usage') return { field: field.id, operator: 'equals', value: blankValue(field.type) };
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

function conditionLabel(condition: TypedFilterCondition, t: (key: string) => string) {
  const value = typeof condition.value.value === 'number'
    ? (condition.value.type === 'timestamp' ? new Date(condition.value.value).toLocaleString() : String(condition.value.value))
    : condition.value.value;
  const upper = condition.upper && typeof condition.upper.value === 'number'
    ? (condition.upper.type === 'timestamp' ? new Date(condition.upper.value).toLocaleString() : String(condition.upper.value))
    : condition.upper?.value;
  return `${t(`filter.field.${condition.field}`)} ${t(`filter.operator.${condition.operator}`)} ${value}${upper === undefined ? '' : ` – ${upper}`}`;
}

/**
 * Request and usage filters operate on the public model name recorded in an
 * activity fact, not the provider-native model name.  The route catalog is
 * therefore the authoritative autocomplete source.  Reusing the upstream
 * catalog here made a valid public model impossible to select whenever its
 * provider used a different native name.
 */
function CatalogModelPicker({ disabled, onSelect, tenant, token, value, upstreams }: {
  disabled: boolean; onSelect: (model: string) => void; tenant: string; token: string; value: string; upstreams: UpstreamAccount[];
}) {
  const { t } = useI18n();
  const [open, setOpen] = useState(false);
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [groups, setGroups] = useState<GroupView[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => {
    if (!open || !token) return;
    const controller = new AbortController();
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    setLoading(true); setError('');
    setRoutes([]); setGroups([]);
    void Promise.all([
      api<ModelRouteView[]>(`/internal/v1/model-routes${query}`, token, { signal: controller.signal }),
      api<GroupView[]>(`/internal/v1/provider-groups${query}`, token, { signal: controller.signal }),
    ])
      .then(([nextRoutes, nextGroups]) => {
        if (controller.signal.aborted) return;
        setRoutes(nextRoutes); setGroups(nextGroups);
      })
      .catch((reason: unknown) => { if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('filter.catalogUnavailable')); })
      .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    return () => controller.abort();
  }, [open, tenant, token, t]);

  return <div className="typed-filter-model-picker">
    <ModelPicker label={t('filter.selectCatalogModel')} disabled={disabled || !token} value={value} onChange={onSelect} options={routeModelOptions(routes, upstreams, groups, t('modelPicker.unknown'))} loading={loading} error={error} onOpen={() => setOpen(true)} />
  </div>;
}

export function TypedFilterBuilder({ ast, onApply, onClear, scope, token, tenant, upstreams, externalChips = [], panelControls, panelActive = false, usageSelection, disabled = false }: {
  ast: TypedFilterAst;
  onApply: (ast: TypedFilterAst, selection?: UsageSelection) => void;
  onClear: () => void;
  scope: BuilderScope;
  token: string;
  tenant: string;
  upstreams: UpstreamAccount[];
  /** Read-only API filters that cannot be represented by the UUID-only AST. */
  externalChips?: Array<{ id: string; label: string }>;
  panelControls?: ReactNode | ((selection: UsageSelection, onChange: (selection: UsageSelection) => void) => ReactNode);
  panelActive?: boolean;
  usageSelection?: UsageSelection;
  disabled?: boolean;
}) {
  const { locale, t } = useI18n();
  const editorId = useId();
  const popoverId = `${editorId}-popover`;
  const [open, setOpen] = useState(false);
  const { anchor, panel, position } = useAnchoredPopover<HTMLButtonElement>(open);
  const [draft, setDraft] = useState<TypedFilterAst>(ast);
  const [draftUsage, setDraftUsage] = useState<UsageSelection>(() => usageSelection ?? defaultUsageSelection());
  const [presets, setPresets] = useState<FilterPresetState>({ named: [], recent: [] });
  const [presetName, setPresetName] = useState('');
  const [assistantPrompt, setAssistantPrompt] = useState('');
  const [assistantPlan, setAssistantPlan] = useState<FilterAssistantPlan>();
  const [assistantSettings, setAssistantSettings] = useState<FilterAssistantSettings | null>();
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const visibleFields = scope === 'usage' ? fields.filter((field) => field.usage && field.id !== 'created_at' && field.id !== 'key_alias') : fields;
  const hasActiveFilters = (scope === 'usage' ? ast.conditions.some((condition) => condition.field !== 'created_at') : ast.conditions.length > 0) || externalChips.length > 0 || panelActive;
  const conditionBudget = 12 - (scope === 'usage' ? 1 + Number(Boolean(draftUsage.filters.keyAlias.trim())) : 0);
  const budgetError = locale === 'zh-CN' ? '筛选条件最多 12 条（包括时间范围和客户端凭据）。' : 'Filters allow at most 12 conditions, including the period and client credential.';
  const usageAstError = locale === 'zh-CN' ? '用量分析不支持重复字段或无法表示的筛选条件。' : 'Usage analysis cannot apply duplicate fields or unsupported conditions.';
  const unassignedError = locale === 'zh-CN' ? '未分配上游可用于查询，但无法保存为类型化筛选。' : 'Unassigned upstream can be queried, but cannot be saved as a typed filter.';
  useEffect(() => { setOpen(false); setAssistantPlan(undefined); }, [tenant, token]);

  // The editor is a draft.  Synchronizing it in a passive effect while it is
  // closed races a clear followed immediately by opening and adding a row: a
  // queued clear can erase that new row.  Initialize from the applied AST at
  // the explicit open boundary instead, so recents and in-dialog edits remain
  // local to the current editor session.
  const openEditor = () => { setDraft(scope === 'usage' ? { ...ast, conditions: ast.conditions.filter((condition) => condition.field !== 'created_at' && condition.field !== 'key_alias') } : ast); if (usageSelection) setDraftUsage(usageSelection); setError(''); setOpen(true); };
  useEffect(() => {
    if (!open || !token) return;
    let current = true;
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    void api<FilterPresetState>(`/internal/v1/filter-presets${query}`, token)
      .then((next) => { if (current) setPresets(next); }).catch((reason: unknown) => { if (current) setError(reason instanceof Error ? reason.message : t('common.requestFailed')); });
    return () => { current = false; };
  }, [open, tenant, token]);
  useEffect(() => {
    if (!open || !token || !tenant) { setAssistantSettings(undefined); return; }
    let current = true;
    void api<FilterAssistantSettings | null>(`/internal/v1/filter-assistant/settings?tenant_external_id=${encodeURIComponent(tenant)}`, token)
      .then((next) => { if (current) setAssistantSettings(next); }).catch(() => { if (current) setAssistantSettings(null); });
    return () => { current = false; };
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
  const apply = async () => {
    const nextSelection = scope === 'usage' ? usageSelectionFromAst(draftUsage, draft, false) : undefined;
    if (scope === 'usage' && !nextSelection) { setError(usageAstError); return; }
    const effective = nextSelection ? usageAstFromSelection(draft, nextSelection) : draft;
    if (!effective) { setError(t('usage.invalidRange')); return; }
    if (draft.conditions.length > conditionBudget || effective.conditions.length > 12) { setError(budgetError); return; }
    if (nextSelection?.filters.upstreamId === 'unassigned') { onApply(effective, nextSelection); setError(unassignedError); return; }
    setError(''); onApply(effective, nextSelection); setBusy(true);
    const body = { tenant_external_id: tenant || undefined, ast: effective };
    try { setPresets(await api<FilterPresetState>('/internal/v1/filter-presets', token, { method: 'POST', body: JSON.stringify(body) })); setOpen(false); setAssistantPlan(undefined); }
    catch (reason) { setError(reason instanceof Error ? reason.message : t('common.requestFailed')); }
    finally { setBusy(false); }
  };
  const saveNamed = async () => {
    if (!presetName.trim()) return;
    const nextSelection = scope === 'usage' ? usageSelectionFromAst(draftUsage, draft, false) : undefined;
    if (scope === 'usage' && !nextSelection) { setError(usageAstError); return; }
    const effective = nextSelection ? usageAstFromSelection(draft, nextSelection) : draft;
    if (!effective) { setError(t('usage.invalidRange')); return; }
    if (draft.conditions.length > conditionBudget || effective.conditions.length > 12) { setError(budgetError); return; }
    if (nextSelection?.filters.upstreamId === 'unassigned') { setError(unassignedError); return; }
    setBusy(true); setError('');
    try { setPresets(await api<FilterPresetState>('/internal/v1/filter-presets', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant || undefined, name: presetName, ast: effective }) })); setPresetName(''); }
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
  const loadPreset = (preset: TypedFilterAst) => {
    if (scope !== 'usage') { setDraft(preset); setError(''); return; }
    const nextSelection = usageSelectionFromAst(defaultUsageSelection(), preset);
    if (!nextSelection) { setError(usageAstError); return; }
    setDraft({ ...preset, conditions: preset.conditions.filter((condition) => condition.field !== 'created_at' && condition.field !== 'key_alias') });
    setDraftUsage(nextSelection); setError('');
  };

  const renderValue = (condition: TypedFilterCondition, index: number, side: 'value' | 'upper') => {
    const value = side === 'value' ? condition.value : condition.upper;
    if (!value) return null;
    if (value.type === 'model') return <CatalogModelPicker disabled={disabled} onSelect={(model) => setValue(index, side, { type: 'model', value: model })} tenant={tenant} token={token} value={value.value} upstreams={upstreams} />;
    if (value.type === 'protocol') return <Select value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'protocol', value: data.value as 'openai' | 'anthropic' | 'openai-image' | 'audio-transcription' | 'generation' })}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="openai-image">OpenAI Images</option><option value="audio-transcription">OpenAI Audio</option><option value="generation">{t('routes.generation')}</option></Select>;
    if (value.type === 'status') return <Select value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'status', value: data.value as 'success' | 'error' | 'pending' })}><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></Select>;
    if (condition.field === 'upstream_account_id') return <Select value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'uuid', value: data.value })}><option value="">{t('common.select')}</option>{upstreams.filter((account) => account.status === 'active').map((account) => <option value={account.id} key={account.id}>{account.name}</option>)}</Select>;
    if (value.type === 'timestamp') return <Input type="datetime-local" value={localDateTimeInput(value.value, 'minute')} disabled={disabled} onChange={(_, data) => { const next = Date.parse(data.value); if (Number.isFinite(next)) setValue(index, side, { type: 'timestamp', value: next }); }} />;
    if (value.type === 'integer' || value.type === 'money_micros') return <Input type="number" step="1" value={String(value.value)} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: value.type, value: Number(data.value) })} />;
    return <Input value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: value.type, value: data.value } as TypedFilterValue)} placeholder={value.type === 'uuid' ? '019f…' : undefined} />;
  };

  return <div className="typed-filter-builder" style={scope === 'usage' ? { minWidth: 0 } : undefined}>
    <div className="typed-filter-chips" style={scope === 'usage' ? { minWidth: 0 } : undefined} aria-label={t('filter.applied')}>
      {ast.conditions.filter((condition) => scope !== 'usage' || condition.field !== 'created_at' || usageSelection?.preset !== '24h').map((condition, index) => <span className="filter-chip" key={`${condition.field}-${index}`}>{conditionLabel(condition, t)}</span>)}
      {externalChips.map((chip) => <span className="filter-chip" key={chip.id}>{chip.label}</span>)}
      {!hasActiveFilters && <span className="muted">{t('filter.noneApplied')}</span>}
    </div>
    <div className="typed-filter-actions"><Button ref={anchor} type="button" appearance="secondary" disabled={disabled} aria-haspopup="dialog" aria-expanded={open} aria-controls={popoverId} onClick={() => open ? setOpen(false) : openEditor()}>{t('filter.open')}</Button>{!panelControls && hasActiveFilters && <Button type="button" appearance="secondary" disabled={disabled} onClick={onClear}>{t('filter.clear')}</Button>}</div>
    {open && <section ref={panel} id={popoverId} popover="auto" style={position} className="typed-filter-dialog" role="dialog" aria-label={t('filter.title')} onKeyDown={(event) => { if (event.key === 'Escape') { event.preventDefault(); event.stopPropagation(); setOpen(false); anchor.current?.focus(); } }} onToggle={(event) => { if (event.target === event.currentTarget && event.newState === 'closed') setOpen(false); }}>
      <div className="panel-title"><div><h2>{t('filter.title')}</h2><p className="muted">{t('filter.description')}</p></div><Button autoFocus type="button" appearance="secondary" onClick={() => { setOpen(false); anchor.current?.focus(); }}>{t('common.close')}</Button></div>
      {error && <div className="notice error" role="alert">{error}</div>}
      {panelControls && <section className="typed-filter-panel-controls">{typeof panelControls === 'function' ? panelControls(draftUsage, setDraftUsage) : panelControls}</section>}
      <div className="typed-filter-rows">{draft.conditions.map((condition, index) => {
        const field = fieldDefinition(condition.field); const operators = operatorsFor(field, scope);
        // Field selection replaces only the value editor. Keeping the row
        // mounted preserves the freshly selected field while React swaps that
        // editor from a scalar input to the catalog picker.
        return <div className="typed-filter-row" key={index}>
          <label><span id={`${editorId}-field-${index}`}>{t('filter.field')}</span><Select aria-labelledby={`${editorId}-field-${index}`} value={condition.field} disabled={disabled} onChange={(_, data) => selectField(index, data.value as TypedFilterField)}>{visibleFields.map((option) => <option key={option.id} value={option.id}>{t(`filter.field.${option.id}`)}</option>)}</Select></label>
          <label><span id={`${editorId}-operator-${index}`}>{t('filter.operator')}</span><Select aria-labelledby={`${editorId}-operator-${index}`} value={condition.operator} disabled={disabled} onChange={(_, data) => { const operator = data.value as TypedFilterOperator; updateCondition(index, { operator, upper: operator === 'between' ? blankValue(field.type) : undefined }); }}>{operators.map((operator) => <option key={operator} value={operator}>{t(`filter.operator.${operator}`)}</option>)}</Select></label>
          <label className="typed-filter-value" data-filter-field={condition.field}><span>{t('filter.value')}</span>{renderValue(condition, index, 'value')}</label>
          {condition.operator === 'between' && <label className="typed-filter-value" data-filter-field={condition.field}><span>{t('filter.upper')}</span>{renderValue(condition, index, 'upper')}</label>}
          <div className="typed-filter-row-actions"><Button type="button" appearance="secondary" disabled={disabled || index === 0} aria-label={t('common.moveUp')} onClick={() => setDraft((current) => { const conditions = [...current.conditions]; [conditions[index - 1], conditions[index]] = [conditions[index], conditions[index - 1]]; return { ...current, conditions }; })}>↑</Button><Button type="button" appearance="secondary" disabled={disabled || index + 1 === draft.conditions.length} aria-label={t('common.moveDown')} onClick={() => setDraft((current) => { const conditions = [...current.conditions]; [conditions[index], conditions[index + 1]] = [conditions[index + 1], conditions[index]]; return { ...current, conditions }; })}>↓</Button><Button type="button" appearance="secondary" disabled={disabled} aria-label={t('common.remove')} onClick={() => setDraft((current) => ({ ...current, conditions: current.conditions.filter((_, itemIndex) => itemIndex !== index) }))}>×</Button></div>
        </div>;
      })}</div>
      <div className="typed-filter-footer"><Button type="button" appearance="secondary" disabled={disabled || draft.conditions.length >= conditionBudget} onClick={() => setDraft((current) => ({ ...current, conditions: [...current.conditions, blankCondition(scope)] }))}>{t('filter.addCondition')}</Button><div className="typed-filter-footer-actions">{panelControls && <Button type="button" appearance="secondary" disabled={disabled || !hasActiveFilters} onClick={() => { onClear(); setOpen(false); anchor.current?.focus(); }}>{t('filter.clear')}</Button>}<Button type="button" disabled={disabled || busy} onClick={() => void apply()}>{t('filter.apply')}</Button></div></div>
      <section className="typed-filter-presets"><h3>{t('filter.saved')}</h3><div className="typed-filter-preset-list">{presets.named.map((preset) => <Button type="button" appearance="secondary" key={preset.name} onClick={() => loadPreset(preset.ast)}>{preset.name}</Button>)}{presets.recent.map((recent, index) => <Button type="button" appearance="secondary" key={`recent-${index}`} onClick={() => loadPreset(recent)}>{t('filter.recent')} {index + 1}</Button>)}</div><div className="typed-filter-save"><Input value={presetName} maxLength={80} onChange={(_, data) => setPresetName(data.value)} placeholder={t('filter.namePlaceholder')} /><Button type="button" appearance="secondary" disabled={busy || !presetName.trim()} onClick={() => void saveNamed()}>{t('filter.save')}</Button></div></section>
      {scope === 'requests' && <section className="typed-filter-assistant"><h3>{t('filter.assistant')}</h3>{!tenant ? <div className="empty">{t('filter.assistantTenantRequired')}</div> : assistantSettings === undefined ? <div className="muted">{t('common.loading')}</div> : assistantSettings === null ? <div className="empty">{t('filter.assistantNotConfigured')}</div> : <><label>{t('filter.assistantPrompt')}<Textarea value={assistantPrompt} maxLength={2000} onChange={(_, data) => setAssistantPrompt(data.value)} placeholder={t('filter.assistantPlaceholder')} /><small>{t('filter.assistantExecutionHint')}</small></label><Button type="button" appearance="secondary" disabled={busy || !assistantPrompt.trim()} onClick={() => void planWithAssistant()}>{t('filter.createPreview')}</Button>{assistantPlan && <div className="typed-filter-preview"><b>{t('filter.preview')}</b><div className="typed-filter-chips">{assistantPlan.ast.conditions.map((condition, index) => <span className="filter-chip" key={`${condition.field}-${index}`}>{conditionLabel(condition, t)}</span>)}</div><Button type="button" onClick={() => { setDraft(assistantPlan.ast); setAssistantPlan(undefined); }}>{t('filter.usePreview')}</Button></div>}</>}</section>}
    </section>}
  </div>;
}
