import { CopyButton } from '../../CopyButton.js';
import { useI18n } from '../../i18n';

export type Translate = (key: string, variables?: Record<string, string | number>) => string;

export function queryForTenant(tenant: string, existing = '') {
  const params = new URLSearchParams(existing);
  if (tenant) params.set('tenant_external_id', tenant);
  const query = params.toString();
  return query ? `?${query}` : '';
}

export function messageOf(reason: unknown, fallback: string) {
  return reason instanceof Error ? reason.message : fallback;
}

export function enumLabel(t: Translate, prefix: string, value: string) {
  const key = `${prefix}.${value}`;
  const translated = t(key);
  return translated === key ? value : translated;
}

export function WriteScopeNotice({ tenant }: { tenant: string }) {
  const { t } = useI18n();
  if (tenant) return null;
  return <div className="scope-context" role="status"><span aria-hidden="true">◎</span><p>{t('operator.selectTenantToWrite')}</p></div>;
}

export function OneTimeSecret({ value, message }: { value: string; message: string }) {
  const { t } = useI18n();
  return <div className="one-time" role="status">
    <b>{message}</b>
    <code>{value}</code>
    <CopyButton value={value} label={t('common.copySecret')} />
  </div>;
}
