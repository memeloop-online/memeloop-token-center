export interface ModelConfirmation { scope: string; confirmed: boolean }

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
