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

export async function streamSse<T>(
  path: string,
  credential: string,
  signal: AbortSignal,
  onEvent: (event: T) => void,
): Promise<void> {
  const response = await fetch(path, {
    headers: { Authorization: `Bearer ${credential}` },
    signal,
  });
  if (!response.ok) throw new ApiError(`HTTP ${response.status}`, response.status);
  if (!response.body) throw new Error('浏览器不支持流式响应');
  const reader = response.body.getReader();
  const decoder = new TextDecoder();
  let buffered = '';
  while (!signal.aborted) {
    const { done, value } = await reader.read();
    if (done) return;
    buffered += decoder.decode(value, { stream: true }).replaceAll('\r\n', '\n');
    let boundary = buffered.indexOf('\n\n');
    while (boundary >= 0) {
      const message = buffered.slice(0, boundary);
      buffered = buffered.slice(boundary + 2);
      const data = message
        .split('\n')
        .filter((line) => line.startsWith('data:'))
        .map((line) => line.slice(5).trimStart())
        .join('\n');
      if (data) onEvent(JSON.parse(data) as T);
      boundary = buffered.indexOf('\n\n');
    }
  }
}
