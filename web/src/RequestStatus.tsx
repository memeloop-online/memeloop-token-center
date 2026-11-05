import { DetailTooltip } from './design-system';
import { useI18n } from './i18n';
import { requestEgressLine, requestStatusCopy } from './requestStatusPresentation';
import type { RequestView } from './types';
import './styles/request-surfaces.css';

type StatusCopy = ReturnType<typeof requestStatusCopy>;

// Reason first, the recorded upstream note if any, then the next step, and
// labeled technical identifiers last. The trigger keeps a concise label;
// this content arrives through the tooltip's description relationship.
function StatusTooltipContent({ status }: { status: StatusCopy }) {
  return <div className="request-status-tooltip">
    <p className="request-status-tooltip-reason">{status.cause || status.hint}</p>
    {status.upstreamNote && <p className="request-status-tooltip-note">{status.upstreamNote}</p>}
    {status.nextAction && <p className="request-status-tooltip-action"><b>{status.labels.nextStep}: </b>{status.nextAction}</p>}
    {status.details.length > 0 && <div className="request-status-tooltip-details" role="group" aria-label={status.labels.technicalDetails}>
      <b className="request-status-tooltip-details-heading">{status.labels.technicalDetails}</b>
      <dl>{status.details.map((entry) => <div key={`${entry.label}:${entry.value}`}><dt>{entry.label}</dt><dd>{entry.value}</dd></div>)}</dl>
    </div>}
  </div>;
}

export function RequestStatus({ request }: { request: RequestView }) {
  const { locale } = useI18n();
  const status = requestStatusCopy(request, locale);
  const egress = requestEgressLine(request, locale);
  return <><DetailTooltip content={<StatusTooltipContent status={status} />}><span className={`status request-outcome ${status.tone}`} data-outcome={status.outcome} tabIndex={0} aria-label={status.triggerLabel}>
    {status.label}{request.status_code !== null && <small>{request.status_code}</small>}
  </span></DetailTooltip>{status.cause && <small className="request-status-cause">{status.cause}</small>}{egress && <DetailTooltip content={egress.hover}><small className={`request-egress-line request-egress-${egress.kind}`} data-egress={egress.kind} tabIndex={0} aria-label={`${egress.text}. ${egress.hover}`}>{egress.text}</small></DetailTooltip>}</>;
}
