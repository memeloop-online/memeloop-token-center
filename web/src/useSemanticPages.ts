import { useCallback, useEffect, useRef, useState } from 'react';
import type { SemanticViewLoader, SemanticViewPage } from './semanticViewReader.js';

/** Only one page body resides in memory; history retains signed positions. */
export function useSemanticPages(loader: SemanticViewLoader, request: string) {
  const [page, setPage] = useState<SemanticViewPage>();
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(false);
  const [position, setPosition] = useState(0);
  const history = useRef<(string | undefined)[]>([undefined]);
  const active = useRef<AbortController | null>(null);
  const scope = useRef({ loader, request });
  const revision = useRef<string | null>(null);
  const read = useCallback(async (cursor?: string, target = 0, refresh = false) => {
    active.current?.abort();
    const controller = new AbortController(); active.current = controller;
    setLoading(true); setError(null);
    try {
      const value = await loader.page(request, cursor, controller.signal);
      controller.signal.throwIfAborted();
      if (!refresh && revision.current !== value.manifest_revision) throw new Error('Semantic revision changed; refresh the conversation');
      if (refresh) history.current = [undefined];
      else history.current[target] = cursor;
      revision.current = value.manifest_revision;
      setPosition(target); setPage(value);
    } catch (reason) {
      if (!controller.signal.aborted) setError(reason instanceof Error ? reason : new Error(String(reason)));
    } finally { if (active.current === controller) setLoading(false); }
  }, [loader, request]);
  useEffect(() => {
    scope.current = { loader, request }; revision.current = null;
    history.current = [undefined]; setPage(undefined); setPosition(0);
    if (request) void read(undefined, 0, true);
    return () => active.current?.abort();
  }, [loader, request, read]);
  const visible = scope.current.loader === loader && scope.current.request === request ? page : undefined;
  return {
    page: visible, error, loading,
    refresh: () => read(undefined, 0, true),
    earlier: async () => { if (visible?.next_cursor) await read(visible.next_cursor, position + 1); },
    newer: async () => { if (position > 0) await read(history.current[position - 1], position - 1); },
    hasNewer: position > 0,
  };
}
