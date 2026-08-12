export class ApiError extends Error {
  constructor(
    message: string,
    public readonly status: number,
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
  const body = text ? (JSON.parse(text) as T | { error?: { message?: string } }) : ({} as T);
  if (!response.ok) {
    const message =
      typeof body === 'object' && body && 'error' in body
        ? body.error?.message
        : undefined;
    throw new ApiError(message ?? `HTTP ${response.status}`, response.status);
  }
  return body as T;
}
