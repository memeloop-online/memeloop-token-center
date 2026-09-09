import { useEffect, useMemo, useRef, useState } from 'react';
import {
  projectSessionReplay,
  SESSION_REPLAY_MAX_REQUESTS,
  type ReplayUnknownReason,
  type SessionReplayItem,
} from './sessionReplayProjection.js';
import { useI18n } from './i18n.js';
import type { LogicalSessionDetail, RequestDetail, RequestView } from './types.js';
import './sessionReplay.css';

const REPLAY_ARCHIVE_CONCURRENCY = 4;
const REPLAY_EXPAND_TEXT_LENGTH = 1_600;

export type SessionReplayArchiveLoader = (request: RequestView, signal: AbortSignal) => Promise<RequestDetail>;

interface ReplayEntry {
  item: SessionReplayItem;
  index: number;
  archiveBodies?: 2;
}

type Translate = ReturnType<typeof useI18n>['t'];

function requestOrder(left: RequestView, right: RequestView) {
  return left.created_at - right.created_at || left.request_id.localeCompare(right.request_id);
}

function unknownLabel(t: Translate, reason: ReplayUnknownReason | null, requestAndResponse = false) {
  switch (reason) {
    case 'archive_unavailable': return requestAndResponse ? t('sessionReplay.archiveUnavailableBoth') : t('sessionReplay.archiveUnavailable');
    case 'redacted': return t('sessionReplay.redacted');
    case 'unsupported_body': return t('sessionReplay.unsupportedBody');
    case 'missing_text': return t('sessionReplay.textUnavailable');
    case 'outside_session': return t('sessionReplay.sessionMismatch');
    case 'input_limit': return t('sessionReplay.requestLimit');
    default: return t('sessionReplay.unknown');
  }
}

function messageLabel(t: Translate, role: 'user' | 'assistant') {
  return role === 'user' ? t('sessionReplay.user') : t('sessionReplay.agent');
}

function bodyOrder(item: SessionReplayItem) {
  return item.body === 'request' ? 0 : 1;
}

function ArchiveText({ value, kind, t }: { value: string; kind: 'message' | 'tool'; t: Translate }) {
  const [expanded, setExpanded] = useState(false);
  const body = <pre className={`session-replay-body ${kind}`}>{value}</pre>;
  if (value.length <= REPLAY_EXPAND_TEXT_LENGTH) return body;
  return <>
    {!expanded && <p className={`session-replay-preview ${kind}`}>{value}</p>}
    <details className={`session-replay-expand ${kind}`} onToggle={(event) => setExpanded(event.currentTarget.open)}>
      <summary>{expanded ? t('sessionReplay.collapse') : t('sessionReplay.expand', { count: value.length.toLocaleString() })}</summary>
      {body}
    </details>
  </>;
}

