import { useI18n } from '../i18n';
import './pricingPresentation.css';

/** Imported provenance is audit metadata, not the name of a pricing provider. */
export function isHistoricalPriceSource(source: string): boolean {
  return source.startsWith('cpamp:') || source.startsWith('copied:');
}

export function PriceSource({ source }: { source: string }) {
  const { t } = useI18n();
  if (!isHistoricalPriceSource(source)) return <span className="pill price-source">{source}</span>;
  return <details className="price-provenance"><summary>{t('pricing.historicalSource')}</summary><div><b>{t('pricing.originalSource')}</b><code>{source}</code></div></details>;
}
