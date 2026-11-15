import { useEffect, useMemo, useRef, useState } from 'react';
import type { ConversationMessageListProjection } from 'memeloop';
import { Button } from './design-system';
import { useI18n } from './i18n.js';
import type { LogicalSessionDetail } from './types.js';
import { SemanticConversationViewer, type SemanticCoverage } from './SemanticConversationViewer.js';
import { semanticMessageId } from './semanticConversationIdentity.js';
import { exportSemanticContext, type SemanticViewInstance, type SemanticViewLoader, type SemanticViewPage } from './semanticViewReader.js';

const previewBudget = 128 * 1024;
const pending: SemanticCoverage = { observed_terminal: false, projection_state: 'partial', gaps: ['semantic_publication_pending'], omissions: [] };

function AtomContent({ loader, request, cursor }: { loader: SemanticViewLoader; request: string; cursor: string }) {
  const { locale } = useI18n();
  const zh = locale.startsWith('zh');
  const [text, setText] = useState('');
  const [complete, setComplete] = useState(false);
  const [error, setError] = useState('');
  useEffect(() => {
    const controller = new AbortController();
    setText(''); setComplete(false); setError('');
    void (async () => {
      const response = await loader.atom(request, cursor, controller.signal);
      const reader = response.body?.getReader();
      if (!reader) throw new Error('Missing semantic stream');
      const decoder = new TextDecoder(); let used = 0; let value = '';
      try {
        while (true) {
          const result = await reader.read();
          controller.signal.throwIfAborted();
          if (result.done) { value += decoder.decode(); setText(value); setComplete(true); break; }
          const remaining = previewBudget - used;
          value += decoder.decode(result.value.subarray(0, remaining), { stream: true });
          used += result.value.length;
          setText(value);
          if (used >= previewBudget) { await reader.cancel(); break; }
        }
      } finally { reader.releaseLock(); }
    })().catch(reason => { if (!controller.signal.aborted) setError(String(reason)); });
    return () => controller.abort();
  }, [loader, request, cursor]);
  let atom: { role?: string; parts?: Record<string, unknown>[] } | undefined;
  if (complete) { try { atom = JSON.parse(text); } catch { /* Surface the intact wire payload below. */ } }
  return <div className="semantic-atom-content">
    {error && <p role="alert">{error}</p>}
    {atom?.parts ? <><b>{atom.role}</b>{atom.parts.map((part, index) => <section key={index}>
      <small>{String(part.kind ?? part.type ?? '')}</small>
      {typeof part.text === 'string' ? <pre className="break-anywhere">{part.text}</pre> : <pre className="break-anywhere">{JSON.stringify(part, null, 2)}</pre>}
    </section>)}</> : <pre className="break-anywhere">{text}</pre>}
    {!complete && <p>{zh ? '正在读取或仅显示有界预览；正文未据此标记完整。' : 'Loading or bounded preview; this does not mark the content complete.'}</p>}
  </div>;
}

