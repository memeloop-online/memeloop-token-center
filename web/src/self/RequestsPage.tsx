import { useEffect, useRef, useState, type FormEvent } from 'react';
import { api } from '../api';
import { Buckets, NumberMetric, RequestTable } from '../components';
import { Button, Disclosure, Field, Input, Select } from '../design-system';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { KeyView, RequestView, SelfStats } from '../types';
import { selfErrorMessage } from './errors';
import { emptyRequestFilters, requestPageSize, requestsPath, statsPath, type RequestFilters } from './requestPaths';
import { selfRequestRefreshIntervals, useSelfRequestRefresh } from './useSelfRequestRefresh';
import './requestFilters.css';

type FetchMode = 'replace' | 'append' | 'refresh';

export function RequestsPage({ credential, credentialView, onError, onOpenRequest, onOpenSession }: {
  credential: string;
  credentialView: KeyView;
  onError: (message: string) => void;
  onOpenRequest: (request: RequestView) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { locale, t } = useI18n();
  const zh = locale === 'zh-CN';
  const [filters, setFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [stats, setStats] = useState<SelfStats>();
  const [hasOlder, setHasOlder] = useState(false);
  const [loading, setLoading] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const appliedFilters = useRef<RequestFilters>(emptyRequestFilters);
  const requestSequence = useRef(0);
  const requestController = useRef<AbortController | undefined>(undefined);
  const inFlight = useRef(false);
  const historyLoaded = useRef(false);

  async function fetchPage(nextFilters: RequestFilters, mode: FetchMode = 'replace') {
    const from = nextFilters.from ? new Date(nextFilters.from).getTime() : Number.NaN;
    const to = nextFilters.to ? new Date(nextFilters.to).getTime() : Number.NaN;
    if (Number.isFinite(from) && Number.isFinite(to) && from > to) {
      onError(t('self.invalidRange'));
      return;
    }
    const sequence = ++requestSequence.current;
    requestController.current?.abort();
    const controller = new AbortController();
    requestController.current = controller;
    inFlight.current = true;
    if (mode === 'refresh') setRefreshing(true);
    else { setLoading(true); onError(''); }
    try {
      if (mode === 'append') {
        const page = await api<RequestView[]>(requestsPath(nextFilters, requests.at(-1)), credential, { signal: controller.signal });
        if (sequence !== requestSequence.current || controller.signal.aborted) return;
        setRequests((current) => {
          const known = new Set(current.map((request) => request.request_id));
          return [...current, ...page.filter((request) => !known.has(request.request_id))];
        });
        setHasOlder(page.length === requestPageSize);
        historyLoaded.current = true;
        return;
      }
      const [page, pageStats] = await Promise.all([
        api<RequestView[]>(requestsPath(nextFilters), credential, { signal: controller.signal }),
        api<SelfStats>(statsPath(nextFilters), credential, { signal: controller.signal }),
      ]);
      if (sequence !== requestSequence.current || controller.signal.aborted) return;
      if (mode === 'refresh') {
        if (historyLoaded.current) {
          // A loaded history window must never gain, lose or reorder rows from a
          // poll; only update rows that the fresh first page still reports.
          const fresh = new Map(page.map((request) => [request.request_id, request]));
          setRequests((current) => current.map((request) => fresh.get(request.request_id) ?? request));
        } else {
          setRequests(page);
          setHasOlder(page.length === requestPageSize);
        }
        setStats(pageStats);
        // A current successful poll is authoritative for this page and clears
        // a retryable error from an earlier poll. Stale responses returned
        // above, so they can never erase an error from the active identity.
        onError('');
        return;
      }
      setRequests(page);
      setStats(pageStats);
      setHasOlder(page.length === requestPageSize);
      historyLoaded.current = false;
      appliedFilters.current = nextFilters;
    } catch (reason) {
      if (sequence === requestSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (requestController.current === controller) inFlight.current = false;
      if (sequence === requestSequence.current) {
        setLoading(false);
        setRefreshing(false);
      }
    }
  }

  useEffect(() => {
    requestSequence.current += 1;
    requestController.current?.abort();
    requestController.current = undefined;
    inFlight.current = false;
    historyLoaded.current = false;
    appliedFilters.current = emptyRequestFilters;
    setFilters(emptyRequestFilters);
    setRequests([]);
    setStats(undefined);
    setHasOlder(false);
    setRefreshing(false);
    void fetchPage(emptyRequestFilters);
    return () => {
      requestSequence.current += 1;
      requestController.current?.abort();
      requestController.current = undefined;
      inFlight.current = false;
    };
  }, [credential]);

  const { intervalMs, paused, setIntervalMs } = useSelfRequestRefresh(() => {
    // Automatic ticks never interrupt an explicit apply / load-older in flight.
    if (inFlight.current || document.hidden) return;
    void fetchPage(appliedFilters.current, 'refresh');
  }, stats !== undefined);

  function applyFilters(event: FormEvent) {
    event.preventDefault();
    void fetchPage(filters);
  }

  function clearFilters() {
    setFilters(emptyRequestFilters);
    void fetchPage(emptyRequestFilters);
  }

  function filterBy(next: Partial<RequestFilters>) {
    const merged = { ...emptyRequestFilters, ...next };
    setFilters(merged);
    void fetchPage(merged);
  }

  function patchFilters(patch: Partial<RequestFilters>) {
    setFilters((current) => ({ ...current, ...patch }));
  }

  const cadenceLabels = zh ? ['手动', '5秒', '30秒', '1分', '5分'] : ['Manual', '5s', '30s', '1m', '5m'];
  const cadenceIndex = Math.max(0, selfRequestRefreshIntervals.findIndex((interval) => interval === intervalMs));
  const refreshState = refreshing
    ? (zh ? '轮询 · 正在更新第一页与统计' : 'Polling · updating the first page and summary')
    : intervalMs === 0
    ? (zh ? '手动 · 点击“刷新”更新列表与统计，不会自动轮询' : 'Manual · press Refresh to update the list and summary; no automatic polling')
    : paused
      ? (zh ? '轮询 · 后台已暂停，返回后继续' : 'Polling · paused in background; resumes on return')
      : (zh ? `轮询 · 每 ${cadenceLabels[cadenceIndex]} 更新第一页与统计` : `Polling · first page and summary every ${cadenceLabels[cadenceIndex]}`);

  return <div className="self-page self-requests-page" data-self-page="requests">
    {stats && <section className="metrics self-request-summary">
      <NumberMetric label={t('traffic.total')} value={stats.summary.total_requests} />
      <NumberMetric label={t('traffic.success')} value={stats.summary.successful_requests} tone="positive" />
      <NumberMetric label={t('traffic.failure')} value={stats.summary.failed_requests} tone="negative" />
      <NumberMetric label={t('request.tokens')} value={stats.summary.input_tokens + stats.summary.output_tokens} />
    </section>}
    {stats && stats.errors.length > 0 && <article className="panel self-request-errors"><h2>{t('traffic.errors')}</h2><Buckets values={stats.errors} onSelect={(bucket) => filterBy({ status: 'error', errorCode: bucket.name })} /></article>}
    <article className="panel self-history">
      <div className="panel-title self-request-header">
        <div>
          <h2>{t('self.recent')}</h2>
          <span>{t('self.loadedRequests', { count: formatNumber(requests.length, locale) })}</span>
        </div>
        <div className="self-request-refresh">
          <Field label={zh ? '刷新节奏' : 'Refresh cadence'} className="self-request-cadence">
            <Select value={String(intervalMs)} onChange={(event) => setIntervalMs(Number(event.target.value))}>
              {selfRequestRefreshIntervals.map((interval, index) => <option key={interval} value={String(interval)}>{cadenceLabels[index]}</option>)}
            </Select>
          </Field>
          <Button appearance="secondary" type="button" disabled={loading || refreshing}
            onClick={() => void fetchPage(appliedFilters.current, 'refresh')}>{t('usage.refresh')}</Button>
          <span className={paused && intervalMs !== 0 ? 'self-request-refresh-state paused' : 'self-request-refresh-state'} role="status">{refreshState}</span>
        </div>
      </div>
      <form className="self-request-filter-panel" onSubmit={applyFilters}>
        <div className="self-request-filter-grid">
          <Field label={t('traffic.from')}><Input type="datetime-local" value={filters.from} onChange={(event) => patchFilters({ from: event.target.value })} /></Field>
          <Field label={t('traffic.to')}><Input type="datetime-local" value={filters.to} onChange={(event) => patchFilters({ to: event.target.value })} /></Field>
          <Field label={t('request.model')}><Input value={filters.model} onChange={(event) => patchFilters({ model: event.target.value })} placeholder={t('self.exactMatch')} /></Field>
          <Field label={t('request.protocol')}><Input value={filters.protocol} onChange={(event) => patchFilters({ protocol: event.target.value })} placeholder={t('self.exactMatch')} /></Field>
          <Field label={t('request.status')} className="self-request-field-narrow">
            <Select value={filters.status} onChange={(event) => patchFilters({ status: event.target.value })}>
              <option value="">{t('common.all')}</option>
              <option value="success">{t('traffic.success')}</option>
              <option value="error">{t('traffic.failure')}</option>
              <option value="pending">{t('common.running')}</option>
            </Select>
          </Field>
        </div>
        <div className="self-request-advanced">
          <Disclosure title={zh ? '高级筛选' : 'Advanced filters'}>
            <div className="self-request-filter-grid">
              <Field label={t('traffic.errorCode')}><Input value={filters.errorCode} onChange={(event) => patchFilters({ errorCode: event.target.value })} placeholder={t('self.exactMatch')} /></Field>
              <Field label={t('traffic.upstreamId')}><Input value={filters.upstreamAccountId} onChange={(event) => patchFilters({ upstreamAccountId: event.target.value })} placeholder="019f…" /></Field>
              <Field label={t('traffic.routeId')}><Input value={filters.routeId} onChange={(event) => patchFilters({ routeId: event.target.value })} placeholder="019f…" /></Field>
              <Field label={t('traffic.minDuration')}><Input type="number" min={0} value={filters.minDurationMs} onChange={(event) => patchFilters({ minDurationMs: event.target.value })} /></Field>
              <Field label={t('traffic.maxDuration')}><Input type="number" min={0} value={filters.maxDurationMs} onChange={(event) => patchFilters({ maxDurationMs: event.target.value })} /></Field>
              <Field label={t('traffic.minCost')}><Input inputMode="decimal" value={filters.minCost} onChange={(event) => patchFilters({ minCost: event.target.value })} /></Field>
              <Field label={t('traffic.maxCost')}><Input inputMode="decimal" value={filters.maxCost} onChange={(event) => patchFilters({ maxCost: event.target.value })} /></Field>
            </div>
          </Disclosure>
        </div>
        <div className="self-request-filter-actions">
          <Button appearance="primary" type="submit" disabled={loading}>{loading ? t('common.loading') : t('traffic.applyFilters')}</Button>
          <Button appearance="secondary" type="button" onClick={clearFilters} disabled={loading}>{t('traffic.clearFilters')}</Button>
        </div>
      </form>
      <div aria-busy={loading || refreshing}>
        {loading && requests.length === 0 ? <div className="boot">{t('common.loading')}</div> : <RequestTable requests={requests} currency={credentialView.currency} credentialAlias={credentialView.alias} onSelect={onOpenRequest} onOpenSession={onOpenSession} />}
      </div>
      {hasOlder && <div className="load-more"><Button appearance="secondary" type="button" disabled={loading} onClick={() => void fetchPage(appliedFilters.current, 'append')}>{loading ? t('common.loading') : t('traffic.loadOlder')}</Button></div>}
    </article>
  </div>;
}
