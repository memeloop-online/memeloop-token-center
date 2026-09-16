import type { Locale } from './i18n.js';
import type { ConversationRequest, ExecutionMetadata, LogicalSessionSummary, RequestCredentialIdentity, RequestSessionContext } from './types.js';

type Observation = Pick<ConversationRequest, 'created_at' | 'request_id'> & {
  session_context?: Pick<RequestSessionContext, 'session_name'> | null;
  execution?: Pick<ExecutionMetadata, 'session_name'>;
  credential_identity?: Pick<RequestCredentialIdentity, 'key_alias' | 'key_id'> | null;
};
type TitleInput = { requests: readonly Observation[] };
type Translate = (key: string, variables?: Record<string, string | number>) => string;

function later(left: Observation, right: Observation) {
  return left.created_at > right.created_at || (left.created_at === right.created_at && left.request_id.localeCompare(right.request_id) > 0);
}

/** Use retained declarations, not input order, model names or an archive read. */
export function latestDeclaredSessionName(detail: TitleInput) {
  let latest: { request: Observation; name: string } | undefined;
  for (const request of detail.requests) {
    const name = request.session_context?.session_name?.trim() || request.execution?.session_name?.trim();
    if (name && (!latest || later(request, latest.request))) latest = { request, name };
  }
  return latest?.name;
}

/** The list and detail share the same summary context when one is available. */
export function sessionFallback(detail: TitleInput, summary?: Pick<LogicalSessionSummary, 'last_activity_at' | 'key_alias' | 'key_id'>) {
  if (summary) return { time: summary.last_activity_at, credential: summary.key_alias.trim() || summary.key_id };
  let latest: Observation | undefined;
  let identified: Observation | undefined;
  for (const request of detail.requests) {
    if (!latest || later(request, latest)) latest = request;
    if (request.credential_identity && (!identified || later(request, identified))) identified = request;
  }
  const identity = identified?.credential_identity;
  return { time: latest?.created_at, credential: identity?.key_alias.trim() || identity?.key_id };
}

/** Activity time is context, not a claimed session creation time or real name. */
export function unnamedSessionName(t: Translate, locale: Locale, time: number | undefined, credential?: string, compact = false) {
  if (time === undefined || !Number.isFinite(time)) return credential ? t('sessions.contextSession', { credential }) : t('sessions.logicalSession');
  const when = compact ? new Date(time).toLocaleTimeString(locale, { hour: '2-digit', minute: '2-digit', second: '2-digit', hour12: false }) : new Date(time).toLocaleString(locale);
  return credential
    ? t(compact ? 'sessions.compactContextSession' : 'sessions.unnamedSession', { time: when, credential })
    : t('sessions.unnamedSessionNoCredential', { time: when });
}
