import { MessageContent } from '@memeloop/react-ui/chat';
import type { ConversationMessageListProjection } from 'memeloop';
import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Button, DataSurface, Disclosure } from './design-system';
import { useI18n } from './i18n.js';
import { exportSemanticMedia, type SemanticContentChunk, type SemanticContentField, type SemanticPartDescriptor, type SemanticPartsPage, type SemanticViewInstance, type SemanticViewLoader } from './semanticViewReader.js';

function ContentField({ loader, request, instance, revision, part, field }: {
  loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string;
  part: SemanticPartDescriptor; field: SemanticContentField;
}) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const [chunks, setChunks] = useState<{ index: number; value: SemanticContentChunk }[]>([]);
  const chunksRef = useRef(chunks); chunksRef.current = chunks;
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const positions = useRef([{ cursor: field.content_cursor, offset: 0 }]);
  const active = useRef<AbortController | null>(null);
  const stream = useRef<{ nextIndex: number; iterator: AsyncIterator<SemanticContentChunk>; controller: AbortController } | null>(null);
  const viewport = useRef<HTMLDivElement>(null);
  const pendingAnchor = useRef<{ index: string; top: number } | null>(null);
  useLayoutEffect(() => {
    const saved = pendingAnchor.current; pendingAnchor.current = null;
    const node = viewport.current;
    if (!saved || !node) return;
    const anchor = node.querySelector<HTMLElement>(`[data-content-index="${saved.index}"]`);
    if (anchor) node.scrollTop += anchor.getBoundingClientRect().top - saved.top;
  }, [chunks]);
  const move = async (target: number) => {
    if (active.current || chunksRef.current.some(chunk => chunk.index === target)) return;
    const entry = positions.current[target]; if (!entry) return;
    if (!stream.current || stream.current.nextIndex !== target) {
      stream.current?.controller.abort();
      void Promise.resolve(stream.current?.iterator.return?.()).catch(() => undefined);
      const controller = new AbortController();
      if (loader.content) stream.current = { nextIndex: target, iterator: loader.content(request, entry.cursor, controller.signal)[Symbol.asyncIterator](), controller };
    }
    const controller = stream.current?.controller ?? new AbortController(); active.current = controller;
    setLoading(true); setError('');
    try {
      if (!loader.content) throw new Error(zh ? '当前无法读取消息，请刷新后重试。' : 'Message content is unavailable. Refresh to try again.');
      const frame = await stream.current!.iterator.next();
      if (frame.done) throw new Error('Semantic content stream ended before verification');
      const value = frame.value;
      stream.current!.nextIndex = target + 1;
      if (value.complete) {
        await stream.current!.iterator.return?.();
        stream.current = null;
      }
      controller.signal.throwIfAborted();
      if (value.manifest_revision !== revision || value.instance_id !== instance.instance_id
        || value.part_index !== part.part_index || value.field !== field.name || value.format !== field.format
        || value.byte_offset !== entry.offset) throw new Error('Semantic content identity or offset changed; refresh the conversation');
      const size = new TextEncoder().encode(value.text).byteLength;
      if (value.next_cursor) {
        if (!size || positions.current.slice(0, target + 1).some(item => item.cursor === value.next_cursor)) throw new Error('Semantic content continuation did not advance');
        positions.current[target + 1] = { cursor: value.next_cursor, offset: value.byte_offset + size };
      }
      const current = chunksRef.current;
      const backwards = current.length > 0 && target < current[0].index;
      const next = backwards ? [{ index: target, value }, ...current].slice(0, 3)
        : [...current, { index: target, value }].slice(-3);
      const node = viewport.current;
      const anchor = node?.querySelector<HTMLElement>(`[data-content-index="${backwards ? current[0]?.index : current.at(-1)?.index}"]`);
      const top = anchor?.getBoundingClientRect().top;
      if (anchor?.dataset.contentIndex && top !== undefined) pendingAnchor.current = { index: anchor.dataset.contentIndex, top };
      chunksRef.current = next; setChunks(next);
    } catch (reason) {
      if (!controller.signal.aborted) setError(String(reason));
      stream.current?.controller.abort(); void Promise.resolve(stream.current?.iterator.return?.()).catch(() => undefined); stream.current = null;
    }
    finally { if (active.current === controller) { active.current = null; setLoading(false); } }
  };
  useEffect(() => { void move(0); return () => { stream.current?.controller.abort(); void Promise.resolve(stream.current?.iterator.return?.()).catch(() => undefined); active.current?.abort(); }; }, []);
  const first = chunks[0]; const last = chunks.at(-1);
  const role = instance.role;
  const richText: ConversationMessageListProjection | undefined = field.name === 'text' && field.format === 'text'
    && chunks.length === 1 && first?.index === 0 && first.value.complete
    && (role === 'user' || role === 'assistant' || role === 'tool' || role === 'agent' || role === 'error') ? {
      messageId: JSON.stringify([revision, instance.instance_id, part.part_index]), conversationId: request,
      turnId: instance.turn_id ?? instance.instance_id, role, content: first.value.text,
      originNodeId: instance.origin, originSequence: instance.ordinal, lamportClock: instance.ordinal, timestamp: 0,
    } : undefined;
  return <section aria-label={field.name}>
    <div ref={viewport} tabIndex={0} data-semantic-field={field.name} style={{ maxHeight: '24rem', overflow: 'auto', overflowAnchor: 'none' }}
      onWheel={event => {
        const node = event.currentTarget;
        if (event.deltaY > 0 && node.scrollTop + node.clientHeight >= node.scrollHeight - 64 && last?.value.next_cursor) void move(last.index + 1);
        else if (event.deltaY < 0 && node.scrollTop < 32 && first && first.index > 0) void move(first.index - 1);
      }}
      onScroll={event => {
        if (loading) return;
        const node = event.currentTarget;
        if (node.scrollTop < 32 && first && first.index > 0) void move(first.index - 1);
        else if (node.scrollTop + node.clientHeight >= node.scrollHeight - 64 && last?.value.next_cursor) void move(last.index + 1);
      }}>
      {richText ? <MessageContent message={richText} /> : chunks.map(chunk => <span key={chunk.index} data-content-index={chunk.index} style={{ whiteSpace: 'pre-wrap', overflowWrap: 'anywhere' }}>{chunk.value.text}</span>)}
    </div>
    {!last?.value.complete && <p role="status">{zh ? '已显示部分内容，完整性尚待校验。' : 'Partial content is shown; full verification is pending.'}</p>}
    {error && <p role="alert">{error}</p>}
    {loading && <p role="status">{zh ? '正在读取内容…' : 'Loading content…'}</p>}
    <div>
      {first && first.index > 0 && <Button appearance="secondary" disabled={loading} onClick={() => void move(first.index - 1)}>{zh ? '前面的内容' : 'Previous content'}</Button>}
      {last?.value.next_cursor && <Button appearance="secondary" disabled={loading} onClick={() => void move(last.index + 1)}>{zh ? '后续内容' : 'More content'}</Button>}
      {error && <Button appearance="secondary" disabled={loading} onClick={() => void move(last ? last.index + 1 : 0)}>{zh ? '重试' : 'Retry'}</Button>}
    </div>
  </section>;
}

