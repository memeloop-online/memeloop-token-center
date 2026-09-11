export interface ModelConfirmation { scope: string; confirmed: boolean }

/** Consent belongs to one scope; returning to an old scope cannot revive it. */
export function confirmationForScope(state: ModelConfirmation, scope: string): ModelConfirmation {
  return state.scope === scope ? state : { scope, confirmed: false };
}

export function modelConfirmationValidity({ hasValue, catalogListed, customAllowed, customConfirmed }: {
  hasValue: boolean;
  catalogListed: boolean;
  customAllowed: boolean;
  customConfirmed: boolean;
}) {
  const needsCustomConfirmation = hasValue && !catalogListed;
  const allowCustom = Boolean(needsCustomConfirmation && customAllowed && customConfirmed);
  return { needsCustomConfirmation, allowCustom, valid: catalogListed || allowCustom };
}
