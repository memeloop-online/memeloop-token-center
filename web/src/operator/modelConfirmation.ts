export interface ModelConfirmation { scope: string; confirmed: boolean }

/** Consent belongs to one scope; returning to an old scope cannot revive it. */
export function confirmationForScope(state: ModelConfirmation, scope: string): ModelConfirmation {
  return state.scope === scope ? state : { scope, confirmed: false };
}

export function modelConfirmationValidity({ hasValue, selected, customAllowed, customConfirmed }: {
  hasValue: boolean;
  selected: { complete_coverage: boolean } | undefined;
  customAllowed: boolean;
  customConfirmed: boolean;
}) {
  const selectedValid = Boolean(selected);
  const needsCustomConfirmation = hasValue && !selectedValid;
  const allowCustom = Boolean(needsCustomConfirmation && customAllowed && customConfirmed);
  return { needsCustomConfirmation, allowCustom, valid: selectedValid || allowCustom };
}
