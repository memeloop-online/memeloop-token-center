import { DrawerFrame, RequestTable } from './components';
import { formatCurrency, formatMetricNumber, formatMilliseconds, formatPercent } from './format';
import { useI18n } from './i18n';
import type { LogicalSessionDetail, LogicalSessionSummary, RequestView, UsageAnalysisCost } from './types';

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
      : t('sessions.sessionTitle', { model: session.model || t('common.none') });
    return <article className="session-card" key={`${session.key_id}:${session.session_id}`}>
      <div className="session-card-heading"><b>{title}</b><span className={`status ${statusTone(session.last_status)}`}>{t(`sessions.status.${session.last_status}`)}</span></div>
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
        <span><small>{t('sessions.errors')}</small><b><SessionMetricNumber value={session.errors} /></b></span>
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
  const title = detail.unlinked ? t('sessions.unlinkedRequests') : summary?.model || t('sessions.logicalSession');
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
    <h3>{t('sessions.timeline')}</h3>
    {detail.has_more && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={onLoadOlder}>{loading ? t('common.loading') : t('sessions.loadEarlier')}</button></div>}
    <RequestTable requests={detail.requests} currency={currency ?? summaryCurrency} onSelect={onSelect} />
    <h3>{t('sessions.relationships')}</h3>
    {detail.edges_truncated && <div className="notice warning">{t('sessions.edgesTruncated')}</div>}
    <div className="edge-list">{confirmedEdges.map((edge) => <div className="edge" key={`${edge.from_request_id ?? 'root'}-${edge.to_request_id}-${edge.relation}`}>
      <span className="status ok">{t(`conversationRelation.${edge.relation}`)}</span>
      <span>{t('sessions.relationshipSentence', { from: requestLabel(edge.from_request_id), to: requestLabel(edge.to_request_id) })}</span>
      <small className="muted">{t('sessions.confidence', { value: formatPercent(edge.confidence, locale) })}</small>
    </div>)}{confirmedEdges.length === 0 && <div className="empty">{detail.unlinked ? t('sessions.noGuessedEdges') : t('sessions.singleObservation')}</div>}</div>
    {candidateEdges.length > 0 && <details className="candidate-edges"><summary>{t('sessions.candidateRelationships', { count: candidateEdges.length })}</summary><p className="muted">{t('sessions.candidateHint')}</p><div className="edge-list">{candidateEdges.map((edge) => <div className="edge" key={`candidate-${edge.from_request_id ?? 'root'}-${edge.to_request_id}`}><span className="status pending">{t('conversationRelation.candidate')}</span><span>{t('sessions.relationshipSentence', { from: requestLabel(edge.from_request_id), to: requestLabel(edge.to_request_id) })}</span><small className="muted">{t('sessions.confidence', { value: formatPercent(edge.confidence, locale) })}</small></div>)}</div></details>}
  </DrawerFrame>;
}
