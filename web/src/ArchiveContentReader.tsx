import { useEffect, useRef, useState, type ReactNode } from 'react';
import { ArchiveChangedError, ArchiveUnavailableError, type ArchiveRangeLoader } from './archiveRange.js';
import { ArchiveItemStream, type ArchiveStreamItem } from './archiveItemStream.js';
import { Button, Spinner } from './design-system';
import { useI18n } from './i18n.js';
import { projectSessionReplay, type SessionReplayItem } from './sessionReplayProjection.js';
import type { RequestDetail, RequestView } from './types.js';
import './archiveContentReader.css';

const BYTE_PAGE = 64 * 1024;
const CONTENT_PAGE = 30;

interface ReadCursor {
  parser: ArchiveItemStream;
  decoder: TextDecoder;
  byteOffset: number;
  totalBytes?: number;
  etag?: string;
  pending: SessionReplayItem[];
  frame?: { detail: RequestDetail; offset: number };
  itemOffset: number;
  unknown: Set<string>;
  eof: boolean;
}

function frameDetail(request: RequestView, side: 'request' | 'response', frame: ArchiveStreamItem): RequestDetail {
  const value = frame.format === 'input' && typeof frame.value === 'string' ? frame.value : [frame.value];
  return {
    ...request,
    archive_complete: true,
    request_body: side === 'request' ? { [frame.format]: value } : { input: [] },
    response_body: side === 'response' ? { [frame.format]: frame.format === 'output_text' ? frame.value : value } : { output: [] },
  };
}

