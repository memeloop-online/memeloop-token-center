import { DrawerFrame, RequestTable } from './components';
import type { CSSProperties } from 'react';
import { formatCurrency, formatMetricNumber, formatMilliseconds, formatPercent } from './format';
import { useI18n } from './i18n';
import type { LogicalSessionDetail, LogicalSessionSummary, RequestView, UsageAnalysisCost } from './types';

const semanticPalette = ['#6859d9', '#18a999', '#e68a2e', '#d74f70', '#4078c0', '#8a63b8'];

function decimalMicros(value: string): bigint | undefined {
  const match = /^(\d+)(?:\.(\d{1,6}))?$/.exec(value);
  if (!match) return undefined;
  return BigInt(match[1]) * 1_000_000n + BigInt((match[2] ?? '').padEnd(6, '0'));
}

function microsDecimal(value: bigint) {
  const whole = value / 1_000_000n;
  const fraction = String(value % 1_000_000n).padStart(6, '0').replace(/0+$/, '');
  return fraction ? `${whole}.${fraction}` : String(whole);
}

function SemanticExecutionPanel({ detail }: { detail: LogicalSessionDetail }) {
  const { locale, t } = useI18n();
  const observations = detail.requests
    .filter((request) => request.execution)
    .sort((left, right) => left.created_at - right.created_at || left.request_id.localeCompare(right.request_id));
  if (!observations.length) return <div className="semantic-empty"><b>{t('sessions.semantic')}</b><span>{t('sessions.semanticFallback')}</span></div>;

  const taskCounts = new Map<string, number>();
  for (const request of observations) {
    const kind = request.execution?.task_kind || t('sessions.taskUnclassified');
    taskCounts.set(kind, (taskCounts.get(kind) ?? 0) + 1);
  }
  const taskEntries = [...taskCounts].sort((left, right) => right[1] - left[1]);
  const total = taskEntries.reduce((sum, [, count]) => sum + count, 0);
  let cursor = 0;
  const stops = taskEntries.map(([, count], index) => {
    const start = cursor;
    cursor += count / total * 100;
    return `${semanticPalette[index % semanticPalette.length]} ${start}% ${cursor}%`;
  }).join(', ');
  const timelineStart = Math.min(...observations.map((request) => request.created_at));
  const timelineEnd = Math.max(...observations.map((request) => request.created_at + Math.max(0, request.duration_ms ?? 0)));
  const timelineDuration = Math.max(1, timelineEnd - timelineStart);
  const agentParents = new Map(observations.flatMap((request) => {
    const execution = request.execution;
    return execution?.agent_id && execution.parent_agent_id ? [[execution.agent_id, execution.parent_agent_id] as const] : [];
  }));
  const depthOf = (agent?: string | null) => {
    let depth = 0;
    let current = agent;
    const visited = new Set<string>();
    while (current && agentParents.has(current) && depth < 8 && !visited.has(current)) {
      visited.add(current); current = agentParents.get(current); depth += 1;
    }
    return depth;
  };
  const latestName = [...observations].reverse().find((request) => request.execution?.session_name)?.execution?.session_name;
  const latestTrace = [...observations].reverse().find((request) => request.execution?.trace_id)?.execution?.trace_id;
  const labels = new Map<string, string>();
  for (const request of observations) Object.entries(request.execution?.labels ?? {}).forEach(([key, value]) => labels.set(key, value));
  const agentCosts = new Map<string, bigint>();
  for (const request of observations) {
    const micros = decimalMicros(request.cost);
    if (!micros || !request.currency) continue;
    const agent = request.execution?.agent_id || t('sessions.agentUnknown');
    const key = `${request.currency}\0${agent}`;
    agentCosts.set(key, (agentCosts.get(key) ?? 0n) + micros);
  }

  return <section className="semantic-execution" aria-label={t('sessions.semantic')}>
    <div className="semantic-heading"><div><span className="eyebrow">{t('sessions.semantic')}</span><h3>{latestName || t('sessions.semanticDeclared')}</h3></div><span className="status ok">{t('sessions.declaredMetadata')}</span></div>
    {(latestTrace || labels.size > 0) && <div className="semantic-chips">
      {latestTrace && <span><small>{t('sessions.trace')}</small><code>{latestTrace}</code></span>}
      {[...labels].map(([key, value]) => <span key={key}><small>{key}</small><b>{value}</b></span>)}
    </div>}
    <div className="semantic-grid">
      <div className="execution-lanes"><div className="execution-title"><h4>{t('sessions.executionTimeline')}</h4><small>{t('sessions.timelineRange', { duration: formatMilliseconds(timelineDuration, locale) })}</small></div>{observations.map((request, index) => {
        const execution = request.execution!;
        const left = Math.min(99.5, Math.max(0, ((request.created_at - timelineStart) / timelineDuration) * 100));
        const rawWidth = (Math.max(0, request.duration_ms ?? 0) / timelineDuration) * 100;
        const width = Math.max(0.5, Math.min(100 - left, Math.max(2, rawWidth)));
        const depth = depthOf(execution.agent_id);
        return <div className="execution-lane" key={request.request_id} style={{ '--agent-depth': depth } as CSSProperties}>
          <span className="execution-agent"><b>{execution.agent_id || t('sessions.agentUnknown')}</b>{execution.parent_agent_id && <small>← {execution.parent_agent_id}</small>}</span>
          <span className="execution-track"><span className="execution-span" style={{ left: `${left}%`, width: `${width}%` }} title={`${new Date(request.created_at).toLocaleString(locale)} · ${request.duration_ms ?? 0} ms`}><span>{execution.task_kind || t('sessions.taskUnclassified')}</span><small>{index + 1} · {request.duration_ms ?? 0} ms</small></span></span>
        </div>;
      })}<div className="execution-axis" aria-hidden="true"><span>0</span><span>{formatMilliseconds(timelineDuration, locale)}</span></div></div>
      <div className="task-breakdown"><h4>{t('sessions.taskBreakdown')}</h4><div className="task-pie" style={{ background: `conic-gradient(${stops})` }} role="img" aria-label={t('sessions.taskBreakdown')} /><ul>{taskEntries.map(([kind, count], index) => <li key={kind}><i style={{ background: semanticPalette[index % semanticPalette.length] }} /><span>{kind}</span><b>{count}</b></li>)}</ul><small>{t('sessions.taskBreakdownBasis')}</small></div>
    </div>
    {agentCosts.size > 0 && <div className="agent-costs"><h4>{t('sessions.agentCosts')}</h4><div>{[...agentCosts].sort(([left], [right]) => left.localeCompare(right)).map(([key, micros]) => { const [currency, agent] = key.split('\0'); return <span key={key}><b>{agent}</b><small>{currency}</small><strong>{formatCurrency(microsDecimal(micros), currency, locale)}</strong></span>; })}</div><small>{t('sessions.agentCostsBasis')}</small></div>}
  </section>;
}

