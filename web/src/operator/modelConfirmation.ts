export interface ModelConfirmation { scope: string; confirmed: boolean }

/** Eligible counts all active candidates, including unresolved snapshots. */
export function catalogEvidenceVerified(catalog: {
  eligible_account_count: number;
  unknown_account_count: number;
  stale_account_count: number;
} | undefined, explicitCandidateCount?: number) {
  if (!catalog) return false;
  const { eligible_account_count: eligible, unknown_account_count: unknown, stale_account_count: stale } = catalog;
  return Number.isSafeInteger(eligible) && eligible > 0 && unknown === 0
    && Number.isSafeInteger(stale) && stale >= 0 && stale <= eligible
    && (explicitCandidateCount === undefined || eligible === explicitCandidateCount);
}

/** Consent belongs to one scope; returning to an old scope cannot revive it. */
export function confirmationForScope(state: ModelConfirmation, scope: string): ModelConfirmation {
  return state.scope === scope ? state : { scope, confirmed: false };
}

export function modelConfirmationValidity({ hasValue, catalogListed, customAllowed, customConfirmed, catalogVerified = true }: {
  hasValue: boolean;
  catalogListed: boolean;
  customAllowed: boolean;
  customConfirmed: boolean;
  catalogVerified?: boolean;
}) {
  // A pending/failed directory cannot prove absence or authorize an explicit
  // custom bypass, including when editing a previously confirmed route.
  if (!catalogVerified) return { needsCustomConfirmation: false, allowCustom: false, valid: false };
  const needsCustomConfirmation = hasValue && !catalogListed;
  const allowCustom = Boolean(needsCustomConfirmation && customAllowed && customConfirmed);
  return { needsCustomConfirmation, allowCustom, valid: catalogListed || allowCustom };
}
