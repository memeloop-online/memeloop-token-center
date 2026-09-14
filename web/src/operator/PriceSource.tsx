import { useI18n } from '../i18n';
import { DetailTooltip } from '../design-system';
import './pricingPresentation.css';

/** Imported provenance is audit metadata, not a pricing-provider label. */
export function isHistoricalPriceSource(source: string): boolean {
  return source.startsWith('cpamp:') || source.startsWith('copied:');
}

export function PriceSource({ source }: { source: string }) {
  const { t } = useI18n();
  const label = isHistoricalPriceSource(source) ? t('pricing.historicalSource') : source === 'manual' ? t('pricing.manualSource') : source;
  return <DetailTooltip content={`${t('pricing.originalSource')}: ${source}`}><span className="price-provenance" tabIndex={0}>{label}</span></DetailTooltip>;
}
