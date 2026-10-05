import { createHash } from 'node:crypto';
import { readFileSync, writeFileSync } from 'node:fs';
import { join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

export const sourceDigest = 'sha256:bac1a74de365ea59270ba134016762c6be05654d55246518de256113bd1c225e';
export const sourceImage = 'docker.io/metacubex/mihomo:v1.19.32';
export const targetImage = 'ghcr.io/memeloop-online/mtc-backup-egress';
export const targetTag = 'v1.19.32-bac1a74de365';
const indexType = 'application/vnd.oci.image.index.v1+json';
const manifestType = 'application/vnd.oci.image.manifest.v1+json';

interface Descriptor { digest: string; size: number; mediaType: string; urls?: unknown }
interface Limits { objects: number; totalBytes: number; blobBytes: number; manifestBytes: number }
export interface Receipt { digest: string; size: number; kind: 'manifests' | 'blobs' }
class EvidenceError extends Error {}

export function sha256(bytes: Uint8Array): string {
  return `sha256:${createHash('sha256').update(bytes).digest('hex')}`;
}

export async function verifyGraph(
  root: Descriptor,
  request: (route: string) => Promise<Response>,
  bounds: Partial<Limits> = {},
): Promise<Receipt[]> {
  const limits: Limits = { objects: 64, totalBytes: 512 * 1024 * 1024, blobBytes: 128 * 1024 * 1024, manifestBytes: 1024 * 1024, ...bounds };
  const receipts = new Map<string, Receipt>();
  let totalBytes = 0;
  async function visit(descriptor: Descriptor, kind: Receipt['kind'], depth: number): Promise<void> {
    if (!descriptor || !/^sha256:[a-f0-9]{64}$/.test(descriptor.digest) || !Number.isSafeInteger(descriptor.size) || descriptor.size < 0 || descriptor.urls !== undefined) throw new EvidenceError('Invalid or external descriptor');
    if (depth > 4 || descriptor.size > (kind === 'manifests' ? limits.manifestBytes : limits.blobBytes)) throw new EvidenceError('Descriptor bound exceeded');
    const existing = receipts.get(descriptor.digest);
    if (existing) {
      if (existing.size !== descriptor.size || existing.kind !== kind) throw new EvidenceError('Conflicting descriptor');
      return;
    }
    if (receipts.size >= limits.objects || totalBytes + descriptor.size > limits.totalBytes) throw new EvidenceError('Graph bound exceeded');
    const response = await request(`${kind}/${descriptor.digest}`);
    if (!response.ok || !response.body) throw new EvidenceError(`Registry GET failed: HTTP ${response.status}`);
    const hash = createHash('sha256');
    const chunks: Buffer[] = [];
    let size = 0;
    const reader = response.body.getReader();
    try {
      while (true) {
        const chunk = await reader.read();
        if (chunk.done) break;
        size += chunk.value.byteLength;
        if (size > descriptor.size) throw new EvidenceError('Response exceeds descriptor size');
        hash.update(chunk.value);
        if (kind === 'manifests') chunks.push(Buffer.from(chunk.value));
      }
    } finally {
      await reader.cancel();
    }
    if (size !== descriptor.size || `sha256:${hash.digest('hex')}` !== descriptor.digest) throw new EvidenceError('Response size or digest mismatch');
    totalBytes += size;
    receipts.set(descriptor.digest, { digest: descriptor.digest, size, kind });
    if (kind === 'blobs') return;
    const manifest = JSON.parse(Buffer.concat(chunks).toString('utf8'));
    if (manifest.schemaVersion !== 2 || manifest.mediaType !== descriptor.mediaType) throw new EvidenceError('Manifest type mismatch');
    if (manifest.mediaType === indexType && Array.isArray(manifest.manifests)) {
      for (const child of manifest.manifests) await visit(child, 'manifests', depth + 1);
    } else if (manifest.mediaType === manifestType && manifest.config && Array.isArray(manifest.layers)) {
      await visit(manifest.config, 'blobs', depth + 1);
      for (const layer of manifest.layers) await visit(layer, 'blobs', depth + 1);
    } else {
      throw new EvidenceError('Unsupported manifest graph');
    }
  }
  await visit(root, 'manifests', 0);
  return [...receipts.values()];
}

export function verifyIndexBytes(source: Uint8Array, target: Uint8Array): Descriptor {
  if (sha256(source) !== sourceDigest || sha256(target) !== sourceDigest) throw new EvidenceError('Unreviewed source or changed target digest');
  return { digest: sourceDigest, size: source.byteLength, mediaType: indexType };
}

async function main(directory: string): Promise<void> {
  const root = verifyIndexBytes(readFileSync(join(directory, 'source-index.json')), readFileSync(join(directory, 'target-index.json')));
  const provenance = {
    source: `${sourceImage}@${sourceDigest}`,
    target: `${targetImage}@${sourceDigest}`,
    targetTag,
    sourceDigest,
    targetDigest: sourceDigest,
    copy: { tool: 'crane', version: '0.21.9', completeIndex: true, rebuilt: false, verified: true },
    workflow: { repository: process.env.GITHUB_REPOSITORY, revision: process.env.GITHUB_SHA, runId: process.env.GITHUB_RUN_ID, runAttempt: process.env.GITHUB_RUN_ATTEMPT },
    anonymousRead: { verified: false, objects: [] as Receipt[], totalBytes: 0, failure: '' },
    clusterColdPullVerified: false,
    recordedAt: new Date().toISOString(),
  };
  const persist = () => writeFileSync(join(directory, 'provenance.json'), `${JSON.stringify(provenance, null, 2)}\n`);
  persist();
  try {
    const deadline = AbortSignal.timeout(8 * 60 * 1000);
    const signal = () => AbortSignal.any([deadline, AbortSignal.timeout(60 * 1000)]);
    const tokenResponse = await fetch('https://ghcr.io/token?service=ghcr.io&scope=repository:memeloop-online/mtc-backup-egress:pull', { signal: signal() });
    if (!tokenResponse.ok) throw new EvidenceError(`Anonymous token denied: HTTP ${tokenResponse.status}; package owner must enable public visibility and rerun`);
    const token = (await tokenResponse.json() as { token?: string }).token;
    if (!token || /[\r\n]/.test(token)) throw new EvidenceError('Invalid anonymous registry token');
    provenance.anonymousRead.objects = await verifyGraph(root, route => fetch(`https://ghcr.io/v2/memeloop-online/mtc-backup-egress/${route}`, {
      headers: { Authorization: `Bearer ${token}`, Accept: `${indexType}, ${manifestType}` }, signal: signal(),
    }));
    provenance.anonymousRead.totalBytes = provenance.anonymousRead.objects.reduce((total, receipt) => total + receipt.size, 0);
    provenance.anonymousRead.verified = true;
    persist();
    console.log(`Anonymous complete-index verification passed: ${provenance.anonymousRead.objects.length} objects, ${provenance.anonymousRead.totalBytes} bytes; cluster verification remains required`);
  } catch (error) {
    provenance.anonymousRead.failure = error instanceof EvidenceError ? error.message : 'Registry transport or manifest parsing failed';
    persist();
    throw new EvidenceError(provenance.anonymousRead.failure);
  }
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  main(process.argv[2] ?? '').catch(error => {
    console.error(error instanceof EvidenceError ? error.message : 'Archive provenance failed');
    process.exitCode = 1;
  });
}
