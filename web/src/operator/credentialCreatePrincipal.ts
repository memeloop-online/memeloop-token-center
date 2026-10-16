// The create request always carries an explicit principal. A dedicated
// override binds the credential to an existing user or application; without
// one the readable alias doubles as the principal, so one name creates both.
// Blankness is only a check: an entered or picked identity is submitted
// exactly, never rewritten, because principals may legitimately carry
// surrounding whitespace and stay distinct identities.
export function credentialCreatePrincipal(alias: unknown, override: string): string {
  if (!override.trim()) return typeof alias === 'string' ? alias : '';
  return override;
}