function Part({ part, ...props }: { part: SemanticPartDescriptor; loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string }) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const [expanded, setExpanded] = useState(false);
  const [error, setError] = useState(''); const [saving, setSaving] = useState(false);
  const active = useRef<AbortController | null>(null);
  useEffect(() => () => active.current?.abort(), []);
  const fields = part.fields.map(field => <ContentField key={JSON.stringify([part.part_index, field.name, field.content_cursor])} {...props} part={part} field={field} />);
  const download = async () => {
    if (!part.media) return;
    const controller = new AbortController(); active.current = controller; setSaving(true); setError('');
    try { await exportSemanticMedia(props.loader, props.request, part.media.read_cursor, controller.signal); }
    catch (reason) { if (!controller.signal.aborted) setError(String(reason)); }
    finally { if (active.current === controller) setSaving(false); }
  };
  const content = <>
    {part.call_id && <p>{zh ? '调用标识' : 'Call ID'}: {part.call_id}</p>}
    {part.completeness === 'partial' && <p>{zh ? '此部分仅记录了部分内容。' : 'This part was partially recorded.'}</p>}
    {part.metadata_deferred && <p>{zh ? '完整详细信息见下方内容。' : 'Full details are available below.'}</p>}
    {fields}
    {!fields.length && !part.media && <p>{zh ? '此内容没有可显示的文本，可导出消息查看原始记录。' : 'This content has no displayable text. Export the message to inspect its recorded form.'}</p>}
    {part.media && <DataSurface><b>{zh ? '附件' : 'Attachment'}</b><p>{part.media.media_type} · {part.media.byte_length}</p>
      <Button appearance="secondary" disabled={saving} onClick={() => void download()}>{zh ? '保存附件' : 'Save attachment'}</Button>
      {saving && <Button appearance="secondary" onClick={() => active.current?.abort()}>{zh ? '取消' : 'Cancel'}</Button>}
      {error && <p role="alert">{error}</p>}
    </DataSurface>}
  </>;
  if (part.kind === 'text') return content;
  const label = part.kind === 'reasoning' ? (zh ? '推理过程' : 'Reasoning')
    : ['tool_call', 'function_call', 'tool-call'].includes(part.kind) ? `${zh ? '工具调用' : 'Tool call'} · ${part.tool_name ?? ''}`
    : ['tool_result', 'function_call_output', 'tool-result'].includes(part.kind) ? `${zh ? '工具结果' : 'Tool result'} · ${part.tool_name ?? ''}`
    : `${zh ? '内容类型' : 'Content type'} · ${part.kind}`;
  return <Disclosure title={label} open={expanded} onOpenChange={setExpanded}>{expanded && content}</Disclosure>;
}

