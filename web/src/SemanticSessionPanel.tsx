import { useEffect, useMemo, useRef, useState } from 'react';
import type { ConversationMessageListProjection } from 'memeloop';
import { Button, DataSurface, Disclosure, Field, FormSection, Select } from './design-system';
import { MessageContent } from '@memeloop/react-ui/chat';
import { useI18n } from './i18n.js';
import type { LogicalSessionDetail } from './types.js';
import { SemanticConversationViewer, type SemanticCoverage } from './SemanticConversationViewer.js';
import { semanticMessageId } from './semanticConversationIdentity.js';
import { exportSemanticContext, type SemanticViewInstance, type SemanticViewLoader, type SemanticViewPage } from './semanticViewReader.js';

const previewBudget = 128 * 1024;
function knownRole(value: unknown): value is ConversationMessageListProjection['role'] {
  return value === 'user' || value === 'assistant' || value === 'tool' || value === 'agent' || value === 'error';
}
const pending: SemanticCoverage = { observed_terminal: false, projection_state: 'partial', gaps: ['semantic_publication_pending'], omissions: [] };

function SemanticPart({ part, role, index }: { part: Record<string, unknown>; role: string; index: number }) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const kind = String(part.kind ?? part.type ?? 'unknown');
  const text = typeof part.text === 'string' ? part.text : '';
  const projection: ConversationMessageListProjection = {
    messageId: String(index), turnId: String(index), conversationId: 'semantic-part',
    role: knownRole(role) ? role : 'agent', content: text, originNodeId: 'semantic',
    originSequence: index, lamportClock: index, timestamp: 0,
  };
  if (kind === 'text') return <MessageContent message={projection} />;
  if (kind === 'reasoning') return <Disclosure title={zh ? '推理过程' : 'Reasoning'}>
    {part.completeness === 'partial' && <p>{zh ? '已记录部分推理' : 'Partially recorded reasoning'}</p>}
    <MessageContent message={projection} />
  </Disclosure>;
  if (['tool_call', 'function_call', 'tool-call'].includes(kind)) {
    const name = typeof part.tool_name === 'string' ? part.tool_name : typeof part.name === 'string' ? part.name : '';
    return <Disclosure title={`${zh ? '工具调用' : 'Tool call'} · ${name}`}>
      <p>{String(part.call_id ?? part.tool_call_id ?? '')}</p>
      <pre className="break-anywhere">{part.arguments !== undefined ? JSON.stringify(part.arguments, null, 2) : String(part.arguments_text ?? '')}</pre>
      {part.completeness === 'partial' && <p>{zh ? '参数记录不完整' : 'Arguments partially recorded'}</p>}
    </Disclosure>;
  }
  if (['tool_result', 'function_call_output', 'tool-result'].includes(kind)) return <Disclosure title={`${zh ? '工具结果' : 'Tool result'} · ${String(part.tool_name ?? '')}`}>
    <p>{String(part.tool_call_id ?? part.call_id ?? '')} · {String(part.result_status ?? '')}</p>
    {text ? <MessageContent message={projection} /> : <pre className="break-anywhere">{JSON.stringify(part.result_content ?? null, null, 2)}</pre>}
  </Disclosure>;
  if (['attachment', 'image', 'audio', 'file', 'input_image', 'input_audio'].includes(kind) || part.object_locator) return <DataSurface>
    <b>{zh ? '附件' : 'Attachment'}</b><p>{String(part.media_type ?? '')} · {String(part.byte_length ?? '')}</p>
    <p role="status">{zh ? '附件引用已记录，授权查看接口尚未就绪。' : 'Attachment reference recorded; its authorized reader is not yet available.'}</p>
  </DataSurface>;
  return <Disclosure title={`${zh ? '未识别内容类型' : 'Unrecognized content type'} · ${kind}`}>
    <pre className="break-anywhere">{JSON.stringify(part, null, 2)}</pre>
  </Disclosure>;
}

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
  let atom: { role: string; parts: Record<string, unknown>[] } | undefined;
  if (complete) {
    try {
      const value: unknown = JSON.parse(text);
      if (value && typeof value === 'object' && 'role' in value && typeof value.role === 'string'
        && 'parts' in value && Array.isArray(value.parts)
        && value.parts.every(part => part && typeof part === 'object' && !Array.isArray(part))) {
        atom = { role: value.role, parts: value.parts as Record<string, unknown>[] };
      }
    } catch { /* The bounded wire preview remains available for malformed content. */ }
  }
  return <div className="semantic-atom-content">
    {error && <p role="alert">{error}</p>}
    {atom ? <><b>{atom.role}</b>{atom.parts.map((part, index) => <SemanticPart key={index} part={part} role={atom.role} index={index} />)}</>
      : <><pre className="break-anywhere">{text}</pre>{complete && <p role="alert">{zh ? '语义消息结构无法识别。' : 'Semantic message structure is not recognized.'}</p>}</>}
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
  const messages = useMemo<ConversationMessageListProjection[]>(() => visibleInstances.flatMap(instance => {
    const role = instance.role;
    if (!knownRole(role)) return [];
    return [{
    messageId: semanticMessageId(revision, instance), turnId: instance.turn_id ?? instance.instance_id,
    conversationId: detail.session_id, originNodeId: instance.origin, originSequence: instance.ordinal,
    lamportClock: instance.ordinal, timestamp: requests.find(request => request.request_id === requestId)?.created_at ?? 0,
    role, content: '', metadata: { instance_id: instance.instance_id, branch_id: instance.branch_id },
  }]; }), [visibleInstances, revision, detail.session_id, requestId, requests]);
  return <DataSurface aria-label={zh ? '会话记录' : 'Conversation record'}>
    <FormSection title={zh ? '会话记录' : 'Conversation record'}>
    <Field label={zh ? '请求上下文' : 'Request context'}><Select value={requestId} onChange={event => setRequestId(event.target.value)}>
      {requests.map(request => <option key={request.request_id} value={request.request_id}>{new Date(request.created_at).toLocaleString(locale)} · {request.model}</option>)}
    </Select></Field>
    <Button appearance="secondary" disabled={loading} onClick={() => void read()}>{zh ? '刷新会话记录' : 'Refresh conversation'}</Button>
    {visiblePage?.next_cursor && <Button appearance="secondary" disabled={loading} onClick={() => void read(visiblePage.next_cursor ?? undefined)}>{zh ? '较早消息' : 'Earlier messages'}</Button>}
    <Button appearance="secondary" disabled={exporting || visiblePage?.status !== 'published'} onClick={() => void exportContext()}>{zh ? '流式导出此上下文' : 'Stream context export'}</Button>
    {exporting && <Button appearance="secondary" onClick={() => exportActive.current?.abort()}>{zh ? '取消导出' : 'Cancel export'}</Button>}
    </FormSection>
    {visibleInstances.filter(instance => !knownRole(instance.role)).map(instance => {
      const id = semanticMessageId(revision, instance);
      return <section key={id} data-semantic-role="unrecorded">
        <p>{zh ? '消息角色未记录，未归为用户或助手。' : 'Message role is unrecorded; it is not classified as user or assistant.'}</p>
        <Button appearance="secondary" onClick={() => setOpened(id)}>{zh ? '读取消息' : 'Read message'} · {instance.ordinal}</Button>
        {opened === id && <AtomContent key={instance.atom_cursor} loader={loader} request={requestId} cursor={instance.atom_cursor} />}
      </section>;
    })}
    <SemanticConversationViewer revision={revision} coverage={visiblePage?.coverage ?? pending}
      readOnlyMessage={zh ? '此视图只读' : 'This view is read-only'}
      incompleteMessage={zh ? '语义记录尚未完整发布。' : 'The semantic record is not fully published.'}
      adapter={{ conversationId: detail.session_id, messages, isRunning: false, isLoading: loading, error,
        residentMessageLimit: 32, residentContentByteLimit: previewBudget, residentRenderRowLimit: 32,
        hasMoreBefore: !!visiblePage?.next_cursor, loadMoreBefore: signal => read(visiblePage?.next_cursor ?? undefined, signal) }}
      renderMessageContent={message => {
        const instance = visibleInstances.find(item => semanticMessageId(revision, item) === message.messageId);
        if (!instance) return null;
        return <section data-semantic-role={message.role}><Button appearance="secondary" onClick={() => setOpened(message.messageId)}>{zh ? '读取消息' : 'Read message'} · {instance.ordinal}</Button>
          <Button appearance="secondary" disabled={exporting} onClick={() => void exportContext(instance)}>{zh ? '导出消息' : 'Export message'}</Button>
          {opened === message.messageId && <AtomContent key={instance.atom_cursor} loader={loader} request={requestId} cursor={instance.atom_cursor} />}</section>;
      }} />
  </DataSurface>;
}
