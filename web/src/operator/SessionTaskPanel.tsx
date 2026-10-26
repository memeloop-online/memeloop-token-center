import { useEffect, useMemo, useRef, useState } from 'react';
import { ToggleButton } from '@fluentui/react-components';
import { api } from '../api.js';
import { Button, Checkbox, Combobox, LoadingProgress, LoadingState, Option } from '../design-system';
import { formatCurrencyDisplay, formatMetricDisplay } from '../format.js';
import { useI18n } from '../i18n.js';
import { credentialDisplayName } from '../identityPresentation.js';
import { unnamedSessionName } from '../sessionTitles.js';
import { analyzeSessionTasks, microsToCostString, sessionRelationships } from './sessionTasks.js';
import type {
  LogicalSessionListResponse, LogicalSessionSummary, LogicalSessionSummaryBatchResponse, SessionIdentity,
  SessionTaskAnalysis, SessionTaskScope, SessionTaskUsage,
} from '../types.js';

function sessionLabel(session: LogicalSessionSummary, t: (key: string) => string, locale: 'en' | 'zh-CN') {
  return session.unlinked
    ? t('sessions.unlinkedRequests')
    : session.session_name?.trim()
      || unnamedSessionName(t, locale, session.last_activity_at, credentialDisplayName(session.key_alias, t), true);
}

function TaskCosts({ costs }: { costs: Record<string, number> }) {
  const { locale } = useI18n();
  const entries = Object.entries(costs).sort(([left], [right]) => left.localeCompare(right));
  if (!entries.length) return <span>—</span>;
  return <span className="usage-cost-lines">{entries.map(([currency, micros]) => {
    const display = formatCurrencyDisplay(microsToCostString(micros), currency, locale);
    return <span key={currency} title={display.title}>{display.text}</span>;
  })}</span>;
}

function UsageTotals({ usage, unknownNote }: { usage: SessionTaskUsage; unknownNote?: string }) {
  const { locale, t } = useI18n();
  const requests = formatMetricDisplay(usage.requests, locale);
  const tokens = formatMetricDisplay(usage.input_tokens + usage.output_tokens, locale);
  return <>
    <span><small>{t('usage.requests')}</small><b title={requests.title}>{requests.text}</b></span>
    <span><small>{t('usage.tokens')}</small><b title={tokens.title}>{tokens.text}</b></span>
    <span><small>{t('sessions.cost')}</small><b><TaskCosts costs={usage.costs} /></b>{unknownNote && <small className="mtc-secondary-text">{unknownNote}</small>}</span>
  </>;
}

function AnalysisSummary({ analysis }: { analysis: SessionTaskAnalysis }) {
  const { locale, t } = useI18n();
  const at = (epoch: number | null) => epoch == null ? '—' : new Date(epoch).toLocaleString(locale);
  return <>
    <p className="session-card-metrics usage-task-metrics">
      <UsageTotals usage={analysis.totals}
        unknownNote={analysis.totals.unknown_cost_requests > 0 ? t('tasks.unknownCost', { count: analysis.totals.unknown_cost_requests }) : undefined} />
      <span><small>{t('tasks.firstActivity')}</small><b>{at(analysis.first_request_at)}</b></span>
      <span><small>{t('tasks.lastActivity')}</small><b>{at(analysis.last_request_at)}</b></span>
      <span><small>{t('tasks.spanDays')}</small><b>{formatMetricDisplay(analysis.calendar_span_days, locale).text}</b></span>
      <span><small>{t('tasks.activeDays')}</small><b>{formatMetricDisplay(analysis.active_days, locale).text}</b></span>
    </p>
    <p className="muted">{t('tasks.dayBoundary', { zone: analysis.timezone })} · {t('tasks.coverageNote')}</p>
    {analysis.daily.length > 0 && <div className="table-scroll"><table>
      <thead><tr><th>{t('tasks.dailyTrend')}</th><th>{t('usage.requests')}</th><th>{t('usage.tokens')}</th><th>{t('sessions.cost')}</th></tr></thead>
      <tbody>{analysis.daily.map((day) => {
        const date = new Date(day.day_start_at).toLocaleDateString(locale, { timeZone: analysis.timezone });
        const requests = formatMetricDisplay(day.usage.requests, locale);
        const tokens = formatMetricDisplay(day.usage.input_tokens + day.usage.output_tokens, locale);
        return <tr key={day.day_bucket}><td>{date}</td><td title={requests.title}>{requests.text}</td><td title={tokens.title}>{tokens.text}</td><td><TaskCosts costs={day.usage.costs} /></td></tr>;
      })}</tbody>
    </table></div>}
  </>;
}

