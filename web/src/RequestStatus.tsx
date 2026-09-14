import { DetailTooltip } from './design-system';
import { useI18n } from './i18n';
import { requestStatusCopy } from './requestStatusPresentation';
import type { RequestView } from './types';
import './styles/request-surfaces.css';

export function RequestStatus({ request }: { request: RequestView }) {
  const { locale } = useI18n();
  const status = requestStatusCopy(request, locale);
  return <DetailTooltip content={status.hint}><span className={`status request-outcome ${status.tone}`} data-outcome={status.outcome} tabIndex={0} aria-label={`${status.label}. ${status.hint}`}>
    {status.label}{request.status_code !== null && <small>{request.status_code}</small>}
  </span></DetailTooltip>;
}
