export const clientCredentialPortalUrl = 'https://token.k3s.onetwo.website/portal';

const clientKeyShape = /^mtc_[0-9a-f]{32}_.{32,}$/i;

export function isOrdinaryClientCredential(candidate: string): boolean {
  return clientKeyShape.test(candidate.trim());
}