function EntryContent({ entry, t }: { entry: ReplayEntry; t: Translate }) {
  const { item } = entry;
  if (item.kind === 'unknown') return <article className="session-replay-entry unknown" data-replay-kind={item.kind}>
    <header><b>{t('sessionReplay.archiveUnknown')}</b></header>
    <p>{unknownLabel(t, item.reason, entry.archiveBodies === 2)}</p>
  </article>;

  if (item.kind === 'message') return <article className={`session-replay-entry message ${item.role}`} data-replay-kind={item.kind}>
    <header><b>{messageLabel(t, item.role)}</b><span>{item.body === 'request' ? t('request.request') : t('request.response')}</span></header>
    <ArchiveText kind="message" t={t} value={item.text ?? unknownLabel(t, item.unknown)} />
    {item.truncated && <small className="session-replay-flag">{t('sessionReplay.truncated')}</small>}
  </article>;

  if (item.kind === 'tool_call') return <article className="session-replay-entry tool-call" data-replay-kind={item.kind}>
    <header><b>{t('sessionReplay.toolCall')}</b><span>{item.body === 'request' ? t('request.request') : t('request.response')}</span></header>
    <p className="session-replay-tool-name">{item.name ?? t('sessionReplay.unknown')}</p>
    {item.pairing !== 'paired' && <small className="session-replay-flag">{t(`sessionReplay.pairing.${item.pairing}`)}</small>}
    <details className="session-replay-technical"><summary>{t('request.technicalDetails')}</summary>
      <dl>
        <div><dt>{t('sessionReplay.callId')}</dt><dd><code>{item.callId ?? t('sessionReplay.unknown')}</code></dd></div>
        <div><dt>{t('sessionReplay.pairing')}</dt><dd>{t(`sessionReplay.pairing.${item.pairing}`)}</dd></div>
      </dl>
      <ArchiveText kind="tool" t={t} value={item.arguments ?? unknownLabel(t, item.unknownFields.includes('arguments') ? 'missing_text' : null)} />
    </details>
    {item.truncated && <small className="session-replay-flag">{t('sessionReplay.truncated')}</small>}
  </article>;

  return <article className="session-replay-entry tool-result" data-replay-kind={item.kind}>
    <header><b>{t('sessionReplay.toolResult')}</b><span>{item.body === 'request' ? t('request.request') : t('request.response')}</span></header>
    {item.name && <p className="session-replay-tool-name">{item.name}</p>}
    <ArchiveText kind="tool" t={t} value={item.output ?? unknownLabel(t, item.unknownFields.includes('output') ? 'missing_text' : null)} />
    {item.pairing !== 'paired' && <small className="session-replay-flag">{t(`sessionReplay.pairing.${item.pairing}`)}</small>}
    <details className="session-replay-technical"><summary>{t('request.technicalDetails')}</summary>
      <dl>
        <div><dt>{t('sessionReplay.callId')}</dt><dd><code>{item.callId ?? t('sessionReplay.unknown')}</code></dd></div>
        <div><dt>{t('sessionReplay.pairing')}</dt><dd>{t(`sessionReplay.pairing.${item.pairing}`)}</dd></div>
      </dl>
    </details>
    {item.truncated && <small className="session-replay-flag">{t('sessionReplay.truncated')}</small>}
  </article>;
}

/**
 * Displays the bounded, typed P1 archive projection. Archive reads are supplied
 * by the owning surface so this component never owns credentials or broadens
 * archive authorization.
 */
