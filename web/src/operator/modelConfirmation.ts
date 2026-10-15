export interface ModelConfirmation { scope: string; confirmed: boolean; selection?: 'existing' | 'catalog' | 'custom' }

/** Eligible counts all active candidates, including unresolved snapshots. */
export function catalogEvidenceVerified(catalog: {
  eligible_account_count: number;
  unknown_account_count: number;
  unsupported_account_count?: number;
  stale_account_count: number;
} | undefined, explicitCandidateCount?: number, allowUnsupportedDiscovery = false) {
  if (!catalog) return false;
  const { eligible_account_count: eligible, unknown_account_count: unknown, stale_account_count: stale, unsupported_account_count: unsupported = 0 } = catalog;
  return Number.isSafeInteger(eligible) && eligible > 0
    && Number.isSafeInteger(unknown) && unknown >= 0 && unknown <= eligible
    && Number.isSafeInteger(unsupported) && unsupported >= 0 && unsupported <= unknown
    && (unknown === 0 || (allowUnsupportedDiscovery && unknown === unsupported))
    && Number.isSafeInteger(stale) && stale >= 0 && stale <= eligible - unknown
    && (explicitCandidateCount === undefined || eligible === explicitCandidateCount);
}

/** Consent belongs to one scope; returning to an old scope cannot revive it. */
export function confirmationForScope(state: ModelConfirmation, scope: string): ModelConfirmation {
  return state.scope === scope ? state : { scope, confirmed: false };
}

export function modelConfirmationValidity({ hasValue, catalogListed, customConfirmed, selection, catalogVerified = true }: {
  hasValue: boolean;
  catalogListed: boolean;
  customConfirmed: boolean;
  selection?: ModelConfirmation['selection'];
  catalogVerified?: boolean;
}) {
  const unverified = hasValue && !(catalogVerified && catalogListed);
  const allowCustom = hasValue && customConfirmed
    && (selection === 'existing' || (selection === 'custom' && !catalogListed));
  return { unverified, allowCustom, valid: hasValue && (Boolean(selection) || (catalogVerified && catalogListed)) };
}
