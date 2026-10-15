// The create request always carries an explicit principal. A dedicated
// override binds the credential to an existing user or application; without
// one the readable alias doubles as the principal, so one name creates both.
export function credentialCreatePrincipal(alias: unknown, override: string): string {
  const custom = override.trim();
  return custom || (typeof alias === 'string' ? alias : '');
}
