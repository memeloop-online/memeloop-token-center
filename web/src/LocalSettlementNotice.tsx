import { DetailTooltip } from './design-system';
import { useI18n, type Locale } from './i18n';

export function localSettlementLabel(locale: Locale) {
  return locale === 'zh-CN' ? '本地结算' : 'Local settlement';
}

export function LocalSettlementNotice() {
  const { locale } = useI18n();
  const detail = locale === 'zh-CN'
    ? '按本地账本结算，可能包含保守上限结算；不是供应商实际消耗或发票。部分历史用量来源未记录，不据此重算或推定实际消耗占比。'
    : 'Settled by the local ledger and may include conservative ceiling settlements. This is not supplier consumption or an invoice. Some historical usage provenance is unavailable; this total does not recompute it or infer the share of actual usage.';
  return <DetailTooltip content={detail}><span tabIndex={0}>{localSettlementLabel(locale)} ⓘ</span></DetailTooltip>;
}
