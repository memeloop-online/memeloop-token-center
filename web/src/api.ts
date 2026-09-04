export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status: number,
    public readonly code?: string,
  ) {
    super(message);
  }
}

export async function api<T>(
  path: string,
  credential: string,
  init: RequestInit = {},
): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: {
      Authorization: `Bearer ${credential}`,
      ...(init.body ? { 'Content-Type': 'application/json' } : {}),
      ...init.headers,
    },
  });
  const text = await response.text();
  let body: T | { error?: { code?: string; message?: string } } = {} as T;
  if (text) {
    try { body = JSON.parse(text) as T | { error?: { code?: string; message?: string } }; }
    catch {
      if (!response.ok) throw new ApiError(`HTTP ${response.status}`, response.status);
      throw new ApiError(`HTTP ${response.status}: invalid JSON response`, response.status);
    }
  }
  if (!response.ok) {
    const error = typeof body === 'object' && body && 'error' in body ? body.error : undefined;
    throw new ApiError(error?.message ?? `HTTP ${response.status}`, response.status, error?.code);
  }
  return body as T;
}

const retryableReadStatuses = new Set([502, 503, 504]);

function waitForReadRetry(milliseconds: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, milliseconds));
}

/**
 * Retry an idempotent JSON read across a short transient gateway or database
 * outage. Mutating requests intentionally continue to use `api` directly.
 */
export async function apiRead<T>(
  path: string,
  credential: string,
  attempts = 4,
): Promise<T> {
  let lastError: unknown;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    try {
      return await api<T>(path, credential);
    } catch (reason) {
      lastError = reason;
      const retryable = reason instanceof TypeError
        || (reason instanceof ApiError && retryableReadStatuses.has(reason.status));
      if (!retryable || attempt + 1 >= attempts) throw reason;
      await waitForReadRetry(150 * (2 ** attempt));
    }
  }
  throw lastError;
}

/**
 * Data-bearing SSE messages emitted by Token Center.
 *
 * Contract: every message has a non-empty `id` field. For request events this is
 * the request-event UUID and must equal `data.event_id`; callers use it together
 * with the JSON `event_at` value as the durable resume cursor.
 */
export interface SseMessage<T> {
  id: string;
  event: string;
  data: T;
}

function normalizeSseLineEndings(value: string): string {
  const trailingCarriageReturn = value.endsWith('\r');
  const complete = trailingCarriageReturn ? value.slice(0, -1) : value;
  return complete.replaceAll('\r\n', '\n').replaceAll('\r', '\n')
    + (trailingCarriageReturn ? '\r' : '');
}

function parseSseMessage<T>(message: string): SseMessage<T> | undefined {
  let id: string | undefined;
  let event = 'message';
  const data: string[] = [];
  for (const line of message.split('\n')) {
    if (!line || line.startsWith(':')) continue;
    const separator = line.indexOf(':');
    const field = separator < 0 ? line : line.slice(0, separator);
    const rawValue = separator < 0 ? '' : line.slice(separator + 1);
    const value = rawValue.startsWith(' ') ? rawValue.slice(1) : rawValue;
    if (field === 'data') data.push(value);
    else if (field === 'event') event = value || 'message';
    else if (field === 'id' && !value.includes('\0')) id = value;
  }
  if (data.length === 0) return undefined;
  if (!id) throw new Error('SSE data event is missing its required id field');
  return { id, event, data: JSON.parse(data.join('\n')) as T };
}

export async function streamSse<T>(
  path: string,
  credential: string,
  signal: AbortSignal,
  onEvent: (message: SseMessage<T>) => void,
  onOpen?: () => void,
): Promise<void> {
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${credential}` },
    signal,
  });
  if (!response.ok) throw new ApiError(`HTTP ${response.status}`, response.status);
  if (!response.body) throw new Error('浏览器不支持流式响应');
  onOpen?.();
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = '';
  try {
    while (!signal.aborted) {
      const { done, value } = await reader.read();
      if (done) return;
      buffered = normalizeSseLineEndings(buffered + decoder.decode(value, { stream: true }));
      let boundary = buffered.indexOf('\n\n');
      while (boundary >= 0) {
        const parsed = parseSseMessage<T>(buffered.slice(0, boundary));
        buffered = buffered.slice(boundary + 2);
        if (parsed) onEvent(parsed);
        boundary = buffered.indexOf('\n\n');
      }
    }
  } finally {
    try { await reader.cancel(); } catch { /* Abort and remote close can already release the reader. */ }
    reader.releaseLock();
  }
}
