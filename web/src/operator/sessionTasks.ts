import { api } from '../api.js';
import { useEffect, useRef, useState } from 'react';
import type {
  LogicalSessionSummaryBatchResponse,
  SessionIdentity, SessionNameUpdate, SessionRelationshipsResponse,
  SessionTaskAnalysis, SessionTaskScope, SessionTitleState,
} from '../types.js';

/** Backend rollup costs are integer micros; the shared currency display takes decimals. */
export function microsToCostString(micros: number): string {
  const negative = micros < 0;
  const absolute = Math.abs(Math.trunc(micros));
  const whole = Math.floor(absolute / 1_000_000);
  const fraction = String(absolute % 1_000_000).padStart(6, '0').replace(/0+$/, '');
  return `${negative ? '-' : ''}${whole}${fraction ? `.${fraction}` : ''}`;
}

function tenantQuery(tenant: string, keyId: string) {
  const params = new URLSearchParams({ key_id: keyId });
  if (tenant) params.set('tenant_external_id', tenant);
  return params;
}

/** Read-only analysis is a POST because identities ride in the body. */
export function analyzeSessionTasks(
  tenant: string, token: string,
  identities: SessionIdentity[], scope: SessionTaskScope, signal: AbortSignal,
  includeSubtrees = false,
) {
  return api<SessionTaskAnalysis>('/internal/v1/sessions/analyze', token.trim(), {
    method: 'POST',
    body: JSON.stringify({
      ...(tenant ? { tenant_external_id: tenant } : {}),
      identities,
      scope,
      timezone: Intl.DateTimeFormat().resolvedOptions().timeZone,
      include_subtrees: includeSubtrees,
    }),
    signal: AbortSignal.any([signal, AbortSignal.timeout(20_000)]),
  });
}

export function sessionRelationships(
  tenant: string, token: string, identity: SessionIdentity, signal: AbortSignal,
) {
  return api<SessionRelationshipsResponse>(
    `/internal/v1/sessions/${encodeURIComponent(identity.session_id)}/relationships?${tenantQuery(tenant, identity.key_id)}`,
    token.trim(),
    { signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]) },
  );
}

export function changeSessionRelationship(
  tenant: string, token: string, identity: SessionIdentity,
  target: SessionIdentity, kind: 'parent' | 'related', evidence: string | undefined,
  remove: boolean, signal: AbortSignal,
) {
  return api<{ ok: boolean }>(
    `/internal/v1/sessions/${encodeURIComponent(identity.session_id)}/relationships?${tenantQuery(tenant, identity.key_id)}`,
    token.trim(),
    {
      method: remove ? 'DELETE' : 'PUT',
      body: JSON.stringify({ target, kind, ...(evidence?.trim() ? { evidence: evidence.trim() } : {}) }),
      signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]),
    },
  );
}

/** Compare-and-set rename. A 409 means the title changed underneath; re-read. */
export function updateSessionName(
  tenant: string, token: string, identity: SessionIdentity,
  update: SessionNameUpdate, signal: AbortSignal,
) {
  return api<SessionTitleState>(
    `/internal/v1/sessions/${encodeURIComponent(identity.session_id)}/name?${tenantQuery(tenant, identity.key_id)}`,
    token.trim(),
    {
      method: 'PUT',
      body: JSON.stringify(update),
      signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]),
    },
  );
}

export function enqueueSessionTitles(
  tenant: string, token: string, identities: SessionIdentity[], signal: AbortSignal,
) {
  return api<{ queued: number; results: Array<SessionIdentity & { status: 'queued' | 'already_queued' | 'named' }> }>('/internal/v1/sessions/titles', token.trim(), {
    method: 'POST',
    body: JSON.stringify({
      ...(tenant ? { tenant_external_id: tenant } : {}),
      identities,
    }),
    signal: AbortSignal.any([signal, AbortSignal.timeout(20_000)]),
  });
}

const sessionIdentityUuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/**
 * Resolve the persisted parent session label for one confirmed request
 * context. Bounded to drawer use: one relationships read plus at most one
 * summary read per identity, never per table row.
 */
export function useSessionParentLabel(
  tenant: string, token: string, keyId: string | undefined, sessionId: string | null | undefined,
  formatLabel: (session: { session_name: string | null; last_activity_at: number; key_alias: string; key_id: string }) => string,
): { label: string; identity: SessionIdentity } | undefined {
  const [parent, setParent] = useState<{ scope: string; label: string; identity?: SessionIdentity }>();
  const scope = `${tenant}\0${token}\0${keyId ?? ''}\0${sessionId ?? ''}`;
  const labelRef = useRef(formatLabel);
  labelRef.current = formatLabel;
  useEffect(() => {
    if (!keyId || !sessionId || !sessionIdentityUuid.test(sessionId) || !token.trim()) return;
    const controller = new AbortController();
    void (async () => {
      try {
        const response = await sessionRelationships(tenant, token, { key_id: keyId, session_id: sessionId }, controller.signal);
        if (controller.signal.aborted) return;
        const row = response.relationships.find((relationship) => relationship.kind === 'parent'
          && relationship.key_id === keyId && relationship.session_id === sessionId);
        if (!row) { setParent({ scope, label: '' }); return; }
        const summaries = await api<LogicalSessionSummaryBatchResponse>('/internal/v1/sessions/summaries', token.trim(), {
          method: 'POST',
          body: JSON.stringify({ ...(tenant ? { tenant_external_id: tenant } : {}), identities: [row.target], state: 'all' }),
          signal: controller.signal,
        });
        if (controller.signal.aborted) return;
        const summary = summaries.sessions[0];
        setParent({ scope, label: `${summary ? labelRef.current(summary) : row.target.session_id} · ${row.target.key_id.slice(-8)}/${row.target.session_id.slice(-8)}`, identity: row.target });
      } catch {
        if (!controller.signal.aborted) setParent({ scope, label: '' });
      }
    })();
    return () => controller.abort();
  }, [scope]);
  return parent?.scope === scope && parent.identity ? { label: parent.label, identity: parent.identity } : undefined;
}
