import { useEffect, useReducer, useRef } from 'react';
import { api } from '../../api';
import { clearRememberedCredential, readRememberedCredential, rememberCredential } from '../../credentialStorage';
import { useI18n } from '../../i18n';
import type { TenantView } from '../../types';
import { messageOf } from '../scope/operatorShared';

type ScopeStatus =
  | { kind: 'disconnected' }
  | { kind: 'authenticating'; candidate: string }
  | { kind: 'ready' }
  | { kind: 'failed'; message: string };

interface ScopeState {
  credential: string;
  validated: boolean;
  credentialInput: string;
  tenant: string;
  tenants: TenantView[];
  status: ScopeStatus;
}

type ScopeAction =
  | { type: 'credential-input'; value: string }
  | { type: 'authenticate'; candidate: string }
  | { type: 'authenticated'; credential: string; tenants: TenantView[]; tenant: string }
  | { type: 'authentication-failed'; message: string; preserve: boolean }
  | { type: 'select-tenant'; tenant: string }
  | { type: 'clear' };

function initialState(): ScopeState {
  const credential = readRememberedCredential('operator');
  return {
    credential,
    validated: false,
    credentialInput: '',
    tenant: '',
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
        credentialInput: state.credentialInput.trim() === action.credential ? '' : state.credentialInput,
        tenants: action.tenants,
        tenant: action.tenant,
        status: { kind: 'ready' },
      };
    case 'authentication-failed':
      return action.preserve
        ? { ...state, status: { kind: 'failed', message: action.message } }
        : { credential: '', validated: false, credentialInput: state.credentialInput, tenant: '', tenants: [], status: { kind: 'failed', message: action.message } };
    case 'select-tenant':
      return { ...state, tenant: action.tenant, status: { kind: 'ready' } };
    case 'clear':
      return { credential: '', validated: false, credentialInput: '', tenant: '', tenants: [], status: { kind: 'disconnected' } };
  }
}

function tenantForCredential(tenants: TenantView[], previousTenant: string, replacing: boolean) {
  if (tenants.length === 1) return tenants[0].external_id;
  if (replacing) return '';
  return tenants.some((value) => value.external_id === previousTenant) ? previousTenant : '';
}

export function useOperatorScope() {
  const { t } = useI18n();
  const [state, dispatch] = useReducer(reducer, undefined, initialState);
  const stateRef = useRef(state);
  const sequence = useRef(0);
  stateRef.current = state;

  async function authenticate(rawCandidate: string, replacing = true) {
    const candidate = rawCandidate.trim();
    if (!candidate) return;
    const request = ++sequence.current;
    const before = stateRef.current;
    const preserve = before.validated && Boolean(before.credential);
    dispatch({ type: 'authenticate', candidate });
    try {
      // Tenant discovery is the only authentication-phase request. Page data
      // stays unmounted until its route is selected, so a singleton credential
      // never performs an accidental all-tenant query.
      const tenants = await api<TenantView[]>('/internal/v1/tenants', candidate);
      if (request !== sequence.current) return;
      const tenant = tenantForCredential(tenants, before.tenant, replacing);
      rememberCredential('operator', candidate);
      dispatch({ type: 'authenticated', credential: candidate, tenants, tenant });
    } catch (reason) {
      if (request !== sequence.current) return;
      if (!preserve) clearRememberedCredential('operator');
      dispatch({
        type: 'authentication-failed',
        message: messageOf(reason, t('common.connectionFailed')),
        preserve,
      });
    }
  }

  useEffect(() => {
    if (state.credential && state.status.kind === 'authenticating') {
      void authenticate(state.credential, false);
    }
    // The remembered credential is intentionally attempted once on mount.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  function clearCredential() {
    sequence.current += 1;
    clearRememberedCredential('operator');
    dispatch({ type: 'clear' });
  }

  const activeCredential = state.status.kind === 'ready' || state.status.kind === 'failed'
    ? state.credential
    : '';

  return {
    ...state,
    activeCredential,
    authenticating: state.status.kind === 'authenticating',
    error: state.status.kind === 'failed' ? state.status.message : '',
    setCredentialInput: (value: string) => dispatch({ type: 'credential-input', value }),
    setTenant: (tenant: string) => dispatch({ type: 'select-tenant', tenant }),
    authenticate,
    clearCredential,
  };
}