function VisiblePart(props: Parameters<typeof Part>[0]) {
  const host = useRef<HTMLDivElement>(null);
  const [visible, setVisible] = useState(false);
  const [height, setHeight] = useState(64);
  useEffect(() => {
    const observer = new IntersectionObserver(entries => {
      const next = entries.some(entry => entry.isIntersecting);
      if (!next && host.current) setHeight(host.current.getBoundingClientRect().height);
      setVisible(next);
    }, { rootMargin: '100px' });
    if (host.current) observer.observe(host.current);
    return () => observer.disconnect();
  }, []);
  return <div ref={host} style={{ minHeight: visible ? undefined : height }}>{visible && <Part {...props} />}</div>;
}

function VisibleParts({ loader, request, instance, revision }: { loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string }) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const [page, setPage] = useState<SemanticPartsPage>();
  const [position, setPosition] = useState(0); const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const history = useRef([instance.atom_cursor]);
  const active = useRef<AbortController | null>(null);
  const read = async (cursor: string, target: number) => {
    active.current?.abort(); const controller = new AbortController(); active.current = controller; setLoading(true); setError('');
    try {
      if (!loader.parts) throw new Error(zh ? '当前无法读取消息，请刷新后重试。' : 'Message content is unavailable. Refresh to try again.');
      const value = await loader.parts(request, cursor, controller.signal); controller.signal.throwIfAborted();
      if (value.manifest_revision !== revision || value.instance_id !== instance.instance_id) throw new Error('Semantic message revision changed; refresh the conversation');
      if (value.next_cursor && history.current.slice(0, target + 1).includes(value.next_cursor)) throw new Error('Repeated semantic parts page');
      history.current[target] = cursor; setPosition(target); setPage(value);
    } catch (reason) { if (!controller.signal.aborted) setError(String(reason)); }
    finally { if (active.current === controller) setLoading(false); }
  };
  useEffect(() => { void read(instance.atom_cursor, 0); return () => active.current?.abort(); }, []);
  return <div className="semantic-atom-content">
    {error && <p role="alert">{error}</p>}
    {loading && <p role="status">{zh ? '正在读取消息…' : 'Loading message…'}</p>}
    {page?.parts.map(part => <VisiblePart key={JSON.stringify([revision, instance.instance_id, part.part_index])} part={part} loader={loader} request={request} instance={instance} revision={revision} />)}
    {position > 0 && <Button appearance="secondary" disabled={loading} onClick={() => void read(history.current[position - 1], position - 1)}>{zh ? '前面的内容' : 'Previous parts'}</Button>}
    {page?.next_cursor && <Button appearance="secondary" disabled={loading} onClick={() => void read(page.next_cursor!, position + 1)}>{zh ? '后续内容' : 'More parts'}</Button>}
    {error && <Button appearance="secondary" onClick={() => void read(history.current[position], position)}>{zh ? '重试' : 'Retry'}</Button>}
  </div>;
}

/** Offscreen message bodies and active readers are released together. */
export function SemanticMessageParts(props: { loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string }) {
  const host = useRef<HTMLDivElement>(null); const [visible, setVisible] = useState(false);
  const [height, setHeight] = useState(64);
  useEffect(() => {
    const observer = new IntersectionObserver(entries => {
      const next = entries.some(entry => entry.isIntersecting);
      if (!next && host.current) setHeight(host.current.getBoundingClientRect().height);
      setVisible(next);
    }, { rootMargin: '200px' });
    if (host.current) observer.observe(host.current);
    return () => observer.disconnect();
  }, []);
  return <div ref={host} style={{ minHeight: visible ? undefined : height }}>{visible && <VisibleParts key={JSON.stringify([props.request, props.revision, props.instance.instance_id, props.instance.atom_cursor])} {...props} />}</div>;
}
