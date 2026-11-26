import { useEffect, useReducer, useRef } from 'react';
import { ApiError, api, apiDiagnosticMessage } from '../../api';
import {
  clearRememberedCredential,
  clearOperatorLoginCycle,
  ensureOperatorLoginCycle,
  readOperatorLoginCycle,
  operatorLoginCycleStorageKey,
  operatorCredentialStorageKey,
  readRememberedCredential,
  readRememberedOperatorTenant,
  rememberCredential,
  rememberOperatorTenant,
  tenantForCredential,
} from '../../credentialStorage';
import { useI18n } from '../../i18n';
import type { TenantView } from '../../types';
import { isOrdinaryClientCredential } from '../clientCredentialGuidance';
import { messageOf } from '../scope/operatorShared';

type ScopeStatus =
  | { kind: 'disconnected' }
  | { kind: 'authenticating'; candidate: string }
  | { kind: 'ready' }
  | { kind: 'failed'; message: string }
  // A transport, gateway or database outage is not an authorization answer.
  // The remembered credential, the validated session and the user's draft all
  // stay mounted; `candidate` lets retry re-attempt the exact submission.
  | { kind: 'unavailable'; message: string; candidate: string };

interface ScopeState {
  credential: string;
  validated: boolean;
  remembered: boolean;
  loginCycle?: string;
  credentialInput: string;
  tenant: string;
  tenants: TenantView[];
  status: ScopeStatus;
}

type ScopeAction =
  | { type: 'credential-input'; value: string }
  | { type: 'authenticate'; candidate: string }
  | { type: 'authenticated'; credential: string; tenants: TenantView[]; tenant: string; loginCycle?: string; remembered: boolean }
  | { type: 'tenants-refreshed'; tenants: TenantView[]; tenant: string }
  // reject: the active identity itself is revoked, clear it. denied: the
  // validated identity got a definitive permission answer on the baseline
  // tenant-scope read; deactivate it while keeping the saved credential and
  // the server's own message visible. session-kept: a replacement candidate
  // failed, which proves nothing about the active identity, so the validated
  // session continues.
  | { type: 'authentication-failed'; message: string; outcome: 'reject' | 'denied' | 'session-kept' }
  | { type: 'authentication-unavailable'; message: string; candidate: string }
  | { type: 'select-tenant'; tenant: string }
  | { type: 'clear' };

function initialState(): ScopeState {
  const credential = readRememberedCredential('operator');
  return {
    credential,
    validated: false,
    remembered: Boolean(credential),
    loginCycle: readOperatorLoginCycle(),
    credentialInput: '',
    tenant: readRememberedOperatorTenant(),
    tenants: [],
    status: credential ? { kind: 'authenticating', candidate: credential } : { kind: 'disconnected' },
  };
}

function reducer(state: ScopeState, action: ScopeAction): ScopeState {
  switch (action.type) {
    case 'credential-input':
      return { ...state, credentialInput: action.value };
    case 'authenticate':
      return { ...state, status: { kind: 'authenticating', candidate: action.candidate } };
    case 'authenticated':
      return {
        credential: action.credential,
        validated: true,
        remembered: action.remembered,
        loginCycle: action.loginCycle,
        credentialInput: state.credentialInput.trim() === action.credential ? '' : state.credentialInput,
        tenants: action.tenants,
        tenant: action.tenant,
        status: { kind: 'ready' },
      };
    case 'tenants-refreshed':
      // A successful read is the only proof of recovery: it clears a
      // preserved fault, its retry and a stale candidate error. Tenant
      // selection alone never does.
      return { ...state, tenants: action.tenants, tenant: action.tenant, status: { kind: 'ready' } };
    case 'authentication-failed':
      if (action.outcome === 'denied') return { ...state, validated: false, loginCycle: undefined, status: { kind: 'failed', message: action.message } };
      if (action.outcome === 'session-kept') return { ...state, status: { kind: 'failed', message: action.message } };
      return { credential: '', validated: false, remembered: false, credentialInput: state.credentialInput, tenant: '', tenants: [], status: { kind: 'failed', message: action.message } };
    case 'authentication-unavailable':
      return { ...state, status: { kind: 'unavailable', message: action.message, candidate: action.candidate } };
    case 'select-tenant':
      // Switching tenants is not proof of recovery: an active fault, a
      // denial or an in-flight revalidation stays untouched until a read
      // succeeds or the identity is cleared.
      return { ...state, tenant: action.tenant };
    case 'clear':
      return { credential: '', validated: false, remembered: false, credentialInput: '', tenant: '', tenants: [], status: { kind: 'disconnected' } };
  }
}