/** One explicitly opened archive, framed incrementally and displayed a page at a time. */
export function ArchiveContentReader({ request, sessionId, side, loadRange, renderItem }: {
  request: RequestView;
  sessionId: string;
  side: 'request' | 'response';
  loadRange: ArchiveRangeLoader;
  renderItem: (item: SessionReplayItem, index: number) => ReactNode;
}) {
  const { t } = useI18n();
  const key = `${sessionId}\0${request.request_id}\0${side}`;
  const [opened, setOpened] = useState(false);
  const [page, setPage] = useState<{ key: string; loader: ArchiveRangeLoader; offset: number; items: SessionReplayItem[]; done: boolean }>({ key, loader: loadRange, offset: 0, items: [], done: false });
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState('');
  const [progress, setProgress] = useState({ loaded: 0, total: 0 });
  const cursor = useRef<ReadCursor | undefined>(undefined);
  const controller = useRef<AbortController | undefined>(undefined);
  const visible = page.key === key && page.loader === loadRange ? page : undefined;

  useEffect(() => {
    cursor.current = undefined;
    setOpened(false); setLoading(false); setError('');
    setPage({ key, loader: loadRange, offset: 0, items: [], done: false });
    setProgress({ loaded: 0, total: 0 });
    return () => { controller.current?.abort(); cursor.current = undefined; };
  }, [key, loadRange]);

  function newCursor(): ReadCursor {
    return { parser: new ArchiveItemStream(side), decoder: new TextDecoder('utf-8', { fatal: true }), byteOffset: 0, pending: [], itemOffset: 0, unknown: new Set(), eof: false };
  }

  async function readPage(offset: number) {
    controller.current?.abort();
    const pending = new AbortController();
    controller.current = pending;
    const current = () => controller.current === pending && !pending.signal.aborted;
    setOpened(true); setLoading(true); setError('');
    if (!cursor.current || cursor.current.itemOffset !== offset) cursor.current = newCursor();
    const read = cursor.current;
    const items: SessionReplayItem[] = [];
    try {
      if (request.session_context?.session_id !== sessionId) throw new Error('Archive session changed');
      while (items.length < CONTENT_PAGE && !read.eof) {
        if (!current()) return;
        if (read.pending.length) {
          const item = read.pending.shift()!;
          if (read.itemOffset++ >= offset) items.push(item);
          continue;
        }
        if (read.frame) {
          const projection = projectSessionReplay(sessionId, [read.frame.detail], read.frame.offset);
          read.pending = projection.items.filter(item => {
            if (item.kind !== 'unknown') return true;
            const identity = `${item.body}:${item.reason}`;
            if (read.unknown.has(identity)) return false;
            read.unknown.add(identity); return true;
          }).map(item => (item.kind === 'tool_call' || item.kind === 'tool_result') && item.pairing === 'unpaired'
            ? { ...item, pairing: 'unknown' as const } : item);
          read.frame = projection.nextItemOffset === null ? undefined : { detail: read.frame.detail, offset: projection.nextItemOffset };
          continue;
        }
        const frame = read.parser.take(1)[0];
        if (frame) { read.frame = { detail: frameDetail(request, side, frame), offset: 0 }; continue; }
        if (read.totalBytes !== undefined && read.byteOffset >= read.totalBytes) {
          read.parser.finish(); read.eof = true; break;
        }
        const part = await loadRange(request.request_id, side, read.byteOffset, BYTE_PAGE, read.etag, pending.signal);
        if (!current()) return;
        if (!part.bytes.length || part.offset !== read.byteOffset) throw new Error('Invalid archive byte page');
        if (read.etag && part.etag !== read.etag) throw new ArchiveChangedError();
        if (read.totalBytes !== undefined && part.totalBytes !== read.totalBytes) throw new ArchiveChangedError();
        read.etag = part.etag; read.totalBytes = part.totalBytes;
        read.byteOffset += part.bytes.length;
        read.parser.append(read.decoder.decode(part.bytes, { stream: read.byteOffset < part.totalBytes }));
        setProgress({ loaded: read.byteOffset, total: part.totalBytes });
        // Yield between byte pages instead of parsing a large archive in one render.
        await new Promise<void>(resolve => window.setTimeout(resolve, 0));
      }
      if (current()) setPage(previous => !items.length && read.eof && offset > 0
        && previous.key === key && previous.loader === loadRange && previous.offset + previous.items.length === offset
        ? { ...previous, done: true } : { key, loader: loadRange, offset, items, done: read.eof });
    } catch (reason) {
      if (!current()) return;
      cursor.current = undefined;
      if (reason instanceof ArchiveChangedError) {
        setPage({ key, loader: loadRange, offset: 0, items: [], done: false });
        setProgress({ loaded: 0, total: 0 });
        setError(t('sessionReplay.archiveChanged'));
      } else {
        if (items.length) setPage({ key, loader: loadRange, offset, items, done: false });
        setError(reason instanceof ArchiveUnavailableError
          ? t(reason.reason === 'archive_pending' ? 'sessionReplay.archivePending' : reason.reason === 'media_body_not_archived_by_policy' ? 'sessionReplay.archiveNotRetained' : 'sessionReplay.archiveUnavailable')
          : reason instanceof Error && reason.message.startsWith('Unsupported archive') ? t('sessionReplay.unsupportedBody')
            : reason instanceof Error && reason.message.startsWith('Archive stream') ? t('sessionReplay.archiveInterrupted')
              : t('sessionReplay.archiveReadFailed'));
      }
    } finally { if (current()) setLoading(false); }
  }

  if (!opened) return <section className="archive-content-reader collapsed" aria-label={t('sessionReplay.content')}>
    <Button appearance="primary" onClick={() => void readPage(0)}>{t('sessionReplay.readFullSide', { side: t(side === 'request' ? 'request.request' : 'request.response') })}</Button>
  </section>;
  return <section className="archive-content-reader" aria-label={t('sessionReplay.content')}>
    <header className="archive-content-reader-header">
      <b>{t(side === 'request' ? 'request.request' : 'request.response')}</b>
      {loading && <Spinner size="extra-small" aria-label={t('sessionReplay.loading')}
        label={t('sessionReplay.readBytes', { loaded: progress.loaded.toLocaleString(), total: progress.total > 0 ? progress.total.toLocaleString() : '…' })} />}
      <Button appearance="subtle" onClick={() => { controller.current?.abort(); cursor.current = undefined; setLoading(false); setOpened(false); setError(''); setPage({ key, loader: loadRange, offset: 0, items: [], done: false }); setProgress({ loaded: 0, total: 0 }); }}>{t('common.close')}</Button>
    </header>
    {error && <div className="archive-content-reader-notice error" role="alert"><span>{error}</span><Button appearance="secondary" disabled={loading} onClick={() => void readPage(visible?.offset ?? 0)}>{t('sessionReplay.retryArchive')}</Button></div>}
    <ol className="session-replay-feed">{visible?.items.map((item, index) => <li key={`${visible.offset + index}:${item.kind}`}>{renderItem(item, index)}</li>)}</ol>
    {visible?.done && !visible.items.length && !loading && <p className="archive-content-reader-end">{t('sessionReplay.archiveEnd')}</p>}
    <nav className="archive-content-reader-nav" aria-label={t('sessionReplay.content')}>
      <Button appearance="secondary" disabled={loading || !visible || visible.offset === 0} onClick={() => void readPage(Math.max(0, (visible?.offset ?? 0) - CONTENT_PAGE))}>{t('sessionReplay.previousContent')}</Button>
      <span className="archive-content-reader-range" role="status">{visible && visible.items.length > 0 ? t('sessionReplay.itemRange', { start: visible.offset + 1, end: visible.offset + visible.items.length, total: visible.done ? visible.offset + visible.items.length : '…' }) : ''}</span>
      <Button appearance="secondary" disabled={loading || Boolean(error) || !visible || visible.done} onClick={() => void readPage((visible?.offset ?? 0) + (visible?.items.length ?? 0))}>{t('sessionReplay.nextContent')}</Button>
    </nav>
  </section>;
}
