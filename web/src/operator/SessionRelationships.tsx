import { useEffect, useMemo, useState } from 'react';
import { api, ApiError } from '../api.js';
import { Button, Disclosure, Input, LoadingState } from '../design-system';
import { useI18n } from '../i18n.js';
import { credentialDisplayName } from '../identityPresentation.js';
import { unnamedSessionName } from '../sessionTitles.js';
import { changeSessionRelationship, sessionRelationships } from './sessionTasks.js';
import type { LogicalSessionSummaryBatchResponse, SessionIdentity, SessionRelationship } from '../types.js';

const UUID_SHAPE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

function sameIdentity(left: SessionIdentity, right: SessionIdentity) {
  return left.key_id === right.key_id && left.session_id === right.session_id;
}

/** The other endpoint of a relationship row, from this session's perspective. */
function peerOf(row: SessionRelationship, self: SessionIdentity): SessionIdentity {
  return row.key_id === self.key_id && row.session_id === self.session_id
    ? row.target
    : { key_id: row.key_id, session_id: row.session_id };
}

export function SessionRelationships({ token, tenant, identity, onOpenSession }: {
  token: string;
  tenant: string;
  identity: SessionIdentity;
  onOpenSession?: (target: SessionIdentity) => void;
}) {
  const { locale, t } = useI18n();
  const scopeKey = [tenant, token, identity.key_id, identity.session_id].join('');
  const [relationships, setRelationships] = useState<SessionRelationship[]>([]);
  const [names, setNames] = useState<Map<string, string>>(new Map());
  const [state, setState] = useState<{ scope: string; phase: 'loading' | 'ready' | 'failed'; error: string }>({ scope: '', phase: 'loading', error: '' });
  const [revision, setRevision] = useState(0);
  const [adding, setAdding] = useState(false);
  const [targetSession, setTargetSession] = useState('');
  const [targetKey, setTargetKey] = useState('');
  const [pending, setPending] = useState(false);
  const [mutationError, setMutationError] = useState('');

  useEffect(() => {
    const controller = new AbortController();
    setState({ scope: scopeKey, phase: 'loading', error: '' });
    setRelationships([]);
    setNames(new Map());
    void (async () => {
      try {
        const response = await sessionRelationships(tenant, token, identity, controller.signal);
        if (controller.signal.aborted) return;
        setRelationships(response.relationships);
        const peers = [...new Map(response.relationships.map((row) => {
          const peer = peerOf(row, identity);
          return [peer.key_id + '' + peer.session_id, peer];
        })).values()];
        if (peers.length > 0) {
          const summaries = await api<LogicalSessionSummaryBatchResponse>('/internal/v1/sessions/summaries', token.trim(), {
            method: 'POST',
            body: JSON.stringify({
              ...(tenant ? { tenant_external_id: tenant } : {}),
              identities: peers,
              state: 'all',
            }),
            signal: controller.signal,
          });
          if (controller.signal.aborted) return;
          setNames(new Map(summaries.sessions.map((session) => [
            session.key_id + '' + session.session_id,
            session.session_name?.trim()
              || unnamedSessionName(t, locale, session.last_activity_at, credentialDisplayName(session.key_alias, t), true),
          ])));
        }
        setState({ scope: scopeKey, phase: 'ready', error: '' });
      } catch (reason) {
        if (controller.signal.aborted) return;
        setState({ scope: scopeKey, phase: 'failed', error: reason instanceof Error ? reason.message : t('sessions.rel.failed') });
      }
    })();
    return () => controller.abort();
  }, [scopeKey, revision]);

  const visible = state.scope === scopeKey;
  const parent = useMemo(() => relationships.find((row) => row.kind === 'parent'
    && row.key_id === identity.key_id && row.session_id === identity.session_id), [relationships, identity]);
  const children = useMemo(() => relationships.filter((row) => row.kind === 'parent'
    && row.target.key_id === identity.key_id && row.target.session_id === identity.session_id), [relationships, identity]);
  const related = useMemo(() => relationships.filter((row) => row.kind === 'related'), [relationships, identity]);
  const labelFor = (peer: SessionIdentity) => names.get(peer.key_id + '' + peer.session_id) ?? peer.session_id;

  const mutate = async (target: SessionIdentity, kind: 'parent' | 'related', remove: boolean) => {
    if (pending) return;
    setPending(true);
    setMutationError('');
    const controller = new AbortController();
    try {
      await changeSessionRelationship(tenant, token, identity, target, kind, undefined, remove, controller.signal);
      setTargetSession('');
      setTargetKey('');
      setAdding(false);
      setRevision((value) => value + 1);
    } catch (reason) {
      if (!controller.signal.aborted) {
        setMutationError(reason instanceof ApiError && reason.status === 403
          ? t('sessions.title.permissionDenied') : t('sessions.rel.failed'));
      }
    } finally {
      setPending(false);
    }
  };

  const targetValid = UUID_SHAPE.test(targetSession.trim()) && UUID_SHAPE.test(targetKey.trim());
  return <Disclosure title={t('sessions.rel.manage')}>
    {visible && state.phase === 'loading' && <LoadingState label={t('common.loading')} variant="detail" />}
    {visible && state.phase === 'failed' && <div className="notice error" role="alert"><span>{state.error}</span><Button appearance="secondary" onClick={() => setRevision((value) => value + 1)}>{t('sessions.retryLoad')}</Button></div>}
    {visible && state.phase === 'ready' && <>
      {parent && <p><b>{t('sessions.rel.parent')}</b> {onOpenSession
        ? <Button appearance="transparent" className="table-link" onClick={() => onOpenSession(parent.target)}>{labelFor(parent.target)}</Button>
        : <span>{labelFor(parent.target)}</span>}</p>}
      {children.length > 0 && <p><b>{t('sessions.rel.child')}</b> {children.map((row) => {
        const child = { key_id: row.key_id, session_id: row.session_id };
        return onOpenSession
          ? <Button key={child.key_id + ':' + child.session_id} appearance="transparent" className="table-link" onClick={() => onOpenSession(child)}>{labelFor(child)}</Button>
          : <span key={child.key_id + ':' + child.session_id}>{labelFor(child)}</span>;
      })}</p>}
      {related.length === 0 && !adding && <p className="muted">{t('sessions.rel.none')}</p>}
      {related.map((row) => {
        const peer = peerOf(row, identity);
        return <p key={peer.key_id + ':' + peer.session_id}>
          {onOpenSession
            ? <Button appearance="transparent" className="table-link" onClick={() => onOpenSession(peer)}>{labelFor(peer)}</Button>
            : <span>{labelFor(peer)}</span>}
          <Button appearance="secondary" disabled={pending} onClick={() => { void mutate(peer, 'related', true); }}>{t('sessions.rel.remove')}</Button>
        </p>;
      })}
      {adding ? <>
        <p className="muted">{t('sessions.rel.targetHint')}</p>
        <label>{t('sessions.copy.session')}<Input value={targetSession} disabled={pending} onChange={(_, data) => setTargetSession(data.value)} /></label>
        <label>{t('sessions.copy.credential')}<Input value={targetKey} disabled={pending} onChange={(_, data) => setTargetKey(data.value)} /></label>
        <Button appearance="primary" disabled={pending || !targetValid || sameIdentity(identity, { key_id: targetKey.trim(), session_id: targetSession.trim() })}
          onClick={() => { void mutate({ key_id: targetKey.trim(), session_id: targetSession.trim() }, 'related', false); }}>{t('sessions.rel.add')}</Button>
        <Button appearance="secondary" disabled={pending} onClick={() => setAdding(false)}>{t('common.cancel')}</Button>
      </> : <Button appearance="secondary" disabled={pending} onClick={() => { setMutationError(''); setAdding(true); }}>{t('sessions.rel.add')}</Button>}
      {mutationError && <p role="alert">{mutationError}</p>}
    </>}
  </Disclosure>;
}
