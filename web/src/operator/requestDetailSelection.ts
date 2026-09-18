import type { RequestDetail, RequestView } from '../types.js';

export type RequestDetailPhase = 'idle' | 'loading' | 'ready' | 'failed';

export interface RequestDetailSelection {
  scope: string;
  request?: RequestView;
  detail?: RequestDetail;
  phase: RequestDetailPhase;
  error: string;
}

export function emptyRequestDetailSelection(): RequestDetailSelection {
  return { scope: '', phase: 'idle', error: '' };
}

export function beginRequestDetailSelection(request: RequestView, scope: string): RequestDetailSelection {
  return { scope, request, phase: 'loading', error: '' };
}

function matchesPendingRequest(selection: RequestDetailSelection, scope: string, requestId: string) {
  return selection.phase === 'loading' && selection.scope === scope && selection.request?.request_id === requestId;
}

export function resolveRequestDetailSelection(selection: RequestDetailSelection, scope: string, requestId: string, detail: RequestDetail): RequestDetailSelection {
  return matchesPendingRequest(selection, scope, requestId)
    ? { ...selection, detail, phase: 'ready' }
    : selection;
}

export function rejectRequestDetailSelection(selection: RequestDetailSelection, scope: string, requestId: string, error: string): RequestDetailSelection {
  return matchesPendingRequest(selection, scope, requestId)
    ? { ...selection, phase: 'failed', error }
    : selection;
}

export function requestDetailSelectionInScope(selection: RequestDetailSelection, scope: string) {
  return selection.phase !== 'idle' && selection.scope === scope ? selection : emptyRequestDetailSelection();
}
