import { useEffect, useId, useMemo, useState, type ReactNode } from 'react';
import { ApiError, api } from '../api';
import { Button, Input, LoadingState, Select, Textarea } from '../design-system';
import { useI18n } from '../i18n';
import { ModelPicker } from '../ModelPicker';
import { useAnchoredPopover } from '../useAnchoredPopover';
import { FilterCredentialAliasPicker } from './FilterCredentialAliasPicker';
import { FilterTimeRangeControls, type QuickRangePreset } from './FilterTimeRangeControls';
import { filterModelNamesUrl } from './filterModelCatalog';
import { filterPresetPresentation } from './filterPresetPresentation';
import { defaultUsageSelection, localDateTimeInput, usageAstFromSelection, usageRange, usageSelectionFromAst, usageSelectionFromDraft, type UsageSelection } from './usageState';
import type {
  FilterAssistantPlan, FilterAssistantSettings, FilterPresetState, RequestListCursor,
  TypedFilterAst, TypedFilterCondition, TypedFilterField, TypedFilterOperator, TypedFilterValue,
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
  { id: 'key_alias', type: 'text', usage: true },
  { id: 'principal', type: 'text', usage: false },
  { id: 'session_association', type: 'text', usage: false },
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
  if (field.id === 'session_association') return exactOperators;
  if (field.type === 'timestamp' || field.type === 'integer' || field.type === 'money_micros') return numericOperators;
  if (field.type === 'text' || field.type === 'model') return textOperators;
  return exactOperators;
}

function conditionLabel(condition: TypedFilterCondition, t: (key: string) => string) {
  const value = typeof condition.value.value === 'number'
    ? (condition.value.type === 'timestamp' ? new Date(condition.value.value).toLocaleString() : String(condition.value.value))
    : condition.field === 'session_association' ? t(`filter.association.${condition.value.value}`) : condition.value.value;
  const upper = condition.upper && typeof condition.upper.value === 'number'
    ? (condition.upper.type === 'timestamp' ? new Date(condition.upper.value).toLocaleString() : String(condition.upper.value))
    : condition.upper?.value;
  return `${t(`filter.field.${condition.field}`)} ${t(`filter.operator.${condition.operator}`)} ${value}${upper === undefined ? '' : ` – ${upper}`}`;
}

function isBetweenTimeRange(condition: TypedFilterCondition) {
  return condition.field === 'created_at' && condition.operator === 'between'
    && condition.value.type === 'timestamp' && condition.upper?.type === 'timestamp';
}

function timeRangeCondition(from: number, to: number): TypedFilterCondition {
  return { field: 'created_at', operator: 'between', value: { type: 'timestamp', value: from }, upper: { type: 'timestamp', value: to } };
}

/** Update the single between created_at row in place, or insert one first. */
function upsertTimeRange(ast: TypedFilterAst, from: number, to: number): TypedFilterAst {
  const index = ast.conditions.findIndex(isBetweenTimeRange);
  const condition = timeRangeCondition(from, to);
  if (index < 0) return { ...ast, conditions: [condition, ...ast.conditions] };
  return { ...ast, conditions: ast.conditions.map((current, itemIndex) => itemIndex === index ? condition : current) };
}

/** A quick-range choice owns the period: every existing time row collapses to the chosen window. */
function replaceTimeRange(ast: TypedFilterAst, from: number, to: number): TypedFilterAst {
  const first = ast.conditions.findIndex((condition) => condition.field === 'created_at');
  const rest = ast.conditions.filter((condition) => condition.field !== 'created_at');
  rest.splice(first < 0 ? 0 : Math.min(first, rest.length), 0, timeRangeCondition(from, to));
  return { ...ast, conditions: rest };
}

/**
 * Request and usage filters match the public model name recorded on each
 * activity fact — the same name across every upstream account and channel.
 * Suggestions are a flat, distinct list of those public names; they carry no
 * account, provider, or availability badge because the field deliberately
 * does not restrict the route. A separate upstream-account condition remains
 * the intentional AND restriction. Manually entered historical names and
 * "contains" fragments always stay valid.
 */
