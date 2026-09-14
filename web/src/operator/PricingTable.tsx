import { useState } from 'react';
import { Button, DetailTooltip, Input, Select } from '../design-system';
import { formatCurrency, formatNumber } from '../format';
import { useI18n } from '../i18n';
import type { ModelPriceUsageSummary, ModelPriceView } from '../types';
import { PriceSource } from './PriceSource';

export interface PricingRow {
  model: string;
  usage?: ModelPriceUsageSummary['models'][number];
  tier?: NonNullable<ModelPriceView['tiers']>[number];
}

export function PricingTable({ rows, currency, loading, usageLoading, usageFailed }: {
  rows: PricingRow[]; currency: string; loading: boolean; usageLoading: boolean; usageFailed: boolean;
}) {
  const { locale, t } = useI18n();
  const [search, setSearch] = useState('');
  const [filter, setFilter] = useState('all');
  const usageReady = !usageLoading && !usageFailed;
  const query = search.trim().toLocaleLowerCase(locale);
  const filtered = rows.filter(row => (!query || `${row.model} ${row.tier?.source ?? ''}`.toLocaleLowerCase(locale).includes(query))
    && (filter === 'all' || filter === 'missing' && usageReady && !loading && !row.tier || filter === 'used' && usageReady && (row.usage?.calls ?? 0) > 0));
  const unavailableFilter = filter !== 'all' && !usageReady;
  const tierLabel = (tier: string) => tier === 'default' ? t('pricing.tierDefault') : tier === 'priority' ? t('pricing.tierPriority') : tier === 'flex' ? t('pricing.tierFlex') : tier;
  return <section className="pricing-catalog">
    <div className="pricing-list-controls">
      <label>{t('pricing.search')}<Input aria-label={t('pricing.search')} type="search" value={search} onChange={event => setSearch(event.target.value)} placeholder={t('pricing.searchHint')} /></label>
      <label>{t('pricing.filter')}<Select aria-label={t('pricing.filter')} value={filter} onChange={event => setFilter(event.target.value)}>
        <option value="all">{t('common.all')}</option><option value="used" disabled={!usageReady}>{t('pricing.filterUsed')}</option><option value="missing" disabled={!usageReady}>{t('pricing.filterMissing')}</option>
      </Select></label>
      {(search || filter !== 'all') && <Button appearance="subtle" onClick={() => { setSearch(''); setFilter('all'); }}>{t('pricing.clearFilters')}</Button>}
    </div>
    <p className="pricing-table-caption" role="status">{t('pricing.tableUnit', { currency })} · {t('pricing.visibleRows', { count: formatNumber(filtered.length, locale) })}{loading && ` · ${t('pricing.loadingPrices')}`}</p>
    <div className="table-scroll token-pricing-scroll" tabIndex={0} role="region" aria-label={t('pricing.title')}>
      <table className="token-pricing-table"><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.calls')}</th><th>{t('pricing.inputShort')}</th><th>{t('pricing.cachedShort')}</th><th>{t('pricing.writeShort')}</th><th>{t('pricing.outputShort')}</th><th>{t('pricing.source')}</th></tr></thead>
        <tbody>{filtered.map(row => <tr key={`${row.model}-${row.tier?.service_tier ?? 'missing'}`}>
          <td><strong className="pricing-model-name">{row.model}</strong>{row.tier && <small>{tierLabel(row.tier.service_tier)}</small>}</td>
          <td><DetailTooltip content={t(usageLoading ? 'pricing.usageLoading' : usageFailed ? 'pricing.usageUnavailable' : 'pricing.callsHint')}><span tabIndex={0}>{usageLoading ? t('pricing.loadingShort') : usageFailed ? '—' : row.usage ? formatNumber(row.usage.calls, locale) : t('pricing.noCalls')}</span></DetailTooltip></td>
          {(['input_per_million', 'cached_input_per_million', 'cache_write_per_million', 'output_per_million'] as const).map(field => <td key={field}>{row.tier ? <>{formatCurrency(row.tier[field], currency, locale)}{row.tier.cache_price_estimated && (field === 'cached_input_per_million' || field === 'cache_write_per_million') && <DetailTooltip content={t('pricing.estimatedHint')}><small tabIndex={0}>{t('pricing.estimated')}</small></DetailTooltip>}</> : '—'}</td>)}
          <td>{row.tier ? <><PriceSource source={row.tier.source} /><DetailTooltip content={`${t('pricing.updated')}: ${new Date(row.tier.updated_at).toLocaleString(locale)}`}><time tabIndex={0} dateTime={new Date(row.tier.updated_at).toISOString()}>{new Date(row.tier.updated_at).toLocaleDateString(locale)}</time></DetailTooltip></> : <span>{loading ? t('pricing.loadingPrices') : t('pricing.missing')}</span>}</td>
        </tr>)}</tbody>
      </table>
      {!filtered.length && <div className="empty" role="status">{unavailableFilter ? t(usageLoading ? 'pricing.usageLoading' : filter === 'used' ? 'pricing.usedFilterUnavailable' : 'pricing.missingFilterUnavailable') : loading ? t('pricing.loadingPrices') : search || filter !== 'all' ? t('pricing.noMatches') : t('pricing.noPricesForCurrency', { currency })}</div>}
    </div>
  </section>;
}
