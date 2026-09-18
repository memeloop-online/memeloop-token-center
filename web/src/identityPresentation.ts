export const RETIRED_CREDENTIAL = '__retired_credential__';
export const RETIRED_PRINCIPAL = '__retired_principal__';
export const RETIRED_UPSTREAM = '__retired_upstream__';

type Translate = (key: string, variables?: Record<string, string | number>) => string;

export function credentialDisplayName(alias: string | null | undefined, t: Translate) {
  const value = alias?.trim();
  return value === RETIRED_CREDENTIAL ? t('request.retiredCredential') : value || t('request.unnamedCredential');
}

export function principalDisplayName(value: string | null | undefined, t: Translate) {
  return value === RETIRED_PRINCIPAL ? t('request.retiredPrincipal') : value;
}

export function upstreamDisplayName(value: string, t: Translate) {
  return value === RETIRED_UPSTREAM ? t('request.retiredUpstream') : value;
}
