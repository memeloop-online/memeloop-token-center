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
    ? '这是本地账本已确认的结算合计，可能包含保守上限结算，且部分历史用量来源未记录；不是供应商实际消耗或发票。标为“未观测用量”的请求会释放预留并默认按 0 结算；只有额外的供应商用量或账单证据才会保留费用。缺口数量按终态请求中 usage_basis=not_observed 的请求数统计。'
    : 'This is the confirmed local-ledger settlement total and may include conservative ceiling settlements or historical rows without usage provenance; it is not supplier consumption or an invoice. Requests marked “Usage not observed” release their reservation and default to zero; a non-zero amount is retained only with additional supplier usage or billing evidence. The gap count is the number of terminal requests with usage_basis=not_observed.';
  return <DetailTooltip content={detail}><span tabIndex={0}>{localSettlementLabel(locale)} ⓘ</span></DetailTooltip>;
}
