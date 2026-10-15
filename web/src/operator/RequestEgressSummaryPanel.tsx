import { useState } from 'react';
import { ApiError } from '../api';
import { Button, Field, FormSection, LoadingProgress, LoadingState, Select } from '../design-system';
import { useI18n } from '../i18n';
import type { RequestEgressSummaryResponse } from '../types';
import { useOperatorResource } from './hooks/useOperatorResource';
import { transportProxyRequest } from './transportProxyGroups';
import { egressBytes, egressGroupKey, egressSelectionKey, egressSummaryPath } from './requestEgress';
import { EgressSelection } from './RequestEgressDetails';
import { requestEgressCopy } from './requestEgressCopy';

export function RequestEgressSummaryPanel({ token, tenant }: { token: string; tenant: string }) {
  const { locale } = useI18n();
  const copy = requestEgressCopy(locale);
  const [hours, setHours] = useState(24);
  const [end, setEnd] = useState(Date.now);
  const [group, setGroup] = useState('');
  const start = end - hours * 60 * 60 * 1000;
  const resource = useOperatorResource(Boolean(token && tenant), `${token}\0${tenant}\0${start}\0${end}`, async signal => {
    try {
      const result = await transportProxyRequest<RequestEgressSummaryResponse>(egressSummaryPath(tenant, start, end), token, { signal });
      if (result.coverage !== 'recorded_codex_attempts_only' || result.measurement_basis !== 'application_payload_estimate' || !Array.isArray(result.groups)) throw new Error();
      return result;
    } catch (reason) {
      throw new Error(reason instanceof ApiError && [401, 403].includes(reason.status) ? copy.denied : copy.failed);
    }
  }, copy.failed);
  const state = resource.state;
  const failure = state.kind === 'failed' ? state.message : state.kind === 'ready' ? state.refreshError : undefined;
  const data = state.kind === 'ready' && !failure ? state.value : undefined;
  const busy = state.kind === 'idle' || state.kind === 'loading' || state.kind === 'ready' && state.refreshing === true;
  const choices = new Map<string, string>();
  for (const row of data?.groups ?? []) {
    const key = egressGroupKey(row.selected);
    if (!choices.has(key)) choices.set(key, row.selected.group_id ? row.selected.group_name || copy.unnamedGroup
      : row.selected.kind === 'direct' ? copy.direct : row.selected.kind === 'proxy' ? copy.ungrouped : copy.unknown);
  }
  const activeGroup = choices.has(group) ? group : '';
  const rows = data?.groups.filter(row => !activeGroup || egressGroupKey(row.selected) === activeGroup) ?? [];
  return <section className="request-egress" aria-label={copy.title}>
    <FormSection title={copy.title} description={copy.scope}>
      <p className="muted">{copy.exclusions}</p>
      <div className="egress-controls">
        <Field label={copy.period}><Select value={String(hours)} onChange={event => { setHours(Number(event.target.value)); setEnd(Date.now()); setGroup(''); }}>
          <option value="1">{copy.hour}</option><option value="24">{copy.day}</option><option value="168">{copy.week}</option>
        </Select></Field>
        <Field label={copy.groupFilter}><Select value={activeGroup} disabled={!data} onChange={event => setGroup(event.target.value)}>
          <option value="">{copy.allGroups}</option>{[...choices].map(([key, name]) => <option key={key} value={key}>{name}</option>)}
        </Select></Field>
        <Button appearance="secondary" type="button" disabled={busy} onClick={() => setEnd(Date.now())}>{copy.refresh}</Button>
      </div>
      <LoadingProgress active={busy} label={copy.loading} />
      {failure && <div className="notice error" role="alert"><span>{failure}</span><Button appearance="secondary" type="button" onClick={() => void resource.reload()}>{copy.retry}</Button></div>}
      {!data && busy && <LoadingState label={copy.loading} variant="compact" />}
      {data && <>
        <p>{new Date(data.start).toLocaleString(locale)} ≤ {copy.observed} &lt; {new Date(data.end).toLocaleString(locale)}</p>
        <p className="muted">{copy.historical}</p>
        <p>{copy.wire}: {copy.unknown} · {copy.billed}: {copy.unknown}</p>
        {!rows.length ? <p role="status">{copy.empty}</p> : rows.map(row => <article className="egress-card" key={egressSelectionKey(row.selected)} data-egress-summary>
          <EgressSelection selected={row.selected} />
          <dl className="egress-facts">
            <div><dt>{copy.recorded}</dt><dd>{row.recorded_attempts}</dd></div>
            <div><dt>{copy.completed}</dt><dd>{row.completed_attempts}</dd></div>
            <div><dt>{copy.finalCount}</dt><dd>{row.final_response_attempts}</dd></div>
            <div><dt>{copy.requestBytes}</dt><dd>{egressBytes(row.request_payload_bytes, locale)}<small>{copy.coverage(row.request_known_attempts, row.recorded_attempts)}</small></dd></div>
            <div><dt>{copy.responseBytes}</dt><dd>{egressBytes(row.response_payload_bytes, locale)}<small>{copy.coverage(row.response_known_attempts, row.recorded_attempts)}</small></dd></div>
          </dl>
        </article>)}
      </>}
    </FormSection>
  </section>;
}
