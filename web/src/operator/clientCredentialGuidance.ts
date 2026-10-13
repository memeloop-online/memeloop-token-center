export function clientCredentialPortalUrl(origin: string): string {
  const portal = new URL('/portal', new URL(origin).origin);
  const labels = portal.hostname.split('.');
  if (labels.length > 1 && labels[0].endsWith('-operator')) {
    labels[0] = labels[0].slice(0, -'-operator'.length);
    portal.hostname = labels.join('.');
  }
  return portal.href;
}

const clientKeyShape = /^mtc_[0-9a-f]{32}_.{32,}$/i;

export function isOrdinaryClientCredential(candidate: string): boolean {
  return clientKeyShape.test(candidate.trim());
}
