import { useEffect, useState } from 'react';
import { Button } from './design-system';
import { useI18n } from './i18n.js';
import {
  appendContextChunk, contextMessageText, type ContextContentState,
  type SessionContextItem, type SessionContextLoader,
} from './sessionContextReader.js';

/**
 * Mount only inside an opened disclosure. A large tool's remaining chunks are
 * explicitly requested, never downloaded merely because its parent is visible.
 */
export function SessionContextContent({ item, requestId, loader, textOnly = false }: {
  item: SessionContextItem; requestId: string; loader: SessionContextLoader; textOnly?: boolean;
}) {
  const { t } = useI18n();
  const [state, setState] = useState<ContextContentState>();
  const [error, setError] = useState(false);
  const [loading, setLoading] = useState(false);
  const [requested, setRequested] = useState(item.content_cursor);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setError(false);
    void loader.content(requestId, requested, controller.signal).then((chunk) => {
      if (controller.signal.aborted) return;
      // Validate before state publication; a malformed completion must produce
      // a recoverable read error rather than crash the React render.
      const next = appendContextChunk(state, item.node_id, chunk);
      setState(next);
    }).catch(() => {
      if (!controller.signal.aborted) setError(true);
    }).finally(() => {
      if (!controller.signal.aborted) setLoading(false);
    });
    return () => controller.abort();
    // Each cursor owns exactly one read. State is intentionally its committed
    // predecessor; including state would recursively read the same cursor.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [loader, requestId, item.node_id, requested, retry]);
  const complete = state?.nextCursor === null;
  let content = state?.chunks.join('') ?? '';
  let invalidJson = false;
  if (complete) {
    try {
      const value: unknown = JSON.parse(content);
      content = textOnly ? contextMessageText(value) ?? '' : JSON.stringify(value, null, 2);
    } catch { invalidJson = true; }
  }
  return <div className="session-context-content" aria-busy={loading}>
    {!textOnly && content && <pre>{content}</pre>}
    {textOnly && complete && !invalidJson && <div className="session-context-message">{content}</div>}
    {!complete && state && <p className="mtc-secondary-text">
      {t('sessionReplay.contentProgress', { loaded: state.loadedChars, total: state.totalChars })}
    </p>}
    {loading && <p role="status">{t('common.loading')}</p>}
    {(error || invalidJson) && <p role="status">{t('sessionReplay.contentFailed')}</p>}
    {error && <Button appearance="secondary" disabled={loading} onClick={() => setRetry((value) => value + 1)}>{t('common.retry')}</Button>}
    {state?.nextCursor && !error && <Button appearance="secondary" disabled={loading} onClick={() => setRequested(state.nextCursor!)}>{t('sessionReplay.loadMoreContent')}</Button>}
  </div>;
}

/** The content component (and its HTTP effect) does not exist until opened. */
export function SessionContextDisclosure({ item, requestId, loader, label, textOnly }: {
  item: SessionContextItem; requestId: string; loader: SessionContextLoader; label: string; textOnly?: boolean;
}) {
  const [open, setOpen] = useState(false);
  return <details onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>{label}</summary>
    {open && <SessionContextContent key={`${requestId}:${item.node_id}`} item={item} requestId={requestId} loader={loader} textOnly={textOnly} />}
  </details>;
}

/** User text is visible in full by default; each sequential request is bounded.
 * Text-only cursors cannot expose tool/media blocks from a mixed user message.
 */
export function SessionUserText({ item, requestId, loader }: {
  item: SessionContextItem; requestId: string; loader: SessionContextLoader;
}) {
  const { t } = useI18n();
  const [state, setState] = useState<ContextContentState>();
  const [failed, setFailed] = useState(false);
  const [loading, setLoading] = useState(true);
  const [retry, setRetry] = useState(0);
  useEffect(() => {
    const controller = new AbortController();
    setFailed(false);
    setLoading(true);
    const read = async () => {
      let current = state;
      let next = current?.nextCursor ?? item.text_cursor;
      if (!next) throw new Error('Missing message text cursor');
      while (next) {
        const chunk = await loader.content(requestId, next, controller.signal);
        controller.signal.throwIfAborted();
        if (chunk.representation !== 'message_text') throw new Error('Expected message text');
        current = appendContextChunk(current, item.node_id, chunk);
        setState(current);
        next = current.nextCursor;
      }
    };
    void read().catch(() => {
      if (!controller.signal.aborted) setFailed(true);
    }).finally(() => {
      if (!controller.signal.aborted) setLoading(false);
    });
    return () => controller.abort();
  }, [item.node_id, item.text_cursor, requestId, loader, retry]);
  return <div className="session-context-message" aria-busy={loading}>
    {state?.chunks.join('')}
    {loading && <p role="status">{t('common.loading')}</p>}
    {failed && <><p role="status">{t('sessionReplay.contentFailed')}</p>
      <Button appearance="secondary" onClick={() => setRetry((value) => value + 1)}>{t('common.retry')}</Button></>}
  </div>;
}
