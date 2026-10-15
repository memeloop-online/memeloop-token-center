import { Disclosure, FormSection } from '../design-system';
import { useI18n } from '../i18n';
import type { RequestEgressAttempt, SelectedRequestEgress } from '../types';
import { egressAddress, egressBytes, egressSelectionName } from './requestEgress';
import { requestEgressCopy } from './requestEgressCopy';
import './requestEgress.css';

export function EgressSelection({ selected }: { selected: SelectedRequestEgress }) {
  const { locale } = useI18n();
  const copy = requestEgressCopy(locale);
  return <div className="egress-selection">
    <strong>{egressSelectionName(selected, locale)}</strong>
    {selected.group_version != null && <span>{copy.version(selected.group_version)}</span>}
    <span>{copy.address}: {selected.kind === 'direct' ? copy.direct : selected.kind === 'proxy' ? egressAddress(selected, locale) : copy.unknown}</span>
  </div>;
}

export function RequestEgressDetails({ attempts, accountNames }: { attempts?: RequestEgressAttempt[]; accountNames?: ReadonlyMap<string, string> }) {
  const { locale } = useI18n();
  const copy = requestEgressCopy(locale);
  return <section className="request-egress" aria-label={copy.attemptsTitle}>
    <FormSection title={copy.attemptsTitle} description={copy.attemptHint}>
      <p className="muted">{copy.historical}</p>
      <p className="muted">{copy.scope}</p>
      {!attempts?.length ? <p role="status">{copy.emptyAttempts}</p> : attempts.map((attempt, index) => <article className="egress-card" key={attempt.attempt_id} data-egress-attempt={attempt.attempt_id}>
        <h4>{copy.attempt(index + 1)} · {attempt.final_response ? copy.final : copy.other}</h4>
        <EgressSelection selected={attempt.selected} />
        <dl className="egress-facts">
          <div><dt>{copy.account}</dt><dd>{accountNames?.get(attempt.upstream_account_id) || copy.unnamedAccount}</dd></div>
          <div><dt>{copy.observed}</dt><dd>{new Date(attempt.observed_at).toLocaleString(locale)}</dd></div>
          <div><dt>{copy.requestBytes}</dt><dd>{egressBytes(attempt.request_payload_bytes, locale)}</dd></div>
          <div><dt>{copy.responseBytes}</dt><dd>{egressBytes(attempt.response_payload_bytes, locale)}</dd></div>
        </dl>
        <p>{attempt.completed ? copy.ended : copy.notEnded} · {attempt.response_body_complete ? copy.eof : copy.noEof}</p>
        <Disclosure title={copy.technical}>
          <dl className="egress-facts">
            <div><dt>{copy.attemptId}</dt><dd>{attempt.attempt_id}</dd></div>
            <div><dt>{copy.groupId}</dt><dd>{attempt.selected.group_id ?? copy.unknown}</dd></div>
            <div><dt>{copy.memberId}</dt><dd>{attempt.selected.member_id ?? copy.unknown}</dd></div>
            <div><dt>{copy.accountId}</dt><dd>{attempt.upstream_account_id}</dd></div>
            <div><dt>{copy.routeId}</dt><dd>{attempt.route_id}</dd></div>
            <div><dt>{copy.counters}</dt><dd>{attempt.outbound_attempt} / {attempt.connect_attempt}</dd></div>
          </dl>
        </Disclosure>
      </article>)}
    </FormSection>
  </section>;
}