export function useOperatorScope() {
  const { t } = useI18n();
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const stateRef = useRef(state);
  const sequence = useRef(0);
  stateRef.current = state;

  // Safe surface evidence only: an HTTP status, a server-supplied message and
  // a correlation ID. Never a URL, a credential or a fabricated cause.
  function outageMessage(reason: unknown) {
    const diagnostic = apiDiagnosticMessage(reason, t('common.connectionFailed'), {
      requestId: t('request.correlationId'),
      streamInterrupted: t('request.streamInterrupted'),
    });
    return `${t('operator.serviceUnavailable')} · ${diagnostic}`;
  }

  function ownsRememberedIdentity(identity: ScopeState): boolean {
    return readRememberedCredential('operator') === (identity.remembered ? identity.credential : '')
      && readOperatorLoginCycle() === identity.loginCycle;
  }

  function clearLocalIdentity() {
    sequence.current += 1;
    dispatch({ type: 'clear' });
  }

  async function authenticate(rawCandidate: string) {
    const candidate = rawCandidate.trim();
    if (!candidate) return;
    const before = stateRef.current;
    if (before.credential && !ownsRememberedIdentity(before)) { clearLocalIdentity(); return; }
    const request = ++sequence.current;
    const rememberedBefore = readRememberedCredential('operator');
    const cycleBefore = readOperatorLoginCycle();
    dispatch({ type: 'authenticate', candidate });
    try {
      // Tenant discovery is the only authentication-phase request. Page data
      // stays unmounted until its route is selected, so a singleton credential
      // never performs an accidental all-tenant query.
      const tenants = await api<TenantView[]>('/internal/v1/tenants', candidate);
      if (request !== sequence.current) return;
      if (readRememberedCredential('operator') !== rememberedBefore || readOperatorLoginCycle() !== cycleBefore) { clearLocalIdentity(); return; }
      // Honor the tenant the user selected while this read was in flight;
      // the snapshot from the request start must not roll their choice back.
      const tenant = tenantForCredential(tenants, stateRef.current.tenant || before.tenant);
      rememberCredential('operator', candidate);
      rememberOperatorTenant(tenant);
      const loginCycle = readRememberedCredential('operator') === candidate ? ensureOperatorLoginCycle() : undefined;
      dispatch({ type: 'authenticated', credential: candidate, tenants, tenant, loginCycle, remembered: readRememberedCredential('operator') === candidate });
    } catch (reason) {
      if (request !== sequence.current) return;
      if (readRememberedCredential('operator') !== rememberedBefore || readOperatorLoginCycle() !== cycleBefore) { clearLocalIdentity(); return; }
      // A failing replacement candidate proves nothing about the active
      // identity: the validated session, its saved credential and its pages
      // stay; only the candidate's own failure is reported.
      const replacement = before.validated && Boolean(before.credential) && candidate !== before.credential;
      if (reason instanceof ApiError && reason.status === 401) {
        const message = isOrdinaryClientCredential(candidate)
          ? t('operator.clientKeyRejected')
          : messageOf(reason, t('common.connectionFailed'));
        // Only a definitive 401 against the active identity (including an
        // explicit revocation) ends the authorization and clears the saved
        // credential.
        if (replacement) {
          dispatch({ type: 'authentication-failed', message, outcome: 'session-kept' });
          return;
        }
        clearRememberedCredential('operator');
        dispatch({ type: 'authentication-failed', message, outcome: 'reject' });
        return;
      }
      // A 403 is a definitive permission answer, never a retryable outage.
      // Tenant discovery is the console's baseline authorization read, so a
      // 403 here means the identity holds no management authorization at all
      // — narrower per-API 403s surface inside their own pages and never
      // reach this hook. Against the validated identity it deactivates the
      // session: the saved credential and the server's message stay visible
      // but no longer authorize pages or streams. An unproven candidate is
      // dropped and never promoted.
      if (reason instanceof ApiError && reason.status === 403) {
        const message = messageOf(reason, t('common.connectionFailed'));
        if (replacement) {
          dispatch({ type: 'authentication-failed', message, outcome: 'session-kept' });
          return;
        }
        if (before.validated) {
          clearOperatorLoginCycle();
          dispatch({ type: 'authentication-failed', message, outcome: 'denied' });
          return;
        }
        clearRememberedCredential('operator');
        dispatch({ type: 'authentication-failed', message, outcome: 'reject' });
        return;
      }
      // A network failure, 502/503 or an unavailable database keeps the saved
      // credential, the ready read-only page and the unsubmitted draft, and is
      // surfaced as an explicit outage with retry instead of a sign-in.
      dispatch({ type: 'authentication-unavailable', message: outageMessage(reason), candidate });
    }
  }

  useEffect(() => {
    if (state.credential && state.status.kind === 'authenticating') {
      void authenticate(state.credential);
    }
    // The remembered credential is intentionally attempted once on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    function revoked(event: StorageEvent) {
      if (event.storageArea !== window.localStorage) return;
      if (event.key !== null && event.key !== operatorLoginCycleStorageKey && event.key !== operatorCredentialStorageKey) return;
      const current = stateRef.current;
      if (current.validated && ownsRememberedIdentity(current)) return;
      sequence.current += 1;
      dispatch({ type: 'clear' });
    }
    window.addEventListener('storage', revoked);
    return () => { sequence.current += 1; window.removeEventListener('storage', revoked); };
  }, []);

  function clearCredential() {
    sequence.current += 1;
    const current = stateRef.current;
    if (current.credential && ownsRememberedIdentity(current)) clearRememberedCredential('operator');
    dispatch({ type: 'clear' });
  }

  async function refreshTenants() {
    const before = stateRef.current;
    const credential = before.credential;
    if (!credential || !before.validated) return;
    if (!ownsRememberedIdentity(before)) { clearLocalIdentity(); return; }
    const request = ++sequence.current;

    try {
      const tenants = await api<TenantView[]>('/internal/v1/tenants', credential);
      if (request !== sequence.current || stateRef.current.credential !== credential) return;
      if (!ownsRememberedIdentity(before)) { clearLocalIdentity(); return; }
      const tenant = tenantForCredential(tenants, stateRef.current.tenant);
      rememberOperatorTenant(tenant);
      dispatch({ type: 'tenants-refreshed', tenants, tenant });
    } catch (reason) {
      if (request !== sequence.current || stateRef.current.credential !== credential) return;
      if (!ownsRememberedIdentity(before)) { clearLocalIdentity(); return; }
      if (reason instanceof ApiError && reason.status === 401) {
        clearRememberedCredential('operator');
        dispatch({ type: 'authentication-failed', message: messageOf(reason, t('common.connectionFailed')), outcome: 'reject' });
        return;
      }
      if (reason instanceof ApiError && reason.status === 403) {
        clearOperatorLoginCycle();
        dispatch({ type: 'authentication-failed', message: messageOf(reason, t('common.connectionFailed')), outcome: 'denied' });
        return;
      }
      dispatch({ type: 'authentication-unavailable', message: outageMessage(reason), candidate: credential });
    }
  }

  // Pages and streams stay gated on a validated identity. A validated session
  // also keeps its page and drafts mounted while its own credential is being
  // revalidated (retry), while a replacement candidate, a denied identity and
  // a first connect never borrow the active credential.
  const activeCredential = state.validated && (
    state.status.kind === 'ready'
    || state.status.kind === 'failed'
    || state.status.kind === 'unavailable'
    || (state.status.kind === 'authenticating' && state.status.candidate === state.credential)
  ) ? state.credential : '';
  const writeTenant = state.tenant;

  function retry() {
    const current = stateRef.current;
    if (current.status.kind !== 'unavailable') return;
    void authenticate(current.status.candidate);
  }

  return {
    ...state,
    activeCredential,
    loginCycle: state.validated && state.status.kind === 'ready' && state.tenants.some((tenant) => tenant.external_id === state.tenant)
      ? state.loginCycle : undefined,
    writeTenant,
    authenticating: state.status.kind === 'authenticating',
    error: state.status.kind === 'failed' ? state.status.message : '',
    serviceFault: state.status.kind === 'unavailable' ? state.status.message : '',
    setCredentialInput: (value: string) => dispatch({ type: 'credential-input', value }),
    setTenant: (tenant: string) => {
      rememberOperatorTenant(tenant);
      dispatch({ type: 'select-tenant', tenant });
    },
    authenticate,
    retry,
    clearCredential,
    refreshTenants,
  };
}
