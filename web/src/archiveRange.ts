import { ApiError } from './api.js';

export interface ArchiveRangeChunk {
  bytes: Uint8Array;
  offset: number;
  totalBytes: number;
  etag: string;
}

export type ArchiveRangeLoader = (requestId: string, side: 'request' | 'response', offset: number, length: number, etag: string | undefined, signal: AbortSignal) => Promise<ArchiveRangeChunk>;

/** A changed object must never be combined with bytes from its previous version. */
export class ArchiveChangedError extends Error {
  constructor() { super('Archive changed while reading'); }
}

export class ArchiveUnavailableError extends Error {
  constructor(readonly reason: string) { super('Archive content unavailable'); }
}

/** The owner supplies an authorized product API path, never an object-store URL. */
export async function readArchiveRange(path: string, credential: string, offset: number, length: number, etag: string | undefined, signal: AbortSignal): Promise<ArchiveRangeChunk> {
  if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(length) || length <= 0 || !Number.isSafeInteger(offset + length)) throw new Error('Invalid archive byte range');
  const response = await fetch(path, {
    signal,
    headers: { Authorization: `Bearer ${credential}`, Range: `bytes=${offset}-${offset + length - 1}`, ...(etag ? { 'If-Match': etag } : {}) },
    cache: 'no-store',
  });
  if (response.status === 412) { await response.body?.cancel(); throw new ArchiveChangedError(); }
  if (response.status === 409) {
    const problem: unknown = await response.json().catch(() => undefined);
    const reason = problem && typeof problem === 'object' && 'error' in problem && problem.error && typeof problem.error === 'object'
      && 'reason' in problem.error && typeof problem.error.reason === 'string' ? problem.error.reason : 'unknown';
    throw new ArchiveUnavailableError(reason);
  }
  if (response.status !== 206) {
    await response.body?.cancel();
    throw new ApiError(`HTTP ${response.status}`, response.status);
  }
  const identity = response.headers.get('etag');
  const range = /^bytes (\d+)-(\d+)\/(\d+)$/.exec(response.headers.get('content-range') ?? '');
  if (!identity || identity.startsWith('W/') || !range || !response.body) {
    await response.body?.cancel();
    throw new Error('Invalid archive range response');
  }
  if (etag && identity !== etag) { await response.body.cancel(); throw new ArchiveChangedError(); }
  const [start, end, totalBytes] = range.slice(1).map(Number);
  if (![start, end, totalBytes].every(Number.isSafeInteger) || start !== offset || end < start || end >= totalBytes || end - start + 1 > length) {
    await response.body.cancel();
    throw new Error('Invalid archive range response');
  }
  const bytes = new Uint8Array(end - start + 1);
  const reader = response.body.getReader();
  let received = 0;
  try {
    while (true) {
      const part = await reader.read();
      if (part.done) break;
      if (received + part.value.length > bytes.length) throw new Error('Archive range exceeded its declared length');
      bytes.set(part.value, received);
      received += part.value.length;
    }
    if (received !== bytes.length) throw new Error('Archive range ended before its declared length');
  } catch (reason) {
    await reader.cancel().catch(() => undefined);
    throw reason;
  } finally { reader.releaseLock(); }
  return { bytes, offset, totalBytes, etag: identity };
}