function SessionCosts({ values }: { values: UsageAnalysisCost[] }) {
  const { locale } = useI18n();
  if (!values.length) return <span>—</span>;
  return <span className="session-costs">{[...values]
    .sort((left, right) => left.currency.localeCompare(right.currency))
    .map((value) => <span key={value.currency} title={`${value.cost} ${value.currency}`}>{formatCurrency(value.cost, value.currency, locale)}</span>)}</span>;
}

function statusTone(status: LogicalSessionSummary['last_status']) {
  if (status === 'success') return 'ok';
  if (status === 'error') return 'bad';
  return 'pending';
}

function CopyDiagnostic({ value, kind }: { value: string; kind: 'session' | 'credential' }) {
  const { t } = useI18n();
  return <button type="button" className="secondary session-copy" aria-label={t(`sessions.copy.${kind}`)} onClick={() => void navigator.clipboard.writeText(value)}>{t(`sessions.copy.${kind}`)}</button>;
}

function SessionMetricNumber({ value }: { value: number }) {
  const { locale } = useI18n();
  const formatted = formatMetricNumber(value, locale);
  return <span title={formatted.title}>{formatted.text}</span>;
}

export function SessionList({ values, loading, showCredential, onSelect }: {
  values: LogicalSessionSummary[];
  loading: boolean;
  showCredential: boolean;
  onSelect: (session: LogicalSessionSummary) => void;
}) {
  const { locale, t } = useI18n();
  if (!values.length) return <div className="empty">{loading ? t('common.loading') : t('sessions.empty')}</div>;
  return <div className="session-list">{values.map((session) => {
    const title = session.unlinked
      ? t('sessions.unlinkedRequests')
      : session.session_name || t('sessions.sessionTitle', { model: session.model || t('common.none') });
    return <article className="session-card" key={`${session.key_id}:${session.session_id}`}>
      <div className="session-card-heading"><b>{title}</b><span>{session.task_kind && <span className="pill">{session.task_kind}</span>}<span className={`status ${statusTone(session.last_status)}`}>{t(`sessions.status.${session.last_status}`)}</span></span></div>
      {session.unlinked && <span className="session-unlinked-label">{t('sessions.unlinkedReason')}</span>}
      <span className="session-card-context">
        {showCredential && <span><small>{t('sessions.credential')}</small><b>{session.key_alias || t('common.none')}</b></span>}
        <span><small>{t('request.model')}</small><b>{session.model || t('common.none')}</b><code>{session.protocol || t('common.none')}</code></span>
        <span><small>{t('sessions.lastActivity')}</small><b>{new Date(session.last_activity_at).toLocaleString(locale)}</b></span>
      </span>
      <span className="session-card-metrics">
        <span><small>{t('sessions.requests')}</small><b><SessionMetricNumber value={session.requests} /></b></span>
        <span><small>{t('sessions.archivedOnly')}</small><b><SessionMetricNumber value={session.archived_only_requests} /></b></span>
        <span><small>{t('sessions.activeRequests')}</small><b><SessionMetricNumber value={session.active_requests} /></b></span>
        <span className={session.errors > 0 ? 'session-metric-negative' : undefined}><small>{t('sessions.errors')}</small><b><SessionMetricNumber value={session.errors} /></b></span>
        <span><small>{t('sessions.tokens')}</small><b><SessionMetricNumber value={session.input_tokens + session.output_tokens} /></b></span>
        <span><small>{t('sessions.averageLatency')}</small><b>{formatMilliseconds(session.avg_duration_ms, locale)}</b></span>
        <span><small>{t('sessions.cost')}</small><b><SessionCosts values={session.costs} /></b></span>
      </span>
      {session.archived_only_requests > 0 && <div className="session-archive-metrics"><b>{t('sessions.archiveOnlyTitle')}</b><span>{t('sessions.archiveOnlySummary', { requests: formatMetricNumber(session.archived_only_requests, locale).text, errors: formatMetricNumber(session.archived_only_errors, locale).text, tokens: formatMetricNumber(session.archived_only_input_tokens + session.archived_only_output_tokens, locale).text, latency: formatMilliseconds(session.archived_only_avg_duration_ms, locale) })}</span></div>}
      <div className="session-card-actions"><button type="button" disabled={loading} onClick={() => onSelect(session)} aria-label={t('sessions.open', { name: title })}>{t('sessions.openTimeline')}</button>{(showCredential || !session.unlinked) && <details><summary>{t('sessions.diagnostics')}</summary><code>{session.session_id}</code><CopyDiagnostic value={session.session_id} kind="session" />{showCredential && <><code>{session.key_id}</code><CopyDiagnostic value={session.key_id} kind="credential" /></>}</details>}</div>
    </article>;
  })}</div>;
}

