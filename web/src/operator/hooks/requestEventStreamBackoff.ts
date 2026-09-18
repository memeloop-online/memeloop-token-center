export const REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS = 1_000;
export const REQUEST_EVENT_RECONNECT_MAX_DELAY_MS = 30_000;
export const REQUEST_EVENT_RECONNECT_JITTER_RATIO = 0.2;

function boundedUnit(value: number): number {
  if (!Number.isFinite(value)) return 0.5;
  return Math.min(1, Math.max(0, value));
}

/**
 * Return the delay before the next request-event stream attempt.
 *
 * `attempt` is zero-based and is reset by the hook after a stream opens.
 * `jitterSample` is kept as an argument so callers and tests can supply a
 * deterministic value without coupling this policy to a clock or a source of
 * randomness.
 */
export function requestEventReconnectDelayMs(attempt: number, jitterSample = 0.5): number {
  const safeAttempt = Number.isFinite(attempt) ? Math.max(0, Math.floor(attempt)) : 0;
  const base = Math.min(
    REQUEST_EVENT_RECONNECT_MAX_DELAY_MS,
    REQUEST_EVENT_RECONNECT_INITIAL_DELAY_MS * (2 ** Math.min(safeAttempt, 30)),
  );
  const jitter = (boundedUnit(jitterSample) * 2 - 1) * REQUEST_EVENT_RECONNECT_JITTER_RATIO;
  return Math.max(0, Math.min(
    REQUEST_EVENT_RECONNECT_MAX_DELAY_MS,
    Math.round(base * (1 + jitter)),
  ));
}