export function SessionReplayPanel({ detail, loadArchiveDetail }: {
  detail: LogicalSessionDetail;
  loadArchiveDetail?: SessionReplayArchiveLoader;
}) {
  const { t } = useI18n();
  const [archiveDetails, setArchiveDetails] = useState<RequestDetail[]>([]);
  const [mismatchedIds, setMismatchedIds] = useState<Set<string>>(new Set());
  const [archiveLoading, setArchiveLoading] = useState(Boolean(loadArchiveDetail));
  const sequence = useRef(0);
  const entryRefs = useRef(new Map<number, HTMLElement>());
  const [selectedTurn, setSelectedTurn] = useState<number>();

  const sourceRequests = useMemo(
    () => [...detail.requests].sort(requestOrder).slice(0, SESSION_REPLAY_MAX_REQUESTS),
    [detail.requests],
  );
  const requestKey = useMemo(
    () => sourceRequests.map((request) => `${request.request_id}:${request.created_at}`).join('\0'),
    [sourceRequests],
  );

  useEffect(() => {
    if (!loadArchiveDetail) return undefined;
    const current = ++sequence.current;
    const controller = new AbortController();
    setArchiveDetails([]);
    setMismatchedIds(new Set());
    setSelectedTurn(undefined);
    if (!sourceRequests.length) {
      setArchiveLoading(false);
      return () => controller.abort();
    }
    setArchiveLoading(true);

    void (async () => {
      const loaded: RequestDetail[] = [];
      const mismatched = new Set<string>();
      let cursor = 0;
      const loadNext = async () => {
        while (!controller.signal.aborted) {
          const request = sourceRequests[cursor++];
          if (!request) return;
          try {
            const candidate = await loadArchiveDetail(request, controller.signal);
            if (candidate.request_id !== request.request_id || candidate.session_context?.session_id !== detail.session_id) {
              mismatched.add(request.request_id);
            } else {
              loaded.push(candidate);
            }
          } catch {
            // Archive read failures remain an explicit archive-unavailable entry.
          }
        }
      };
      await Promise.all(Array.from({ length: Math.min(REPLAY_ARCHIVE_CONCURRENCY, sourceRequests.length) }, loadNext));
      if (controller.signal.aborted || current !== sequence.current) return;
      setArchiveDetails(loaded);
      setMismatchedIds(mismatched);
      setArchiveLoading(false);
    })();

    return () => controller.abort();
  }, [detail.session_id, loadArchiveDetail, requestKey, sourceRequests]);

  if (!loadArchiveDetail) return null;

  const projection = projectSessionReplay(detail.session_id, archiveDetails);
  const loadedIds = new Set(archiveDetails.map((request) => request.request_id));
  const missingEntries: SessionReplayItem[] = archiveLoading ? [] : sourceRequests
    .filter((request) => !loadedIds.has(request.request_id))
    .flatMap((request) => {
      const reason: ReplayUnknownReason = mismatchedIds.has(request.request_id)
        ? 'outside_session'
        : 'archive_unavailable';
      return [
        { kind: 'unknown' as const, requestId: request.request_id, body: 'request' as const, reason },
        { kind: 'unknown' as const, requestId: request.request_id, body: 'response' as const, reason },
      ];
    });
  const requestPositions = new Map(sourceRequests.map((request, index) => [request.request_id, index]));
  const orderedEntries = [...projection.items, ...missingEntries]
    .map((item, index) => ({ item, index }))
    .sort((left, right) => (requestPositions.get(left.item.requestId) ?? Number.MAX_SAFE_INTEGER) - (requestPositions.get(right.item.requestId) ?? Number.MAX_SAFE_INTEGER)
      || bodyOrder(left.item) - bodyOrder(right.item)
      || left.index - right.index);
  const entries = orderedEntries.reduce<ReplayEntry[]>((merged, entry) => {
    const previous = merged.at(-1);
    if (
      entry.item.kind === 'unknown'
      && previous?.item.kind === 'unknown'
      && entry.item.requestId === previous.item.requestId
      && entry.item.reason === previous.item.reason
      && entry.item.body !== previous.item.body
    ) {
      previous.archiveBodies = 2;
      return merged;
    }
    merged.push(entry);
    return merged;
  }, []);
  const turns = entries.filter((entry) => entry.item.kind === 'message' && entry.item.role === 'user');
  const incompleteCount = archiveDetails.filter((request) => !request.archive_complete).length;

  function selectTurn(turn: ReplayEntry) {
    setSelectedTurn(turn.index);
    entryRefs.current.get(turn.index)?.scrollIntoView({ block: 'center', behavior: 'smooth' });
  }

  return <section className="session-replay" aria-label={t('sessionReplay.title')}>
    <header className="session-replay-heading">
      <div><span className="eyebrow">{t('sessionReplay.title')}</span><h3>{t('sessionReplay.content')}</h3></div>
      <div className="session-replay-status" role="status" aria-live="polite">
        {archiveLoading && <span>{t('sessionReplay.loading')}</span>}
        {incompleteCount > 0 && <span>{t('sessionReplay.incomplete', { count: incompleteCount })}</span>}
        {(projection.truncated || detail.requests.length > sourceRequests.length) && <span>{t('sessionReplay.truncated')}</span>}
      </div>
    </header>
    <div className="session-replay-layout">
      <aside className="session-replay-turns" aria-label={t('sessionReplay.userTurns')}>
        <div className="session-replay-turn-heading"><span>{t('sessionReplay.userTurns')}</span><b>{turns.length}</b></div>
        {turns.length > 0 ? <ol>{turns.map((turn, index) => {
          const message = turn.item.kind === 'message' ? turn.item : undefined;
          const fullText = message?.text ?? unknownLabel(t, message?.unknown ?? null);
          return <li key={`${turn.item.requestId}:${turn.index}`}><button
            type="button"
            title={fullText}
            aria-label={t('sessionReplay.userTurn', { index: index + 1, text: fullText })}
            aria-pressed={selectedTurn === turn.index}
            onClick={() => selectTurn(turn)}
          ><span>{index + 1}</span><b>{fullText}</b></button></li>;
        })}</ol> : <p>{t('sessionReplay.noUserTurns')}</p>}
      </aside>
      <ol className="session-replay-feed" aria-label={t('sessionReplay.archiveSequence')}>
        {entries.map((entry) => <li
          key={`${entry.item.requestId}:${entry.item.body}:${entry.index}`}
          className={selectedTurn === entry.index ? 'selected' : undefined}
          ref={(node) => {
            if (node) entryRefs.current.set(entry.index, node);
            else entryRefs.current.delete(entry.index);
          }}
        ><EntryContent entry={entry} t={t} /></li>)}
      </ol>
    </div>
  </section>;
}
