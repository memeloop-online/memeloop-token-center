export interface ModelConfirmation { scope: string; confirmed: boolean }

/** Consent belongs to one scope; returning to an old scope cannot revive it. */
export function confirmationForScope(state: ModelConfirmation, scope: string): ModelConfirmation {
  return state.scope === scope ? state : { scope, confirmed: false };
}

export function modelConfirmationValidity({ hasValue, selected, catalogFresh, partialConfirmed, customAllowed, customConfirmed }: {
  hasValue: boolean;
  selected: { complete_coverage: boolean } | undefined;
  catalogFresh: boolean;
  partialConfirmed: boolean;
  customAllowed: boolean;
  customConfirmed: boolean;
}) {
  const selectedValid = Boolean(selected && catalogFresh && (selected.complete_coverage || partialConfirmed));
  const needsCustomConfirmation = hasValue && (!selected || !catalogFresh);
  const allowCustom = Boolean(needsCustomConfirmation && customAllowed && customConfirmed);
  return { needsCustomConfirmation, allowCustom, valid: selectedValid || allowCustom };
}
