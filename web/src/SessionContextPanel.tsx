import { useEffect, useState, type ReactNode } from 'react';
import { Button, Select } from './design-system';
import { useI18n } from './i18n.js';
import { SessionContextDisclosure, SessionUserText } from './SessionContextContent.js';
import { prependContextPage, type SessionContextLoader, type SessionContextPage, type SessionResponseSummary } from './sessionContextReader.js';
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
  return <details onToggle={(event) => setOpen(event.currentTarget.open)}>
    <summary>{label}</summary>{open ? children : null}
  </details>;
}

function RequestContext({ requestId, loader }: { requestId: string; loader: SessionContextLoader }) {
  const { t } = useI18n();
  const [page, setPage] = useState<SessionContextPage>();
  const [cursor, setCursor] = useState<string>();
  const [retry, setRetry] = useState(0);
  const [loading, setLoading] = useState(true);
  const [failed, setFailed] = useState(false);
  const [response, setResponse] = useState<SessionResponseSummary>();
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
  const users = items.filter((item) => item.role === 'user' && ['message', 'agent_message'].includes(item.kind));
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
        {answers.map((item) => <article className="session-replay-entry" key={item.node_id}><div className="session-context-message">{item.text}</div></article>)}
        {response?.state === 'ready' && response.text !== null && <article className="session-replay-entry">
          {response.total_chars > Array.from(response.text).length && response.text_cursor && response.node_id
            ? <SessionUserText item={{ node_id: response.node_id, depth: 1, role: 'assistant', kind: 'message', phase: 'final',
              text: null, text_complete: false, source_chars: response.total_chars,
              content_cursor: response.text_cursor, text_cursor: response.text_cursor }} requestId={requestId} loader={loader} />
            : <div className="session-context-message">{response.text}</div>}
        </article>}
        {response && ['pending', 'running'].includes(response.state) && <p role="status">{t('sessionReplay.responsePreparing')}</p>}
        {response?.state === 'unavailable' && <p role="status">{t('sessionReplay.responseUnavailable')}</p>}
        {!loading && answers.length === 0 && (!response || response.state === 'ready' && response.text === null) && <p className="mtc-secondary-text">{t('sessionReplay.noFinalResponse')}</p>}
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

/** Each observation retains its own path, including the paths before compaction. */
export function SessionContextPanel({ detail, loader }: {
  detail: LogicalSessionDetail; loader: SessionContextLoader;
}) {
  const { locale, t } = useI18n();
  const requests = [...detail.requests].sort((a, b) => b.created_at - a.created_at || b.request_id.localeCompare(a.request_id));
  const [selection, setSelection] = useState<string>();
  const selected = requests.find((request) => request.request_id === selection) ?? requests[0];
  return <section className="session-replay" aria-label={t('sessionReplay.title')}>
    <header className="session-replay-heading"><h3>{t('sessionReplay.content')}</h3>
      <Select aria-label={t('sessionReplay.contextObservation')} value={selected?.request_id ?? ''}
        onChange={(event) => setSelection(event.target.value)}>
        {requests.map((request) => <option key={request.request_id} value={request.request_id}>
          {new Date(request.created_at).toLocaleString(locale)} · {request.request_id.slice(-8)}
        </option>)}
      </Select>
    </header>
    <p className="mtc-secondary-text">{t('sessionReplay.contextScope')}</p>
    {selected && <RequestContext key={`${loaderIdentity(loader)}:${selected.request_id}`} requestId={selected.request_id} loader={loader} />}
  </section>;
}