type AnalysisState =
  | { key: string; phase: 'loading' }
  | { key: string; phase: 'ready'; value: SessionTaskAnalysis }
  | { key: string; phase: 'failed'; error: string };

function identityKey(identity: SessionIdentity) {
  return `${identity.key_id}\0${identity.session_id}`;
}

export function SessionTaskPanel({ token, tenant }: { token: string; tenant: string }) {
  const { locale, t } = useI18n();
  const [query, setQuery] = useState('');
  const [results, setResults] = useState<LogicalSessionSummary[]>([]);
  const [searching, setSearching] = useState(false);
  const [searchError, setSearchError] = useState(false);
  const [selected, setSelected] = useState<LogicalSessionSummary>();
  const [scope, setScope] = useState<SessionTaskScope | 'separate'>('session');
  const [includeSubtrees, setIncludeSubtrees] = useState(false);
  const [analysis, setAnalysis] = useState<AnalysisState>();
  const [separate, setSeparate] = useState<Map<string, AnalysisState>>(new Map());
  const [separateProgress, setSeparateProgress] = useState<{ done: number; total: number }>();
  const [relatedOptions, setRelatedOptions] = useState<SessionIdentity[]>();
  const [relatedChecked, setRelatedChecked] = useState<Set<string>>(new Set());
  const analysisSequence = useRef(0);
  const analysisRequest = useRef<AbortController | undefined>(undefined);
  const relationRequest = useRef<AbortController | undefined>(undefined);
  const [cancelled, setCancelled] = useState(false);
  const [relationError, setRelationError] = useState(false);
  const [labels, setLabels] = useState<Map<string, string>>(new Map());

  const search = query.trim();
  useEffect(() => {
    setResults([]);
    setSearchError(false);
    if (!token.trim() || !search) { setResults([]); setSearching(false); return; }
    const controller = new AbortController();
    const timer = setTimeout(() => {
      setSearching(true);
      const params = new URLSearchParams({ limit: '10', q: search });
      if (tenant) params.set('tenant_external_id', tenant);
      void api<LogicalSessionListResponse>(`/internal/v1/sessions?${params}`, token.trim(), { signal: controller.signal })
        .then((page) => {
          if (controller.signal.aborted) return;
          const sessions = page.sessions.filter((session) => !session.unlinked);
          setResults(sessions);
          setLabels((current) => {
            const next = new Map(current);
            for (const session of sessions) next.set(identityKey(session), sessionLabel(session, t, locale));
            return next;
          });
        })
        .catch(() => { if (!controller.signal.aborted) setSearchError(true); })
        .finally(() => { if (!controller.signal.aborted) setSearching(false); });
    }, 300);
    return () => { clearTimeout(timer); controller.abort(); };
  }, [token, tenant, search, t, locale]);

  const selectedIdentity = useMemo<SessionIdentity | undefined>(() => selected
    ? { key_id: selected.key_id, session_id: selected.session_id }
    : undefined, [selected]);

  // Related scope needs the persisted relation set before it can aggregate.
  useEffect(() => {
    if (scope !== 'related' || !selectedIdentity || cancelled) return;
    const controller = new AbortController();
    relationRequest.current = controller;
    void sessionRelationships(tenant, token, selectedIdentity, controller.signal)
      .then((response) => {
        if (controller.signal.aborted) return;
        const peers = response.relationships
          .filter((row) => row.kind === 'related')
          .map((row) => row.key_id === selectedIdentity.key_id && row.session_id === selectedIdentity.session_id
            ? row.target
            : { key_id: row.key_id, session_id: row.session_id });
        setRelatedOptions(peers);
        setRelatedChecked(new Set(peers.map(identityKey)));
        if (peers.length > 0) {
          void api<LogicalSessionSummaryBatchResponse>('/internal/v1/sessions/summaries', token.trim(), {
            method: 'POST',
            body: JSON.stringify({ ...(tenant ? { tenant_external_id: tenant } : {}), identities: peers, state: 'all' }),
            signal: controller.signal,
          }).then((summaries) => {
            if (controller.signal.aborted) return;
            setLabels((current) => {
              const next = new Map(current);
              for (const session of summaries.sessions) next.set(identityKey(session), sessionLabel(session, t, locale));
              return next;
            });
          }).catch(() => { /* Labels degrade to stable IDs; the rollup still works. */ });
        }
      })
      .catch(() => { if (!controller.signal.aborted) setRelationError(true); });
    return () => controller.abort();
  }, [scope, selectedIdentity, tenant, token, t, locale, cancelled]);

  useEffect(() => {
    if (!selectedIdentity || cancelled) { setAnalysis(undefined); setSeparate(new Map()); return; }
    const sequence = ++analysisSequence.current;
    const controller = new AbortController();
    analysisRequest.current = controller;
    const run = async () => {
      if (scope === 'separate') {
        setAnalysis({ key: 'separate', phase: 'loading' });
        setSeparate(new Map());
        setSeparateProgress(undefined);
        try {
          const tree = await analyzeSessionTasks(tenant, token, [selectedIdentity], 'subtree', controller.signal);
          if (controller.signal.aborted || sequence !== analysisSequence.current) return;
          const members = tree.identities.length > 0 ? tree.identities : [selectedIdentity];
          const summaries = await api<LogicalSessionSummaryBatchResponse>('/internal/v1/sessions/summaries', token.trim(), {
            method: 'POST',
            body: JSON.stringify({ ...(tenant ? { tenant_external_id: tenant } : {}), identities: members, state: 'all' }),
            signal: controller.signal,
          });
          if (controller.signal.aborted) return;
          setLabels((current) => {
            const next = new Map(current);
            for (const session of summaries.sessions) next.set(identityKey(session), sessionLabel(session, t, locale));
            return next;
          });
          setSeparateProgress({ done: 0, total: members.length });
          const states = new Map<string, AnalysisState>();
          for (const member of members) {
            const key = identityKey(member);
            try {
              const value = await analyzeSessionTasks(tenant, token, [member], 'session', controller.signal);
              states.set(key, { key, phase: 'ready', value });
            } catch (reason) {
              states.set(key, { key, phase: 'failed', error: reason instanceof Error ? reason.message : t('tasks.loadFailed') });
            }
            if (controller.signal.aborted || sequence !== analysisSequence.current) return;
            setSeparate(new Map(states));
            setSeparateProgress({ done: states.size, total: members.length });
          }
          setAnalysis({ key: 'separate', phase: 'ready', value: tree });
        } catch (reason) {
          if (!controller.signal.aborted && sequence === analysisSequence.current) {
            setAnalysis({ key: 'separate', phase: 'failed', error: reason instanceof Error ? reason.message : t('tasks.loadFailed') });
          }
        }
        return;
      }
      if (scope === 'related') {
        if (relatedOptions === undefined) { setAnalysis(undefined); return; }
        const identities = [selectedIdentity, ...relatedOptions.filter((option) => relatedChecked.has(identityKey(option)))];
        if (identities.length === 1) { setAnalysis(undefined); return; }
        const key = `related:${identities.map(identityKey).sort().join('|')}`;
        setAnalysis({ key, phase: 'loading' });
        try {
          const value = await analyzeSessionTasks(tenant, token, identities, 'related', controller.signal, includeSubtrees);
          if (!controller.signal.aborted && sequence === analysisSequence.current) setAnalysis({ key, phase: 'ready', value });
        } catch (reason) {
          if (!controller.signal.aborted && sequence === analysisSequence.current) {
            setAnalysis({ key, phase: 'failed', error: reason instanceof Error ? reason.message : t('tasks.loadFailed') });
          }
        }
        return;
      }
      const key = `${scope}:${identityKey(selectedIdentity)}`;
      setAnalysis({ key, phase: 'loading' });
      try {
        const value = await analyzeSessionTasks(tenant, token, [selectedIdentity], scope, controller.signal);
        if (!controller.signal.aborted && sequence === analysisSequence.current) setAnalysis({ key, phase: 'ready', value });
      } catch (reason) {
        if (!controller.signal.aborted && sequence === analysisSequence.current) {
          setAnalysis({ key, phase: 'failed', error: reason instanceof Error ? reason.message : t('tasks.loadFailed') });
        }
      }
    };
    void run();
    return () => controller.abort();
  }, [selectedIdentity, scope, relatedOptions, relatedChecked, includeSubtrees, tenant, token, t, cancelled, locale]);

  const clearAnalysis = () => {
    analysisRequest.current?.abort();
    relationRequest.current?.abort();
    analysisSequence.current += 1;
    setAnalysis(undefined);
    setSeparate(new Map());
    setSeparateProgress(undefined);
    setCancelled(false);
    setRelationError(false);
  };

  const selectSession = (session: LogicalSessionSummary) => {
    clearAnalysis();
    setSelected(session);
    setScope('session');
    setQuery('');
    setRelatedOptions(undefined);
    setRelatedChecked(new Set());
  };

  const analysisLoading = analysis?.phase === 'loading';
  const relationsLoading = scope === 'related' && relatedOptions === undefined && !relationError && !cancelled;
  const scopeButton = (value: SessionTaskScope | 'separate', label: string) => (
    <ToggleButton appearance="secondary" checked={scope === value} disabled={!selected}
      onClick={() => {
        if (value === scope && !cancelled) return;
        clearAnalysis();
        setRelatedOptions(undefined);
        setScope(value);
      }}>{label}</ToggleButton>
  );
  return <article className="panel usage-task-panel" aria-label={t('tasks.title')}>
    <div className="panel-title"><h2>{t('tasks.title')}</h2></div>
    <label>{t('tasks.select')}
      <Combobox aria-label={t('tasks.select')} inlinePopup value={query}
        placeholder={t('sessions.searchPlaceholder')}
        input={{ onChange: (event) => setQuery(event.target.value) }}
        onOptionSelect={(_, data) => {
          const match = results.find((session) => `${session.key_id}:${session.session_id}` === data.optionValue);
          if (match) selectSession(match);
        }}>
        {results.map((session) => {
          const value = `${session.key_id}:${session.session_id}`;
          const label = `${sessionLabel(session, t, locale)} · ${credentialDisplayName(session.key_alias, t)} · ${session.key_id.slice(-8)}/${session.session_id.slice(-8)}`;
          return <Option key={value} value={value} text={label}>{label}</Option>;
        })}
      </Combobox>
    </label>
    {searching && <LoadingProgress active label={t('common.loading')} level="section" />}
    {searchError && <p role="alert">{t('sessions.loadFailed')}</p>}
    {selected && <>
      <p><b>{sessionLabel(selected, t, locale)}</b> <span className="muted">{credentialDisplayName(selected.key_alias, t)}</span></p>
      <div className="segmented" role="group" aria-label={t('tasks.title')}>
        {scopeButton('session', t('tasks.scope.session'))}
        {scopeButton('subtree', t('tasks.scope.subtree'))}
        {scopeButton('separate', t('tasks.scope.separate'))}
        {scopeButton('related', t('sessions.rel.related'))}
      </div>
      {scope === 'related' && !cancelled && <Checkbox checked={includeSubtrees} label={t('tasks.scope.subtree')}
        onChange={(_, data) => setIncludeSubtrees(data.checked === true)} />}
      {scope === 'related' && !cancelled && relatedOptions !== undefined && (
        relatedOptions.length === 0
          ? <p className="muted">{t('tasks.relatedEmpty')}</p>
          : <div>{relatedOptions.map((option) => {
              const key = identityKey(option);
              return <Checkbox key={key} checked={relatedChecked.has(key)}
                label={`${labels.get(key) ?? t('sessions.logicalSession')} · ${option.key_id.slice(-8)}/${option.session_id.slice(-8)}`}
                onChange={(_, data) => {
                  const next = new Set(relatedChecked);
                  if (data.checked === true) next.add(key); else next.delete(key);
                  setRelatedChecked(next);
                }} />;
            })}</div>
      )}
      {(analysisLoading || relationsLoading) && <>
        <LoadingState label={t('common.loading')} variant="detail" />
        <Button appearance="secondary" onClick={() => { clearAnalysis(); setCancelled(true); }}>{t('common.cancel')}</Button>
      </>}
      {(relationError || cancelled) && <p role="status">{t(cancelled ? 'tasks.cancelled' : 'sessions.rel.failed')} <Button appearance="secondary"
        onClick={() => { clearAnalysis(); setRelatedOptions(undefined); setSelected((current) => current && { ...current }); }}>{t('sessions.retryLoad')}</Button></p>}
      {analysis?.phase === 'failed' && <div className="notice error" role="alert"><span>{analysis.error}</span>
        <Button appearance="secondary" onClick={() => { setSelected((current) => current && { ...current }); }}>{t('sessions.retryLoad')}</Button></div>}
      {analysis?.phase === 'ready' && scope !== 'separate' && <AnalysisSummary analysis={analysis.value} />}
      {scope === 'separate' && <>
        {separateProgress && separateProgress.done < separateProgress.total && <p className="muted" role="status">{t('tasks.separateProgress', { done: separateProgress.done, total: separateProgress.total })}</p>}
        {separate.size > 0 && <h3>{t('tasks.perSession')}</h3>}
        {[...separate.values()].map((entry) => <section key={entry.key} className="usage-task-separate">
          <h4>{labels.get(entry.key) ?? entry.key.replace('\0', ' · ')}</h4>
          {entry.phase === 'ready'
            ? <AnalysisSummary analysis={entry.value} />
            : entry.phase === 'failed'
              ? <div className="notice error" role="alert">{entry.error}</div>
              : <LoadingState label={t('common.loading')} variant="detail" />}
        </section>)}
      </>}
    </>}
  </article>;
}