function CatalogModelPicker({ disabled, onSelect, tenant, token, value }: {
  disabled: boolean; onSelect: (model: string) => void; tenant: string; token: string; value: string;
}) {
  const { t } = useI18n();
  const [opened, setOpened] = useState(false);
  const [names, setNames] = useState<string[]>([]);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  // The editable picker's text is the controlled value itself, so it is the
  // search source of truth: typing updates it via onChange and loading a
  // preset replaces it via props. Both paths must refresh suggestions.
  const search = value.trim();
  useEffect(() => {
    // No request before the picker is first opened.
    if (!opened || !token) return;
    const controller = new AbortController();
    setNames([]); setLoading(true); setError('');
    const timer = setTimeout(() => {
      void api<string[]>(filterModelNamesUrl(tenant, search), token, { signal: controller.signal })
        .then((next) => {
          if (controller.signal.aborted) return;
          setNames([...new Set(next.filter((name) => typeof name === 'string' && name.trim()))]);
        })
        // A failed suggestion read must not clear the entered or selected name.
        .catch((reason) => { if (!controller.signal.aborted) setError(reason instanceof ApiError && reason.status === 403 ? t('filter.catalogDenied') : t('filter.catalogInputFallback')); })
        .finally(() => { if (!controller.signal.aborted) setLoading(false); });
    }, 200);
    return () => { clearTimeout(timer); controller.abort(); };
  }, [opened, tenant, token, search, t]);
  const options = useMemo(() => names.map((name) => ({ key: name, value: name, label: name, provider: '', upstream: '' })), [names]);

  return <div className="typed-filter-model-picker">
    <ModelPicker label={t('filter.inputOrChooseModel')} editable groupBy="none" disabled={disabled} value={value} onChange={onSelect} options={options} loading={loading} error={error} emptyText={t('filter.catalogInputEmpty')} onOpen={() => setOpened(true)} />
  </div>;
}