export function SessionDrawer({ detail, summary, currency, showDiagnosticIds = false, loading, onLoadOlder, onSelect, onClose }: {
  detail: LogicalSessionDetail;
  summary?: LogicalSessionSummary;
  currency?: string;
  showDiagnosticIds?: boolean;
  loading: boolean;
  onLoadOlder: () => void;
  onSelect: (request: RequestView) => void;
  onClose: () => void;
}) {
  const { locale, t } = useI18n();
  const declaredSessionName = [...detail.requests].reverse().find((request) => request.execution?.session_name)?.execution?.session_name;
  const title = detail.unlinked ? t('sessions.unlinkedRequests') : declaredSessionName || summary?.model || t('sessions.logicalSession');
  const summaryCurrency = summary?.costs.length === 1 ? summary.costs[0].currency : undefined;
  const requestPositions = new Map(detail.requests.map((request, index) => [request.request_id, { index: index + 1, createdAt: request.created_at }]));
  const confirmedEdges = detail.edges.filter((edge) => edge.relation !== 'candidate');
  const candidateEdges = detail.edges.filter((edge) => edge.relation === 'candidate');
  const requestLabel = (requestId: string | null) => {
    if (!requestId) return t('sessions.root');
    const request = requestPositions.get(requestId);
    return request ? t('sessions.timelinePoint', { index: request.index, time: new Date(request.createdAt).toLocaleString(locale) }) : t('sessions.timelineRequest');
  };
  return <DrawerFrame title={title} eyebrow={t('sessions.logicalSession')} onClose={onClose}>
    {showDiagnosticIds && <details className="session-diagnostics"><summary>{t('sessions.diagnostics')}</summary><code className="break-anywhere">{detail.session_id}</code><CopyDiagnostic value={detail.session_id} kind="session" /></details>}
    {detail.unlinked && <div className="notice warning" role="status"><b>{t('sessions.unlinkedRequests')}</b><br />{t('sessions.unlinkedDetail')}</div>}
    {!detail.unlinked && <SemanticExecutionPanel detail={detail} />}
    <h3>{t('sessions.timeline')}</h3>
    {detail.has_more && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('sessions.loadEarlier')}</button></div>}
    <RequestTable requests={detail.requests} currency={currency ?? summaryCurrency} onSelect={onSelect} />
    <h3>{t('sessions.relationships')}</h3>
    {detail.edges_truncated && <div className="notice warning">{t('sessions.edgesTruncated')}</div>}
    <div className="edge-list">{confirmedEdges.map((edge) => <div className="edge" key={`${edge.from_request_id ?? 'root'}-${edge.to_request_id}-${edge.relation}`}>
      <span className="status ok">{t(`conversationRelation.${edge.relation}`)}</span>
      <span>{t(`sessions.relationship.${edge.relation}`, { from: requestLabel(edge.from_request_id), to: requestLabel(edge.to_request_id) })}</span>
      <small className="muted">{t('sessions.confidence', { value: formatPercent(edge.confidence, locale) })}</small>
    </div>)}{confirmedEdges.length === 0 && <div className="empty">{detail.unlinked ? t('sessions.noGuessedEdges') : t('sessions.singleObservation')}</div>}</div>
    {candidateEdges.length > 0 && <details className="candidate-edges"><summary>{t('sessions.candidateRelationships', { count: candidateEdges.length })}</summary><p className="muted">{t('sessions.candidateHint')}</p><div className="edge-list">{candidateEdges.map((edge) => <div className="edge" key={`candidate-${edge.from_request_id ?? 'root'}-${edge.to_request_id}`}><span className="status pending">{t('conversationRelation.candidate')}</span><span>{t('sessions.relationshipSentence', { from: requestLabel(edge.from_request_id), to: requestLabel(edge.to_request_id) })}</span><small className="muted">{t('sessions.confidence', { value: formatPercent(edge.confidence, locale) })}</small></div>)}</div></details>}
  </DrawerFrame>;
}
