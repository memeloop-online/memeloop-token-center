import { useEffect, useRef, useState, type FormEvent } from 'react';
import { api } from '../api';
import { Buckets, NumberMetric, RequestTable } from '../components';
import { formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { KeyView, RequestView, SelfStats } from '../types';
import { selfErrorMessage } from './errors';
import { emptyRequestFilters, requestPageSize, requestsPath, statsPath, type RequestFilters } from './requestPaths';

export function RequestsPage({ credential, credentialView, onError, onOpenRequest, onOpenSession }: {
  credential: string;
  credentialView: KeyView;
  onError: (message: string) => void;
  onOpenRequest: (request: RequestView) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { locale, t } = useI18n();
  const [filters, setFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [appliedFilters, setAppliedFilters] = useState<RequestFilters>(emptyRequestFilters);
  const [requests, setRequests] = useState<RequestView[]>([]);
  const [stats, setStats] = useState<SelfStats>();
  const [hasOlder, setHasOlder] = useState(false);
  const [loading, setLoading] = useState(false);
  const requestSequence = useRef(0);
  const requestController = useRef<AbortController | undefined>(undefined);

  async function fetchPage(nextFilters: RequestFilters, append = false) {
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
    setLoading(true);
    onError('');
    try {
      const before = append ? requests.at(-1) : undefined;
      const [page, filteredStats] = append
        ? [await api<RequestView[]>(requestsPath(nextFilters, before), credential, { signal: controller.signal }), undefined]
        : await Promise.all([
          api<RequestView[]>(requestsPath(nextFilters, before), credential, { signal: controller.signal }),
          api<SelfStats>(statsPath(nextFilters), credential, { signal: controller.signal }),
        ]);
      if (sequence !== requestSequence.current || controller.signal.aborted) return;
      setRequests((current) => {
        if (!append) return page;
        const known = new Set(current.map((request) => request.request_id));
        return [...current, ...page.filter((request) => !known.has(request.request_id))];
      });
      setAppliedFilters(nextFilters);
      if (filteredStats) setStats(filteredStats);
      setHasOlder(page.length === requestPageSize);
    } catch (reason) {
      if (sequence === requestSequence.current && !controller.signal.aborted) onError(selfErrorMessage(reason, t, t('common.requestFailed')));
    } finally {
      if (sequence === requestSequence.current) setLoading(false);
    }
  }

  useEffect(() => {
    requestSequence.current += 1;
    requestController.current?.abort();
    setFilters(emptyRequestFilters);
    setAppliedFilters(emptyRequestFilters);
    setRequests([]);
    setStats(undefined);
    setHasOlder(false);
    setLoading(true);
    void fetchPage(emptyRequestFilters);
    return () => {
      requestSequence.current += 1;
      requestController.current?.abort();
    };
  }, [credential]);

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

  return <div className="self-page self-requests-page" data-self-page="requests">
    {stats && <section className="metrics self-request-summary">
      <NumberMetric label={t('traffic.total')} value={stats.summary.total_requests} />
      <NumberMetric label={t('traffic.success')} value={stats.summary.successful_requests} tone="positive" />
      <NumberMetric label={t('traffic.failure')} value={stats.summary.failed_requests} tone="negative" />
      <NumberMetric label={t('request.tokens')} value={stats.summary.input_tokens + stats.summary.output_tokens} showCompact={false} />
    </section>}
    {stats && stats.errors.length > 0 && <article className="panel self-request-errors"><h2>{t('traffic.errors')}</h2><Buckets values={stats.errors} onSelect={(bucket) => filterBy({ status: 'error', errorCode: bucket.name })} /></article>}
    <article className="panel self-history">
      <div className="panel-title"><h2>{t('self.recent')}</h2><span>{t('self.loadedRequests', { count: formatNumber(requests.length, locale) })}</span></div>
      <form className="self-request-filters" onSubmit={applyFilters}>
        <label><span>{t('traffic.from')}</span><input type="datetime-local" value={filters.from} onChange={(event) => setFilters((current) => ({ ...current, from: event.target.value }))} /></label>
        <label><span>{t('traffic.to')}</span><input type="datetime-local" value={filters.to} onChange={(event) => setFilters((current) => ({ ...current, to: event.target.value }))} /></label>
        <label><span>{t('request.model')}</span><input value={filters.model} onChange={(event) => setFilters((current) => ({ ...current, model: event.target.value }))} placeholder={t('self.exactMatch')} /></label>
        <label><span>{t('request.protocol')}</span><input value={filters.protocol} onChange={(event) => setFilters((current) => ({ ...current, protocol: event.target.value }))} placeholder={t('self.exactMatch')} /></label>
        <label><span>{t('request.status')}</span><select value={filters.status} onChange={(event) => setFilters((current) => ({ ...current, status: event.target.value }))}><option value="">{t('common.all')}</option><option value="success">{t('traffic.success')}</option><option value="error">{t('traffic.failure')}</option><option value="pending">{t('common.running')}</option></select></label>
        <label><span>{t('traffic.errorCode')}</span><input value={filters.errorCode} onChange={(event) => setFilters((current) => ({ ...current, errorCode: event.target.value }))} placeholder={t('self.exactMatch')} /></label>
        <label><span>{t('traffic.upstreamId')}</span><input value={filters.upstreamAccountId} onChange={(event) => setFilters((current) => ({ ...current, upstreamAccountId: event.target.value }))} placeholder="019f…" /></label>
        <label><span>{t('traffic.routeId')}</span><input value={filters.routeId} onChange={(event) => setFilters((current) => ({ ...current, routeId: event.target.value }))} placeholder="019f…" /></label>
        <label><span>{t('traffic.minDuration')}</span><input type="number" min="0" value={filters.minDurationMs} onChange={(event) => setFilters((current) => ({ ...current, minDurationMs: event.target.value }))} /></label>
        <label><span>{t('traffic.maxDuration')}</span><input type="number" min="0" value={filters.maxDurationMs} onChange={(event) => setFilters((current) => ({ ...current, maxDurationMs: event.target.value }))} /></label>
        <label><span>{t('traffic.minCost')}</span><input inputMode="decimal" value={filters.minCost} onChange={(event) => setFilters((current) => ({ ...current, minCost: event.target.value }))} /></label>
        <label><span>{t('traffic.maxCost')}</span><input inputMode="decimal" value={filters.maxCost} onChange={(event) => setFilters((current) => ({ ...current, maxCost: event.target.value }))} /></label>
        <div className="filter-actions"><button type="submit" disabled={loading}>{loading ? t('common.loading') : t('traffic.applyFilters')}</button><button type="button" className="secondary" onClick={clearFilters} disabled={loading}>{t('traffic.clearFilters')}</button></div>
      </form>
      {loading && requests.length === 0 ? <div className="boot">{t('common.loading')}</div> : <RequestTable requests={requests} currency={credentialView.currency} onSelect={onOpenRequest} onOpenSession={onOpenSession} />}
      {hasOlder && <div className="load-more"><button type="button" className="secondary" disabled={loading} onClick={() => void fetchPage(appliedFilters, true)}>{loading ? t('common.loading') : t('traffic.loadOlder')}</button></div>}
    </article>
  </div>;
}
