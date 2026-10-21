import { useEffect, useReducer, useRef, useState } from 'react';
import { api } from '../../api';
import type { OperatorUsageAnalysis } from '../../types';

export type UsageProjection = 'overview' | 'trends' | 'dimensions' | 'sessions' | 'heatmap' | 'output_rate';
type ProjectionState = { status: 'loading' } | { status: 'ready'; value: OperatorUsageAnalysis } | { status: 'error'; message: string };

/** Cache only the current credential, tenant, exact period and refresh scope. */
export function useUsageProjection(scope: object, token: string, query: string | undefined, projection: UsageProjection | undefined, fallback: string) {
  const cache = useRef({ scope, values: new Map<UsageProjection, OperatorUsageAnalysis>() });
  if (cache.current.scope !== scope) cache.current = { scope, values: new Map() };
  const current = cache.current;
  const [remote, setRemote] = useState<{ scope: object; projection: UsageProjection; state: ProjectionState }>();
  const [revision, retry] = useReducer((value: number) => value + 1, 0);
  const fallbackRef = useRef(fallback);
  fallbackRef.current = fallback;

  useEffect(() => {
    if (!token.trim() || !query || !projection || current.values.has(projection)) return;
    const controller = new AbortController();
    setRemote({ scope, projection, state: { status: 'loading' } });
    void api<OperatorUsageAnalysis>(`/internal/v1/usage-analysis${query}&projection=${projection}`, token.trim(), { signal: controller.signal })
      .then((value) => {
        if (controller.signal.aborted || cache.current !== current) return;
        current.values.set(projection, value);
        setRemote({ scope, projection, state: { status: 'ready', value } });
      })
      .catch((reason: unknown) => {
        if (!controller.signal.aborted && cache.current === current) setRemote({ scope, projection, state: { status: 'error', message: reason instanceof Error ? reason.message : fallbackRef.current } });
      });
    return () => controller.abort();
  }, [scope, token, query, projection, current, revision]);

  const cached = projection ? current.values.get(projection) : undefined;
  const state: ProjectionState | undefined = !projection || !token.trim() ? undefined
    : cached ? { status: 'ready', value: cached }
      : remote?.scope === scope && remote.projection === projection ? remote.state : { status: 'loading' };
  return { state, retry: () => { if (projection) current.values.delete(projection); retry(); } };
}
