import { useEffect, useRef, useState } from 'react';
import { Button, DataSurface, Disclosure } from './design-system';
import { useI18n } from './i18n.js';
import { exportSemanticMedia, type SemanticContentChunk, type SemanticContentField, type SemanticPartDescriptor, type SemanticPartsPage, type SemanticViewInstance, type SemanticViewLoader } from './semanticViewReader.js';

function ContentField({ loader, request, instance, revision, part, field }: {
  loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string;
  part: SemanticPartDescriptor; field: SemanticContentField;
}) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
  const [chunk, setChunk] = useState<SemanticContentChunk>();
  const [position, setPosition] = useState(0);
  const [error, setError] = useState('');
  const [loading, setLoading] = useState(false);
  const positions = useRef([{ cursor: field.content_cursor, offset: 0 }]);
  const active = useRef<AbortController | null>(null);
  const viewport = useRef<HTMLDivElement>(null);
  const move = async (target: number, fromEnd = false) => {
    if (active.current) return;
    const entry = positions.current[target]; if (!entry) return;
    const controller = new AbortController(); active.current = controller;
    setLoading(true); setError('');
    try {
      if (!loader.content) throw new Error(zh ? '当前无法读取消息，请刷新后重试。' : 'Message content is unavailable. Refresh to try again.');
      const value = await loader.content(request, entry.cursor, controller.signal);
      controller.signal.throwIfAborted();
      if (value.manifest_revision !== revision || value.instance_id !== instance.instance_id
        || value.part_index !== part.part_index || value.field !== field.name || value.format !== field.format
        || value.byte_offset !== entry.offset) throw new Error('Semantic content identity or offset changed; refresh the conversation');
      const size = new TextEncoder().encode(value.text).byteLength;
      if (value.next_cursor) {
        if (!size || positions.current.slice(0, target + 1).some(item => item.cursor === value.next_cursor)) throw new Error('Semantic content continuation did not advance');
        positions.current[target + 1] = { cursor: value.next_cursor, offset: value.byte_offset + size };
      }
      setChunk(value); setPosition(target);
      requestAnimationFrame(() => {
        const node = viewport.current;
        if (node) node.scrollTop = fromEnd ? Math.max(0, node.scrollHeight - node.clientHeight - 1) : 1;
      });
    } catch (reason) { if (!controller.signal.aborted) setError(String(reason)); }
    finally { if (active.current === controller) { active.current = null; setLoading(false); } }
  };
  useEffect(() => { void move(0); return () => active.current?.abort(); }, []);
  return <section aria-label={field.name}>
    <div ref={viewport} tabIndex={0} style={{ maxHeight: '24rem', overflow: 'auto', overflowAnchor: 'none' }}
      onScroll={event => {
        if (loading) return;
        const node = event.currentTarget;
        if (node.scrollTop === 0 && position > 0) void move(position - 1, true);
        else if (node.scrollTop + node.clientHeight >= node.scrollHeight - 1 && chunk?.next_cursor) void move(position + 1);
      }}>
      <pre className="break-anywhere" style={{ whiteSpace: 'pre-wrap', margin: 0 }}>{chunk?.text}</pre>
    </div>
    {error && <p role="alert">{error}</p>}
    {loading && <p role="status">{zh ? '正在读取内容…' : 'Loading content…'}</p>}
    <div>
      {position > 0 && <Button appearance="secondary" disabled={loading} onClick={() => void move(position - 1, true)}>{zh ? '上一段内容' : 'Previous content'}</Button>}
      {chunk?.next_cursor && <Button appearance="secondary" disabled={loading} onClick={() => void move(position + 1)}>{zh ? '下一段内容' : 'Next content'}</Button>}
      {error && <Button appearance="secondary" disabled={loading} onClick={() => void move(position)}>{zh ? '重试' : 'Retry'}</Button>}
    </div>
  </section>;
}

function Part({ part, ...props }: { part: SemanticPartDescriptor; loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string }) {
  const { locale } = useI18n(); const zh = locale.startsWith('zh');
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
    {fields}
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
  return <Disclosure title={label}>{content}</Disclosure>;
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
    {page?.parts.map(part => <Part key={JSON.stringify([revision, instance.instance_id, part.part_index])} part={part} loader={loader} request={request} instance={instance} revision={revision} />)}
    {position > 0 && <Button appearance="secondary" disabled={loading} onClick={() => void read(history.current[position - 1], position - 1)}>{zh ? '前面的内容' : 'Previous parts'}</Button>}
    {page?.next_cursor && <Button appearance="secondary" disabled={loading} onClick={() => void read(page.next_cursor!, position + 1)}>{zh ? '后续内容' : 'More parts'}</Button>}
    {error && <Button appearance="secondary" onClick={() => void read(history.current[position], position)}>{zh ? '重试' : 'Retry'}</Button>}
  </div>;
}

/** Offscreen message bodies and active readers are released together. */
export function SemanticMessageParts(props: { loader: SemanticViewLoader; request: string; instance: SemanticViewInstance; revision: string }) {
  const host = useRef<HTMLDivElement>(null); const [visible, setVisible] = useState(false);
  useEffect(() => {
    const observer = new IntersectionObserver(entries => setVisible(entries.some(entry => entry.isIntersecting)), { rootMargin: '200px' });
    if (host.current) observer.observe(host.current);
    return () => observer.disconnect();
  }, []);
  return <div ref={host} style={{ minHeight: '4rem' }}>{visible && <VisibleParts key={JSON.stringify([props.revision, props.instance.instance_id, props.instance.atom_cursor])} {...props} />}</div>;
}
