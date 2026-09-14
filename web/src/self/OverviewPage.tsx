import { useEffect, useRef, useState } from 'react';
import { api } from '../api';
import { Metric, NumberMetric, RequestTable } from '../components';
import { formatCompactCurrency, formatNumber, formatPercent } from '../format';
import { useI18n } from '../i18n';
import { LimitSnapshot } from '../LimitSnapshot';
import type { KeyLimitSnapshot, KeyView, RequestView, SelfStats } from '../types';
import { selfErrorMessage } from './errors';
import { emptyRequestFilters, requestsPath, statsPath } from './requestPaths';

const overviewRequestCount = 5;

export function OverviewPage({ credential, credentialView, onError, onOpenRequest, onOpenSession }: {
  credential: string;
  credentialView: KeyView;
  onError: (message: string) => void;
  onOpenRequest: (request: RequestView) => void;
  onOpenSession: (sessionId: string) => void;
}) {
  const { locale, t } = useI18n();
  const [currentKey, setCurrentKey] = useState(credentialView);
  const [stats, setStats] = useState<SelfStats>();
  const [limits, setLimits] = useState<KeyLimitSnapshot>();
  const [recentRequests, setRecentRequests] = useState<RequestView[]>([]);
  const [loading, setLoading] = useState(true);
  const sequence = useRef(0);

  useEffect(() => {
    const current = ++sequence.current;
    const filters = { ...emptyRequestFilters, from: new Date(Date.now() - 86_400_000).toISOString() };
    setLoading(true);
    onError('');
    void Promise.allSettled([
      api<KeyView>('/self/v1/key', credential),
      api<KeyLimitSnapshot>('/self/v1/key/limits', credential),
      api<SelfStats>(statsPath(filters), credential),
      api<RequestView[]>(requestsPath(emptyRequestFilters, undefined, overviewRequestCount), credential),
    ]).then((results) => {
      if (current !== sequence.current) return;
      const [nextKey, nextLimits, nextStats, nextRequests] = results;
      if (nextKey.status === 'fulfilled') setCurrentKey(nextKey.value);
      if (nextLimits.status === 'fulfilled') setLimits(nextLimits.value);
      if (nextStats.status === 'fulfilled') setStats(nextStats.value);
      if (nextRequests.status === 'fulfilled') setRecentRequests(nextRequests.value);
      const failures = results.filter((result) => result.status === 'rejected');
      if (failures.length === results.length) {
        onError(selfErrorMessage(failures[0].reason, t, t('common.requestFailed')));
      } else if (failures.length) {
        onError(t('self.partialLoad', { count: formatNumber(failures.length, locale) }));
      }
    }).finally(() => {
      if (current === sequence.current) setLoading(false);
    });
    return () => { sequence.current += 1; };
  }, [credential]);

  if (loading && !stats && !limits) return <div className="boot">{t('common.loading')}</div>;
  const summary = stats?.summary;
  const balance = formatCompactCurrency(currentKey.available_balance, currentKey.currency, locale);
  const cost = formatCompactCurrency(summary?.total_cost, currentKey.currency, locale);
  const successRate = summary && summary.total_requests > 0
    ? summary.successful_requests / summary.total_requests
    : null;
  return <div className="self-page self-overview" data-self-page="overview">
    <div className="self-overview-caption">{t('usage.preset.24h')}</div>
    <section className="metrics self-overview-metrics" aria-label={t('usage.preset.24h')}>
      <NumberMetric label={t('traffic.total')} value={summary?.total_requests} />
      <Metric label={t('usage.successRate')} value={formatPercent(successRate, locale)} tone="positive" />
      <NumberMetric label={t('traffic.failure')} value={summary?.failed_requests} tone="negative" />
      <NumberMetric label={t('request.tokens')} value={summary ? summary.input_tokens + summary.output_tokens : undefined} />
      <Metric label={t('traffic.cost')} value={<span title={cost.title}>{cost.text}</span>} />
    </section>
    <article className="panel key-summary self-account-summary">
      <div><span className="eyebrow">{t('self.stableCredential')}</span><h2 title={currentKey.key_id} tabIndex={0}>{currentKey.alias}</h2><span>{t(`enforcementMode.${currentKey.policy.enforcement_mode}`)}</span></div>
      <Metric label={t('self.balance', { currency: currentKey.currency })} value={<span title={balance.title}>{balance.text}</span>} />
    </article>
    {limits && <article className="panel self-limit-snapshot"><LimitSnapshot value={limits} enforcementMode={currentKey.policy.enforcement_mode} /></article>}
    <article className="panel self-history self-overview-recent">
      <div className="panel-title"><h2>{t('self.recent')}</h2><span>{locale === 'zh-CN' ? '不限时间 · ' : 'All time · '}{t('self.loadedRequests', { count: formatNumber(recentRequests.length, locale) })}</span></div>
      <RequestTable requests={recentRequests} currency={currentKey.currency} credentialAlias={currentKey.alias} onSelect={onOpenRequest} onOpenSession={onOpenSession} />
    </article>
  </div>;
}
