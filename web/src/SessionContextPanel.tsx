import { useCallback, useEffect, useState, type ReactNode } from 'react';
import { Button, Disclosure, Select } from './design-system';
import { useI18n } from './i18n.js';
import { SessionContextDisclosure, SessionUserText } from './SessionContextContent.js';
import { prependContextPage, type SessionContextItem, type SessionContextLoader, type SessionContextPage, type SessionResponseSummary } from './sessionContextReader.js';
import type { LogicalSessionDetail } from './types.js';

const loaderIdentities = new WeakMap<SessionContextLoader, number>();
let loaderSequence = 0;
function loaderIdentity(loader: SessionContextLoader) {
  let identity = loaderIdentities.get(loader);
  if (identity === undefined) { identity = ++loaderSequence; loaderIdentities.set(loader, identity); }
  return identity;
}

export function DeferredSessionDetails({ label, children }: { label: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  return <Disclosure title={label} open={open} onOpenChange={setOpen}>
    {open ? children : null}
  </Disclosure>;
}

function RequestContext({ requestId, loader, hiddenUsers, onItems, contextAnswers = true }: {
  requestId: string; loader: SessionContextLoader; hiddenUsers?: Set<string>;
  onItems?: (requestId: string, items: SessionContextItem[]) => void; contextAnswers?: boolean;
}) {
  const { t } = useI18n();
  const [page, setPage] = useState<SessionContextPage>();
  const [cursor, setCursor] = useState<string>();
  const [retry, setRetry] = useState(0);
  const [loading, setLoading] = useState(true);
  const [failed, setFailed] = useState(false);
  const [response, setResponse] = useState<SessionResponseSummary>();
  useEffect(() => {
    if (page) onItems?.(requestId, page.items);
  }, [page, requestId, onItems]);
  useEffect(() => {
    const controller = new AbortController();
    setLoading(true);
    setFailed(false);
    void loader.page(requestId, cursor, false, controller.signal).then((next) => {
      if (controller.signal.aborted) return;
      const merged = prependContextPage(page, next);
      setPage(merged);
    }).catch(() => {
      if (!controller.signal.aborted) setFailed(true);
    }).finally(() => {
      if (!controller.signal.aborted) setLoading(false);
    });
    return () => controller.abort();
    // `page` is the predecessor of this cursor, never a reason to refetch it.
  }, [loader, requestId, cursor, retry]);
  useEffect(() => {
    const initial = page?.response;
    if (!initial) return;
    setResponse(initial);
    if (!loader.response || !['pending', 'running'].includes(initial.state)) return;
    const controller = new AbortController();
    let timer: ReturnType<typeof setTimeout> | undefined;
    const poll = async () => {
      try {
        const next = await loader.response!(requestId, controller.signal);
        if (controller.signal.aborted) return;
        setResponse(next);
        if (['pending', 'running'].includes(next.state)) timer = setTimeout(() => { void poll(); }, 3000);
      } catch {
        if (!controller.signal.aborted) timer = setTimeout(() => { void poll(); }, 10_000);
      }
    };
    timer = setTimeout(() => { void poll(); }, 1500);
    return () => { controller.abort(); clearTimeout(timer); };
  }, [page?.response, loader, requestId]);
  const items = page?.items ?? [];
  const users = items.filter((item) => item.role === 'user' && ['message', 'agent_message'].includes(item.kind) && !hiddenUsers?.has(item.node_id));
  const tools = items.filter((item) => !['message', 'agent_message'].includes(item.kind) || item.role === 'tool');
  // One visible answer per retained user turn. Explicit final markers take
  // precedence over unmarked candidates; commentary is never promoted.
  const answers: typeof items = [];
  let candidate: typeof items[number] | undefined;
  const flush = () => { if (candidate) answers.push(candidate); candidate = undefined; };
  for (const item of items) {
    if (item.role === 'user' && ['message', 'agent_message'].includes(item.kind)) flush();
    if (item.role === 'assistant' && item.text !== null && (!item.phase || ['final', 'final_answer'].includes(item.phase))) {
      if (!candidate?.phase || item.phase) candidate = item;
    }
  }
  flush();
  return <div aria-busy={loading}>
    {loading && <p role="status">{t('common.loading')}</p>}
    {failed && <div role="status"><p>{t('sessionReplay.contentFailed')}</p><Button disabled={loading} onClick={() => setRetry((value) => value + 1)}>{t('common.retry')}</Button></div>}
    {page?.next_cursor && !failed && <Button appearance="secondary" disabled={loading} onClick={() => setCursor(page.next_cursor!)}>{t('sessionReplay.contextEarlier')}</Button>}
    <div className="session-context-columns">
      <section aria-label={t('sessionReplay.agentOutput')}>
        {contextAnswers && answers.map((item) => <article className="session-replay-entry" key={item.node_id}><div className="session-context-message">{item.text}</div></article>)}
        {response?.state === 'ready' && response.text !== null && <article className="session-replay-entry">
          {response.total_chars > Array.from(response.text).length && response.text_cursor && response.node_id
            ? <SessionUserText item={{ node_id: response.node_id, depth: 1, role: 'assistant', kind: 'message', phase: 'final',
              text: null, text_complete: false, source_chars: response.total_chars,
              content_cursor: response.text_cursor, text_cursor: response.text_cursor }} requestId={requestId} loader={loader} />
            : <div className="session-context-message">{response.text}</div>}
        </article>}
        {response && ['pending', 'running'].includes(response.state) && <p role="status">{t('sessionReplay.responsePreparing')}</p>}
        {response?.state === 'unavailable' && <p role="status">{t('sessionReplay.responseUnavailable')}</p>}
        {!loading && (!contextAnswers || answers.length === 0) && (!response || response.state === 'ready' && response.text === null) && <p className="mtc-secondary-text">{t('sessionReplay.noFinalResponse')}</p>}
      </section>
      <aside aria-label={t('sessionReplay.userInput')}>
        {users.map((item) => <article className="session-replay-entry" key={item.node_id}>
          {item.text_complete ? <div className="session-context-message">{item.text}</div>
            : item.text_cursor ? <SessionUserText item={item} requestId={requestId} loader={loader} />
              : <><p className="mtc-secondary-text">{t('sessionReplay.largeMessage')}</p>
                <SessionContextDisclosure item={item} requestId={requestId} loader={loader} textOnly label={t('sessionReplay.fullMessage')} /></>}
        </article>)}
      </aside>
    </div>
    <DeferredSessionDetails label={t('sessionReplay.responseDetails')}>
      {items.filter((item) => !tools.includes(item)).map((item) =>
        <SessionContextDisclosure key={item.node_id} item={item} requestId={requestId} loader={loader} textOnly
          label={t('sessionReplay.contextMessage', { index: item.depth })} />)}
      <DeferredSessionDetails label={t('sessionReplay.toolCount', { count: tools.length })}>
        {tools.map((item) => <DeferredSessionDetails key={item.node_id} label={`${item.kind} · ${item.depth}`}>
          <SessionContextDisclosure item={item} requestId={requestId} loader={loader}
            label={t(item.kind.includes('output') || item.role === 'tool' ? 'sessionReplay.output' : 'sessionReplay.arguments')} />
        </DeferredSessionDetails>)}
      </DeferredSessionDetails>
    </DeferredSessionDetails>
  </div>;
}

const REQUEST_WINDOW = 4;

/** Keep only one bounded request window mounted. Prefix node identity includes
 * ancestry, so repeated text and reset branches are not text-deduplicated. */
function ContextWindow({ requests, loader }: {
  requests: LogicalSessionDetail['requests']; loader: SessionContextLoader;
}) {
  const { locale } = useI18n();
  const [pages, setPages] = useState<Record<string, SessionContextItem[]>>({});
  const onItems = useCallback((requestId: string, items: SessionContextItem[]) => {
    setPages((previous) => previous[requestId] === items ? previous : { ...previous, [requestId]: items });
  }, []);
  const seen = new Set<string>();
  return <>{[...requests].reverse().map((request, index) => {
    const hiddenUsers = new Set(seen);
    for (const item of pages[request.request_id] ?? []) seen.add(item.node_id);
    return <section key={request.request_id} className="session-context-turn" data-request-id={request.request_id}>
      <h4><time dateTime={new Date(request.created_at).toISOString()}>{new Date(request.created_at).toLocaleString(locale)}</time></h4>
      <RequestContext requestId={request.request_id} loader={loader} hiddenUsers={hiddenUsers}
        onItems={onItems} contextAnswers={index === 0} />
    </section>;
  })}</>;
}

/** Each observation retains its own path, including the paths before compaction. */
export function SessionContextPanel({ detail, loader }: {
  detail: LogicalSessionDetail; loader: SessionContextLoader;
}) {
  const { locale, t } = useI18n();
  const requests = [...detail.requests].sort((a, b) => b.created_at - a.created_at || b.request_id.localeCompare(a.request_id));
  const [selection, setSelection] = useState<string>();
  const [anchor, setAnchor] = useState<string>();
  const selected = requests.find((request) => request.request_id === selection);
  const start = Math.max(0, requests.findIndex((request) => request.request_id === anchor));
  const window = requests.slice(start, start + REQUEST_WINDOW);
  return <section className="session-replay" aria-label={t('sessionReplay.title')}>
    <header className="session-replay-heading"><h3>{t('sessionReplay.content')}</h3>
      <Select aria-label={t('sessionReplay.contextObservation')} value={selected?.request_id ?? ''}
        onChange={(event) => setSelection(event.target.value || undefined)}>
        <option value="">{t('sessionReplay.conversationHistory')}</option>
        {requests.map((request) => <option key={request.request_id} value={request.request_id}>
          {new Date(request.created_at).toLocaleString(locale)} · {request.request_id.slice(-8)}
        </option>)}
      </Select>
    </header>
    <p className="mtc-secondary-text">{t('sessionReplay.contextScope')}</p>
    {selected ? <RequestContext key={`${loaderIdentity(loader)}:${selected.request_id}`} requestId={selected.request_id} loader={loader} />
      : <>
        <div className="session-replay-heading">
          <Button appearance="secondary" disabled={start + REQUEST_WINDOW >= requests.length}
            onClick={() => setAnchor(requests[start + REQUEST_WINDOW]?.request_id)}>{t('sessionReplay.earlierTurns')}</Button>
          <Button appearance="secondary" disabled={start === 0}
            onClick={() => setAnchor(requests[Math.max(0, start - REQUEST_WINDOW)]?.request_id)}>{t('sessionReplay.newerTurns')}</Button>
        </div>
        <ContextWindow key={`${loaderIdentity(loader)}:${window.map((request) => request.request_id).join(':')}`} requests={window} loader={loader} />
      </>}
  </section>;
}
