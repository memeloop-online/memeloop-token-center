import { useCallback, useEffect, useReducer, useRef } from 'react';

export type ResourceState<T> =
  | { kind: 'idle'; scopeKey: string }
  | { kind: 'loading'; scopeKey: string }
  | { kind: 'ready'; scopeKey: string; value: T; refreshError?: string }
  | { kind: 'failed'; scopeKey: string; message: string };

type ResourceAction<T> =
  | { type: 'reset'; scopeKey: string }
  | { type: 'loading'; scopeKey: string }
  | { type: 'ready'; scopeKey: string; value: T }
  | { type: 'failed'; scopeKey: string; message: string };

function reducer<T>(state: ResourceState<T>, action: ResourceAction<T>): ResourceState<T> {
  switch (action.type) {
    case 'reset': return { kind: 'idle', scopeKey: action.scopeKey };
    // A manual refresh must not unmount an already rendered workspace. Besides
    // producing a distracting flash, unmounting loses in-progress OAuth state
    // and action confirmations owned by that workspace.
    case 'loading': return state.kind === 'ready' && state.scopeKey === action.scopeKey
      ? { ...state, refreshError: undefined } : { kind: 'loading', scopeKey: action.scopeKey };
    case 'ready': return { kind: 'ready', scopeKey: action.scopeKey, value: action.value };
    // Keep an already usable workspace mounted when a background refresh
    // fails. Action-local feedback and OAuth/form state must not disappear.
    case 'failed': return state.kind === 'ready' && state.scopeKey === action.scopeKey
      ? { ...state, refreshError: action.message }
      : { kind: 'failed', scopeKey: action.scopeKey, message: action.message };
  }
}

export function useOperatorResource<T>(
  enabled: boolean,
  scopeKey: string,
  load: () => Promise<T>,
  fallbackMessage: string,
) {
  const [state, dispatch] = useReducer(reducer<T>, { kind: 'idle', scopeKey });
  const loadRef = useRef(load);
  const fallbackMessageRef = useRef(fallbackMessage);
  const sequence = useRef(0);
  loadRef.current = load;
  fallbackMessageRef.current = fallbackMessage;

  const reload = useCallback(async () => {
    if (!enabled) {
      sequence.current += 1;
      dispatch({ type: 'reset', scopeKey });
      return;
    }
    const request = ++sequence.current;
    dispatch({ type: 'loading', scopeKey });
    try {
      const value = await loadRef.current();
      if (request === sequence.current) dispatch({ type: 'ready', scopeKey, value });
    } catch (reason) {
      if (request === sequence.current) {
        dispatch({ type: 'failed', scopeKey, message: reason instanceof Error ? reason.message : fallbackMessageRef.current });
      }
    }
  }, [enabled, scopeKey]);

  useEffect(() => {
    // A real scope change must discard the previous tenant's value. Locale and
    // translated fallback changes deliberately do not participate in this
    // lifecycle, so they cannot reset business state.
    dispatch({ type: 'reset', scopeKey });
    void reload();
    return () => { sequence.current += 1; };
  }, [reload, scopeKey]);

  // Effects run after paint. Never expose a ready value from the previous
  // tenant/token during the render in which scopeKey changes.
  const visibleState: ResourceState<T> = state.scopeKey === scopeKey
    ? state
    : { kind: 'loading', scopeKey };
  return { state: visibleState, reload };
}