export function SemanticSessionPanel({ detail, loader }: { detail: LogicalSessionDetail; loader: SemanticViewLoader }) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const requests = detail.requests;
  const [requestId, setRequestId] = useState(requests.at(-1)?.request_id ?? '');
  const [page, setPage] = useState<SemanticViewPage>();
  const [loadedScope, setLoadedScope] = useState<{ loader: SemanticViewLoader; request: string }>();
  const [instances, setInstances] = useState<SemanticViewInstance[]>([]);
  const [error, setError] = useState<Error | null>(null);
  const [loading, setLoading] = useState(false);
  const [opened, setOpened] = useState<string>();
  const active = useRef<AbortController | null>(null);
  const exportActive = useRef<AbortController | null>(null);
  const [exporting, setExporting] = useState(false);
  const exportContext = async (only?: SemanticViewInstance) => {
    exportActive.current?.abort(); const controller = new AbortController(); exportActive.current = controller;
    setExporting(true); setError(null);
    try { await exportSemanticContext(loader, requestId, controller.signal, only); }
    catch (reason) { if (!controller.signal.aborted) setError(reason instanceof Error ? reason : new Error(String(reason))); }
    finally { if (exportActive.current === controller) setExporting(false); }
  };
  const read = async (cursor?: string, signal?: AbortSignal) => {
    active.current?.abort(); const controller = new AbortController(); active.current = controller;
    const abort = () => controller.abort(); signal?.addEventListener('abort', abort, { once: true });
    if (signal?.aborted) controller.abort();
    setLoading(true); setError(null);
    try {
      const next = await loader.page(requestId, cursor, controller.signal);
      controller.signal.throwIfAborted();
      if (cursor && page?.manifest_revision !== next.manifest_revision) throw new Error('Semantic revision changed; refresh');
      setLoadedScope({ loader, request: requestId }); setPage(next); setOpened(undefined);
      // Replace the resident page. Older pages remain available through the signed cursor.
      setInstances(next.instances);
    } catch (reason) { if (!controller.signal.aborted) setError(reason instanceof Error ? reason : new Error(String(reason))); }
    finally { signal?.removeEventListener('abort', abort); if (active.current === controller) setLoading(false); }
  };
  useEffect(() => {
    setPage(undefined); setInstances([]); setOpened(undefined);
    if (requestId) void read();
    return () => { active.current?.abort(); exportActive.current?.abort(); };
  }, [loader, requestId]);
  const visiblePage = loadedScope?.loader === loader && loadedScope.request === requestId ? page : undefined;
  const visibleInstances = visiblePage ? instances : [];
  const revision = String(visiblePage?.manifest_revision ?? 'pending');
  const messages = useMemo<ConversationMessageListProjection[]>(() => visibleInstances.map(instance => ({
    messageId: semanticMessageId(revision, instance), turnId: instance.turn_id ?? instance.instance_id,
    conversationId: detail.session_id, originNodeId: instance.origin, originSequence: instance.ordinal,
    lamportClock: instance.ordinal, timestamp: requests.find(request => request.request_id === requestId)?.created_at ?? 0,
    role: 'agent', content: '', metadata: { instance_id: instance.instance_id, branch_id: instance.branch_id },
  })), [visibleInstances, revision, detail.session_id, requestId, requests]);
  return <section aria-label={zh ? '会话记录' : 'Conversation record'}>
    <label>{zh ? '请求上下文' : 'Request context'}<select value={requestId} onChange={event => setRequestId(event.target.value)}>
      {requests.map(request => <option key={request.request_id} value={request.request_id}>{new Date(request.created_at).toLocaleString(locale)} · {request.model}</option>)}
    </select></label>
    <Button appearance="secondary" disabled={loading} onClick={() => void read()}>{zh ? '刷新会话记录' : 'Refresh conversation'}</Button>
    <Button appearance="secondary" disabled={exporting || visiblePage?.status !== 'published'} onClick={() => void exportContext()}>{zh ? '流式导出此上下文' : 'Stream context export'}</Button>
    {exporting && <Button appearance="secondary" onClick={() => exportActive.current?.abort()}>{zh ? '取消导出' : 'Cancel export'}</Button>}
    <SemanticConversationViewer revision={revision} coverage={visiblePage?.coverage ?? pending}
      readOnlyMessage={zh ? '此视图只读' : 'This view is read-only'}
      incompleteMessage={zh ? '语义记录尚未完整发布。' : 'The semantic record is not fully published.'}
      adapter={{ conversationId: detail.session_id, messages, isRunning: false, isLoading: loading, error,
        residentMessageLimit: 32, residentContentByteLimit: previewBudget, residentRenderRowLimit: 32,
        hasMoreBefore: !!visiblePage?.next_cursor, loadMoreBefore: signal => read(visiblePage?.next_cursor ?? undefined, signal) }}
      renderMessageContent={message => {
        const instance = visibleInstances.find(item => semanticMessageId(revision, item) === message.messageId);
        if (!instance) return null;
        return <><Button appearance="secondary" onClick={() => setOpened(message.messageId)}>{zh ? '读取消息' : 'Read message'} · {instance.ordinal}</Button>
          <Button appearance="secondary" disabled={exporting} onClick={() => void exportContext(instance)}>{zh ? '导出消息' : 'Export message'}</Button>
          {opened === message.messageId && <AtomContent key={instance.atom_cursor} loader={loader} request={requestId} cursor={instance.atom_cursor} />}</>;
      }} />
  </section>;
}
