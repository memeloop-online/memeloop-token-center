import { useEffect, useMemo, useRef, useState } from 'react';
import type { ConversationMessageListProjection } from 'memeloop';
import { Button, DataSurface, Field, FormSection, Select } from './design-system';
import { SemanticMessageParts } from './SemanticMessageParts.js';
import { useSemanticPages } from './useSemanticPages.js';
import { useI18n } from './i18n.js';
import type { LogicalSessionDetail } from './types.js';
import { SemanticConversationViewer } from './SemanticConversationViewer.js';
import { semanticMessageId } from './semanticConversationIdentity.js';
import { exportSemanticContext, type SemanticViewInstance, type SemanticViewLoader } from './semanticViewReader.js';

function knownRole(value: unknown): value is ConversationMessageListProjection['role'] {
  return value === 'user' || value === 'assistant' || value === 'tool' || value === 'agent' || value === 'error';
}

export function SemanticSessionPanel({ detail, loader }: { detail: LogicalSessionDetail; loader: SemanticViewLoader }) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const requests = detail.requests;
  const [requestId, setRequestId] = useState(requests.at(-1)?.request_id ?? '');
  const { page: visiblePage, error, loading, refresh, earlier, newer, hasNewer } = useSemanticPages(loader, requestId);
  const [exportError, setExportError] = useState<Error | null>(null);
  const exportActive = useRef<AbortController | null>(null);
  const [exporting, setExporting] = useState(false);
  const exportContext = async (only?: SemanticViewInstance) => {
    exportActive.current?.abort(); const controller = new AbortController(); exportActive.current = controller;
    setExporting(true); setExportError(null);
    try { await exportSemanticContext(loader, requestId, controller.signal, only, only ? visiblePage?.manifest_revision ?? undefined : undefined); }
    catch (reason) { if (!controller.signal.aborted) setExportError(reason instanceof Error ? reason : new Error(String(reason))); }
    finally { if (exportActive.current === controller) setExporting(false); }
  };
  useEffect(() => () => exportActive.current?.abort(), [loader, requestId]);
  const visibleInstances = visiblePage?.instances ?? [];
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
    <Button appearance="secondary" disabled={loading} onClick={() => void refresh()}>{zh ? '刷新会话记录' : 'Refresh conversation'}</Button>
    {visiblePage?.next_cursor && <Button appearance="secondary" disabled={loading} onClick={() => void earlier()}>{zh ? '较早消息' : 'Earlier messages'}</Button>}
    {hasNewer && <Button appearance="secondary" disabled={loading} onClick={() => void newer()}>{zh ? '较新消息' : 'Newer messages'}</Button>}
    <Button appearance="secondary" disabled={exporting || visiblePage?.status !== 'published'} onClick={() => void exportContext()}>{zh ? '流式导出此上下文' : 'Stream context export'}</Button>
    {exporting && <Button appearance="secondary" onClick={() => exportActive.current?.abort()}>{zh ? '取消导出' : 'Cancel export'}</Button>}
    </FormSection>
    {exportError && <p role="alert">{exportError.message}</p>}
    {visibleInstances.filter(instance => !knownRole(instance.role)).map(instance => {
      const id = semanticMessageId(revision, instance);
      return <section key={id} data-semantic-role="unrecorded">
        <p>{zh ? '消息角色未记录，未归为用户或助手。' : 'Message role is unrecorded; it is not classified as user or assistant.'}</p>
        <SemanticMessageParts loader={loader} request={requestId} instance={instance} revision={revision} />
      </section>;
    })}
    {!visiblePage && <p role={error ? 'alert' : 'status'}>{error ? String(error.message) : loading
      ? (zh ? '正在读取会话记录…' : 'Loading conversation record…')
      : (zh ? '尚未读取会话记录。' : 'Conversation record has not been read.')}</p>}
    {visiblePage && <SemanticConversationViewer revision={revision} coverage={visiblePage.coverage}
      readOnlyMessage={zh ? '此视图只读' : 'This view is read-only'}
      incompleteMessage={zh ? '语义记录尚未完整发布。' : 'The semantic record is not fully published.'}
      adapter={{ conversationId: detail.session_id, messages, isRunning: false, isLoading: loading, error,
        residentMessageLimit: 32, residentContentByteLimit: 32 * 64 * 1024, residentRenderRowLimit: 32,
        hasMoreBefore: Boolean(visiblePage.next_cursor), loadMoreBefore: earlier }}
      renderMessageContent={message => {
        const instance = visibleInstances.find(item => semanticMessageId(revision, item) === message.messageId);
        if (!instance) return null;
        return <section data-semantic-role={message.role} data-semantic-ordinal={instance.ordinal}>
          <Button appearance="secondary" disabled={exporting} onClick={() => void exportContext(instance)}>{zh ? '导出消息' : 'Export message'}</Button>
          <SemanticMessageParts loader={loader} request={requestId} instance={instance} revision={revision} /></section>;
      }} />}
  </DataSurface>;
}
