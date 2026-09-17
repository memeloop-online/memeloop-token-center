import { DetailTooltip } from './design-system';
import { useI18n, type Locale } from './i18n';

export function localSettlementLabel(locale: Locale) {
  return locale === 'zh-CN' ? '本地结算' : 'Local settlement';
}

export function localSettlementTrendLabel(locale: Locale) {
  return locale === 'zh-CN' ? '本地结算趋势' : 'Local settlement trend';
}

export function LocalSettlementNotice() {
  const { locale } = useI18n();
  const detail = locale === 'zh-CN'
    ? '本地账本结算合计。失败请求的费用按 0 计算；供应商已回传用量的请求保留对应结算金额。历史保守上限可在请求明细中核对，供应商账单以供应商记录为准。'
    : 'Local-ledger settlement total. Failed requests have a cost of 0; requests with provider-reported usage keep their settled amount. Review historical settlement ceilings in request details and use provider records for supplier invoices.';
  return <DetailTooltip content={detail}><span tabIndex={0}>{localSettlementLabel(locale)} ⓘ</span></DetailTooltip>;
}