export function TypedFilterBuilder({ ast, onApply, onClear, scope, token, tenant, upstreams, externalChips = [], usageSelection, disabled = false }: {
  ast: TypedFilterAst;
  onApply: (ast: TypedFilterAst, selection?: UsageSelection) => void;
  onClear: () => void;
  scope: BuilderScope;
  token: string;
  tenant: string;
  upstreams: UpstreamAccount[];
  /** Read-only API filters that cannot be represented by the UUID-only AST. */
  externalChips?: Array<{ id: string; label: string }>;
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
  const [draftPreset, setDraftPreset] = useState<QuickRangePreset>('custom');
  const [presets, setPresets] = useState<FilterPresetState>({ named: [], recent: [] });
  const [presetName, setPresetName] = useState('');
  const [assistantPrompt, setAssistantPrompt] = useState('');
  const [assistantPlan, setAssistantPlan] = useState<FilterAssistantPlan>();
  const [assistantSettings, setAssistantSettings] = useState<FilterAssistantSettings | null>();
  const [error, setError] = useState('');
  const [busy, setBusy] = useState(false);
  const visibleFields = scope === 'usage' ? fields.filter((field) => field.usage) : fields;
  const hasActiveFilters = (scope === 'usage'
    ? ast.conditions.some((condition) => condition.field !== 'created_at')
      || Boolean(usageSelection && (usageSelection.preset !== '24h' || usageSelection.granularity !== 'auto'))
    : ast.conditions.length > 0) || externalChips.length > 0;
  const conditionBudget = 12;
  const budgetError = locale === 'zh-CN' ? '筛选条件最多 12 条（包括时间范围和客户端凭据）。' : 'Filters allow at most 12 conditions, including the period and client credential.';
  const usageAstError = locale === 'zh-CN' ? '用量分析不支持重复字段或无法表示的筛选条件。' : 'Usage analysis cannot apply duplicate fields or unsupported conditions.';
  const unassignedError = locale === 'zh-CN' ? '未分配上游可用于查询，但无法保存为类型化筛选。' : 'Unassigned upstream can be queried, but cannot be saved as a typed filter.';
  useEffect(() => { setOpen(false); setAssistantPlan(undefined); }, [tenant, token]);

  // The editor is a draft.  Synchronizing it in a passive effect while it is
  // closed races a clear followed immediately by opening and adding a row: a
  // queued clear can erase that new row.  Initialize from the applied AST at
  // the explicit open boundary instead, so recents and in-dialog edits remain
  // local to the current editor session.
  const openEditor = () => {
    setDraft(ast);
    if (scope === 'usage') setDraftUsage(usageSelection ?? defaultUsageSelection());
    else setDraftPreset(ast.conditions.some(isBetweenTimeRange) ? 'custom' : 'alltime');
    setError('');
    setOpen(true);
  };
  useEffect(() => {
    if (!open || !token) return;
    const controller = new AbortController();
    const query = tenant ? `?tenant_external_id=${encodeURIComponent(tenant)}` : '';
    void api<FilterPresetState>(`/internal/v1/filter-presets${query}`, token, { signal: controller.signal })
      .then((next) => { if (!controller.signal.aborted) setPresets(next); }).catch((reason: unknown) => { if (!controller.signal.aborted) setError(reason instanceof Error ? reason.message : t('common.requestFailed')); });
    return () => controller.abort();
  }, [open, tenant, token]);
  useEffect(() => {
    if (!open || !token || !tenant) { setAssistantSettings(undefined); return; }
    const controller = new AbortController();
    void api<FilterAssistantSettings | null>(`/internal/v1/filter-assistant/settings?tenant_external_id=${encodeURIComponent(tenant)}`, token, { signal: controller.signal })
      .then((next) => { if (!controller.signal.aborted) setAssistantSettings(next); }).catch(() => { if (!controller.signal.aborted) setAssistantSettings(null); });
    return () => controller.abort();
  }, [open, tenant, token]);

  const updateCondition = (index: number, patch: Partial<TypedFilterCondition>) => {
    setDraft((current) => ({ ...current, conditions: current.conditions.map((condition, itemIndex) => itemIndex === index ? { ...condition, ...patch } : condition) }));
  };
  const selectField = (index: number, fieldId: TypedFilterField) => {
    const field = fieldDefinition(fieldId); const options = operatorsFor(field, scope); const operator = options[0];
    const value: TypedFilterValue = field.id === 'session_association' ? { type: 'text', value: 'unlinked' } : blankValue(field.type);
    updateCondition(index, { field: field.id, operator, value, upper: operator === 'between' ? blankValue(field.type) : undefined });
    if (draft.conditions[index]?.field === 'created_at' || field.id === 'created_at') { setDraftPreset('custom'); setDraftUsage((current) => ({ ...current, preset: 'custom' })); }
  };
  const setValue = (index: number, side: 'value' | 'upper', value: TypedFilterValue) => {
    const next = { ...draft, conditions: draft.conditions.map((condition, itemIndex) => itemIndex !== index ? condition : side === 'value' ? { ...condition, value } : { ...condition, upper: value }) };
    setDraft(next);
    // A hand-edited time row is an explicit custom range, never a quick preset.
    if (draft.conditions[index]?.field === 'created_at') {
      setDraftPreset('custom');
      const period = next.conditions.find(isBetweenTimeRange);
      if (period) setDraftUsage((current) => ({ ...current, preset: 'custom', customFrom: localDateTimeInput(Number(period.value.value)), customTo: localDateTimeInput(Number(period.upper?.value)) }));
    }
  };
  const activePreset: QuickRangePreset = scope === 'usage' ? draftUsage.preset : draftPreset;
  const periodCondition = draft.conditions.find(isBetweenTimeRange);
  const selectPreset = (preset: QuickRangePreset) => {
    // All time removes only the time rows; model, credential, and other
    // conditions stay exactly as entered.  Usage analysis has no open-ended
    // queries, so it never offers this choice.
    if (preset === 'alltime') {
      setDraft((current) => ({ ...current, conditions: current.conditions.filter((condition) => condition.field !== 'created_at') }));
      setDraftPreset('alltime');
      return;
    }
    const resolvedAt = Date.now();
    if (preset === 'custom') {
      const range = periodCondition
        ? { from: Number(periodCondition.value.value), to: Number(periodCondition.upper?.value) }
        : { from: resolvedAt - 86_400_000, to: resolvedAt };
      setDraft((current) => upsertTimeRange(current, range.from, range.to));
      setDraftPreset('custom');
      setDraftUsage((current) => ({ ...current, preset: 'custom', customFrom: localDateTimeInput(range.from), customTo: localDateTimeInput(range.to) }));
      return;
    }
    const range = usageRange({ preset, customFrom: '', customTo: '', resolvedAt });
    if (!range) return;
    setDraft((current) => replaceTimeRange(current, range.from, range.to));
    setDraftPreset(preset);
    setDraftUsage((current) => ({ ...current, preset, customFrom: localDateTimeInput(range.from), customTo: localDateTimeInput(range.to), resolvedAt }));
  };
  const apply = async () => {
    const nextSelection = scope === 'usage' ? usageSelectionFromDraft(draftUsage, draft) : undefined;
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
    const nextSelection = scope === 'usage' ? usageSelectionFromDraft(draftUsage, draft) : undefined;
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
    if (scope !== 'usage') {
      setDraft(preset);
      setDraftPreset(preset.conditions.some(isBetweenTimeRange) ? 'custom' : 'alltime');
      setError('');
      return;
    }
    const nextSelection = usageSelectionFromAst(defaultUsageSelection(), preset);
    if (!nextSelection) { setError(usageAstError); return; }
    setDraft(preset);
    setDraftUsage(nextSelection); setError('');
  };

  const renderValue = (condition: TypedFilterCondition, index: number, side: 'value' | 'upper') => {
    const value = side === 'value' ? condition.value : condition.upper;
    if (!value) return null;
    if (condition.field === 'session_association') return <Select value={String(value.value)} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'text', value: data.value })}>{['unlinked', 'confirmed'].map(association => <option key={association} value={association}>{t(`filter.association.${association}`)}</option>)}</Select>;
    if (value.type === 'model') return <CatalogModelPicker disabled={disabled} onSelect={(model) => setValue(index, side, { type: 'model', value: model })} tenant={tenant} token={token} value={value.value} />;
    if (value.type === 'protocol') return <Select value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'protocol', value: data.value as 'openai' | 'anthropic' | 'openai-image' | 'audio-transcription' | 'generation' })}><option value="openai">OpenAI</option><option value="anthropic">Anthropic</option><option value="openai-image">OpenAI Images</option><option value="audio-transcription">OpenAI Audio</option><option value="generation">{t('routes.generation')}</option></Select>;
    if (value.type === 'status') return <Select value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'status', value: data.value as 'success' | 'error' | 'pending' })}><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></Select>;
    if (condition.field === 'upstream_account_id') return <Select value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: 'uuid', value: data.value })}><option value="">{t('common.select')}</option>{upstreams.filter((account) => account.status === 'active').map((account) => <option value={account.id} key={account.id}>{account.name}</option>)}</Select>;
    if (value.type === 'timestamp') return <Input type="datetime-local" step="0.001" value={localDateTimeInput(value.value)} disabled={disabled} onChange={(_, data) => { const next = Date.parse(data.value); if (Number.isFinite(next)) setValue(index, side, { type: 'timestamp', value: next }); }} />;
    if (condition.field === 'key_alias') return <FilterCredentialAliasPicker disabled={disabled} onChange={(alias) => setValue(index, side, { type: 'text', value: alias })} tenant={tenant} token={token} value={typeof value.value === 'string' ? value.value : ''} />;
    if (value.type === 'integer' || value.type === 'money_micros') return <Input type="number" step="1" value={String(value.value)} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: value.type, value: Number(data.value) })} />;
    return <Input value={value.value} disabled={disabled} onChange={(_, data) => setValue(index, side, { type: value.type, value: data.value } as TypedFilterValue)} placeholder={value.type === 'uuid' ? '019f…' : undefined} />;
  };

  return <div className="typed-filter-builder" style={scope === 'usage' ? { minWidth: 0 } : undefined}>
    <div className="typed-filter-chips" style={scope === 'usage' ? { minWidth: 0 } : undefined} aria-label={t('filter.applied')}>
      {ast.conditions.filter((condition) => scope !== 'usage' || condition.field !== 'created_at' || usageSelection?.preset !== '24h').map((condition, index) => <span className="filter-chip" key={`${condition.field}-${index}`}>{conditionLabel(condition, t)}</span>)}
      {externalChips.map((chip) => <span className="filter-chip" key={chip.id}>{chip.label}</span>)}
      {!hasActiveFilters && <span className="muted">{t('filter.noneApplied')}</span>}
    </div>
    <div className="typed-filter-actions"><Button ref={anchor} type="button" appearance="secondary" disabled={disabled} aria-haspopup="dialog" aria-expanded={open} aria-controls={popoverId} onClick={() => open ? setOpen(false) : openEditor()}>{t('filter.open')}</Button>{hasActiveFilters && <Button type="button" appearance="secondary" disabled={disabled} onClick={onClear}>{t('filter.clear')}</Button>}</div>
    {open && <section ref={panel} id={popoverId} popover="auto" style={position} className="typed-filter-dialog" role="dialog" aria-label={t('filter.title')} onKeyDown={(event) => { const target = event.target; const comboboxOpen = target instanceof HTMLElement && target.getAttribute('role') === 'combobox' && target.getAttribute('aria-expanded') === 'true'; if (event.key === 'Escape' && !comboboxOpen) { event.preventDefault(); event.stopPropagation(); setOpen(false); anchor.current?.focus(); } }} onToggle={(event) => { if (event.target === event.currentTarget && event.newState === 'closed') setOpen(false); }}>
      <div className="panel-title"><div><h2>{t('filter.title')}</h2><p className="muted">{t('filter.description')}</p></div><Button autoFocus type="button" appearance="secondary" onClick={() => { setOpen(false); anchor.current?.focus(); }}>{t('common.close')}</Button></div>
      {error && <div className="notice error" role="alert">{error}</div>}
      <FilterTimeRangeControls allowAllTime={scope === 'requests'} disabled={disabled} granularity={scope === 'usage' ? draftUsage.granularity : undefined} onGranularityChange={scope === 'usage' ? (granularity) => setDraftUsage((current) => ({ ...current, granularity })) : undefined} onSelectPreset={selectPreset} preset={activePreset} />
      <div className="typed-filter-rows">{draft.conditions.map((condition, index) => {
        const field = fieldDefinition(condition.field); const operators = operatorsFor(field, scope);
        // Field selection replaces only the value editor. Keeping the row
        // mounted preserves the freshly selected field while React swaps that
        // editor from a scalar input to the model-name picker.
        // The model-name picker labels its own editable input, which is the
        // cell's single visible label. Adding the outer value caption would
        // duplicate the label and merge both texts into the input's
        // accessible name, so the model cell uses a plain container that
        // keeps the same layout styling without a second label.
        const valueCell = (side: 'value' | 'upper', caption: string) => {
          const cellValue = side === 'value' ? condition.value : condition.upper;
          if (cellValue?.type === 'model') return <div className="typed-filter-value" data-filter-field={condition.field}>{renderValue(condition, index, side)}</div>;
          return <label className="typed-filter-value" data-filter-field={condition.field}><span>{caption}</span>{renderValue(condition, index, side)}</label>;
        };
        return <div className="typed-filter-row" key={index}>
          <label><span id={`${editorId}-field-${index}`}>{t('filter.field')}</span><Select aria-labelledby={`${editorId}-field-${index}`} value={condition.field} disabled={disabled} onChange={(_, data) => selectField(index, data.value as TypedFilterField)}>{visibleFields.map((option) => <option key={option.id} value={option.id}>{t(`filter.field.${option.id}`)}</option>)}</Select></label>
          <label><span id={`${editorId}-operator-${index}`}>{t('filter.operator')}</span><Select aria-labelledby={`${editorId}-operator-${index}`} value={condition.operator} disabled={disabled} onChange={(_, data) => { const operator = data.value as TypedFilterOperator; updateCondition(index, { operator, upper: operator === 'between' ? blankValue(field.type) : undefined }); }}>{operators.map((operator) => <option key={operator} value={operator}>{t(`filter.operator.${operator}`)}</option>)}</Select></label>
          {valueCell('value', t('filter.value'))}
          {condition.operator === 'between' && valueCell('upper', t('filter.upper'))}
          <div className="typed-filter-row-actions"><Button type="button" appearance="secondary" disabled={disabled || index === 0} aria-label={t('common.moveUp')} onClick={() => setDraft((current) => { const conditions = [...current.conditions]; [conditions[index - 1], conditions[index]] = [conditions[index], conditions[index - 1]]; return { ...current, conditions }; })} icon={<span aria-hidden="true">↑</span>} /><Button type="button" appearance="secondary" disabled={disabled || index + 1 === draft.conditions.length} aria-label={t('common.moveDown')} onClick={() => setDraft((current) => { const conditions = [...current.conditions]; [conditions[index], conditions[index + 1]] = [conditions[index + 1], conditions[index]]; return { ...current, conditions }; })} icon={<span aria-hidden="true">↓</span>} /><Button type="button" appearance="secondary" disabled={disabled} aria-label={t('common.remove')} onClick={() => setDraft((current) => ({ ...current, conditions: current.conditions.filter((_, itemIndex) => itemIndex !== index) }))} icon={<span aria-hidden="true">×</span>} /></div>
          {/* Row-level hint: it spans the full row below the controls so the
              explanation never changes the control baseline or height. */}
          {condition.value.type === 'model' && <div className="typed-filter-row-hint">{t('filter.modelInputHint')}</div>}
        </div>;
      })}</div>
      <div className="typed-filter-footer"><Button type="button" appearance="secondary" disabled={disabled || draft.conditions.length >= conditionBudget} onClick={() => setDraft((current) => ({ ...current, conditions: [...current.conditions, blankCondition(scope)] }))}>{t('filter.addCondition')}</Button><div className="typed-filter-footer-actions"><Button type="button" disabled={disabled || busy} onClick={() => void apply()}>{t('filter.apply')}</Button></div></div>
      <section className="typed-filter-presets"><h3>{t('filter.saved')}</h3><div className="typed-filter-preset-list">{presets.named.map((preset) => {
        const preview = filterPresetPresentation(preset.ast, locale, t, upstreams);
        return <Button type="button" appearance="secondary" className="typed-filter-preset" key={preset.name} title={`${preset.name}: ${preview.detail}`} aria-label={`${preset.name}: ${preview.detail}`} onClick={() => loadPreset(preset.ast)}><span className="typed-filter-preset-name">{preset.name}</span><span className="typed-filter-preset-summary">{preview.summary}</span></Button>;
      })}{presets.recent.map((recent, index) => {
        const preview = filterPresetPresentation(recent, locale, t, upstreams);
        return <Button type="button" appearance="secondary" className="typed-filter-preset" key={`recent-${index}`} title={preview.detail} aria-label={preview.detail} onClick={() => loadPreset(recent)}><span className="typed-filter-preset-summary">{preview.summary}</span></Button>;
      })}</div><div className="typed-filter-save"><Input value={presetName} maxLength={80} onChange={(_, data) => setPresetName(data.value)} placeholder={t('filter.namePlaceholder')} /><Button type="button" appearance="secondary" disabled={busy || !presetName.trim()} onClick={() => void saveNamed()}>{t('filter.save')}</Button></div></section>
      {scope === 'requests' && <section className="typed-filter-assistant"><h3>{t('filter.assistant')}</h3>{!tenant ? <div className="empty">{t('filter.assistantTenantRequired')}</div> : assistantSettings === undefined ? <LoadingState label={t('common.loading')} variant="compact" /> : assistantSettings === null ? <div className="empty">{t('filter.assistantNotConfigured')}</div> : <><label>{t('filter.assistantPrompt')}<Textarea value={assistantPrompt} maxLength={2000} onChange={(_, data) => setAssistantPrompt(data.value)} placeholder={t('filter.assistantPlaceholder')} /><small>{t('filter.assistantExecutionHint')}</small></label><Button type="button" appearance="secondary" disabled={busy || !assistantPrompt.trim()} onClick={() => void planWithAssistant()}>{t('filter.createPreview')}</Button>{assistantPlan && <div className="typed-filter-preview"><b>{t('filter.preview')}</b><div className="typed-filter-chips">{assistantPlan.ast.conditions.map((condition, index) => <span className="filter-chip" key={`${condition.field}-${index}`}>{conditionLabel(condition, t)}</span>)}</div><Button type="button" onClick={() => { setDraft(assistantPlan.ast); setDraftPreset(assistantPlan.ast.conditions.some(isBetweenTimeRange) ? 'custom' : 'alltime'); setAssistantPlan(undefined); }}>{t('filter.usePreview')}</Button></div>}</>}</section>}
    </section>}
  </div>;
}
