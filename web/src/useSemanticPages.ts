import { useCallback, useEffect, useRef, useState } from 'react';
import type { SemanticViewLoader, SemanticViewPage } from './semanticViewReader.js';

/** Only one page body resides in memory; history retains signed positions. */
export function useSemanticPages(loader: SemanticViewLoader, request: string) {
  const [page, setPage] = useState<SemanticViewPage>();
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(false);
  const [position, setPosition] = useState(0);
  const history = useRef<(string | undefined)[]>([undefined]);
  const host = useRef<HTMLDivElement>(null);
  const scrolls = useRef<{ window: number; viewport: number }[]>([]);
  const currentPosition = useRef(0);
  const active = useRef<AbortController | null>(null);
  const scope = useRef({ loader, request });
  const revision = useRef<string | null>(null);
  const read = useCallback(async (cursor?: string, target = 0, refresh = false, signal?: AbortSignal) => {
    scrolls.current[currentPosition.current] = { window: window.scrollY, viewport: host.current?.querySelector('[data-testid=conversation-viewport]')?.scrollTop ?? 0 };
    active.current?.abort();
    const controller = new AbortController(); active.current = controller;
    const abort = () => controller.abort();
    signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) controller.abort();
    setLoading(true); setError(null);
    try {
      const value = await loader.page(request, cursor, controller.signal);
      controller.signal.throwIfAborted();
      if (!refresh && revision.current !== value.manifest_revision) throw new Error('Semantic revision changed; refresh the conversation');
      if (refresh) { history.current = [undefined]; scrolls.current = [scrolls.current[0] ?? { window: 0, viewport: 0 }]; }
      else history.current[target] = cursor;
      revision.current = value.manifest_revision;
      currentPosition.current = target;
      setPosition(target); setPage(value);
      if (!refresh) requestAnimationFrame(() => requestAnimationFrame(() => {
        if (controller.signal.aborted) return;
        const saved = scrolls.current[target];
        const viewport = host.current?.querySelector('[data-testid=conversation-viewport]');
        if (saved) { window.scrollTo({ top: saved.window }); if (viewport) viewport.scrollTop = saved.viewport; }
      }));
    } catch (reason) {
      if (!controller.signal.aborted) setError(reason instanceof Error ? reason : new Error(String(reason)));
    } finally { signal?.removeEventListener('abort', abort); if (active.current === controller) setLoading(false); }
  }, [loader, request]);
  useEffect(() => {
    scope.current = { loader, request }; revision.current = null;
    history.current = [undefined]; scrolls.current = []; currentPosition.current = 0; setPage(undefined); setPosition(0);
    if (request) void read(undefined, 0, true);
    return () => active.current?.abort();
  }, [loader, request, read]);
  const visible = scope.current.loader === loader && scope.current.request === request ? page : undefined;
  return {
    page: visible, error, loading, host,
    refresh: () => read(undefined, 0, true),
    earlier: async (signal?: AbortSignal) => {
      if (loading || !visible?.next_cursor) return;
      if (history.current.slice(0, position + 1).includes(visible.next_cursor)) {
        setError(new Error('Repeated semantic page; refresh the conversation')); return;
      }
      await read(visible.next_cursor, position + 1, false, signal);
    },
    newer: async () => { if (!loading && position > 0) await read(history.current[position - 1], position - 1); },
    hasNewer: position > 0,
  };
}
