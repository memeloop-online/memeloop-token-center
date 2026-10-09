import { useConfirmDialog } from '../../useConfirmDialog';
import RjsfForm, { type FormProps } from '@rjsf/core/lib/components/Form.js';
import type { RJSFSchema } from '@rjsf/utils';
import { useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { ApiError, api, apiRead } from '../../api';
import { CopyButton } from '../../CopyButton';
import { formatCurrency, formatNumber, formatPercent } from '../../format';
import { localizeSchema, useI18n } from '../../i18n';
import { tenantDisplayName } from '../../tenantDisplayName';
import { LimitSnapshot } from '../../LimitSnapshot';
import { ModelPicker } from '../../ModelPicker';
import { schemaFormFields, schemaFormTemplates } from '../../SchemaTemplates';
import { SecureSchemaField } from '../../SecureSchemaField';
import { prepareSecretForm } from '../../secretSchema';
import { safeValidator as validator } from '../../safeValidator';
import type {
  ConfigurationSchemas, CredentialRoutingView, GenerationPriceView, GroupView, KeyLimitSnapshot, KeyListCursor, KeyView,
  ModelPriceSyncResult, ModelPriceUsageSummary, ModelPriceView, ModelRouteView, ProviderType,
  OperatorMonitoringSnapshot, ServiceTokenView, UpstreamAccount, UpstreamDeletionReadiness, UpstreamHealth,
} from '../../types';
import { GroupManager, useGroups } from '../GroupManager';
import { TransportProxyGroups } from '../TransportProxyGroupManager';
import { MultiCombobox, type ComboboxOption } from '../MultiCombobox';
import { ResourceListStatusEmpty, ResourceListStatusFilterControl, useResourceListStatusFilter } from '../ResourceListStatusFilter';
import { UpstreamModelCombobox } from '../UpstreamModelCombobox';
import { ProviderModelCatalog } from '../ProviderModelCatalog';
import { ResourceBoundary } from '../ResourceBoundary';
import { ManagedModelSync } from '../ManagedModelSync';
import { inferManagedRouteProtocol, managedRouteProtocols, type CatalogRouteAction } from '../managedModelSync';
import { consumeRouteDraftPrefill, consumeRouteFocus, storeCatalogRouteAction } from '../routePrefill';
import { providerDisplayName } from '../providerDisplayName';
import { ProviderAccountStatus, providerAccountStatus } from '../providerAccountStatus';
import { useQuotaClock } from '../useQuotaClock';
import '../routeFormScope.css';
import {
  applyKeyPage, canLoadMoreKeys, canReadCredentialLimits, canWriteCredential,
  credentialListPresentation, keyListPath,
  ownsKeyListRequest, shouldLoadCredentialRoutes,
  type KeyListLoadState, type KeyListRequestIdentity,
} from '../keyPagination';
import { directCredentialSchema, isKimiDeviceProvider, oauthCreationProxyMode, supportsDirectConnection } from '../providerConnectionMethods';
import { UpstreamAvailability, manualHealthLabel } from '../UpstreamAvailability';
import { UpstreamQuota } from '../UpstreamQuotaPanel';
import { QuotaSummary } from '../QuotaSummary';
import { useUpstreamQuotaReads } from '../useUpstreamQuotaReads';
import { connectionSchema, isPrivateProxyUrl, ProxyInput, UpstreamConnection } from '../UpstreamConnection';
import { upstreamFormTemplates } from '../UpstreamFormTemplates';
import { providerConfigSchema, providerEditSchema } from '../providerEditSchema';
import { AuthorizationCodeConnection } from '../AuthorizationCodeConnection';
import { OAuthLoginLinkActions } from '../OAuthLoginLinkActions';
import { authorizationCompleteError, canReauthorizeAccount, claudeCompletionLimits, claudeCompletionRetryMillis, claudeCompletionStopReason, parseClaudeCompletion } from '../authorizationCode';
import { providerConnectionCopy } from '../providerConnectionCopy';
import { ProviderAccountIdentity } from '../ProviderAccountIdentity';
import { mergeProviderRenameReceipt, providerSettingsConfigChanged, providerSettingsUpdate, providerSettingsValidator } from '../providerAccountSettings';
import { providerFormWidgets } from '../ProviderFormWidgets';
import { appHref } from '../../app/routes';
import { useNavigationGuard } from '../../app/NavigationGuard';
import { credentialFormTemplates } from '../CredentialFormTemplates';
import { CredentialRouteAuthorization } from '../CredentialRouteAuthorization';
import { RouteListSource } from '../RouteListSource';
import { credentialRouteOptions } from '../credentialRouteOptions';
import { Button, Checkbox, Combobox, Field, Input, Option, Select, DetailTooltip, Disclosure, FormSection, LoadingProgress, LoadingState } from '../../design-system';
import { JourneyDisclosure as AdvancedFormSection } from '../JourneyDisclosure';
import { formJourneyCopy } from '../formJourneyCopy';
import { CreateJourney } from '../CreateJourney';
import { UpstreamCredentialRotation } from '../UpstreamCredentialRotation';
import { upstreamRotationCopy } from '../upstreamRotationCopy';
import { authorizationJourneyCopy } from '../authorizationJourneyCopy';
import { DeviceAuthorizationCode } from '../DeviceAuthorizationCode';
import { clearDeviceLoginRecovery, readDeviceLoginRecovery, saveDeviceLoginRecovery } from '../deviceLoginRecovery';
import { CredentialActionMenu } from '../CredentialActionMenu';
import { credentialBudgetPresentation } from '../credentialListPresentation';
import { fluentFormWidgets } from '../FluentFormWidgets';
import '../formJourney.css';
import '../authorizationJourney.css';
import '../providerEditLayout.css';
import '../providerDirectory.css';
import { credentialCreateSchema, credentialCreateUiSchema, credentialFormFields, credentialPolicySchema, credentialPolicyUiSchema } from '../CredentialForm';
import { upstreamAvailabilityPath, type UpstreamAvailabilityWindow } from '../upstreamAvailabilityWindow';
import { useOperatorResource } from '../hooks/useOperatorResource';
import { loadModelPricePages } from '../pricingLoading';
import { CredentialPolicySummary } from '../CredentialPolicySummary';
import { PricingTable } from '../PricingTable';
import { enumLabel, IssuedCredential, messageOf, queryForTenant, WriteScopeNotice } from '../scope/operatorShared';

export function OperatorSchemaForm(props: FormProps) {
  const create = props.formContext?.providerCreate === true;
  const initialData = create ? undefined : props.formData;
  const prepared = useMemo(() => prepareSecretForm(props.schema, props.validator, initialData), [props.schema, props.validator, initialData]);
  const editDefaults: FormProps['experimental_defaultFormStateBehavior'] = props.formContext?.providerEdit
    ? { emptyObjectFields: 'skipDefaults', arrayMinItems: { populate: 'never' }, constAsDefaults: 'never' }
    : undefined;
  return <RjsfForm {...props} {...prepared} formData={create ? props.formData : prepared.formData} experimental_defaultFormStateBehavior={editDefaults ?? (prepared.schema === props.schema ? props.experimental_defaultFormStateBehavior : { ...props.experimental_defaultFormStateBehavior, emptyObjectFields: 'skipEmptyDefaults', arrayMinItems: { ...props.experimental_defaultFormStateBehavior?.arrayMinItems, computeSkipPopulate: (_validator, schema) => schema.writeOnly === true || schema.format === 'password' } })} fields={{ ...props.fields, SchemaField: SecureSchemaField }} noHtml5Validate onError={() => { /* Validation is rendered inline; never log form data. */ }} />;
}

const Form = OperatorSchemaForm;

const secretResponseRequestPolicy = {
  cache: 'no-store',
  credentials: 'omit',
  referrerPolicy: 'no-referrer',
} as const;

/**
 * Keeps the credential row action independent from pagination and row layout.
 * A future virtual list only needs to provide its current row and this callback.
 */
function CredentialCopyAction({ value, canCopy, busy, secretVisible, t, onCopy }: {
  value: KeyView;
  canCopy: boolean;
  busy: boolean;
  secretVisible: boolean;
  t: (key: string) => string;
  onCopy: (value: KeyView) => void;
}) {
  const reason = !canCopy
    ? t('credentials.copyPermission')
    : value.status === 'revoked'
      ? t('credentials.copyRevoked')
      : value.status === 'suspended'
        ? t('credentials.copySuspended')
        : !value.credential_copy_available
          ? t('credentials.copyNeedsOriginal')
          : undefined;
  const enabled = !reason;
  return <DetailTooltip content={reason ?? t('credentials.copy')}>
    <span tabIndex={enabled ? undefined : 0}>
      <Button appearance="secondary" type="button" disabled={!enabled || busy || secretVisible} onClick={() => onCopy(value)}>
        {busy ? t('common.loading') : t('credentials.copy')}
      </Button>
    </span>
  </DetailTooltip>;
}

function RevealedCredential({ value, message, copyFailedInitially = false, onCopied, onDismiss }: {
  value: string;
  message: string;
  copyFailedInitially?: boolean;
  onCopied: () => void;
  onDismiss: () => void;
}) {
  const { t } = useI18n();
  const [copyFailed, setCopyFailed] = useState(copyFailedInitially);
  const copy = async () => {
    try {
      await navigator.clipboard.writeText(value);
      setCopyFailed(false);
      onCopied();
    } catch {
      setCopyFailed(true);
    }
  };
  return <aside className="one-time">
    <div className="one-time-heading"><div><b>{message}</b><p>{t('credentials.revealedHint')}</p></div><button type="button" className="secondary one-time-close" aria-label={t('common.close')} onClick={onDismiss}>×</button></div>
    <code className="credential-revealed-value" tabIndex={0} aria-label={t('credentials.revealedValue')}>{value}</code>
    <div className="button-row"><button type="button" onClick={() => void copy()}>{t('credentials.copy')}</button></div>
    {copyFailed && <small className="one-time-error" role="alert">{t('credentials.copyManual')}</small>}
  </aside>;
}

function recentAvailabilityPath(tenant: string, now: number) {
  const query = new URLSearchParams({
    scope: tenant ? 'tenant' : 'global',
    from_created_at: String(now - 86_400_000),
    to_created_at: String(now),
  });
  if (tenant) query.set('tenant_external_id', tenant);
  return `/internal/v1/monitoring-snapshot?${query}`;
}

function isPositiveDecimal(value: string) {
  const normalized = value.trim();
  return /^(?:\d+(?:\.\d+)?|\.\d+)$/.test(normalized) && /[1-9]/.test(normalized);
}

class AccountListRefreshError extends Error {}

function UpstreamProviders({ token, tenant, writeTenant = tenant, providers, values, availabilitySnapshot, availabilityWindow, availabilityError, availabilityLoading, onOpenRequest, onOpenPricing, onOpenProxyGroups, onReadFeedbackOwnerChange, onChanged: reloadAccounts }: { token: string; tenant: string; writeTenant?: string; providers: ProviderType[]; values: UpstreamAccount[]; availabilitySnapshot?: OperatorMonitoringSnapshot; availabilityWindow?: UpstreamAvailabilityWindow; availabilityError?: string; availabilityLoading?: boolean; onOpenRequest?: (requestId: string) => void; onOpenPricing?: (tenant: string) => void; onOpenProxyGroups?: (accountId?: string) => void; onReadFeedbackOwnerChange: (caller: boolean) => void; onChanged: (saved?: boolean, caller?: boolean) => Promise<void> }) {
  const { locale, t } = useI18n();
  const onChanged = reloadAccounts;
  const accountStatusNow = useQuotaClock();
  type AccountWorkspace = { kind: 'create' }
    | { kind: 'account' | 'settings' | 'rotation' | 'reauthorization'; account: UpstreamAccount };
  const providerScopeKey = JSON.stringify([token, tenant, writeTenant]);
  const [workspaceState, setWorkspaceState] = useState<AccountWorkspace & { scope: string }>();
  const workspace = workspaceState?.scope === providerScopeKey ? workspaceState : undefined;
  const workspaceGeneration = useRef(0);
  const renderGeneration = workspaceGeneration.current;
  const returnFocus = useRef<{ accountId: string; target: 'manage-account' | 'inline-edit' | 'reauthorization' | 'rotation'; trigger?: HTMLElement | null } | undefined>(undefined);
  const detailHeading = useRef<HTMLHeadingElement>(null);
  const editing = workspace?.kind === 'settings' ? workspace.account : undefined;
  const rotating = workspace?.kind === 'rotation' ? workspace.account : undefined;
  const reauthorizing = workspace?.kind === 'reauthorization' ? workspace.account : undefined;
  const detailAccount = workspace?.kind === 'account' ? values.find(account => account.id === workspace.account.id && account.updated_at > workspace.account.updated_at) ?? workspace.account : undefined;
  const providerWorkspaceOpen = workspace?.kind === 'create';
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [method, setMethod] = useState<'direct' | 'authorization'>('direct');
  const workspaceAccountId = workspace && workspace.kind !== 'create' ? workspace.account.id : undefined;
  useEffect(() => { onReadFeedbackOwnerChange(false); }, [providerScopeKey, workspace?.kind, workspaceAccountId, method, onReadFeedbackOwnerChange]);
  const [driver, setDriver] = useState('');
  const quotaReads = useUpstreamQuotaReads(token, tenant, values);
  const [providerCreateGeneration, setProviderCreateGeneration] = useState(0);
  const [providerCreateDrafts, setProviderCreateDrafts] = useState<Record<string, Record<string, unknown>>>({});
  const providerScope = useRef({ key: providerScopeKey });
  if (providerScope.current.key !== providerScopeKey) { providerScope.current = { key: providerScopeKey }; returnFocus.current = undefined; }
  const renderScope = providerScope.current;
  const providerCreateLock = useRef<object | undefined>(undefined);
  const [providerListRetry, setProviderListRetry] = useState(false);
  const [providerAuthorizationLocked, setProviderAuthorizationLocked] = useState(false);
  const [routeCacheRevisions, setRouteCacheRevisions] = useState<Record<string, number>>({});
  const [proxyEditorOpen, setProxyEditorOpen] = useState(false);
  const [providerEditDraft, setProviderEditDraft] = useState<Record<string, unknown>>();
  const providerSuccess = useRef<HTMLDivElement>(null);
  const [busy, setBusy] = useState('');
  const providerList = useRef<HTMLElement>(null);
  const [health, setHealth] = useState<Record<string, UpstreamHealth>>({});
  const [deletionReadiness, setDeletionReadiness] = useState<Record<string, UpstreamDeletionReadiness>>({});
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  useLayoutEffect(() => {
    if (busy || workspace?.kind === 'rotation' || workspace?.kind === 'reauthorization') return;
    const pending = returnFocus.current;
    if (pending) {
      const trigger = pending.trigger?.isConnected && pending.trigger.getClientRects().length ? pending.trigger
        : Array.from(providerList.current?.querySelectorAll<HTMLButtonElement>(`[data-${pending.target}-trigger="${CSS.escape(pending.accountId)}"]`) ?? []).find(button => button.getClientRects().length && !button.disabled);
      if (trigger && trigger.getClientRects().length && !trigger.matches(':disabled')) { returnFocus.current = undefined; trigger.focus(); return; }
    }
    if (workspace?.kind === 'account') detailHeading.current?.focus();
  }, [workspace, busy]);
  useLayoutEffect(() => { if (message && !workspace && !returnFocus.current) providerSuccess.current?.focus(); }, [message]);
  function navigateWorkspace(next?: AccountWorkspace) {
    workspaceGeneration.current += 1;
    setWorkspaceState(next ? { ...next, scope: providerScopeKey } : undefined);
  }
  const ownsWorkspace = () => providerMounted.current && providerScope.current === renderScope && workspaceGeneration.current === renderGeneration;
  function openAccount(account: UpstreamAccount) { returnFocus.current = undefined; navigateWorkspace({ kind: 'account', account }); }
  function returnToAccount(account: UpstreamAccount, target: 'inline-edit' | 'reauthorization' | 'rotation') {
    returnFocus.current = { accountId: account.id, target };
    navigateWorkspace({ kind: 'account', account });
  }
  function returnToAccountList(account?: UpstreamAccount) {
    if (account) returnFocus.current = { accountId: account.id, target: 'manage-account' };
    navigateWorkspace();
  }
  const providerGroups = useGroups('provider', token, writeTenant);
  const directProviders = providers.filter(supportsDirectConnection);
  const provider = directProviders.find((value) => value.id === driver) ?? directProviders[0];
  const schema = useMemo<RJSFSchema | undefined>(() => {
    if (!provider) return undefined;
    const config = connectionSchema(provider.config_schema as RJSFSchema, t('connection.endpointHint')) as { properties?: Record<string, unknown> };
    if (provider.id === 'http-json' && config.properties) {
      delete config.properties.oauth;
    }
    const credential = directCredentialSchema(provider.credential_schema) as { oneOf?: Array<Record<string, unknown>> } | undefined;
    if (!credential) return undefined;
    if (provider.id === 'http-json' && credential.oneOf) {
      const isApiKey = (option: Record<string, unknown>) => (option.properties as { type?: { const?: unknown } } | undefined)?.type?.const === 'api_key';
      credential.oneOf.sort((left, right) => Number(isApiKey(right)) - Number(isApiKey(left)));
    }
    return providerEditSchema(localizeSchema({ type: 'object', required: ['name', 'config', 'credential'], properties: {
      name: { type: 'string', title: t('providers.name') },
      driver: { type: 'string', default: provider.id, readOnly: true },
      config: { ...config, title: 'Connection configuration' },
      credential: { ...credential, title: 'Access credential' },
    } } as RJSFSchema, locale), locale);
  }, [provider, locale]);
  const createControlledDraft = useMemo(() => {
    const credential = schema?.properties?.credential;
    if (!credential || typeof credential !== 'object') return undefined;
    const first = credential.oneOf?.[0];
    const type = first && typeof first === 'object' ? first.properties?.type : undefined;
    return type && typeof type === 'object' && type.const !== undefined
      ? { credential: { type: type.const } } : undefined;
  }, [schema]);
  const providerDraftKey = JSON.stringify([providerScopeKey, provider?.id]);
  const providerCreateScopeKey = JSON.stringify([providerScopeKey, method, provider?.id]);
  const providerCreateScope = useRef({ key: providerCreateScopeKey });
  if (providerCreateScope.current.key !== providerCreateScopeKey) providerCreateScope.current = { key: providerCreateScopeKey };
  const providerMounted = useRef(true);
  useLayoutEffect(() => { providerMounted.current = true; return () => { providerMounted.current = false; }; }, []);
  async function createProvider(formData?: Record<string, unknown>) {
    if (providerCreateScope.current.key !== providerCreateScopeKey || !token || !writeTenant || !provider || !formData || providerCreateLock.current) return;
    const attempt = providerCreateScope.current;
    const scope = providerScope.current;
    const current = () => providerMounted.current && providerCreateScope.current === attempt && providerScope.current === scope;
    const submittedDraftKey = providerDraftKey;
    providerCreateLock.current = attempt;
    setBusy('create-provider'); setError(''); setMessage('');
    setProviderCreateDrafts(drafts => ({ ...drafts, [submittedDraftKey]: formData }));
    try {
      const result = await api<UpstreamAccount>('/internal/v1/upstreams', token, { method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: writeTenant }) });
      if (!current()) return;
      setProviderCreateDrafts(drafts => { const remaining = { ...drafts }; delete remaining[submittedDraftKey]; return remaining; });
      setProviderCreateGeneration(generation => generation + 1);
      returnToAccountList();
      setMessage(t('providers.created', { name: result.name || String(formData.name ?? '') }));
      try { await onChanged(true); if (current()) setProviderListRetry(false); }
      catch (reason) { if (current()) { setProviderListRetry(!(reason instanceof AccountListRefreshError)); setError(reason instanceof AccountListRefreshError ? '' : t('providers.savedListUnavailable')); } }
    } catch (reason) { if (current()) setError(messageOf(reason, t('common.requestFailed'))); }
    finally {
      if (providerCreateLock.current === attempt) providerCreateLock.current = undefined;
      if (current()) setBusy('');
    }
  }
  async function reloadCreatedProviderList() {
    if (providerCreateLock.current) return;
    const attempt = providerScope.current;
    providerCreateLock.current = attempt;
    setBusy('reload-created-provider');
    try { await onChanged(true); if (providerScope.current === attempt) { setProviderListRetry(false); setError(''); } }
    catch (reason) { if (providerScope.current === attempt) { setProviderListRetry(!(reason instanceof AccountListRefreshError)); setError(reason instanceof AccountListRefreshError ? '' : t('providers.savedListUnavailable')); } }
    finally {
      if (providerCreateLock.current === attempt) providerCreateLock.current = undefined;
      if (providerScope.current === attempt) setBusy('');
    }
  }
  const rotateProvider = rotating ? providers.find((value) => value.id === rotating.driver) : undefined;
  const editProvider = editing ? providers.find((value) => value.id === editing.driver) : undefined;
  const editSchema = useMemo<RJSFSchema | undefined>(() => editing && editProvider ? providerEditSchema(localizeSchema({
    type: 'object',
    additionalProperties: false,
    required: ['name', 'config'],
    properties: {
      name: { type: 'string', minLength: 1, maxLength: 200, title: providerConnectionCopy(locale).displayName },
      config: { ...connectionSchema(editProvider.config_schema as RJSFSchema, t('connection.endpointHint')), title: 'Connection configuration' },
    },
  } as RJSFSchema, locale), locale) : undefined, [editing, editProvider, locale]);
  const providerEditInitialData = useMemo(() => editing && editSchema
    ? prepareSecretForm(editSchema, validator, { name: editing.name, config: editing.config }).formData as Record<string, unknown>
    : undefined, [editing, editSchema]);
  const providerEditValidator = useMemo(() => providerSettingsValidator(validator, providerEditInitialData?.config), [providerEditInitialData]);
  const uiSchema = {
    driver: { 'ui:widget': 'hidden' },
    credential: {
      ...(provider?.id === 'http-json' ? { type: { 'ui:widget': 'hidden' } } : {}),
      header: { 'ui:help': formJourneyCopy(locale).authenticationHeaderHint },
      prefix: { 'ui:help': formJourneyCopy(locale).authenticationPrefixHint, 'ui:emptyValue': '' },
    },
    config: {
      oauth: { 'ui:widget': 'hidden' },
      ...(provider?.id === 'comfyui' ? {
        workflow_template: { 'ui:field': 'JsonObject' },
        parameter_schema: { 'ui:field': 'JsonObject' },
      } : {}),
    },
  };
  useEffect(() => {
    returnFocus.current = undefined;
    navigateWorkspace();
    setProviderEditDraft(undefined); setMethod('direct'); setDriver(''); setProxyEditorOpen(false);
    setProviderCreateDrafts({}); setProviderListRetry(false); setProviderAuthorizationLocked(false); providerCreateLock.current = undefined;
    setBusy(''); setHealth({}); setDeletionReadiness({}); setRouteCacheRevisions({}); setMessage(''); setError('');
  }, [token, tenant, writeTenant]);
  const recoveryScope = useRef('');
  const initialRecoveryScope = useRef(providerScopeKey);
  useEffect(() => {
    const scope = `${token}\0${tenant}\0${writeTenant}`;
    if (providerScopeKey !== initialRecoveryScope.current) return;
    if (recoveryScope.current === scope) return;
    recoveryScope.current = scope;
    const recovery = readDeviceLoginRecovery(writeTenant);
    if (!recovery) return;
    const account = values.find(value => value.id === recovery.account_id && value.driver === 'openai-codex' && value.can_reauthorize);
    if (account) navigateWorkspace({ kind: 'reauthorization', account });
    else if (!recovery.account_id) { setMethod('authorization'); navigateWorkspace({ kind: 'create' }); }
  }, [token, tenant, writeTenant, values]);

  const statusFilter = useResourceListStatusFilter('upstreams', tenant, values, (value) => value.status === 'active');

  const canManage = (value: UpstreamAccount) => Boolean(writeTenant) && (!value.tenant_external_id || value.tenant_external_id === writeTenant);

  function openRotation(account: UpstreamAccount) {
    returnFocus.current = { accountId: account.id, target: 'rotation' };
    setError(''); setMessage('');
    navigateWorkspace({ kind: 'rotation', account });
  }

  function returnFromRotation(account = rotating) {
    if (!account || workspace?.kind !== 'rotation' || !ownsWorkspace()) return;
    returnToAccount(account, 'rotation');
  }

  function openReauthorization(account: UpstreamAccount) {
    returnFocus.current = { accountId: account.id, target: 'reauthorization', trigger: document.activeElement instanceof HTMLElement ? document.activeElement : null };
    setError(''); setMessage('');
    navigateWorkspace({ kind: 'reauthorization', account });
  }

  function returnFromReauthorization(account = reauthorizing) {
    if (!account || workspace?.kind !== 'reauthorization' || !ownsWorkspace()) return;
    returnToAccount(account, 'reauthorization');
  }

  const openCatalogRouteAction = (account: UpstreamAccount, action: CatalogRouteAction) => {
    const target = account.tenant_external_id ?? writeTenant;
    if (!target) return;
    storeCatalogRouteAction(target, account.id, action);
    window.location.assign(appHref('operator', 'routes'));
  };

  async function refreshOAuth(value: UpstreamAccount) {
    if (!canManage(value)) return;
    setBusy(`refresh-${value.id}`);
    setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}/oauth/refresh`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() } });
      setMessage(t('providers.refreshed', { name: value.name }));
      await onChanged();
    } catch (reason) { if (!(reason instanceof AccountListRefreshError)) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  async function disconnectOAuth(value: UpstreamAccount) {
    if (!canManage(value) || !await confirm(t('providers.confirmDisconnect', { name: value.name }))) return;
    setBusy(`disconnect-${value.id}`);
    setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}/oauth/disconnect`, token, {
        method: 'POST',
        body: JSON.stringify({ tenant_external_id: writeTenant, expected_updated_at: value.updated_at }),
      });
      setMessage(t('providers.disconnected', { name: value.name }));
      await onChanged();
    } catch (reason) { if (!(reason instanceof AccountListRefreshError)) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  async function setStatus(value: UpstreamAccount, status: 'active' | 'disabled') {
    if (!canManage(value)) return;
    setBusy(`status-${value.id}`); setError(''); setMessage('');
    try {
      await api(`/internal/v1/upstreams/${value.id}`, token, { method: 'PATCH', body: JSON.stringify({ tenant_external_id: writeTenant, status, expected_updated_at: value.updated_at }) });
      setHealth((current) => { const next = { ...current }; delete next[value.id]; return next; });
      setDeletionReadiness((current) => { const next = { ...current }; delete next[value.id]; return next; });
      setMessage(t(status === 'active' ? 'providers.enabled' : 'providers.disabled', { name: value.name }));
      await onChanged();
      setHealth((current) => { const next = { ...current }; delete next[value.id]; return next; });
    } catch (reason) { if (!(reason instanceof AccountListRefreshError)) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  async function checkHealth(value: UpstreamAccount) {
    if (!canManage(value)) return;
    setBusy(`health-${value.id}`); setError('');
    try {
      const result = await api<UpstreamHealth>(`/internal/v1/upstreams/${value.id}/health${queryForTenant(writeTenant)}`, token, { method: 'POST' });
      setHealth((current) => ({ ...current, [value.id]: result }));
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  }

  function deletionMessages(readiness: UpstreamDeletionReadiness) {
    const messages: string[] = [];
    if (readiness.requires_disabled) messages.push(t('providers.deleteRequiresDisabled'));
    if (readiness.model_route_count > 0) messages.push(t('providers.deleteBlockedRoutes', { count: formatNumber(readiness.model_route_count, locale) }));
    if (readiness.imported_for_audit) messages.push(t('providers.deleteBlockedImport'));
    if (readiness.can_delete) messages.push(t('providers.deleteReady'));
    return messages;
  }

  async function remove(value: UpstreamAccount) {
    if (!canManage(value) || !ownsWorkspace()) return;
    let deletionGeneration = renderGeneration;
    const current = () => providerMounted.current && providerScope.current === renderScope && workspaceGeneration.current === deletionGeneration;
    setBusy(`delete-${value.id}`); setError(''); setMessage('');
    try {
      const readiness = await api<UpstreamDeletionReadiness>(`/internal/v1/upstreams/${value.id}/deletion-readiness${queryForTenant(writeTenant)}`, token);
      if (!current()) return;
      setDeletionReadiness((current) => ({ ...current, [value.id]: readiness }));
      if (!readiness.can_delete) {
        setError(deletionMessages(readiness).join(' '));
        return;
      }
      if (!await confirm(t('providers.confirmDelete', { name: value.name })) || !current()) return;
      const query = new URLSearchParams({ tenant_external_id: writeTenant, expected_updated_at: String(value.updated_at) });
      await api(`/internal/v1/upstreams/${value.id}?${query}`, token, { method: 'DELETE' });
      if (!current()) return;
      if (workspace && workspace.kind !== 'create' && workspace.account.id === value.id) {
        setProviderEditDraft(undefined);
        returnToAccountList(value);
        deletionGeneration = workspaceGeneration.current;
      }
      setMessage(t('providers.deleted', { name: value.name }));
      await onChanged(false);
    } catch (reason) { if (current() && !(reason instanceof AccountListRefreshError)) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (current()) setBusy(''); }
  }

  const connectionCopy = providerConnectionCopy(locale);
  async function requestLeaveProviderWorkspace() {
    if (proxyEditorOpen || busy || !ownsWorkspace()) return false;
    const authorizationOpen = Boolean(reauthorizing || (providerWorkspaceOpen && method === 'authorization'));
    const settingsDirty = editing && providerEditDraft && (providerEditDraft.name !== editing.name || providerSettingsConfigChanged(providerEditInitialData?.config, providerEditDraft));
    const createDraft = providerCreateDrafts[providerDraftKey];
    const createDirty = providerWorkspaceOpen && method === 'direct' && createDraft && JSON.stringify(createDraft) !== JSON.stringify(createControlledDraft);
    if (authorizationOpen) {
      if (!await confirm(t('providers.confirmLeaveAuthorization'))) return false;
    } else if ((settingsDirty || createDirty || rotating) && !await confirm(connectionCopy.discard)) return false;
    return ownsWorkspace();
  }
  useNavigationGuard(requestLeaveProviderWorkspace);
  async function leaveProviderSettings(action: () => void) {
    if (!await requestLeaveProviderWorkspace()) return;
    setProviderEditDraft(undefined);
    action();
  }
  const providerFormContext = editing ? {
    providerEdit: true,
    providerIdentityTitle: connectionCopy.identity,
    providerIdentity: <ProviderAccountIdentity account={editing} editing />,
    providerConnectionTitle: connectionCopy.network,
    providerConnection: <>
      <UpstreamConnection key={`${token}\0${writeTenant}\0${editing.id}\0${editing.credential_generation}`} embedded account={editing} token={token} tenant={writeTenant} disabled={!canManage(editing) || Boolean(busy)} onChanged={onChanged} onEditingChange={setProxyEditorOpen} onSaved={updated => { if (ownsWorkspace()) setWorkspaceState(current => current?.kind === 'settings' && current.account.id === updated.id ? { ...current, account: { ...updated, name: current.account.name, config: current.account.config } } : current); }} />
      {proxyEditorOpen && <p role="status">{t('connection.finishProxyFirst')}</p>}
    </>,
    providerAuthentication: <FormSection title={connectionCopy.authentication}>
      <span>{editProvider?.display_name ?? t('providerDirectory.other')} · {editing.auth_kind === 'oauth' ? t('providers.oauth') : enumLabel(t, 'auth', editing.connection_method)}</span>
      <div className="row-actions">
        {canReauthorizeAccount(editing, providers.find(provider => provider.id === editing.driver)) && <Button data-reauthorization-trigger={editing.id} appearance="secondary" type="button" disabled={Boolean(busy) || proxyEditorOpen} onClick={() => void leaveProviderSettings(() => openReauthorization(editing))}>{editing.driver === 'kimi-oauth' ? connectionCopy.signInAgain : t('providers.reauthorize')}</Button>}
        {editing.can_rotate && <Button data-rotation-trigger={editing.id} appearance="secondary" type="button" disabled={Boolean(busy) || proxyEditorOpen} onClick={() => void leaveProviderSettings(() => openRotation(editing))}>{t('providers.rotateCredential')}</Button>}
      </div>
    </FormSection>,
    providerRouting: <FormSection title={connectionCopy.routing}>
      <p>{t('providers.routes', { count: formatNumber(editing.route_count, locale) })}</p>
      <p>{connectionCopy.routingHint}</p>
      <Button appearance="secondary" type="button" disabled={Boolean(busy) || proxyEditorOpen} onClick={() => void leaveProviderSettings(() => { window.location.assign(appHref('operator', 'routes')); })}>{connectionCopy.openRoutes}</Button>
    </FormSection>,
  } : undefined;
  async function saveProviderSettings(formData?: Record<string, unknown>) {
    if (!editing || !formData || !canManage(editing) || proxyEditorOpen || busy || !ownsWorkspace()) return;
    setBusy(`edit-${editing.id}`);
    setError('');
    try {
      const body = providerSettingsUpdate(formData, providerEditInitialData?.config, writeTenant, editing.updated_at);
      const response = await api<unknown>(`/internal/v1/upstreams/${editing.id}`, token, { method: 'PUT', body: JSON.stringify(body) });
      if (!ownsWorkspace()) return;
      const updated = Object.hasOwn(body, 'config') ? response as UpstreamAccount : mergeProviderRenameReceipt(editing, response, writeTenant, body.name);
      if (!updated) throw new Error(connectionCopy.renameResponseUncertain);
      setMessage(t('providers.updated', { name: updated.name }));
      setProviderEditDraft(undefined);
      setBusy('');
      returnToAccount(updated, 'inline-edit');
      const returnedGeneration = workspaceGeneration.current;
      try { await onChanged(true); }
      catch (reason) { if (!(reason instanceof AccountListRefreshError) && providerMounted.current && providerScope.current === renderScope && workspaceGeneration.current === returnedGeneration) setError(messageOf(reason, t('common.requestFailed'))); }
    } catch (reason) { if (ownsWorkspace()) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (ownsWorkspace()) setBusy(''); }
  }
  const providerEditors = <>
      {editing && editSchema && <div className="inline-editor"><Form key={`${editing.id}-${locale}`} schema={editSchema} liveValidate formContext={providerFormContext} uiSchema={{ config: { oauth: { 'ui:disabled': true }, ...(editing.driver === 'openai-codex' && editing.auth_kind === 'oauth' ? { base_url: { 'ui:widget': 'hidden' } } : {}) } }} formData={providerEditDraft ?? providerEditInitialData} onChange={({ formData }) => setProviderEditDraft(formData)} validator={providerEditValidator} widgets={providerFormWidgets} templates={upstreamFormTemplates} onSubmit={({ formData }) => void saveProviderSettings(formData)}><Button appearance="primary" type="submit" disabled={!canManage(editing) || Boolean(busy) || proxyEditorOpen}>{t('common.save')}</Button></Form></div>}
  </>;
  function renderAccountDetails(value: UpstreamAccount) {
    const providerAvailable = providers.some(provider => provider.id === value.driver);
    const manageable = canManage(value);
    const currentHealth = providerAvailable ? health[value.id] : undefined;
    const currentReadiness = deletionReadiness[value.id];
    const deletionBlockers = currentReadiness ? deletionMessages(currentReadiness) : [];
    const generation = value.credential_generation;
    const cachedQuota = quotaReads.entries[value.id];
    return <section id={`provider-details-${value.id}`} className="provider-detail-workspace" aria-label={t('providerDirectory.details', { name: value.name })}>
            {error && <div className="notice error" role="alert">{error}</div>}
            {message && <div className="notice success" role="status">{message}</div>}
            <div className="provider-detail-heading"><h3 ref={detailHeading} tabIndex={-1}>{value.name}</h3><DetailTooltip content={`${providers.find(provider => provider.id === value.driver)?.display_name ?? t('providerDirectory.other')} · ${t('providerDirectory.account')}`}><span tabIndex={0}>{t('providerDirectory.account')}</span></DetailTooltip>{providerAvailable && <Button appearance="secondary" type="button" data-inline-edit-trigger={value.id} disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => { setProviderEditDraft(undefined); navigateWorkspace({ kind: 'settings', account: value }); }}>{t('providers.edit')}</Button>}<Button appearance="secondary" type="button" disabled={Boolean(busy) || proxyEditorOpen} onClick={() => returnToAccountList(value)}>{t('providerDirectory.close')}</Button></div>
            <ProviderModelCatalog key={`catalog-${value.id}-${generation}`} accountId={value.id} tenant={value.tenant_external_id ?? tenant} token={token} disabled={!manageable || !providerAvailable || value.status !== 'active' || Boolean(busy) || proxyEditorOpen} onRouteAction={(action) => openCatalogRouteAction(value, action)} routeActionDisabled={!manageable || !providerAvailable || value.status !== 'active' || Boolean(busy) || proxyEditorOpen} routeCacheRevision={routeCacheRevisions[value.id] ?? 0} />
            <div className="account-main">
            <ProviderAccountStatus account={value} />
            <ProviderAccountIdentity account={value} />
            <UpstreamConnection key={`connection\0${token}\0${writeTenant}\0${value.id}`} readOnOpen account={value} token={token} tenant={writeTenant} disabled={!manageable || Boolean(busy)} onChanged={onChanged} onEditingChange={setProxyEditorOpen} />
            <Disclosure title={currentHealth ? `${t('providers.recentAvailability')} · ${t(manualHealthLabel(currentHealth))}` : t('providers.recentAvailability')}><UpstreamAvailability account={value} snapshot={availabilitySnapshot} window={availabilityWindow} loading={availabilityLoading} manualHealth={currentHealth} onOpenRequest={onOpenRequest} /></Disclosure>
            <UpstreamQuota key={`${token}\0${tenant}\0${value.id}\0${generation}`} accountId={value.id} accountName={value.name} credentialGeneration={generation} tenant={value.tenant_external_id ?? tenant} token={token} readState={cachedQuota?.generation === generation ? cachedQuota : undefined} onRefresh={() => void quotaReads.read(value)} refreshDisabled={Boolean(quotaReads.progress?.busy)} />
            {currentReadiness && <small className={`status ${currentReadiness.can_delete ? 'ok' : 'pending'}`}>{deletionBlockers.join(' · ')}</small>}
          </div>
          <div className="account-meta">
            <Disclosure title={t('request.technicalDetails')}><dl><div><dt>{t('traffic.upstreamId')}</dt><dd><code>{value.id}</code><CopyButton fluent value={value.id} label={t('providers.copyAccountId')} /></dd></div><div><dt>{t('providers.provider')}</dt><dd><code>{value.driver}</code></dd></div><div><dt>{t('providers.generation')}</dt><dd>{formatNumber(value.credential_generation, locale)}</dd></div></dl></Disclosure>
            <Disclosure title={t('connection.manageAccount')} defaultOpen={returnFocus.current?.accountId === value.id && ['rotation', 'reauthorization'].includes(returnFocus.current.target)}><div className="row-actions">
              {providerAvailable && <>
                <Button appearance="secondary" type="button" disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => void checkHealth(value)}>{t('providers.runManualHealthCheck')}</Button>
                {value.can_refresh && <Button appearance="secondary" type="button" disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => void refreshOAuth(value)}>{t('providers.refreshAuthorization')}</Button>}
                {canReauthorizeAccount(value, providers.find(provider => provider.id === value.driver)) && <Button data-reauthorization-trigger={value.id} appearance="secondary" type="button" disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => openReauthorization(value)}>{value.driver === 'kimi-oauth' ? connectionCopy.signInAgain : t('providers.reauthorize')}</Button>}
                {value.can_rotate && <Button data-rotation-trigger={value.id} appearance="secondary" type="button" disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => openRotation(value)}>{t('providers.rotateCredential')}</Button>}
              </>}
            </div></Disclosure>
            <Disclosure title={t('connection.dangerZone')}><p>{t('connection.dangerHint')}</p><div className="row-actions">
              {value.auth_kind === 'oauth' && <Button appearance="secondary" type="button" className="danger" disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => void disconnectOAuth(value)}>{t('providers.disconnect')}</Button>}
              {(value.status === 'active' || providerAvailable) && <Button appearance="secondary" type="button" className="danger" disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => void setStatus(value, value.status === 'active' ? 'disabled' : 'active')}>{value.status === 'active' ? t('providers.disable') : t('providers.enable')}</Button>}
              <Button appearance="secondary" type="button" className="danger" title={deletionBlockers.length > 0 ? deletionBlockers.join(' ') : undefined} disabled={!manageable || Boolean(busy) || proxyEditorOpen} onClick={() => void remove(value)}>{t('common.remove')}</Button>
            </div></Disclosure>
          </div>
    </section>;
  }
  const providerWorkspaceActive = providerWorkspaceOpen || Boolean(editing || rotating || reauthorizing);
  return <TransportProxyGroups key={`${token}\0${writeTenant}`} token={token} tenant={writeTenant} accounts={values} onChanged={onChanged} onNavigate={onOpenProxyGroups}>{confirmationDialog}<WriteScopeNotice tenant={writeTenant} /><section ref={providerList} className="provider-layout">
    <article className="panel provider-list" hidden={Boolean(workspace)}><div className="panel-title"><div><h2>{t('providers.title')}</h2><p className="muted">{t('providers.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>
      <LoadingProgress active={Boolean(availabilityLoading)} label={t('common.loading')} level="page" />
      <div className="row-actions quota-read-toolbar"><Button appearance="secondary" type="button" disabled={!token || !values.some(account => account.status === 'active' && Boolean(account.tenant_external_id ?? tenant)) || Boolean(quotaReads.progress?.busy) || Object.values(quotaReads.entries).some(entry => entry.busy)} onClick={() => void quotaReads.readAll()}>{t('quota.refreshAll')}</Button>{quotaReads.progress && <span role="status">{t(quotaReads.progress.busy ? 'quota.batchProgress' : 'quota.batchComplete', { done: formatNumber(quotaReads.progress.done, locale), total: formatNumber(quotaReads.progress.total, locale) })}</span>}</div>
      {error && !workspace && <div className="notice error" role="alert">{error}</div>}{providerListRetry && <Button appearance="secondary" type="button" disabled={Boolean(busy)} onClick={() => void reloadCreatedProviderList()}>{t('providers.reloadAccountList')}</Button>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{availabilityError && <div className="notice error" role="alert">{availabilityError}</div>}{message && !workspace && <div ref={providerSuccess} tabIndex={-1} className="notice success" role="status">{message}</div>}
      <div className="account-list provider-directory">{statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('status.active')} empty={t('providers.empty')} />}{statusFilter.values.map((value) => {
        const providerAvailable = providers.some((provider) => provider.id === value.driver);
        const manageable = canManage(value);
        const memberships = providerGroups.groups.filter((group) => group.member_ids.includes(value.id));
        const detailOpen = detailAccount?.id === value.id;
        const accountStatus = providerAccountStatus(value, accountStatusNow);
        const providerName = providers.find(provider => provider.id === value.driver)?.display_name ?? t('providerDirectory.other');
        const facts = availabilityWindow && availabilityWindow.tenant_external_id === (value.tenant_external_id ?? tenant) ? availabilityWindow.accounts.find(account => account.upstream_account_id === value.id) : undefined;
        const terminal = facts ? facts.metrics.successful_requests + facts.metrics.failed_requests : 0;
        const cachedQuota = quotaReads.entries[value.id];
        const generation = value.credential_generation;
        const quota = cachedQuota?.generation === generation ? cachedQuota.snapshot : undefined;
        const quotaRefreshFailed = Boolean(cachedQuota?.generation === generation && cachedQuota.refreshFailed);
        return <div className="account provider-account" data-upstream-id={value.id} key={value.id}>
          <div className="provider-directory-row">
            <div className="provider-directory-identity">
            <DetailTooltip content={`${providerName} · ${t('providerDirectory.account')}`}><b tabIndex={0}>{value.name}</b></DetailTooltip>
            <span>{providerName} · {value.auth_kind === 'oauth' ? t('providers.oauth') : enumLabel(t, 'auth', value.connection_method)}</span>
            {memberships.length > 0 && <div className="table-chip-list provider-group-summary" aria-label={t('groups.provider.title')}>{memberships.map((group) => <span key={group.id}>{group.name}</span>)}</div>}
            {!providerAvailable && <span className="pill">{t('providers.retired')}</span>}
            </div>
            <div className="provider-directory-summary"><ProviderAccountStatus account={value} /><span>{t('providers.routes', { count: formatNumber(value.route_count, locale) })}</span></div>
            <div className="provider-directory-summary" aria-busy={Boolean(availabilityLoading && !facts)}><small>{t('providers.recentAvailability')}</small>{availabilityLoading && !facts ? <LoadingState label={t('common.loading')} variant="inline" /> : <span>{!facts ? t('providerDirectory.unavailable') : terminal > 0 ? t('providerDirectory.successful', { percent: formatPercent(facts.metrics.successful_requests / terminal, locale) }) : t('providerDirectory.noRequests')}</span>}</div>
            <div className="provider-directory-summary" aria-busy={Boolean(cachedQuota?.generation === generation && cachedQuota.busy)}><small>{t('quota.title')}</small><span role="status">{cachedQuota?.generation === generation && cachedQuota.busy ? t('quota.refreshing') : cachedQuota?.generation === generation && cachedQuota.queued ? t('quota.queued') : <QuotaSummary snapshot={quota} refreshFailed={quotaRefreshFailed} showWindowReset showResetCreditExpiryInTooltip={quota?.provider === 'openai-codex'} />}</span></div>
            <div className="provider-directory-actions">
              <Button appearance="secondary" type="button" disabled={!token || !(value.tenant_external_id ?? tenant) || value.status !== 'active' || Boolean(quotaReads.progress?.busy) || Boolean(cachedQuota?.generation === generation && cachedQuota.busy)} onClick={() => void quotaReads.read(value)}>{t('quota.refreshAccount')}</Button>
              <Button data-manage-account-trigger={value.id} appearance="secondary" type="button" aria-expanded={detailOpen} aria-controls={`provider-details-${value.id}`} disabled={Boolean(busy) || proxyEditorOpen || providerWorkspaceActive} onClick={() => detailOpen ? returnToAccountList(value) : openAccount(value)}>{t('providerDirectory.open')}</Button>
              {accountStatus.expired && canReauthorizeAccount(value, providers.find(provider => provider.id === value.driver)) && <Button data-reauthorization-trigger={value.id} appearance="primary" type="button" disabled={!manageable || Boolean(busy) || proxyEditorOpen || providerWorkspaceActive} onClick={() => openReauthorization(value)}>{value.driver === 'kimi-oauth' ? connectionCopy.signInAgain : t('providers.reauthorize')}</Button>}
            </div>
            <div className="provider-sync-slot"><ManagedModelSync accountId={value.id} tenant={value.tenant_external_id ?? tenant} token={token} disabled={!manageable || !providerAvailable || value.status !== 'active' || Boolean(busy) || proxyEditorOpen} reviewModelsDisabled={Boolean(busy) || proxyEditorOpen || providerWorkspaceActive} reviewPricingDisabled={Boolean(busy) || proxyEditorOpen || providerWorkspaceActive} onReviewModels={() => openAccount(value)} onReviewPricing={onOpenPricing ? () => onOpenPricing(value.tenant_external_id ?? tenant) : undefined} onReconciled={() => {
              setRouteCacheRevisions((current) => ({ ...current, [value.id]: (current[value.id] ?? 0) + 1 }));
              void onChanged().catch(reason => { if (!(reason instanceof AccountListRefreshError) && ownsWorkspace()) setError(messageOf(reason, t('common.requestFailed'))); });
            }} /></div>
          </div>

        </div>;
      })}</div>
    </article>
    {detailAccount && renderAccountDetails(detailAccount)}
    {rotating && rotateProvider ? <UpstreamCredentialRotation key={`${token}\0${tenant}\0${writeTenant}\0${rotating.id}`} account={rotating} provider={rotateProvider} token={token} allowed={canManage(rotating)} onBack={() => returnFromRotation()} onSaved={updated => {
      if (!ownsWorkspace()) return;
      returnFromRotation(updated); setProviderEditDraft(undefined); setMessage(upstreamRotationCopy(locale).saved);
      const returnedGeneration = workspaceGeneration.current;
      void onChanged(true).catch(reason => { if (!(reason instanceof AccountListRefreshError) && providerMounted.current && providerScope.current === renderScope && workspaceGeneration.current === returnedGeneration) setMessage(upstreamRotationCopy(locale).reloadFailed); });
    }} /> : <CreateJourney className={editing ? 'provider-edit-workspace' : reauthorizing ? 'provider-reauthorization-workspace' : ''} title={editing ? t('providers.editFor', { name: editing.name }) : reauthorizing ? t('providers.reauthorizeFor', { name: reauthorizing.name }) : t('providers.add')} description={reauthorizing ? authorizationJourneyCopy(locale, reauthorizing.driver).purpose : t('providers.description')} open={providerWorkspaceActive} busy={Boolean(busy) || proxyEditorOpen} onOpenChange={(open) => { if (proxyEditorOpen) return; if (!open && reauthorizing) { returnFromReauthorization(); return; } if (!open && editing) { void leaveProviderSettings(() => { returnToAccount(editing, 'inline-edit'); }); return; } if (open) { returnFocus.current = undefined; navigateWorkspace({ kind: 'create' }); } else returnToAccountList(); }}>
      {editing && message && <div className="notice success" role="status">{message}</div>}
      {error && providerWorkspaceActive && <div className="notice error" role="alert">{error}</div>}
      {editing && <ProviderModelCatalog key={`catalog-edit-${editing.id}-${editing.credential_generation}`} accountId={editing.id} tenant={editing.tenant_external_id ?? writeTenant} token={token} disabled={!editProvider || editing.status !== 'active' || Boolean(busy) || proxyEditorOpen} onRouteAction={(action) => openCatalogRouteAction(editing, action)} routeActionDisabled={!editProvider || editing.status !== 'active' || Boolean(busy) || proxyEditorOpen} routeCacheRevision={routeCacheRevisions[editing.id] ?? 0} />}
      {editing || rotating ? providerEditors : reauthorizing ? <>
      <AuthorizationConnection key={`${token}\0${writeTenant}\0reauthorize-${reauthorizing.id}`} token={token} tenant={writeTenant} providers={providers} existing={reauthorizing} onEditName={() => { if (!ownsWorkspace() || !canManage(reauthorizing) || busy || proxyEditorOpen) return; setProviderEditDraft(undefined); setError(''); setMessage(''); navigateWorkspace({ kind: 'settings', account: reauthorizing }); }} onConnectionChanged={onChanged} onAccountSaved={updated => { if (ownsWorkspace()) setWorkspaceState(current => current?.kind === 'reauthorization' && current.account.id === updated.id ? { ...current, account: updated } : current); }} onEditingChange={setProxyEditorOpen} onChanged={async updated => { if (!ownsWorkspace()) return; await reloadAccounts(Boolean(updated), true); if (!ownsWorkspace()) return; returnFromReauthorization(updated); setProviderEditDraft(undefined); setMessage(authorizationJourneyCopy(locale).saved); }} />
      <Button appearance="secondary" type="button" disabled={Boolean(busy) || proxyEditorOpen} onClick={() => returnFromReauthorization()}>{authorizationJourneyCopy(locale).back}</Button>
    </> : <>
      <div className="segmented" role="group" aria-label={t('providers.method')}><Button appearance="secondary" type="button" disabled={Boolean(busy) || providerAuthorizationLocked} aria-pressed={method === 'direct'} className={method === 'direct' ? 'active' : ''} onClick={() => setMethod('direct')}>{t('providers.direct')}</Button><Button appearance="secondary" type="button" disabled={Boolean(busy) || providerAuthorizationLocked} aria-pressed={method === 'authorization'} className={method === 'authorization' ? 'active' : ''} onClick={() => setMethod('authorization')}>{t('providers.oauth')}</Button></div>
      {method === 'direct' ? <>
        <ModelPicker label={t('providers.provider')} disabled={Boolean(busy)} value={provider?.id ?? ''} onChange={setDriver} groupBy="none" popupLabel={t('providers.directory')} searchPlaceholder={t('providers.searchDirectory')} searchAriaLabel={t('providers.searchDirectory')} emptyText={t('providers.directoryEmpty')} options={directProviders.map(value => ({ key: value.id, value: value.id, label: value.display_name, provider: value.display_name, upstream: '', capabilities: value.protocols }))} />
        {schema ? <Form key={`${providerDraftKey}-${providerCreateGeneration}`} schema={schema} uiSchema={uiSchema} disabled={Boolean(busy)} formData={providerCreateDrafts[providerDraftKey] ?? createControlledDraft} onChange={({ formData }) => { if (providerCreateScope.current.key === providerCreateScopeKey && !providerCreateLock.current) setProviderCreateDrafts(drafts => ({ ...drafts, [providerDraftKey]: formData ?? {} })); }} formContext={{ providerCreate: true, fluentSecrets: true }} fields={schemaFormFields} validator={validator} widgets={fluentFormWidgets} templates={upstreamFormTemplates} onSubmit={({ formData }) => void createProvider(formData)}><Button appearance="primary" type="submit" disabled={!writeTenant || !token || Boolean(busy)}>{t(busy === 'create-provider' ? 'common.loading' : 'providers.create')}</Button></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </> : <AuthorizationConnection key={`${providerScopeKey}-${providerCreateGeneration}`} token={token} tenant={writeTenant} providers={providers} active={providerWorkspaceActive} onLock={setProviderAuthorizationLocked} onChanged={async account => {
        const attempt = providerScope.current;
        if (!ownsWorkspace() || attempt.key !== providerScopeKey) return;
        await reloadAccounts(Boolean(account), true);
        if (!ownsWorkspace() || providerScope.current !== attempt || !account) return;
        setProviderCreateGeneration(generation => generation + 1);
        openAccount(account);
        setMessage(t('providers.created', { name: account.name }));
      }} />}</>}
    </CreateJourney>}
  </section></TransportProxyGroups>;
}

type NativeAuthorizationSession = { login_url?: string; verification_url?: string; user_code?: string; session_token?: string; session_id?: string; resumed?: boolean; expires_at?: number; poll_after_seconds?: number };

function AuthorizationConnection({ token, tenant, providers, existing, active = true, onChanged, onConnectionChanged = onChanged, onAccountSaved, onEditingChange, onLock, onEditName }: { token: string; tenant: string; providers: ProviderType[]; existing?: UpstreamAccount; active?: boolean; onChanged: (account?: UpstreamAccount) => Promise<void>; onConnectionChanged?: () => Promise<void>; onAccountSaved?: (account: UpstreamAccount) => void; onEditingChange?: (editing: boolean) => void; onLock?: (locked: boolean) => void; onEditName?: () => void }) {
  const { locale, t } = useI18n();
  const [connectionEditing, setConnectionEditing] = useState(false);
  useEffect(() => { onEditingChange?.(connectionEditing); }, [connectionEditing, onEditingChange]);
  const oauthProviders = providers.filter((provider) => provider.oauth_adapter);
  const existingOAuthProvider = oauthProviders.find((provider) => provider.id === existing?.driver);
  const initialProvider = existingOAuthProvider ?? (readDeviceLoginRecovery(tenant) ? oauthProviders.find(provider => provider.id === 'openai-codex') : undefined) ?? oauthProviders[0];
  const [providerChoice, setProviderChoice] = useState(initialProvider?.id ?? '');
  const [nativeLocked, setNativeLocked] = useState(false);
  useEffect(() => { setNativeLocked(false); }, [token, tenant]);
  const selectedProvider = oauthProviders.find((provider) => provider.id === providerChoice);
  const journeyCopy = authorizationJourneyCopy(locale, selectedProvider?.id);
  const isClaude = selectedProvider?.oauth_adapter?.flow_kind === 'claude_manual_pkce';
  const [name, setName] = useState(existing?.name ?? initialProvider?.display_name ?? '');
  const [session, setSession] = useState<NativeAuthorizationSession>();
  const [manualCode, setManualCode] = useState('');
  const [proxyUrl, setProxyUrl] = useState('');
  const [useProxy, setUseProxy] = useState(false);
  const proxyMode = oauthCreationProxyMode(selectedProvider);
  const needsProxy = !existing && useProxy;
  const proxyValid = !needsProxy || isPrivateProxyUrl(proxyUrl.trim());
  const [authorizing, setAuthorizing] = useState(false);
  const [polling, setPolling] = useState(false);
  const [pollStopped, setPollStopped] = useState(false);
  const pollLock = useRef(false);
  const pollRequest = useRef<AbortController | undefined>(undefined);
  const [claudePending, setClaudePending] = useState(false);
  const claudeCompletion = useRef<{ deadline: number; attempts: number; nextAt: number; code: string; phase: 'pending' | 'dispatched' | 'unknown' } | undefined>(undefined);
  const [nextPollAt, setNextPollAt] = useState(0);
  const [now, setNow] = useState(Date.now);
  const [listRetry, setListRetry] = useState(false);
  const [listLoading, setListLoading] = useState(false);
  const savedAccount = useRef<UpstreamAccount | undefined>(undefined);
  useEffect(() => { onLock?.(authorizing || polling || listLoading || Boolean(session) || nativeLocked || listRetry || Boolean(savedAccount.current)); }, [authorizing, polling, listLoading, session, nativeLocked, listRetry, onLock]);
  const scopeVersion = useRef(0);
  useEffect(() => () => { scopeVersion.current += 1; pollRequest.current?.abort(); }, [token, tenant]);
  useLayoutEffect(() => {
    if (!isClaude) return;
    if (!active) {
      setPolling(false); setAuthorizing(false);
      if (claudeCompletion.current?.phase === 'dispatched') {
        claudeCompletion.current.phase = 'unknown';
        setClaudePending(false); setError(journeyCopy.completeClosedUnknown);
      }
    }
    return () => { scopeVersion.current += 1; pollRequest.current?.abort(); pollLock.current = false; };
  }, [active, isClaude, token, tenant]);
  const isKimi = isKimiDeviceProvider(selectedProvider);
  const expired = Boolean(!session?.resumed && (!session?.session_id || pollStopped) && session?.expires_at && now >= session.expires_at);
  useEffect(() => {
    if (!session) return;
    setNow(Date.now());
    const timer = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(timer);
  }, [session]);
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const reset = () => { savedAccount.current = undefined; scopeVersion.current += 1; pollRequest.current?.abort(); pollLock.current = false; claudeCompletion.current = undefined; setClaudePending(false); setSession(undefined); setManualCode(''); setProxyUrl(''); setUseProxy(false); setMessage(''); setError(''); setNextPollAt(0); setListRetry(false); setListLoading(false); setAuthorizing(false); setPolling(false); setPollStopped(false); };
  useEffect(() => {
    reset();
    const recovery = readDeviceLoginRecovery(tenant);
    if (selectedProvider?.id === 'openai-codex' && recovery && recovery.account_id === existing?.id) {
      setSession({ session_id: recovery.session_id, expires_at: recovery.expires_at, resumed: true });
      setNextPollAt(Date.now());
    }
  }, [token, tenant]);
  const start = async (providerConfig?: unknown) => {
    if (savedAccount.current || !tenant || !selectedProvider || !name.trim() || connectionEditing || authorizing || polling || listLoading || listRetry || session || !proxyValid) return;
    setAuthorizing(true);
    const attempt = scopeVersion.current;
    const begin = async (request: Promise<NativeAuthorizationSession>) => {
      const result = await request;
      if (scopeVersion.current === attempt) {
        setNow(Date.now());
        setNextPollAt(Date.now() + Math.max(1, result.poll_after_seconds ?? 5) * 1000);
        setSession(result);
        if (selectedProvider.id === 'openai-codex' && result.session_id && result.expires_at) {
          saveDeviceLoginRecovery({ session_id: result.session_id, tenant, account_id: existing?.id, expires_at: result.expires_at });
        }
      }
    };
    try {
      const target = existing ? { upstream_account_id: existing.id } : {};
      const proxy = needsProxy ? { proxy_url: proxyUrl.trim() } : {};
      const flow = selectedProvider.oauth_adapter?.flow_kind;
      if (flow === 'openai_device') {
        await begin(api<NativeAuthorizationSession>('/internal/v1/oauth/codex/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...proxy, ...target }) }));
      } else if (flow === 'claude_manual_pkce') {
        await begin(api<NativeAuthorizationSession>('/internal/v1/oauth/claude/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...proxy, ...target }) }));
      } else if (flow === 'github_device_copilot') {
        await begin(api<NativeAuthorizationSession>('/internal/v1/oauth/copilot/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...proxy, ...target }) }));
      } else if (isKimi) {
        await begin(api<NativeAuthorizationSession>('/internal/v1/oauth/kimi/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, ...proxy, ...target }) }));
      } else if (flow === 'cursor_pkce' && selectedProvider.source === 'builtin') {
        await begin(api<NativeAuthorizationSession>('/internal/v1/oauth/cursor/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_driver: selectedProvider.id, provider_config: existing?.config ?? { base_url: 'https://api2.cursor.sh', network_scope: 'public' }, ...proxy, ...target }) }));
      } else {
        await begin(api<NativeAuthorizationSession>('/internal/v1/oauth/provider-adapter/start', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: tenant, account_name: name, provider_driver: selectedProvider.id, provider_config: existing?.config ?? providerConfig, ...proxy, ...target }) }));
      }
      if (scopeVersion.current !== attempt) return;
      setProxyUrl(''); setMessage(''); setError('');
    } catch (reason) { if (scopeVersion.current === attempt) setError(reason instanceof TypeError || reason instanceof ApiError && [502, 503, 504].includes(reason.status) ? journeyCopy.startTransportFailed : messageOf(reason, t('common.requestFailed'))); }
    finally { if (scopeVersion.current === attempt) setAuthorizing(false); }
  };
  const reloadList = async () => {
    if (listLoading) return;
    setListLoading(true);
    const attempt = scopeVersion.current;
    try { await onChanged(savedAccount.current); if (scopeVersion.current === attempt) { setListRetry(false); setError(''); } }
    catch { if (scopeVersion.current === attempt) { setListRetry(true); setError(t('providers.savedListUnavailable')); } }
    finally { if (scopeVersion.current === attempt) setListLoading(false); }
  };
  const poll = async () => {
    if (!session || pollLock.current || polling || pollStopped || expired || Date.now() < nextPollAt || (!session.session_id && !session.resumed && session.expires_at !== undefined && Date.now() >= session.expires_at)) return;
    if (!selectedProvider) return;
    const flow = selectedProvider.oauth_adapter?.flow_kind;
    const path = flow === 'openai_device' ? '/internal/v1/oauth/codex/poll'
      : flow === 'github_device_copilot' ? '/internal/v1/oauth/copilot/poll'
      : isKimi ? '/internal/v1/oauth/kimi/poll'
      : flow === 'cursor_pkce' && selectedProvider.source === 'builtin' ? '/internal/v1/oauth/cursor/poll'
      : '/internal/v1/oauth/provider-adapter/poll';
    pollLock.current = true;
    const request = new AbortController(); pollRequest.current = request;
    setPolling(true); setError('');
    const attempt = scopeVersion.current;
    try {
      const result = await api<UpstreamAccount | { status: string; message?: string; retry_after_seconds?: number }>(path, token, { method: 'POST', body: JSON.stringify(session.session_id ? { session_id: session.session_id } : { session_token: session.session_token }), signal: AbortSignal.any([request.signal, AbortSignal.timeout(30_000)]) });
      if (scopeVersion.current !== attempt) return;
      if ('id' in result) { savedAccount.current = result; clearDeviceLoginRecovery(session.session_id); setMessage(t(existing ? 'providers.reauthorized' : 'providers.ready', { name: result.name })); setSession(undefined); await reloadList(); }
      else {
        setNextPollAt(Date.now() + Math.max(1, result.retry_after_seconds ?? session.poll_after_seconds ?? 5) * 1000); setNow(Date.now());
        setMessage(journeyCopy.automatic);
      }
    } catch (reason) { if (scopeVersion.current === attempt) {
      const stopped = reason instanceof ApiError && [400, 401, 403, 404, 409, 410, 422].includes(reason.status);
      setPollStopped(stopped); setError(stopped ? journeyCopy.stopped : reason instanceof TypeError || reason instanceof ApiError && [502, 503, 504].includes(reason.status) ? journeyCopy.transportRetrying : journeyCopy.retrying);
      if (stopped) {
        clearDeviceLoginRecovery(session.session_id);
        if (session.expires_at && Date.now() >= session.expires_at) { setSession({ ...session, resumed: false }); setError(t('providers.deviceLoginExpired')); }
      }
      setNextPollAt(Date.now() + 15_000);
    } }
    finally { if (scopeVersion.current === attempt) { pollLock.current = false; setPolling(false); } }
  };
  const pollAction = useRef(poll);
  pollAction.current = poll;
  useEffect(() => {
    if (!session || polling || pollStopped || expired || selectedProvider?.oauth_adapter?.flow_kind === 'claude_manual_pkce') return;
    const timer = window.setTimeout(() => void pollAction.current(), Math.max(0, nextPollAt - Date.now()));
    return () => window.clearTimeout(timer);
  }, [session, polling, pollStopped, expired, nextPollAt, selectedProvider?.oauth_adapter?.flow_kind]);
  const complete = async (automatic = false) => {
    if (!active || !session || !isClaude || pollLock.current || pollStopped || !manualCode.includes('#') || (!automatic && claudePending)) return;
    if (automatic && claudeCompletion.current?.phase !== 'pending') return;
    const budget = claudeCompletion.current ??= { deadline: Date.now() + claudeCompletionLimits.durationMillis, attempts: 0, nextAt: 0, code: manualCode, phase: 'unknown' };
    const stopped = claudeCompletionStopReason(session.expires_at, budget.deadline, budget.attempts, Date.now());
    if (stopped) { budget.phase = 'unknown'; setClaudePending(false); setPollStopped(true); setMessage(''); setError(journeyCopy[stopped]); return; }
    if (Date.now() < budget.nextAt) return;
    if (!automatic) budget.code = manualCode;
    budget.phase = 'dispatched'; setClaudePending(false);
    budget.attempts += 1;
    pollLock.current = true;
    const request = new AbortController(); pollRequest.current = request;
    const attempt = scopeVersion.current;
    const deadline = Math.min(budget.deadline, Number.isFinite(session.expires_at) ? session.expires_at! : Infinity);
    const timer = window.setTimeout(() => request.abort(), Math.min(claudeCompletionLimits.requestMillis, deadline - Date.now()));
    setPolling(true); setError(''); setMessage('');
    try {
      const response = await new Promise<unknown>((resolve, reject) => {
        request.signal.addEventListener('abort', () => reject(new Error('Completion stopped')), { once: true });
        void api<unknown>('/internal/v1/oauth/claude/complete', token, { method: 'POST', body: JSON.stringify({ session_token: session.session_token, authorization_code: budget.code }), signal: request.signal }).then(resolve, reject);
      });
      if (scopeVersion.current !== attempt || request.signal.aborted) return;
      const elapsed = claudeCompletionStopReason(session.expires_at, budget.deadline, 0, Date.now());
      if (elapsed) { budget.phase = 'unknown'; setClaudePending(false); setPollStopped(true); setError(journeyCopy[elapsed]); return; }
      const result = parseClaudeCompletion(response, tenant, existing?.id);
      if (!('id' in result)) {
        budget.phase = 'pending';
        budget.nextAt = Date.now() + claudeCompletionRetryMillis(result.retry_after_seconds);
        setNextPollAt(budget.nextAt); setClaudePending(true);
        return;
      }
      setMessage(t(existing ? 'providers.reauthorized' : 'providers.ready', { name: result.name }));
      savedAccount.current = result; claudeCompletion.current = undefined; setClaudePending(false); setSession(undefined); setManualCode(''); await reloadList();
    } catch (reason) { if (scopeVersion.current === attempt) {
      budget.phase = 'unknown';
      const elapsed = claudeCompletionStopReason(session.expires_at, budget.deadline, 0, Date.now());
      setClaudePending(false); setPollStopped(Boolean(elapsed)); setError(journeyCopy[elapsed ?? authorizationCompleteError(reason)]);
    } }
    finally { window.clearTimeout(timer); if (scopeVersion.current === attempt) { pollLock.current = false; setPolling(false); } }
  };
  const completeAction = useRef(complete);
  completeAction.current = complete;
  useEffect(() => {
    if (!active || !isClaude || !session || !claudePending || polling || pollStopped || claudeCompletion.current?.phase !== 'pending') return;
    const budget = claudeCompletion.current;
    const due = budget.attempts >= claudeCompletionLimits.attempts ? Date.now() : Math.min(budget.nextAt, budget.deadline, Number.isFinite(session.expires_at) ? session.expires_at! : Infinity);
    const attempt = scopeVersion.current;
    const timer = window.setTimeout(() => { if (scopeVersion.current === attempt) void completeAction.current(true); }, Math.max(0, due - Date.now()));
    return () => window.clearTimeout(timer);
  }, [active, isClaude, session, claudePending, polling, pollStopped, nextPollAt]);
  return <div className="authorization-form"><p className="muted">{existing ? t('providers.oauthSecurity') : journeyCopy.setup}</p>
    {journeyCopy.identityHelp && !isKimi && existing && <p>{journeyCopy.identityHelp}</p>}
    {error && <div className="notice error" role="alert">{error}</div>}
    {existing && <>
      <p>{selectedProvider?.display_name ?? existing.driver} · {t('providers.oauth')}</p>
      <ProviderAccountIdentity account={existing} editing />
      <p>{journeyCopy.network}</p>
      <UpstreamConnection key={`${existing.id}-${existing.credential_generation}`} account={existing} token={token} tenant={tenant} readOnOpen
        disabled={authorizing || Boolean(session) || nativeLocked || listLoading} onChanged={onConnectionChanged} onSaved={onAccountSaved} onEditingChange={setConnectionEditing} />
      {connectionEditing && <p role="status">{journeyCopy.proxyEditing}</p>}
    </>}
    {oauthProviders.length === 0 ? <div className="empty">{t('providers.noAdapter')}</div> : <>
    <ModelPicker label={t('providers.provider')} disabled={Boolean(existing) || authorizing || polling || listLoading || Boolean(session) || nativeLocked || listRetry || Boolean(savedAccount.current)} value={providerChoice} onChange={(next) => { setProviderChoice(next); setName(oauthProviders.find(value => value.id === next)?.display_name ?? ''); reset(); }} groupBy="none" popupLabel={t('providers.directory')} searchPlaceholder={t('providers.searchDirectory')} searchAriaLabel={t('providers.searchDirectory')} emptyText={t('providers.directoryEmpty')} options={oauthProviders.map(value => ({ key: value.id, value: value.id, label: value.display_name, provider: value.display_name, upstream: '', capabilities: value.protocols }))} />
    {selectedProvider?.oauth_adapter?.flow_kind === 'authorization_code_pkce' ? <AuthorizationCodeConnection key={`${token}\0${tenant}\0${selectedProvider.id}`} token={token} tenant={tenant} provider={selectedProvider} existing={existing} connectionEditing={connectionEditing} onChanged={onChanged} onLock={setNativeLocked} onEditName={onEditName} /> : <>
    {existing ? <FormSection title={providerConnectionCopy(locale).displayName}><p>{existing.name}</p>{onEditName && <Button appearance="secondary" type="button" disabled={connectionEditing || authorizing || polling || listLoading || Boolean(session) || nativeLocked || listRetry || Boolean(savedAccount.current)} onClick={() => { if (!active || connectionEditing || authorizing || polling || pollLock.current || listLoading || session || nativeLocked || listRetry || savedAccount.current) return; onEditName(); }}>{providerConnectionCopy(locale).editDisplayName}</Button>}</FormSection> : <FormSection title={t('connection.identitySection')}><label>{t('providers.connectionName')} · {t('connection.required')}<Input required maxLength={200} disabled={authorizing || polling || listLoading || Boolean(session)} value={name} onChange={(event) => setName(event.target.value)} /></label></FormSection>}
    {proxyMode !== 'none' && !existing && !session && <FormSection title={t('connection.title')}>
      {!existing && <Checkbox checked={useProxy} disabled={authorizing} label={t('connection.useAccountProxy')} onChange={(_, data) => setUseProxy(data.checked === true)} />}
      {needsProxy && <ProxyInput required value={proxyUrl} onChange={setProxyUrl} disabled={authorizing} hint={t('connection.oauthProxyHint')} />}
      {!existing && !useProxy && <p className="field-hint">{t('connection.directEgress')}</p>}
    </FormSection>}
    {selectedProvider && selectedProvider.source !== 'builtin' && !session ? <Form key={selectedProvider.id} schema={providerConfigSchema(localizeSchema(connectionSchema(selectedProvider.config_schema as RJSFSchema, t('connection.endpointHint')), locale), locale)} formData={existing?.config} readonly={Boolean(existing)} formContext={{ providerConfigRoot: true }} validator={validator} templates={upstreamFormTemplates} widgets={fluentFormWidgets} onSubmit={({ formData }) => void start(formData)}><Button appearance="primary" type="submit" disabled={!tenant || authorizing || connectionEditing || listRetry || Boolean(savedAccount.current) || !proxyValid}>{t('common.startLogin')}</Button></Form> : <div className="button-row">
      <Button appearance="primary" type="button" onClick={() => void start()} disabled={!tenant || !name.trim() || connectionEditing || authorizing || polling || listLoading || Boolean(session) || listRetry || Boolean(savedAccount.current) || !proxyValid}>{authorizing ? t('common.loading') : isKimi && existing ? providerConnectionCopy(locale).signInAgain : t('common.startLogin')}</Button>
      {session && !expired && <>
        <OAuthLoginLinkActions url={session.verification_url ?? session.login_url} />
        {selectedProvider?.oauth_adapter?.flow_kind !== 'claude_manual_pkce' && !pollStopped && <p role="status">{polling ? journeyCopy.checking : journeyCopy.automatic}</p>}
      </>}
      {(expired || pollStopped) && <Button appearance="secondary" type="button" disabled={polling} onClick={reset}>{t('providers.backToLoginSetup')}</Button>}
      {listRetry && <Button appearance="secondary" type="button" disabled={listLoading} onClick={() => void reloadList()}>{t('providers.reloadAccountList')}</Button>}
    </div>}
    {session && isClaude && <>
      {expired ? error !== journeyCopy.completeExpired && <p role="status">{journeyCopy.completeExpired}</p> : (polling || claudePending) && <p role="status">{polling ? journeyCopy.completeChecking : journeyCopy.completePending}</p>}
      <div className="manual-authorization"><label>{t('providers.manualCode')}<Input value={manualCode} disabled={polling || claudePending || expired || pollStopped} onChange={(event) => setManualCode(event.target.value)} placeholder={t('providers.manualCodeHint')} /></label><Button appearance="primary" type="button" disabled={polling || claudePending || expired || pollStopped || !manualCode.includes('#')} onClick={() => void complete()}>{t('providers.completeAuthorization')}</Button></div>
    </>}
    {session?.user_code && !expired && <div className="device-authorization"><p>{selectedProvider?.oauth_adapter?.flow_kind === 'openai_device' ? t('providers.codexSecurity') : t('providers.deviceSecurity', { provider: selectedProvider?.display_name ?? '' })}</p><DeviceAuthorizationCode key={session.user_code} value={session.user_code} /></div>}
    {isKimi && session && selectedProvider && <p role="status">{expired
      ? t('providers.deviceLoginExpired')
      : t('providers.deviceLoginHint', { provider: selectedProvider.display_name }) + (session.expires_at ? ` ${t('providers.deviceLoginValidUntil', { time: new Date(session.expires_at).toLocaleTimeString(locale) })}` : '')}</p>}
    {message && !expired && <div className={session ? 'notice' : 'notice success'} role="status">{message}</div>}
    </>}
    </>}
  </div>;
}

function Pricing({ token, tenant, writeTenant = tenant, schemas, schemasLoading = false, onRequestSchemas }: {
  token: string;
  tenant: string;
  writeTenant?: string;
  schemas?: ConfigurationSchemas;
  schemasLoading?: boolean;
  onRequestSchemas?: () => void;
}) {
  const { locale, t } = useI18n();
  const [prices, setPrices] = useState<ModelPriceView[]>([]);
  const [generationPrices, setGenerationPrices] = useState<GenerationPriceView[]>([]);
  const [usage, setUsage] = useState<ModelPriceUsageSummary>({ models: [] });
  const [syncResult, setSyncResult] = useState<ModelPriceSyncResult>();
  const [syncing, setSyncing] = useState(false);
  const [error, setError] = useState('');
  const [kind, setKind] = useState<'token' | 'generation'>('token');
  const [model, setModel] = useState('');
  const [currency, setCurrency] = useState('USD');
  const [displayCurrency, setDisplayCurrency] = useState('USD');
  const [loadedCurrency, setLoadedCurrency] = useState('');
  const [pricingLoading, setPricingLoading] = useState(false);
  const [priceCatalogFailed, setPriceCatalogFailed] = useState(false);
  const [usageFailed, setUsageFailed] = useState(false);
  const [usageLoading, setUsageLoading] = useState(false);
  const [editorOpen, setEditorOpen] = useState(false);
  const [savingPrice, setSavingPrice] = useState(false);
  const priceSaveLock = useRef(false);
  const [basePricingScope, setBasePricingScope] = useState('');
  const [message, setMessage] = useState('');
  const loadSequence = useRef(0);
  const priceRequest = useRef<AbortController | undefined>(undefined);
  const syncSequence = useRef(0);
  const scopeRef = useRef({ token, tenant, writeTenant, displayCurrency });
  scopeRef.current = { token, tenant, writeTenant, displayCurrency };
  const load = async (requestedCurrency = displayCurrency) => {
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    const loadScope = `${loadToken}\0${loadTenant}`;
    if (!loadToken) return;
    priceRequest.current?.abort();
    const controller = new AbortController();
    priceRequest.current = controller;
    const current = () => !controller.signal.aborted && sequence === loadSequence.current
      && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant
      && scopeRef.current.displayCurrency === requestedCurrency;
    // Usage is invariant for a tenant/token scope. Start its independent read
    // with the first price page rather than holding it behind a long catalog
    // walk (or re-reading it for every currency switch).
    setBasePricingScope(loadScope);
    setPricingLoading(true); setPriceCatalogFailed(false); setPrices([]); setGenerationPrices([]);
    // Publish each independent price table as soon as it arrives. Usage does
    // not vary by currency and must never gate price rendering.
    const results = await Promise.allSettled([
      loadModelPricePages(
        requestedCurrency,
        controller.signal,
        (path, pageSignal) => api<ModelPriceView[]>(path, loadToken, { signal: AbortSignal.any([pageSignal, AbortSignal.timeout(10_000)]) }),
        (value) => { if (current()) { setPrices(value); setLoadedCurrency(requestedCurrency); } },
      ),
      api<GenerationPriceView[]>(`/internal/v1/generation-prices?currency=${encodeURIComponent(requestedCurrency)}`, loadToken, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]) })
        .then((value) => { if (current()) setGenerationPrices(value); }),
    ]);
    if (!current()) return;
    setLoadedCurrency(requestedCurrency); setPricingLoading(false);
    setPriceCatalogFailed(results[0].status === 'rejected');
    const failures = results.filter((result) => result.status === 'rejected');
    setError(failures.length ? t('pricing.partialLoad', { count: formatNumber(failures.length, locale) }) : '');
  };
  useEffect(() => {
    loadSequence.current += 1;
    syncSequence.current += 1;
    setPrices([]); setGenerationPrices([]); setSyncResult(undefined); setLoadedCurrency('');
    setPricingLoading(false); setSyncing(false); setError(''); setMessage(''); setKind('token'); setModel('');
    setUsageLoading(false); setBasePricingScope('');
  }, [token, tenant, writeTenant]);
  useEffect(() => {
    void load(displayCurrency);
    return () => { priceRequest.current?.abort(); loadSequence.current += 1; };
  }, [token, tenant, writeTenant, displayCurrency]);
  useEffect(() => {
    const controller = new AbortController();
    const scope = `${token}\0${tenant}`;
    setUsage({ models: [] }); setUsageFailed(false); setUsageLoading(false);
    if (token && basePricingScope === scope) {
      setUsageLoading(true);
      void api<ModelPriceUsageSummary>(`/internal/v1/model-prices/usage-summary${queryForTenant(tenant)}`, token, { signal: AbortSignal.any([controller.signal, AbortSignal.timeout(10_000)]) })
      .then((value) => { if (!controller.signal.aborted) setUsage(value); })
      .catch(() => { if (!controller.signal.aborted) setUsageFailed(true); })
      .finally(() => { if (!controller.signal.aborted) setUsageLoading(false); });
    }
    return () => controller.abort();
  }, [token, tenant, basePricingScope]);
  const renderCurrency = loadedCurrency || displayCurrency;
  const rows = useMemo(() => {
    const usageByModel = new Map(usage.models.map((value) => [value.model, value]));
    const pricesByModel = new Map(prices.map((value) => [value.model, value]));
    return Array.from(new Set([...usageByModel.keys(), ...pricesByModel.keys()])).sort().flatMap((name) => {
      const price = pricesByModel.get(name);
      const tiers = price?.tiers?.length ? price.tiers : price ? [{ service_tier: 'default', input_per_million: price.input_per_million, cached_input_per_million: price.input_per_million, cache_write_per_million: price.input_per_million, output_per_million: price.output_per_million, source: price.source, updated_at: price.updated_at, cache_price_estimated: true }] : [undefined];
      return tiers.map((tier) => ({ model: name, usage: usageByModel.get(name), tier }));
    });
  }, [prices, usage]);
  const schema = kind === 'generation' ? schemas?.generation_price : schemas?.model_price;
  const pricingSchema = useMemo(() => {
    if (!schema) return undefined;
    const localized = localizeSchema(schema as RJSFSchema, locale);
    const tier = localized.properties?.service_tier;
    if (tier && typeof tier === 'object' && Array.isArray(tier.enum)) {
      tier.oneOf = tier.enum.map(value => ({ const: value, title: value === 'default' ? t('pricing.tierDefault') : value === 'priority' ? t('pricing.tierPriority') : value === 'flex' ? t('pricing.tierFlex') : String(value) }));
      delete tier.enum;
    }
    if (locale === 'zh-CN') for (const [field, key] of Object.entries({ input_per_million: 'pricing.input', output_per_million: 'pricing.output', cached_input_per_million: 'pricing.cachedInput', cache_write_per_million: 'pricing.cacheWrite' })) {
      const definition = localized.properties?.[field];
      if (definition && typeof definition === 'object') definition.title = t(key);
    }
    return localized;
  }, [schema, locale, t]);
  // Prices/usage do not carry provider or account attribution. Offer known
  // models without fabricating that metadata or issuing editor-only reads.
  const modelOptions = useMemo(() => Array.from(new Set(kind === 'generation'
    ? generationPrices.map((price) => price.model)
    : [...prices.map((price) => price.model), ...usage.models.map((item) => item.model)]
  )).sort(), [kind, generationPrices, prices, usage]);
  const sync = async () => {
    if (!writeTenant) return;
    const syncToken = token; const syncTenant = tenant; const syncWriteTenant = writeTenant; const syncCurrency = displayCurrency;
    const sequence = ++syncSequence.current;
    // A pre-sync read must not overwrite newly synchronized prices.
    priceRequest.current?.abort(); loadSequence.current += 1; setPricingLoading(false);
    setSyncing(true); setError(''); setMessage('');
    try {
      const result = await api<ModelPriceSyncResult>('/internal/v1/model-prices/sync', syncToken, { method: 'POST', body: JSON.stringify({ models: usage.models.map((value) => value.model), currency: displayCurrency, tenant_external_id: syncWriteTenant }) });
      if (sequence !== syncSequence.current || scopeRef.current.token !== syncToken || scopeRef.current.tenant !== syncTenant || scopeRef.current.writeTenant !== syncWriteTenant || scopeRef.current.displayCurrency !== syncCurrency) return;
      setSyncResult(result); setPrices(result.prices); setPriceCatalogFailed(false); setLoadedCurrency(syncCurrency); setMessage(t('pricing.synced', { count: formatNumber(result.imported, locale) }));
    } catch (reason) { if (sequence === syncSequence.current && scopeRef.current.token === syncToken && scopeRef.current.tenant === syncTenant && scopeRef.current.writeTenant === syncWriteTenant && scopeRef.current.displayCurrency === syncCurrency) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (sequence === syncSequence.current && scopeRef.current.token === syncToken && scopeRef.current.tenant === syncTenant && scopeRef.current.writeTenant === syncWriteTenant && scopeRef.current.displayCurrency === syncCurrency) setSyncing(false); }
  };
  return <div className="pricing-page"><WriteScopeNotice tenant={writeTenant} />
    <LoadingProgress active={pricingLoading || usageLoading || schemasLoading} label={t('common.loading')} level="page" />
    {usageFailed && <div className="notice error" role="alert">{t('pricing.usageUnavailable')}</div>}
    <article className="panel pricing-overview"><div className="panel-title"><div><h2>{t('pricing.title')}</h2><p className="muted">{t('pricing.description')}</p></div><div className="pricing-heading-actions"><label>{t('pricing.viewCurrency')}<select aria-label={t('pricing.viewCurrency')} value={displayCurrency} onChange={(event) => { const next = event.target.value; syncSequence.current += 1; setSyncing(false); setSyncResult(undefined); setMessage(''); setDisplayCurrency(next); setCurrency(next); }}><option value="USD">USD</option><option value="CNY">CNY</option></select></label><DetailTooltip content={t('pricing.syncHint')}><span><Button appearance="secondary" type="button" onClick={() => void sync()} disabled={!writeTenant || syncing || usageLoading || usageFailed}>{syncing ? t('pricing.syncing') : t('pricing.sync')}</Button></span></DetailTooltip></div></div>
      <div className="pricing-summary">{usageLoading && usage.models.length === 0 ? <LoadingState label={t('pricing.usageLoading')} variant="inline" /> : <span>{usageFailed ? t('pricing.usageUnavailable') : t('pricing.usedModels', { count: formatNumber(usage.models.length, locale) })}</span>}<span>{t('pricing.saved', { count: formatNumber(prices.length, locale) })}</span><span>{t('pricing.sourceOrder')}: models.dev → LiteLLM → OpenRouter</span></div>
      <CreateJourney className="manual-pricing" title={t('pricing.manual')} description={t('pricing.manualHint')} open={editorOpen} onOpenChange={setEditorOpen} onOpen={() => onRequestSchemas?.()} busy={savingPrice}><div className="manual-pricing-body">
        {error && <div className="notice error" role="alert">{error}</div>}
        <label>{t('pricing.type')}<select value={kind} onChange={(event) => setKind(event.target.value as typeof kind)}><option value="token">{t('pricing.tokenModel')}</option><option value="generation">{t('pricing.generationModel')}</option></select></label><label>{t('pricing.model')}<Combobox aria-label={t('pricing.model')} freeform value={model} onChange={event => setModel(event.target.value)} onOptionSelect={(_, data) => { if (data.optionValue) setModel(data.optionValue); }} disabled={savingPrice}>{modelOptions.filter(name => name.toLocaleLowerCase(locale).includes(model.toLocaleLowerCase(locale))).map(name => <Option key={name} value={name}>{name}</Option>)}</Combobox></label><label>{t('pricing.currency')}<select value={currency} onChange={(event) => setCurrency(event.target.value)}><option value="USD">USD</option><option value="CNY">CNY</option></select></label>{pricingSchema ? <Form key={`${kind}-${locale}`} schema={pricingSchema} validator={validator} templates={schemaFormTemplates} widgets={fluentFormWidgets} onSubmit={async ({ formData }) => { if (!writeTenant || priceSaveLock.current) return; priceSaveLock.current = true; setSavingPrice(true); setError(''); const current = () => scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant; try { const prefix = kind === 'generation' ? 'generation-prices' : 'prices'; await api(`/internal/v1/${prefix}/${encodeURIComponent(currency)}/${encodeURIComponent(model)}`, token, { method: 'POST', body: JSON.stringify(formData) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return; setMessage(t('pricing.savedMessage')); if (currency === displayCurrency) await load(currency); else setDisplayCurrency(currency); } catch (reason) { if (current()) setError(messageOf(reason, t('common.requestFailed'))); } finally { priceSaveLock.current = false; if (current()) setSavingPrice(false); } }}><Button appearance="primary" type="submit" disabled={!writeTenant || !model.trim() || savingPrice}>{savingPrice ? t('common.loading') : t('pricing.save')}</Button></Form> : schemasLoading ? <LoadingState label={t('common.loading')} variant="detail" /> : <div className="empty">{t('providers.schemaMissing')}</div>}</div></CreateJourney>
      {error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}
      {syncResult && <><div className="source-status">{syncResult.sourceResults.map((source) => <div className={`source-card ${source.error ? 'failed' : 'healthy'}`} key={source.source}><b>{source.source}</b><span>{source.error ? t('pricing.sourceFailed') : t('pricing.sourceHealthy', { count: formatNumber(source.models, locale) })}</span>{source.error && <small>{source.error}</small>}</div>)}</div><div className="notice success"><b>{t('pricing.result')}</b> · {t('pricing.imported', { count: formatNumber(syncResult.imported, locale) })} · {t('pricing.candidates', { count: formatNumber(syncResult.candidates.length, locale) })} · {t('pricing.unmatched', { count: formatNumber(syncResult.unmatched.length, locale) })} · {t('pricing.preserved', { count: formatNumber(syncResult.preserved.length, locale) })}</div>
        {(syncResult.candidates.length > 0 || syncResult.unmatched.length > 0) && <div className="sync-details"><h3>{t('pricing.candidateDetails')}</h3>{syncResult.candidates.map((candidate) => <Disclosure key={candidate.model} title={`${candidate.model} · ${t('pricing.candidateCount', { count: formatNumber(candidate.candidates.length, locale) })}`}><div className="candidate-list">{candidate.candidates.map((match) => <div key={`${match.source}-${match.sourceModelId}-${match.serviceTier}`}><b>{match.sourceModelId}</b><span>{match.source} · {match.serviceTier} · {match.reason}</span><code>{t('pricing.input')}: {formatCurrency(match.inputPerMillion, renderCurrency, locale)} · {t('pricing.output')}: {formatCurrency(match.outputPerMillion, renderCurrency, locale)}</code></div>)}</div></Disclosure>)}{syncResult.unmatched.length > 0 && <Disclosure title={t('pricing.unmatchedModels')}><div className="model-name-list">{syncResult.unmatched.map((name) => <code key={name}>{name}</code>)}</div></Disclosure>}</div>}
      </>}
      <PricingTable rows={rows} currency={renderCurrency} loading={pricingLoading} catalogFailed={priceCatalogFailed} usageLoading={usageLoading} usageFailed={usageFailed} />
    </article>
    <article className="panel"><div className="panel-title"><h2>{t('pricing.generationPrices')}</h2><span>{formatNumber(generationPrices.length, locale)}</span></div><div className="table-scroll"><table><thead><tr><th>{t('pricing.model')}</th><th>{t('pricing.currency')}</th><th>{t('self.units')}</th><th>{t('pricing.unitPrice')}</th></tr></thead><tbody>{generationPrices.map((price) => <tr key={`${price.currency}-${price.model}`}><td><code>{price.model}</code></td><td>{price.currency}</td><td>{enumLabel(t, 'billingUnit', price.billing_unit)}</td><td>{formatCurrency(price.price_per_unit, price.currency, locale)}</td></tr>)}</tbody></table>{pricingLoading && generationPrices.length === 0 && <LoadingState label={t('common.loading')} variant="list" />}{!pricingLoading && generationPrices.length === 0 && <div className="empty">{t('pricing.noGenerationPrices')}</div>}</div></article>

  </div>;
}

interface RouteDraft extends Pick<ModelRouteView, 'public_model' | 'upstream_model' | 'protocol' | 'priority'> {
  upstream_account_id: string;
  upstream_account_ids: string[];
  included_provider_group_ids: string[];
  excluded_provider_group_ids: string[];
  route_group_ids: string[];
  route_group_names: string[];
  granted_credential_ids: string[];
  custom_model_confirmed: boolean;
}
const emptyRouteDraft: RouteDraft = {
  public_model: '', upstream_account_id: '', upstream_account_ids: [], upstream_model: '', protocol: 'openai', priority: 0,
  included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [], route_group_names: [], granted_credential_ids: [], custom_model_confirmed: false,
};

function selections(ids: string[], options: ComboboxOption[]) {
  return ids.map((id) => options.find((option) => option.value === id) ?? { value: id, label: id });
}

function ExactCredentialCombobox({ token, tenant, credentials, value, onChange, onSelectionLabels }: {
  token: string;
  tenant: string;
  credentials: KeyView[];
  value: string[];
  onChange: (ids: string[]) => void;
  onSelectionLabels?: (labels: Record<string, string>) => void;
}) {
  const { t } = useI18n();
  const [resolved, setResolved] = useState<KeyView[]>([]);
  const [search, setSearch] = useState({ query: '', loading: false, error: '', resultId: '' });
  const sequence = useRef(0);
  const request = useRef<AbortController | undefined>(undefined);
  const scope = useRef({ token, tenant });
  const committedScope = useRef({ token, tenant });
  scope.current = { token, tenant };
  const credentialOptions = [
    ...credentials,
    ...resolved.filter((match) => !credentials.some((credential) => credential.key_id === match.key_id)),
  ].map((credential) => ({ value: credential.key_id, label: credential.alias, description: credential.key_id }));

  const searchCredential = (query: string) => {
    request.current?.abort();
    const currentSequence = ++sequence.current;
    const keyId = query.trim();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(keyId) || !token || !tenant) {
      setSearch({ query: '', loading: false, error: '', resultId: '' });
      return;
    }
    const controller = new AbortController();
    request.current = controller;
    const searchScope = { token, tenant };
    const queryParameters = new URLSearchParams({ tenant_external_id: tenant, key_id: keyId, limit: '1' });
    setSearch({ query: keyId, loading: true, error: '', resultId: '' });
    void apiRead<KeyView[]>(`/internal/v1/keys?${queryParameters}`, token, {
      // Exact lookup has no hidden automatic retry. The explicit bound is
      // configurable per read, query/scope changes cancel it, and a visible
      // retry remains under operator control.
      attempts: 1,
      attemptTimeoutMilliseconds: 15_000,
      signal: controller.signal,
    }).then((matches) => {
      if (controller.signal.aborted || currentSequence !== sequence.current
        || scope.current.token !== searchScope.token || scope.current.tenant !== searchScope.tenant) return;
      setResolved((current) => [
        ...matches,
        ...current.filter((credential) => value.includes(credential.key_id)
          && !matches.some((match) => match.key_id === credential.key_id)),
      ]);
      setSearch({ query: keyId, loading: false, error: '', resultId: matches[0]?.key_id ?? '' });
    }).catch((reason) => {
      if (controller.signal.aborted || currentSequence !== sequence.current
        || scope.current.token !== searchScope.token || scope.current.tenant !== searchScope.tenant) return;
      setSearch({ query: keyId, loading: false, error: messageOf(reason, t('routes.credentialSearchFailed')), resultId: '' });
    });
  };

  useLayoutEffect(() => {
    if (committedScope.current.token === token && committedScope.current.tenant === tenant) return;
    committedScope.current = { token, tenant };
    request.current?.abort();
    sequence.current += 1;
    setResolved([]);
    setSearch({ query: '', loading: false, error: '', resultId: '' });
  }, [token, tenant]);
  useLayoutEffect(() => () => {
    request.current?.abort();
    sequence.current += 1;
  }, []);
  useEffect(() => {
    setResolved((current) => current.filter((credential) => value.includes(credential.key_id) || credential.key_id === search.resultId));
  }, [value, search.resultId]);

  return <MultiCombobox label={t('routes.exactCredentials')} options={credentialOptions} value={selections(value, credentialOptions)} onChange={(selected) => { onSelectionLabels?.(Object.fromEntries(selected.filter(item => credentialOptions.some(option => option.value === item.value)).map(item => [item.value, item.label]))); onChange(selected.map((item) => item.value)); }} placeholder={t('routes.searchCredentials')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.exactCredentialsHint')} onQueryChange={searchCredential} loading={search.loading} loadingText={t('routes.credentialSearchLoading')} error={search.error} retryLabel={t('common.retry')} onRetry={() => searchCredential(search.query)} />;
}

function routeRequest(draft: RouteDraft, customModelConfirmed: boolean) {
  const { upstream_account_id: _legacyAccountId, ...request } = draft;
  return { ...request, custom_model_confirmed: customModelConfirmed };
}

function RouteFields({ token, tenant, draft, upstreams, providers, providerGroups, routeGroups, credentials, onChange, onCatalogValidity }: {
  token: string;
  tenant: string;
  draft: RouteDraft;
  upstreams: UpstreamAccount[];
  providers: ProviderType[];
  providerGroups: GroupView[];
  routeGroups: GroupView[];
  credentials: KeyView[];
  onChange: (draft: RouteDraft) => void;
  onCatalogValidity: (valid: boolean, allowCustom: boolean) => void;
}) {
  const { locale, t } = useI18n();
  const knownProtocols = managedRouteProtocols;
  const journey = formJourneyCopy(locale);
  const priorityHintId = useId();
  const protocolId = useId();
  const labelScope = JSON.stringify([token, tenant]);
  const [credentialLabels, setCredentialLabels] = useState<{ scope: string; labels: Record<string, string> }>();
  const zh = locale.startsWith('zh');
  const includedAccountIds = providerGroups.filter((group) => draft.included_provider_group_ids.includes(group.id)).flatMap((group) => group.member_ids);
  const excludedAccountIds = new Set(providerGroups.filter((group) => draft.excluded_provider_group_ids.includes(group.id)).flatMap((group) => group.member_ids));
  const candidateIds = [...new Set([...draft.upstream_account_ids, ...includedAccountIds])].filter((id) => !excludedAccountIds.has(id));
  const candidateProtocolSets = candidateIds.map((id) => {
    const account = upstreams.find((value) => value.id === id);
    return providers.find((value) => value.id === account?.driver)?.protocols;
  });
  const supportedByAll = candidateIds.length === 0 || candidateProtocolSets.some((values) => !values)
    ? knownProtocols
    : knownProtocols.filter((protocol) => candidateProtocolSets.every((values) => values?.includes(protocol)));
  const selectedManagedProtocol = inferManagedRouteProtocol(draft.protocol);
  const protocolCompatible = selectedManagedProtocol !== undefined && supportedByAll.includes(selectedManagedProtocol);
  const singleProtocol = candidateIds.length > 0
    ? knownProtocols.filter((protocol) => candidateProtocolSets.every((values) => values && values.length > 0 && values.includes(protocol)))
    : [];
  const singleProtocolKey = singleProtocol.join(',');
  useEffect(() => {
    if (singleProtocol.length === 1 && draft.protocol !== singleProtocol[0]) onChange({ ...draft, protocol: singleProtocol[0] });
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [singleProtocolKey, draft.protocol]);
  const upstreamOptions = upstreams.map((value) => ({ value: value.id, label: value.name, description: providerDisplayName(value.driver, providers, locale), details: `${t('providers.provider')}: ${providerDisplayName(value.driver, providers, locale)}; ${zh ? '账号 ID' : 'Account ID'}: ${value.id}; Driver: ${value.driver}` }));
  const providerGroupOptions = providerGroups.map((value) => ({ value: value.id, label: value.name, description: t('groups.memberCount', { count: formatNumber(value.member_count, locale) }) }));
  const routeGroupOptions = routeGroups.map((value) => ({ value: value.id, label: value.name, description: t('groups.memberCount', { count: formatNumber(value.member_count, locale) }) }));
  const routeGroupValue = [
    ...selections(draft.route_group_ids, routeGroupOptions),
    ...draft.route_group_names.map((name) => ({ value: `new:${name}`, label: name, created: true })),
  ];
  const priorityValid = Number.isInteger(draft.priority) && Math.abs(draft.priority) <= 1000000;
  return <div className="route-form-sections form-journey">
    <FormSection title={t('routes.identitySection')} description={t('routes.identityHint')}>
    <label>{t('routes.publicModel')} · {t('connection.required')}<Input required value={draft.public_model} onChange={(_, data) => onChange({ ...draft, public_model: data.value })} /></label>
    </FormSection><FormSection title={t('routes.upstreamSection')}>
    <MultiCombobox label={t('routes.explicitUpstreams')} options={upstreamOptions} value={selections(draft.upstream_account_ids, upstreamOptions)} onChange={(selected) => {
      const upstream_account_ids = selected.map((item) => item.value);
      const upstream_account_id = upstream_account_ids[0] ?? '';
      onChange({ ...draft, upstream_account_id, upstream_account_ids });
    }} placeholder={t('routes.searchUpstreams')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.explicitUpstreamsHint')} />
    <AdvancedFormSection action title={journey.candidates} description={journey.candidatesHint}>
    <div className="route-group-grid">
      <MultiCombobox label={t('routes.includeProviderGroups')} options={providerGroupOptions} value={selections(draft.included_provider_group_ids, providerGroupOptions)} onChange={(selected) => onChange({ ...draft, included_provider_group_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchProviderGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} />
      <MultiCombobox label={t('routes.excludeProviderGroups')} options={providerGroupOptions} value={selections(draft.excluded_provider_group_ids, providerGroupOptions)} onChange={(selected) => onChange({ ...draft, excluded_provider_group_ids: selected.map((item) => item.value) })} placeholder={t('routes.searchProviderGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} hint={t('routes.exclusionWins')} />
    </div>
    </AdvancedFormSection>
    <div className="route-protocol-field"><div className="route-protocol-heading"><label htmlFor={protocolId}>{t('routes.protocol')}</label><DetailTooltip content={t('routes.protocolCompatibilityHint')}><Button appearance="subtle" type="button">{journey.protocolHelp}</Button></DetailTooltip></div><Select id={protocolId} aria-invalid={!protocolCompatible} value={draft.protocol} onChange={(event) => onChange({ ...draft, protocol: event.target.value })}>{knownProtocols.map((protocol) => <option disabled={candidateIds.length > 0 && !supportedByAll.includes(protocol)} key={protocol} value={protocol}>{protocol === 'generation' ? t('routes.generation') : protocol === 'openai-audio' ? 'OpenAI Audio' : protocol === 'anthropic' ? 'Anthropic' : 'OpenAI'}</option>)}</Select></div>
    {!protocolCompatible && <p className="field-error" role="alert">{t('routes.protocolIncompatible')}</p>}
    <UpstreamModelCombobox token={token} tenant={tenant} upstreams={upstreams} providers={providers} accountIds={draft.upstream_account_ids} includedProviderGroupIds={draft.included_provider_group_ids} excludedProviderGroupIds={draft.excluded_provider_group_ids} syncAccountIds={candidateIds} protocol={draft.protocol} value={draft.upstream_model} onChange={(upstream_model) => onChange({ ...draft, upstream_model, public_model: !draft.public_model.trim() || draft.public_model === draft.upstream_model ? upstream_model : draft.public_model, custom_model_confirmed: false })} onProtocolInferred={(protocol) => { const inferredProtocol = inferManagedRouteProtocol(protocol); if (inferredProtocol && draft.protocol !== inferredProtocol) onChange({ ...draft, protocol: inferredProtocol }); }} customModelConfirmed={draft.custom_model_confirmed} onValidityChange={(valid, allowCustom) => onCatalogValidity(valid && protocolCompatible, allowCustom)} />
    <AdvancedFormSection action title={journey.priority} invalid={!priorityValid}>
    <label>{t('routes.priority')}<Input type="number" required step={1} aria-invalid={!priorityValid} aria-describedby={priorityHintId} min={-1000000} max={1000000} value={Number.isNaN(draft.priority) ? '' : String(draft.priority)} onChange={(event) => onChange({ ...draft, priority: event.target.valueAsNumber })} /></label><small id={priorityHintId} className={priorityValid ? 'field-hint' : 'field-error'}>{t('routes.priorityHint')}</small>
    </AdvancedFormSection>
    </FormSection><AdvancedFormSection action title={journey.routeAccess} description={t('routes.accessHint')}>
    <MultiCombobox label={t('routes.routeGroups')} options={routeGroupOptions} value={routeGroupValue} onChange={(selected) => onChange({ ...draft, route_group_ids: selected.filter((item) => !item.created).map((item) => item.value), route_group_names: selected.filter((item) => item.created).map((item) => item.label) })} placeholder={t('routes.searchOrCreateRouteGroups')} emptyText={t('groups.noMatches')} removeLabel={(name) => t('groups.removeMember', { name })} allowCreate createLabel={(name) => t('routes.createRouteGroupNamed', { name })} hint={t('routes.routeGroupsHint')} />
    <AdvancedFormSection action title={t('routes.individualGrants', { count: draft.granted_credential_ids.length })} description={t('routes.individualGrantsHint')}>
      <ExactCredentialCombobox token={token} tenant={tenant} credentials={credentials} value={draft.granted_credential_ids} onChange={(granted_credential_ids) => onChange({ ...draft, granted_credential_ids })} onSelectionLabels={labels => setCredentialLabels({ scope: labelScope, labels })} />
    </AdvancedFormSection>
    </AdvancedFormSection>
    <section className="form-journey-preview" aria-label={journey.preview}>
      <h4>{journey.preview}</h4>
      <dl><div><dt>{t('routes.publicModel')}</dt><dd>{draft.public_model.trim() || journey.noModel}</dd></div>
        <div><dt>{t('routes.upstreamModel')}</dt><dd>{draft.upstream_model.trim() || journey.noModel}</dd></div>
        <div><dt>{t('routes.upstream')}</dt><dd>{candidateIds.length ? candidateIds.map(id => { const account = upstreams.find(value => value.id === id); return <DetailTooltip key={id} content={`${zh ? '账号 ID' : 'Account ID'}: ${id}`}><span tabIndex={0} className="route-scope-item">{account?.name ?? (zh ? '未知账号' : 'Unknown account')} · {providerDisplayName(account?.driver, providers, locale)}</span></DetailTooltip>; }) : journey.noUpstream}</dd></div>
        <div><dt>{t('routes.routeGroups')} ({formatNumber(routeGroupValue.length, locale)})</dt><dd>{routeGroupValue.length ? routeGroupValue.map(group => <DetailTooltip key={group.value} content={group.created ? (zh ? '保存时创建此路由组' : 'Create this route group on save') : `${zh ? '路由组 ID' : 'Route group ID'}: ${group.value}`}><span tabIndex={0} className="route-scope-item">{group.created ? `${group.label} (${zh ? '待创建' : 'New'})` : routeGroups.find(value => value.id === group.value)?.name ?? (zh ? '未知路由组' : 'Unknown route group')}</span></DetailTooltip>) : (zh ? '未加入路由组' : 'No route groups selected')}<p className="field-hint">{zh ? '已授权这些组的凭据可使用此路由；组成员变化会影响访问范围。' : 'Credentials granted these groups can use this route; changing group members changes access.'}</p></dd></div>
        <div><dt>{zh ? '直接授权凭据' : 'Direct credential grants'} ({formatNumber(draft.granted_credential_ids.length, locale)})</dt><dd>{draft.granted_credential_ids.length ? draft.granted_credential_ids.map(id => <DetailTooltip key={id} content={`${zh ? '凭据 ID' : 'Credential ID'}: ${id}`}><span tabIndex={0} className="route-scope-item">{credentials.find(value => value.key_id === id)?.alias ?? (credentialLabels?.scope === labelScope ? credentialLabels.labels[id] : undefined) ?? (zh ? '未取得凭据名称' : 'Credential name unavailable')}</span></DetailTooltip>) : (zh ? '无直接授权；路由组授权单独生效。' : 'No direct grants; route-group grants apply independently.')}</dd></div></dl>
    </section>
  </div>;
}

function RouteWorkspace({ token, tenant, writeTenant = tenant, upstreams, providers }: { token: string; tenant: string; writeTenant?: string; upstreams: UpstreamAccount[]; providers: ProviderType[] }) {
  const routePageSize = 100;
  const maximumFocusedRoutePages = 100;
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  const [credentials, setCredentials] = useState<KeyView[]>([]);
  const [credentialsRequested, setCredentialsRequested] = useState(false);
  const [credentialError, setCredentialError] = useState('');
  const providerGroups = useGroups('provider', token, writeTenant);
  const routeGroups = useGroups('route', token, writeTenant);
  const [form, setForm] = useState<RouteDraft>(emptyRouteDraft);
  const [formCatalog, setFormCatalog] = useState({ valid: false, allowCustom: false });
  const [editing, setEditing] = useState<ModelRouteView>();
  const [workspaceOpen, setWorkspaceOpen] = useState(false);
  const successNotice = useRef<HTMLDivElement>(null);
  const [editForm, setEditForm] = useState<RouteDraft>(emptyRouteDraft);
  const [editCatalog, setEditCatalog] = useState({ valid: false, allowCustom: false });
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const [focusRouteId, setFocusRouteId] = useState('');
  useLayoutEffect(() => { if (message && !workspaceOpen) successNotice.current?.focus(); }, [message, workspaceOpen]);
  const loadSequence = useRef(0);
  const loadAbort = useRef<AbortController | undefined>(undefined);
  const credentialLoadSequence = useRef(0);
  const credentialLoadAbort = useRef<AbortController | undefined>(undefined);
  const scopeRef = useRef({ token, tenant, writeTenant });
  scopeRef.current = { token, tenant, writeTenant };
  const upstreamsRef = useRef(upstreams);
  upstreamsRef.current = upstreams;
  const load = async (requestedFocusRouteId = '') => {
    loadAbort.current?.abort();
    const controller = new AbortController();
    loadAbort.current = controller;
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) { setRoutes([]); setCredentials([]); return; }
    try {
      // The table is the route page's primary content. Credential choices are
      // only used by an opened create/edit form, so never put their paged
      // read on this critical path.
      const nextRoutes: ModelRouteView[] = [];
      const visitedCursors = new Set<string>();
      let beforeCreatedAt: number | undefined;
      let beforeId: string | undefined;
      for (let pageIndex = 0; pageIndex < maximumFocusedRoutePages; pageIndex += 1) {
        const routeQuery = new URLSearchParams({ limit: String(routePageSize) });
        if (beforeCreatedAt !== undefined && beforeId) {
          routeQuery.set('before_created_at', String(beforeCreatedAt));
          routeQuery.set('before_id', beforeId);
        }
        const page = await apiRead<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(loadTenant, routeQuery.toString())}`, loadToken, { signal: controller.signal });
        nextRoutes.push(...page);
        if (!requestedFocusRouteId || page.some((route) => route.id === requestedFocusRouteId) || page.length < routePageSize) break;
        const last = page.at(-1);
        if (!last || !Number.isSafeInteger(last.created_at) || !last.id) throw new Error(t('providerCatalog.routesIncomplete'));
        const cursor = `${last.created_at}\u0000${last.id}`;
        if (visitedCursors.has(cursor)) throw new Error(t('providerCatalog.routesIncomplete'));
        visitedCursors.add(cursor);
        beforeCreatedAt = last.created_at;
        beforeId = last.id;
        if (pageIndex === maximumFocusedRoutePages - 1) throw new Error(t('providerCatalog.routesIncomplete'));
      }
      if (sequence !== loadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      setRoutes(nextRoutes); setError('');
    }
    catch (reason) { if (!controller.signal.aborted && sequence === loadSequence.current && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => {
    credentialLoadAbort.current?.abort();
    if (!credentialsRequested || !token || !writeTenant) {
      setCredentials([]); setCredentialError('');
      return;
    }
    const controller = new AbortController();
    credentialLoadAbort.current = controller;
    const sequence = ++credentialLoadSequence.current;
    const loadToken = token; const loadTenant = tenant; const loadWriteTenant = writeTenant;
    setCredentialError('');
    void apiRead<KeyView[]>(`/internal/v1/keys${queryForTenant(loadWriteTenant)}`, loadToken, { signal: controller.signal })
      .then((nextCredentials) => {
        if (controller.signal.aborted || sequence !== credentialLoadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant || scopeRef.current.writeTenant !== loadWriteTenant) return;
        setCredentials(nextCredentials);
      })
      .catch((reason) => {
        if (controller.signal.aborted || sequence !== credentialLoadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant || scopeRef.current.writeTenant !== loadWriteTenant) return;
        setCredentialError(messageOf(reason, t('common.requestFailed')));
      });
    return () => controller.abort();
  }, [credentialsRequested, token, tenant, writeTenant, t]);
  useEffect(() => {
    loadSequence.current += 1; credentialLoadSequence.current += 1; setRoutes([]); setCredentials([]); setCredentialsRequested(false); setCredentialError(''); setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false });
    setEditing(undefined); setWorkspaceOpen(false); setEditForm(emptyRouteDraft); setEditCatalog({ valid: false, allowCustom: false });
    setBusy(''); setMessage(''); setError('');
    const prefill = consumeRouteDraftPrefill(writeTenant);
    const prefillAccount = prefill && upstreamsRef.current.find((account) => account.id === prefill.accountId
      && (!account.tenant_external_id || account.tenant_external_id === writeTenant)
      && account.status === 'active');
    const prefillProvider = providers.find((provider) => provider.id === prefillAccount?.driver);
    const canUsePrefill = Boolean(prefill && prefillAccount && prefillProvider?.protocols.includes(prefill.protocol));
    if (prefill && canUsePrefill) {
      setForm({ ...emptyRouteDraft, upstream_account_id: prefill.accountId, upstream_account_ids: [prefill.accountId], upstream_model: prefill.upstreamModel, public_model: prefill.publicModel, protocol: prefill.protocol });
      setWorkspaceOpen(true); setCredentialsRequested(true); setMessage(t('routes.prefilled'));
    }
    const focus = canUsePrefill ? '' : consumeRouteFocus(writeTenant) ?? '';
    setFocusRouteId(focus);
    void load(focus);
    return () => { loadAbort.current?.abort(); credentialLoadAbort.current?.abort(); };
  }, [token, tenant, writeTenant]);
  const statusFilter = useResourceListStatusFilter('model-routes', tenant, routes, (route) => route.enabled);
  useEffect(() => {
    if (focusRouteId && routes.some((route) => route.id === focusRouteId && !route.enabled) && !statusFilter.showInactive) {
      statusFilter.setSelection('all');
    }
  }, [focusRouteId, routes, statusFilter.showInactive, statusFilter.setSelection]);
  useLayoutEffect(() => {
    if (!focusRouteId || !statusFilter.values.some((route) => route.id === focusRouteId)) return;
    const row = document.querySelector<HTMLElement>(`[data-route-id="${CSS.escape(focusRouteId)}"]`);
    row?.focus({ preventScroll: true });
    row?.scrollIntoView({ block: 'center' });
  }, [focusRouteId, statusFilter.values]);
  const scopedUpstreams = upstreams.filter((value) => !value.tenant_external_id || value.tenant_external_id === writeTenant);
  const canManage = (route: ModelRouteView) => Boolean(writeTenant) && route.tenant_external_id === writeTenant;
  const canSubmit = (draft: RouteDraft, catalogValid: boolean) => {
    const included = providerGroups.groups.filter((group) => draft.included_provider_group_ids.includes(group.id)).flatMap((group) => group.member_ids);
    const excluded = new Set(providerGroups.groups.filter((group) => draft.excluded_provider_group_ids.includes(group.id)).flatMap((group) => group.member_ids));
    const candidates = [...new Set([...draft.upstream_account_ids, ...included])].filter((id) => !excluded.has(id));
    const compatible = candidates.every((id) => {
      const account = scopedUpstreams.find((value) => value.id === id);
      const provider = providers.find((value) => value.id === account?.driver);
      return !provider || provider.protocols.includes(draft.protocol);
    });
    return Boolean(writeTenant && catalogValid && compatible && candidates.length > 0 && draft.public_model.trim() && draft.upstream_model.trim()
      && Number.isInteger(draft.priority) && Math.abs(draft.priority) <= 1000000);
  };
  const beginEdit = (route: ModelRouteView) => {
    setWorkspaceOpen(true);
    setCredentialsRequested(true);
    setEditing(route);
    setEditCatalog({ valid: false, allowCustom: false });
    setEditForm({
      public_model: route.public_model,
      upstream_account_id: route.upstream_account_id ?? route.upstream_account_ids?.[0] ?? '',
      upstream_account_ids: route.upstream_account_ids ?? (route.upstream_account_id ? [route.upstream_account_id] : []),
      upstream_model: route.upstream_model,
      protocol: route.protocol,
      priority: route.priority,
      included_provider_group_ids: route.included_provider_group_ids ?? [],
      excluded_provider_group_ids: route.excluded_provider_group_ids ?? [],
      route_group_ids: route.route_group_ids ?? [],
      route_group_names: [],
      granted_credential_ids: route.granted_credential_ids ?? [],
      custom_model_confirmed: route.custom_model_confirmed ?? false,
    });
    setMessage(''); setError('');
  };
  const saveEdit = async () => {
    if (!editing || !canSubmit(editForm, editCatalog.valid)) return;
    setBusy(editing.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${editing.id}`, token, { method: 'PUT', body: JSON.stringify({ ...routeRequest(editForm, editCatalog.allowCustom), tenant_external_id: writeTenant, expected_updated_at: editing.updated_at, expected_grant_revision: editing.grant_revision }) });
      if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return;
      setEditing(undefined); setWorkspaceOpen(false); await Promise.all([load(), routeGroups.load()]);
      if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setMessage(t('routes.updated'));
    } catch (reason) {
      if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return;
      if (reason instanceof ApiError && reason.status === 409) {
        await Promise.all([load(), routeGroups.load()]); setError(formJourneyCopy(locale).concurrentDraftPreserved);
      } else setError(messageOf(reason, t('common.requestFailed')));
    }
    finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); }
  };
  const createRoute = async () => {
    if (!canSubmit(form, formCatalog.valid) || busy) return;
    setBusy('create'); setMessage(''); setError('');
    try {
      await api('/internal/v1/model-routes', token, { method: 'POST', body: JSON.stringify({ ...routeRequest(form, formCatalog.allowCustom), tenant_external_id: writeTenant }) });
      if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return;
      setForm(emptyRouteDraft); setFormCatalog({ valid: false, allowCustom: false }); setWorkspaceOpen(false);
      await Promise.all([load(), routeGroups.load()]);
      if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setMessage(t('routes.created'));
    } catch (reason) {
      if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setError(messageOf(reason, t('common.requestFailed')));
    } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); }
  };
  const setEnabled = async (route: ModelRouteView, enabled: boolean) => {
    setBusy(route.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${route.id}`, token, { method: 'PATCH', body: JSON.stringify({ tenant_external_id: writeTenant, enabled, expected_updated_at: route.updated_at }) });
      setEditing(undefined); setMessage(t(enabled ? 'routes.enabled' : 'routes.disabled')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  const remove = async (route: ModelRouteView) => {
    if (route.enabled || !await confirm(t('routes.confirmDelete', { model: route.public_model }))) return;
    setBusy(route.id); setMessage(''); setError('');
    try {
      await api(`/internal/v1/model-routes/${route.id}/archive`, token, { method: 'POST', body: JSON.stringify({ tenant_external_id: writeTenant, expected_updated_at: route.updated_at }) });
      setEditing(undefined); setMessage(t('routes.deleted')); await load();
    } catch (reason) { setError(messageOf(reason, t('common.requestFailed'))); }
    finally { setBusy(''); }
  };
  return <>{confirmationDialog}<WriteScopeNotice tenant={writeTenant} /><section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('routes.title')}</h2><p className="muted">{t('routes.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>{error && <div className="notice error" role="alert">{error}</div>}{providerGroups.error && <div className="notice error" role="alert">{providerGroups.error}</div>}{routeGroups.error && <div className="notice error" role="alert">{routeGroups.error}</div>}{credentialError && <div className="notice error" role="alert">{credentialError}</div>}{message && <div ref={successNotice} tabIndex={-1} className="notice success" role="status">{message}</div>}<div className="table-scroll"><table className="model-route-list"><thead><tr>{!tenant && <th>{t('credentials.tenant')}</th>}<th>{t('routes.publicModel')}</th><th>{t('routes.upstream')}</th><th>{t('routes.groups')}</th><th>{t('routes.upstreamModel')}</th><th>{t('routes.protocol')}</th><th>{t('routes.priority')}</th><th>{t('request.status')}</th><th>{t('routes.actions')}</th></tr></thead><tbody>{statusFilter.values.map((route) => <tr key={route.id} tabIndex={focusRouteId === route.id ? -1 : undefined} data-route-id={route.id} className={focusRouteId === route.id ? 'route-focus' : undefined}>{!tenant && <td><code>{tenantDisplayName(route.tenant_external_id ?? '—', locale)}</code></td>}<td><code className="route-model-name">{route.public_model}</code></td><td><RouteListSource route={route} groups={providerGroups.groups} accounts={upstreams} /></td><td><div className="table-chip-list">{(route.route_group_ids ?? []).map((id) => <span key={id}>{routeGroups.groups.find((value) => value.id === id)?.name ?? id}</span>)}</div></td><td><code className="route-model-name">{route.upstream_model}</code></td><td>{route.protocol}</td><td>{formatNumber(route.priority, locale)}</td><td><span className={`status ${route.enabled ? 'ok' : 'pending'}`}>{route.enabled ? t('common.enabled') : t('common.disabled')}</span></td><td><div className="row-actions"><button type="button" className="secondary" disabled={busy === route.id || !canManage(route)} onClick={() => beginEdit(route)}>{t('routes.edit')}</button><button type="button" className="secondary" disabled={busy === route.id || !canManage(route)} onClick={() => void setEnabled(route, !route.enabled)}>{route.enabled ? t('routes.disable') : t('routes.enable')}</button><button type="button" className="danger" title={route.enabled ? t('routes.disableBeforeDelete') : undefined} disabled={busy === route.id || !canManage(route) || route.enabled} onClick={() => void remove(route)}>{t('routes.archive')}</button></div></td></tr>)}</tbody></table>{statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('common.enabled')} empty={t('routes.empty')} />}</div>
    </article>
    <CreateJourney title={editing ? t('routes.editTitle', { model: editing.public_model }) : t('routes.createTitle')} description={t('routes.description')} open={workspaceOpen} busy={Boolean(busy)} onOpenChange={(open) => { setWorkspaceOpen(open); if (!open) setEditing(undefined); }} onOpen={() => { setCredentialsRequested(true); setMessage(''); }}>
      {message && workspaceOpen && <div className="notice success" role="status">{message}</div>}
      {(error || providerGroups.error || routeGroups.error || credentialError) && <div className="notice error" role="alert">{error || providerGroups.error || routeGroups.error || credentialError}</div>}
      <RouteFields key={editing?.id ?? 'create'} token={token} tenant={writeTenant} draft={editing ? editForm : form} upstreams={scopedUpstreams} providers={providers} providerGroups={providerGroups.groups} routeGroups={routeGroups.groups} credentials={credentials} onChange={editing ? setEditForm : setForm} onCatalogValidity={(valid, allowCustom) => editing ? setEditCatalog({ valid, allowCustom }) : setFormCatalog({ valid, allowCustom })} />
      <div className="journey-actions"><p>{t('routes.description')}</p><Button appearance="primary" type="button" disabled={Boolean(busy) || !canSubmit(editing ? editForm : form, editing ? editCatalog.valid : formCatalog.valid)} onClick={() => void (editing ? saveEdit() : createRoute())}>{t(editing ? 'common.save' : 'routes.create')}</Button></div>
    </CreateJourney>
  </section><section className="routing-group-managers">
    <GroupManager kind="provider" token={token} tenant={writeTenant} groups={providerGroups.groups} resources={scopedUpstreams.map((value) => ({ value: value.id, label: value.name, description: value.driver }))} onChanged={providerGroups.load} />
    <GroupManager kind="route" token={token} tenant={writeTenant} groups={routeGroups.groups} resources={credentialRouteOptions(routes.filter(canManage), scopedUpstreams, providers, locale)} onChanged={async () => { await Promise.all([routeGroups.load(), load()]); }} />
  </section></>;
}

function CredentialWorkspace({ token, tenant, writeTenant = tenant, createSchema, policySchema }: { token: string; tenant: string; writeTenant?: string; createSchema?: Record<string, unknown>; policySchema?: Record<string, unknown> }) {
  const { locale, t } = useI18n();
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const renderScope = useRef({ token, tenant, writeTenant, generation: 0 });
  if (renderScope.current.token !== token || renderScope.current.tenant !== tenant || renderScope.current.writeTenant !== writeTenant) {
    renderScope.current = { token, tenant, writeTenant, generation: renderScope.current.generation + 1 };
  }
  const [values, setValues] = useState<KeyView[]>([]);
  const [routes, setRoutes] = useState<ModelRouteView[]>([]);
  type CredentialEditor = 'policy' | 'routing' | 'rename' | 'grant' | 'limits' | 'credential';
  const [workspace, setWorkspace] = useState<{ kind: 'create' } | { kind: CredentialEditor; keyId: string }>();
  const activeEditorRegion = useRef<HTMLDivElement>(null);
  const editorGeneration = useRef(0);
  const activateEditor = (kind: CredentialEditor, keyId?: string) => { editorGeneration.current += 1; setWorkspace(keyId ? { kind, keyId } : undefined); };
  const editingPolicy = workspace?.kind === 'policy' ? workspace.keyId : undefined;
  const setEditingPolicy = (keyId?: string) => activateEditor('policy', keyId);
  const editingRouting = workspace?.kind === 'routing' ? workspace.keyId : undefined;
  const setEditingRouting = (keyId?: string) => activateEditor('routing', keyId);
  const [routingDraft, setRoutingDraft] = useState<CredentialRoutingView>();
  const routingSaveLock = useRef<symbol | undefined>(undefined);
  useLayoutEffect(() => {
    if (!workspace || workspace.kind === 'create' || activeEditorRegion.current?.contains(document.activeElement)) return;
    const fields = activeEditorRegion.current?.querySelectorAll<HTMLElement>('input:not([type="hidden"]), select, button');
    [...(fields ?? [])].find(field => field.getClientRects().length > 0 && !field.hasAttribute('disabled'))?.focus();
  }, [workspace, routingDraft]);
  const renaming = workspace?.kind === 'rename' ? workspace.keyId : undefined;
  const setRenaming = (keyId?: string) => activateEditor('rename', keyId);
  const [aliasDraft, setAliasDraft] = useState('');
  const editingCredential = workspace?.kind === 'credential' ? workspace.keyId : undefined;
  const [credentialValue, setCredentialValue] = useState('');
  const setEditingCredential = (keyId?: string) => { setCredentialValue(''); activateEditor('credential', keyId); };
  const [limitSnapshots, setLimitSnapshots] = useState<Record<string, KeyLimitSnapshot>>({});
  const granting = workspace?.kind === 'grant' ? workspace.keyId : undefined;
  const setGranting = (keyId?: string) => activateEditor('grant', keyId);
  const [grant, setGrant] = useState({ amount: '', source: '' });
  const [busy, setBusy] = useState('');
  const [newRouteIds, setNewRouteIds] = useState<string[]>([]);
  const [createCredentialDraft, setCreateCredentialDraft] = useState<Record<string, unknown>>();
  const [policyDrafts, setPolicyDrafts] = useState<Record<string, KeyView['policy']>>({});
  const [newRouteGroupIds, setNewRouteGroupIds] = useState<string[]>([]);
  const [groupFilter, setGroupFilter] = useState('all');
  const [search, setSearch] = useState('');
  const [serverStatus, setServerStatus] = useState('active');
  const [creationSource, setCreationSource] = useState(tenant === 'default' ? 'manual_or_unknown' : 'all');
  const [selectedKeys, setSelectedKeys] = useState<string[]>([]);
  const deleteLock = useRef(false);
  const [nextCursor, setNextCursor] = useState<KeyListCursor>();
  const [keyListState, setKeyListState] = useState<KeyListLoadState>('idle');
  const [keyError, setKeyError] = useState('');
  const [routeError, setRouteError] = useState('');
  const [secret, setSecret] = useState<{ value: string; kind: 'issued' | 'revealed'; alias?: string; displayId: string; scopeGeneration: number }>();
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const scopeGeneration = useRef(0);
  const keyRequestGeneration = useRef(0);
  const routeRequestGeneration = useRef(0);
  const keyRequest = useRef<{ identity: KeyListRequestIdentity; controller: AbortController } | undefined>(undefined);
  const routeRequest = useRef<{ generation: number; scopeGeneration: number; controller: AbortController } | undefined>(undefined);
  const secretRequest = useRef<AbortController | undefined>(undefined);
  const secretOperation = useRef<symbol | undefined>(undefined);
  const visibleSecret = secret?.scopeGeneration === renderScope.current.generation ? secret : undefined;
  const secretPriority = useRef<HTMLDivElement>(null);
  const credentialSuccess = useRef<HTMLDivElement>(null);
  useLayoutEffect(() => {
    if (visibleSecret) { secretPriority.current?.focus(); secretPriority.current?.scrollIntoView({ block: 'start' }); }
    else if (message && !workspace) credentialSuccess.current?.focus();
  }, [visibleSecret?.displayId, message, workspace]);
  const secretRef = useRef(visibleSecret);
  secretRef.current = visibleSecret;
  const scopeRef = useRef({ token, tenant, writeTenant });
  scopeRef.current = { token, tenant, writeTenant };
  const ownsSecretScope = (operationToken: string, operationTenant: string, operationWriteTenant: string, operationScopeGeneration: number) =>
    renderScope.current.generation === operationScopeGeneration
    && scopeRef.current.token === operationToken
    && scopeRef.current.tenant === operationTenant
    && scopeRef.current.writeTenant === operationWriteTenant;
  const credentialGroups = useGroups('credential', token, writeTenant);
  const routeGroups = useGroups('route', token, writeTenant);
  const createFormSchema = useMemo(() => createSchema ? credentialCreateSchema(createSchema as RJSFSchema) : undefined, [createSchema]);
  const policyFormSchema = useMemo(() => policySchema ? credentialPolicySchema(policySchema as RJSFSchema) : undefined, [policySchema]);
  const ownsKeyRequest = (request: { identity: KeyListRequestIdentity; controller: AbortController }) => ownsKeyListRequest(keyRequest.current?.identity, request.identity)
    && scopeGeneration.current === request.identity.scopeGeneration
    && scopeRef.current.token === token && scopeRef.current.tenant === tenant;
  const startKeyRequest = (state: Extract<KeyListLoadState, 'initial-loading' | 'loading-more'>) => {
    keyRequest.current?.controller.abort();
    const request = {
      identity: { generation: ++keyRequestGeneration.current, scopeGeneration: scopeGeneration.current },
      controller: new AbortController(),
    };
    keyRequest.current = request;
    setKeyListState(state);
    return request;
  };
  const loadRoutes = async (loadToken: string, loadTenant: string, currentScopeGeneration: number) => {
    routeRequest.current?.controller.abort();
    routeRequest.current = undefined;
    if (!shouldLoadCredentialRoutes(loadTenant)) { setRoutes([]); setRouteError(''); return; }
    const request = {
      generation: ++routeRequestGeneration.current,
      scopeGeneration: currentScopeGeneration,
      controller: new AbortController(),
    };
    routeRequest.current = request;
    setRouteError('');
    try {
      const nextRoutes = await apiRead<ModelRouteView[]>(`/internal/v1/model-routes${queryForTenant(loadTenant)}`, loadToken, { signal: request.controller.signal });
      const active = routeRequest.current;
      if (!active || active.generation !== request.generation || active.scopeGeneration !== request.scopeGeneration || scopeGeneration.current !== request.scopeGeneration || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      routeRequest.current = undefined;
      setRoutes(nextRoutes); setRouteError('');
    } catch (reason) {
      const active = routeRequest.current;
      if (request.controller.signal.aborted || !active || active.generation !== request.generation || active.scopeGeneration !== request.scopeGeneration || scopeGeneration.current !== request.scopeGeneration || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      routeRequest.current = undefined;
      setRouteError(messageOf(reason, t('common.requestFailed')));
    }
  };
  const load = async () => {
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) {
      keyRequest.current?.controller.abort(); keyRequest.current = undefined;
      routeRequest.current?.controller.abort(); routeRequest.current = undefined;
      setValues([]); setRoutes([]); setNextCursor(undefined); setKeyListState('idle'); setKeyError(''); setRouteError('');
      return;
    }
    const request = startKeyRequest('initial-loading');
    setKeyError('');
    try {
      const keyRows = await apiRead<KeyView[]>(keyListPath(loadTenant, undefined, { search, status: serverStatus, source: creationSource }), loadToken, { signal: request.controller.signal });
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      const page = applyKeyPage([], keyRows);
      keyRequest.current = undefined;
      if (!page.ok) {
        setNextCursor(undefined); setKeyListState('failed'); setKeyError(t('credentials.paginationStalled'));
        return;
      }
      setValues(page.values); setNextCursor(page.nextCursor); setKeyListState(page.nextCursor ? 'more' : 'complete');
    }
    catch (reason) {
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      keyRequest.current = undefined;
      setKeyListState('failed'); setKeyError(messageOf(reason, t('common.requestFailed')));
    }
  };
  useEffect(() => {
    secretRequest.current?.abort(); secretRequest.current = undefined; secretOperation.current = undefined; secretRef.current = undefined;
    routingSaveLock.current = undefined; scopeGeneration.current += 1; setValues([]); setRoutes([]); setEditingPolicy(undefined); setEditingRouting(undefined); setRoutingDraft(undefined);
    setRenaming(undefined); setAliasDraft(''); setEditingCredential(undefined); setCredentialValue(''); setLimitSnapshots({}); setGranting(undefined); setGrant({ amount: '', source: '' }); setBusy('');
    setPolicyDrafts({}); setCreateCredentialDraft(undefined); setNewRouteIds([]); setNewRouteGroupIds([]); setGroupFilter('all'); setSearch(''); setNextCursor(undefined); setKeyListState('initial-loading'); setKeyError(''); setRouteError(''); setSecret(undefined); setMessage(''); setError('');
    void loadRoutes(token, tenant, scopeGeneration.current);
    setCreationSource(tenant === 'default' ? 'manual_or_unknown' : 'all');
    return () => { keyRequest.current?.controller.abort(); routeRequest.current?.controller.abort(); secretRequest.current?.abort(); };
  }, [token, tenant, writeTenant]);
  useEffect(() => {
    setValues([]); setNextCursor(undefined); setKeyListState('initial-loading');
    setSelectedKeys([]);
    keyRequest.current?.controller.abort();
    const timer = window.setTimeout(() => void load(), search.trim() ? 250 : 0);
    return () => { window.clearTimeout(timer); keyRequest.current?.controller.abort(); };
  }, [token, tenant, writeTenant, search, serverStatus, creationSource]);
  useEffect(() => { setSelectedKeys([]); }, [groupFilter]);
  const loadMore = async () => {
    if (!canLoadMoreKeys(keyListState, Boolean(nextCursor), Boolean(keyRequest.current)) || !nextCursor || !token) return;
    const loadToken = token; const loadTenant = tenant; const cursor = nextCursor;
    const request = startKeyRequest('loading-more');
    setKeyError('');
    try {
      const keyRows = await apiRead<KeyView[]>(keyListPath(loadTenant, cursor, { search, status: serverStatus, source: creationSource }), loadToken, { signal: request.controller.signal });
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      const page = applyKeyPage(values, keyRows, cursor);
      keyRequest.current = undefined;
      if (!page.ok) {
        setNextCursor(undefined); setKeyListState('failed'); setKeyError(t('credentials.paginationStalled'));
        return;
      }
      setValues(page.values); setNextCursor(page.nextCursor); setKeyListState(page.nextCursor ? 'more' : 'complete');
    } catch (reason) {
      if (!ownsKeyRequest(request) || request.controller.signal.aborted) return;
      keyRequest.current = undefined;
      setKeyListState('failed'); setKeyError(messageOf(reason, t('common.requestFailed')));
    }
  };
  const nonStatusFilteredValues = values.filter((value) => {
    if (groupFilter === 'all' || !writeTenant) return true;
    const memberships = credentialGroups.groups.filter((group) => group.member_ids.includes(value.key_id));
    return groupFilter === 'unassigned' ? memberships.length === 0 : memberships.some((group) => group.id === groupFilter);
  });
  const filteredValues = nonStatusFilteredValues;
  const filtersApplied = Boolean(search.trim()) || serverStatus !== 'all' || groupFilter !== 'all';
  const loadingKeys = keyListState === 'initial-loading' || keyListState === 'loading-more';
  const canLoadMore = canLoadMoreKeys(keyListState, Boolean(nextCursor), Boolean(keyRequest.current));
  const listPresentation = credentialListPresentation(keyListState, filtersApplied);
  const canReadLimits = canReadCredentialLimits(token);
  const canWrite = canWriteCredential(writeTenant);
  const canManage = (value: KeyView) => canWrite && value.tenant_external_id === writeTenant;
  const deleteKeys = async (keyIds: string[]) => {
    if (!canWrite || deleteLock.current || !keyIds.length || keyIds.length > 100 || keyIds.some(id => !values.some(value => value.key_id === id && canManage(value)))) return;
    const operationScope = renderScope.current.generation;
    const current = () => renderScope.current.generation === operationScope;
    deleteLock.current = true;
    try {
      if (!await confirm(t('credentials.deleteConfirmation', { count: formatNumber(keyIds.length, locale), tenant: writeTenant === 'default' ? t('operator.defaultTenant') : writeTenant })) || !current()) return;
      setBusy('delete-credentials'); setError(''); setMessage('');
      const result = await api<{ deleted_key_ids: string[] }>('/internal/v1/keys/delete', token, { method: 'POST', body: JSON.stringify({ tenant_external_id: writeTenant, key_ids: keyIds }) });
      if (!current()) return;
      setSelectedKeys([]); setWorkspace(undefined);
      await load();
      if (current()) setMessage(t('credentials.deleted', { count: formatNumber(result.deleted_key_ids.length, locale) }));
    } catch (reason) {
      if (current()) setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      deleteLock.current = false;
      if (current()) setBusy('');
    }
  };
  const beginSecretOperation = () => {
    if (secretOperation.current || secretRef.current) return undefined;
    const operation = Symbol('credential-secret-operation');
    secretOperation.current = operation;
    return operation;
  };
  const finishSecretOperation = (operation: symbol) => {
    if (secretOperation.current !== operation) return;
    secretOperation.current = undefined;
    setBusy('');
  };
  const showSecret = (next: { value: string; kind: 'issued' | 'revealed'; alias?: string; displayId: string }) => {
    // Update the ref synchronously so another submit in the same browser task
    // cannot replace plaintext before React commits the state update.
    const scoped = { ...next, scopeGeneration: renderScope.current.generation };
    secretRef.current = scoped;
    setWorkspace(undefined);
    setSecret(scoped);
  };
  const dismissSecret = () => {
    secretRef.current = undefined;
    setSecret(undefined);
  };
  const rotateCredential = async (value: KeyView) => {
    if (!canManage(value) || value.status === 'revoked') return;
    const operation = beginSecretOperation();
    if (!operation) return;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant; const operationScopeGeneration = renderScope.current.generation;
    setBusy(`rotate-${value.key_id}`); setError(''); setMessage('');
    const controller = new AbortController();
    secretRequest.current?.abort();
    secretRequest.current = controller;
    try {
      if (!await confirm(`${t('credentials.rotate')} · ${value.alias}\n${value.key_id}`)) return;
      const result = await api<{ key: string }>(`/internal/v1/keys/${value.key_id}/rotate`, operationToken, { ...secretResponseRequestPolicy, method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, signal: controller.signal });
      if (!ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) return;
      showSecret({ value: result.key, kind: 'issued', alias: value.alias, displayId: crypto.randomUUID() });
      setMessage(t('credentials.rotated', { alias: value.alias })); await load();
    } catch (reason) {
      if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      if (secretRequest.current === controller) secretRequest.current = undefined;
      if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) finishSecretOperation(operation);
    }
  };
  const copyCredential = async (value: KeyView) => {
    if (!canManage(value) || value.status !== 'active' || !value.credential_copy_available) return;
    const operation = beginSecretOperation();
    if (!operation) return;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant; const operationScopeGeneration = renderScope.current.generation;
    setBusy(`copy-${value.key_id}`); setError(''); setMessage('');
    const controller = new AbortController();
    secretRequest.current?.abort();
    secretRequest.current = controller;
    try {
      const result = await api<{ key_id: string; credential_generation: number; key: string }>(`/internal/v1/keys/${value.key_id}/copy`, operationToken, {
        ...secretResponseRequestPolicy, method: 'POST', signal: controller.signal,
      });
      if (!ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) return;
      if (result.key_id !== value.key_id || result.credential_generation !== value.credential_generation) throw new Error(t('common.requestFailed'));
      showSecret({ value: result.key, kind: 'revealed', alias: value.alias, displayId: crypto.randomUUID() });
      setMessage(t('credentials.copyReady', { alias: value.alias }));
    } catch (reason) {
      if (!controller.signal.aborted && ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) {
        setError(reason instanceof ApiError && reason.status === 403
          ? t('credentials.copyPermission')
          : reason instanceof ApiError && reason.status === 404
            ? t('credentials.copyNeedsOriginal')
            : messageOf(reason, t('common.requestFailed')));
      }
    } finally {
      if (secretRequest.current === controller) secretRequest.current = undefined;
      if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) finishSecretOperation(operation);
    }
  };
  const routeOptions = routes.filter((route) => route.tenant_external_id === writeTenant);
  const routeGroupOptions = routeGroups.groups;
  const openRouting = async (value: KeyView) => {
    if (editingRouting === value.key_id) { setEditingRouting(undefined); setRoutingDraft(undefined); return; }
    setError('');
    setEditingRouting(value.key_id); setRoutingDraft(undefined);
    const generation = editorGeneration.current;
    const operationToken = token; const operationTenant = tenant;
    try {
      const routing = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing${queryForTenant(operationTenant)}`, operationToken);
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || editorGeneration.current !== generation) return;
      setRoutingDraft(routing);
    } catch (reason) { if (scopeRef.current.token === operationToken && scopeRef.current.tenant === operationTenant && editorGeneration.current === generation) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  const saveRouting = async (value: KeyView, draft: CredentialRoutingView) => {
    if (busy || routingSaveLock.current || !canManage(value)) return;
    const operation = Symbol('credential-routing-save');
    routingSaveLock.current = operation;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant;
    const generation = editorGeneration.current;
    setBusy('edit-routing'); setError('');
    try {
      const saved = await api<CredentialRoutingView>(`/internal/v1/keys/${value.key_id}/routing`, operationToken, { method: 'PUT', body: JSON.stringify({ tenant_external_id: operationWriteTenant, route_ids: draft.route_ids, route_group_ids: draft.route_group_ids, expected_grant_revision: draft.grant_revision }) });
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant || editorGeneration.current !== generation) return;
      setRoutingDraft(saved); setBusy(''); setEditingRouting(undefined); setMessage(t('credentials.routingSaved')); setError('');
    } catch (reason) {
      if (scopeRef.current.token !== operationToken || scopeRef.current.tenant !== operationTenant || scopeRef.current.writeTenant !== operationWriteTenant || editorGeneration.current !== generation) return;
      if (reason instanceof ApiError && reason.status === 409) {
        setError(formJourneyCopy(locale).concurrentDraftPreserved);
      } else setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      if (routingSaveLock.current === operation) routingSaveLock.current = undefined;
      if (scopeRef.current.token === operationToken && scopeRef.current.tenant === operationTenant && scopeRef.current.writeTenant === operationWriteTenant && editorGeneration.current === generation) setBusy('');
    }
  };
  const saveCredentialConfiguration = async (value: KeyView, suffix: 'alias' | 'policy', body: unknown, success: string) => {
    if (busy) return;
    const generation = editorGeneration.current;
    const current = () => editorGeneration.current === generation && scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant;
    setBusy('edit-credential'); setError(''); setMessage('');
    try {
      await api(`/internal/v1/keys/${value.key_id}/${suffix}`, token, { method: suffix === 'alias' ? 'PATCH' : 'PUT', body: JSON.stringify(body) });
      if (!current()) return;
      if (suffix === 'policy') setPolicyDrafts(current => {
        const { [value.key_id]: _savedDraft, ...remaining } = current;
        return remaining;
      });
      setWorkspace(undefined); await load(); if (current()) setMessage(success);
    } catch (reason) { if (current()) setError(messageOf(reason, t('common.requestFailed'))); }
    finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); }
  };
  const activeCredential = workspace && workspace.kind !== 'create' ? values.find(value => value.key_id === workspace.keyId) : undefined;
  const credentialEditor = (value: KeyView) => <div ref={activeEditorRegion} className="credential-active-editor">
          {renaming === value.key_id && <div className="inline-editor form-panel"><h3>{t('credentials.renameFor', { alias: value.alias })}</h3><label>{t('schema.Credential alias')}<Input value={aliasDraft} maxLength={200} onChange={(event) => setAliasDraft(event.target.value)} /></label><Button appearance="primary" type="button" disabled={!canWrite || !aliasDraft.trim()} onClick={() => void saveCredentialConfiguration(value, 'alias', { alias: aliasDraft }, t('credentials.renamed', { alias: aliasDraft.trim() }))}>{t('common.save')}</Button></div>}
          {editingCredential === value.key_id && <div className="inline-editor form-panel"><h3>{t('credentials.storeFor', { alias: value.alias })}</h3><p className="muted">{t('credentials.storeHint')}</p><label>{t('credentials.revealedValue')}<Input type="password" autoComplete="off" value={credentialValue} onChange={(event) => setCredentialValue(event.target.value)} /></label><Button appearance="primary" type="button" disabled={!canManage(value) || Boolean(busy) || !credentialValue} onClick={async () => { const knownValue = credentialValue; setBusy(`store-${value.key_id}`); setError(''); setMessage(''); try { await api<void>(`/internal/v1/keys/${value.key_id}/credential`, token, { ...secretResponseRequestPolicy, method: 'PUT', body: JSON.stringify({ key: knownValue }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return; setCredentialValue(''); setEditingCredential(undefined); setMessage(t('credentials.stored', { alias: value.alias })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); } }}>{t('common.save')}</Button></div>}
          {workspace?.kind === 'limits' && (limitSnapshots[value.key_id] ? <LimitSnapshot value={limitSnapshots[value.key_id]} /> : <><LoadingProgress active label={t('common.loading')} level="page" /><LoadingState label={t('common.loading')} variant="detail" /></>)}
          {editingPolicy === value.key_id && policyFormSchema && <div className="inline-editor form-panel"><h3>{t('credentials.policyFor', { alias: value.alias })}</h3><CredentialPolicySummary policy={value.policy} currency={value.currency} /><Form key={value.key_id} schema={localizeSchema(policyFormSchema as RJSFSchema, locale)} formData={policyDrafts[value.key_id] ?? value.policy} onChange={({ formData }) => setPolicyDrafts(current => ({ ...current, [value.key_id]: formData }))} uiSchema={credentialPolicyUiSchema} fields={credentialFormFields} validator={validator} templates={credentialFormTemplates} widgets={fluentFormWidgets} onSubmit={({ formData }) => void saveCredentialConfiguration(value, 'policy', formData, t('credentials.policySaved'))}><Button appearance="primary" type="submit" disabled={!canWrite || Boolean(busy)}>{t('common.save')}</Button></Form></div>}
          {editingRouting === value.key_id && routingDraft && <div className="inline-editor form-panel routing-editor"><h3>{t('credentials.routingFor', { alias: value.alias })}</h3><p className="muted">{t('credentials.routingHint')}</p>
            <CredentialRouteAuthorization token={token} tenant={writeTenant} routes={routeOptions} groups={routeGroupOptions} routeIds={routingDraft.route_ids} groupIds={routingDraft.route_group_ids} onRoutes={route_ids => setRoutingDraft({ ...routingDraft, route_ids })} onGroups={route_group_ids => setRoutingDraft({ ...routingDraft, route_group_ids })} />
            {routingDraft.effective_route_ids.length > 0 && <small className="field-hint">{t('credentials.effectiveRoutes', { count: formatNumber(routingDraft.effective_route_ids.length, locale) })}</small>}
            <Button appearance="primary" type="button" disabled={!canWrite || Boolean(busy)} onClick={() => void saveRouting(value, routingDraft)}>{t('common.save')}</Button>
          </div>}
          {granting === value.key_id && value.account_id && <div className="inline-editor form-panel"><h3>{t('credentials.grantFor', { alias: value.alias })}</h3><label>{t('credentials.grantAmount')} ({value.currency})<input inputMode="decimal" value={grant.amount} onChange={(event) => setGrant({ ...grant, amount: event.target.value })} /></label><label>{t('credentials.grantSource')}<input value={grant.source} onChange={(event) => setGrant({ ...grant, source: event.target.value })} /></label><Button appearance="primary" type="button" disabled={!canWrite || Boolean(busy) || !isPositiveDecimal(grant.amount) || !grant.source.trim()} onClick={async () => { const amount = grant.amount.trim(); const source = grant.source.trim(); if (!await confirm(`${t('credentials.grantFor', { alias: value.alias })}\n${t('credentials.grantAmount')}: ${amount} ${value.currency}\n${t('credentials.grantSource')}: ${source}`)) return; setBusy(`grant-${value.key_id}`); try { await api(`/internal/v1/accounts/${value.account_id}/grants`, token, { method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, body: JSON.stringify({ amount, source }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setGranting(undefined); setGrant({ amount: '', source: '' }); setMessage(t('credentials.granted')); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setBusy(''); } }}>{t('credentials.confirmGrant')}</Button></div>}
    {workspace?.kind === 'routing' && !routingDraft && <><LoadingProgress active label={t('common.loading')} level="page" /><LoadingState label={t('common.loading')} variant="detail" /></>}
  </div>;
  return <>{confirmationDialog}<WriteScopeNotice tenant={writeTenant} />{visibleSecret && <div ref={secretPriority} tabIndex={-1} className="credential-secret-priority">{visibleSecret.kind === 'issued' ? <IssuedCredential key={visibleSecret.displayId} value={visibleSecret.value} filename="client-credential.txt" onDismiss={dismissSecret} message={t('credentials.issued')} /> : <RevealedCredential key={visibleSecret.displayId} value={visibleSecret.value} message={t('credentials.copyReady', { alias: visibleSecret.alias ?? '' })} onDismiss={dismissSecret} onCopied={() => setMessage(t('credentials.copySuccess', { alias: visibleSecret.alias ?? '' }))} />}</div>}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('credentials.title')}</h2><p className="muted">{t('credentials.description')}</p></div><label>{t('request.status')}<Select aria-label={t('request.status')} value={serverStatus} onChange={event => setServerStatus(event.target.value)}><option value="active">{enumLabel(t, 'status', 'active')}</option><option value="all">{t('common.all')}</option><option value="suspended">{enumLabel(t, 'status', 'suspended')}</option><option value="revoked">{enumLabel(t, 'status', 'revoked')}</option></Select></label></div>
      <div className="credential-list-controls"><label>{t('credentials.search')}<Input type="search" maxLength={200} value={search} onChange={(event) => setSearch(event.target.value)} placeholder={t('credentials.searchPlaceholder')} /></label>{writeTenant && <label>{t('credentials.groupFilter')}<Select value={groupFilter} onChange={(event) => setGroupFilter(event.target.value)}><option value="all">{t('common.all')}</option><option value="unassigned">{t('credentials.ungrouped')}</option>{credentialGroups.groups.map((group) => <option key={group.id} value={group.id}>{group.name}</option>)}</Select></label>}<label>{t('credentials.source')}<Select value={creationSource} onChange={event => setCreationSource(event.target.value)}><option value="manual_or_unknown">{t('credentials.sourceWorkspace')}</option><option value="manual">{t('credentials.sourceManual')}</option><option value="api">{t('credentials.sourceApi')}</option><option value="unknown">{t('credentials.sourceUnknown')}</option><option value="all">{t('common.all')}</option></Select></label></div>
      {canWrite && <div className="row-actions"><Button appearance="subtle" disabled={Boolean(busy) || Boolean(visibleSecret) || !filteredValues.length} onClick={() => setSelectedKeys(filteredValues.filter(canManage).slice(-100).map(value => value.key_id))}>{t('credentials.selectPage')}</Button><Button appearance="subtle" disabled={Boolean(busy) || !selectedKeys.length} onClick={() => setSelectedKeys([])}>{t('credentials.clearSelection')}</Button><Button appearance="secondary" disabled={Boolean(busy) || Boolean(visibleSecret) || !selectedKeys.length} onClick={() => void deleteKeys(selectedKeys)}>{t(busy === 'delete-credentials' ? 'credentials.deleting' : 'credentials.deleteSelected', { count: formatNumber(selectedKeys.length, locale) })}</Button><span className="muted">{t('credentials.selectionHint')}</span></div>}
      <LoadingProgress active={loadingKeys} label={t('credentials.loadingList')} level="page" />
      <div className="credential-list-summary" role={listPresentation === 'loading' ? undefined : 'status'}>{listPresentation === 'loading' ? <LoadingState label={t('credentials.loadingList')} variant="inline" /> : listPresentation === 'loading-more' ? t('credentials.loadingMore', { count: formatNumber(values.length, locale) }) : listPresentation === 'failed' ? t('credentials.loadFailed', { count: formatNumber(values.length, locale) }) : listPresentation === 'filtered' ? t('credentials.filteredLoaded', { shown: formatNumber(filteredValues.length, locale), loaded: formatNumber(values.length, locale) }) : listPresentation === 'more' ? t('credentials.loadedMore', { count: formatNumber(values.length, locale) }) : t('credentials.loadedComplete', { count: formatNumber(values.length, locale) })}</div>
      {keyError && <div className="notice error" role="alert">{keyError}</div>}{routeError && <div className="notice error" role="alert">{routeError}</div>}{error && <div className="notice error" role="alert">{error}</div>}{credentialGroups.error && <div className="notice error" role="alert">{credentialGroups.error}</div>}{routeGroups.error && <div className="notice error" role="alert">{routeGroups.error}</div>}{message && <div ref={credentialSuccess} tabIndex={-1} className="notice success" role="status">{message}</div>}
      <div className="account-list credential-compact-list">{filteredValues.length === 0 && (loadingKeys ? <LoadingState label={t('credentials.loadingList')} variant="list" /> : keyListState === 'failed' ? <div className="empty">{t('credentials.loadFailed', { count: formatNumber(values.length, locale) })}</div> : <ResourceListStatusEmpty totalCount={values.length} normalLabel={t('status.active')} empty={values.length === 0 ? t('credentials.empty') : t('credentials.noFilterResults')} />)}{filteredValues.map((value) => {
        const memberships = credentialGroups.groups.filter((group) => group.member_ids.includes(value.key_id));
        const budget = credentialBudgetPresentation(value, locale, t);
        const technicalDetails = [
          t('credentials.identifierHint', { id: value.key_id }),
          t('credentials.principalHint', { principal: value.principal_external_id ?? t('common.unknownPrincipal') }),
          !tenant ? t('credentials.tenantHint', { tenant: value.tenant_external_id ?? '—' }) : '',
        ].filter(Boolean).join('\n');
        return <div className="managed-resource credential-compact-row" role="group" aria-label={value.alias} key={value.key_id}><div className="managed-resource-header">{canManage(value) && <Checkbox aria-label={t('credentials.selectCredential', { alias: value.alias })} checked={selectedKeys.includes(value.key_id)} disabled={Boolean(busy) || Boolean(visibleSecret) || (!selectedKeys.includes(value.key_id) && selectedKeys.length >= 100)} onChange={(_, data) => setSelectedKeys(current => data.checked ? [...new Set([...current, value.key_id])] : current.filter(id => id !== value.key_id))} />}<div className="credential-row-identity"><b title={`${value.alias}\n${technicalDetails}`}>{value.alias}</b><div className="credential-row-summary"><span className="credential-budget" title={budget.title}>{budget.text}</span>{!tenant && <span className="credential-group-summary">{t('credentials.tenant')}: {value.tenant_external_id ?? '—'}</span>}{memberships.length > 0 && <span className="credential-group-summary" title={memberships.map((group) => group.name).join('\n')}>{t('credentials.groupCount', { count: memberships.length })}</span>}</div></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="credential-generation">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div>
          <div className="credential-row-actions">
            <CredentialCopyAction value={value} canCopy={canManage(value)} busy={busy === `copy-${value.key_id}`} secretVisible={Boolean(visibleSecret)} t={t} onCopy={(next) => void copyCredential(next)} />
          <CredentialActionMenu label={locale.startsWith('zh') ? '更多操作' : 'More actions'} disabled={Boolean(busy) || Boolean(visibleSecret)} actions={[
            { id: 'rename', label: t('credentials.rename'), disabled: !canWrite, onSelect: () => { setRenaming(value.key_id); setAliasDraft(value.alias); } },
            { id: 'delete', label: t('credentials.delete'), disabled: !canManage(value), onSelect: () => deleteKeys([value.key_id]) },
            { id: 'limits', label: t('credentials.viewLimits'), disabled: !canReadLimits, onSelect: async () => {
              activateEditor('limits', value.key_id); const generation = editorGeneration.current;
              try { const snapshot = await api<KeyLimitSnapshot>(`/internal/v1/keys/${value.key_id}/limits`, token); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || editorGeneration.current !== generation) return; setLimitSnapshots(current => ({ ...current, [value.key_id]: snapshot })); }
              catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && editorGeneration.current === generation) setError(messageOf(reason, t('common.requestFailed'))); }
            } },
            { id: 'rotate', label: t('credentials.rotate'), disabled: !canWrite || value.status === 'revoked', onSelect: () => rotateCredential(value) },
            { id: 'credential', label: t('credentials.store'), disabled: !canManage(value) || value.status === 'revoked', onSelect: () => setEditingCredential(value.key_id) },
            { id: 'policy', label: t('credentials.editPolicy'), disabled: !canWrite || value.status === 'revoked', onSelect: () => setEditingPolicy(value.key_id) },
            { id: 'routing', label: t('credentials.routing'), disabled: !canWrite || value.status === 'revoked', onSelect: () => openRouting(value) },
            { id: 'grant', label: t('credentials.grant'), disabled: !canWrite || !value.account_id || value.status === 'revoked', description: !value.account_id ? t('credentials.accountMissing') : undefined, onSelect: () => setGranting(value.key_id) },
            ...(value.status === 'revoked' ? [] : [{ id: 'status', label: value.status === 'active' ? t('credentials.suspend') : t('credentials.resume'), disabled: !canWrite, onSelect: async () => {
              const nextStatus = value.status === 'active' ? 'suspended' : 'active';
              try { await api(`/internal/v1/keys/${value.key_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant) return; setMessage(t(nextStatus === 'active' ? 'credentials.resumed' : 'credentials.suspended', { alias: value.alias })); await load(); }
              catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant) setError(messageOf(reason, t('common.requestFailed'))); }
            } }]),
          ]} /></div>
        </div>})}</div>{keyListState === 'failed' ? <div className="load-more"><button type="button" className="secondary" onClick={() => void load()}>{t('credentials.retryLoad')}</button></div> : nextCursor && (keyListState === 'more' || keyListState === 'loading-more') && <div className="load-more"><button type="button" className="secondary" disabled={!canLoadMore} onClick={() => void loadMore()}>{loadingKeys ? t('common.loading') : t('credentials.loadMore')}</button></div>}</article>
    <CreateJourney title={activeCredential ? `${t('credentials.title')} · ${activeCredential.alias}` : t('credentials.createTitle')} description={formJourneyCopy(locale).credentialFlowHint} open={Boolean(workspace)} busy={Boolean(busy) || Boolean(visibleSecret)} onOpenChange={(open) => { editorGeneration.current += 1; if (!open) setCredentialValue(''); setWorkspace(open ? { kind: 'create' } : undefined); }}>
      {(error || routeError || routeGroups.error) && <div className="notice error" role="alert">{error || routeError || routeGroups.error}</div>}
      {activeCredential && credentialEditor(activeCredential)}
      <div hidden={workspace?.kind !== 'create'}>
      {createFormSchema ? <Form key={`${tenant}-${writeTenant}`} schema={localizeSchema(createFormSchema as RJSFSchema, locale)} formData={createCredentialDraft} onChange={({ formData }) => setCreateCredentialDraft(formData)} uiSchema={{ ...credentialCreateUiSchema, principal_external_id: { 'ui:help': t('credentials.principalHelp') } }} formContext={{ authorizationFields: <FormSection title={formJourneyCopy(locale).access} description={t('credentials.createRoutingHint')}><CredentialRouteAuthorization token={token} tenant={writeTenant} routes={routeOptions} groups={routeGroupOptions} routeIds={newRouteIds} groupIds={newRouteGroupIds} onRoutes={setNewRouteIds} onGroups={setNewRouteGroupIds} /></FormSection> }} fields={credentialFormFields} validator={validator} widgets={fluentFormWidgets} templates={credentialFormTemplates} onSubmit={async ({ formData }) => {
        if (!writeTenant) return;
        const operation = beginSecretOperation();
        if (!operation) return;
        const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant; const operationScopeGeneration = renderScope.current.generation;
        setBusy('create-credential'); setError(''); setMessage('');
        const controller = new AbortController();
        secretRequest.current?.abort();
        secretRequest.current = controller;
        try {
          const created = await api<{ key: string; key_id: string }>('/internal/v1/keys', operationToken, { ...secretResponseRequestPolicy, method: 'POST', body: JSON.stringify({ ...formData, tenant_external_id: operationWriteTenant, creation_source: 'manual', route_ids: newRouteIds, route_group_ids: newRouteGroupIds }), signal: controller.signal });
          if (!ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) return;
          setCreateCredentialDraft(undefined); setNewRouteIds([]); setNewRouteGroupIds([]); showSecret({ value: created.key, kind: 'issued', displayId: crypto.randomUUID() });
          setMessage(t(newRouteIds.length || newRouteGroupIds.length ? 'credentials.created' : 'credentials.createdNoRoutes')); await load();
        } catch (reason) {
          if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) setError(messageOf(reason, t('common.requestFailed')));
        } finally {
          if (secretRequest.current === controller) secretRequest.current = undefined;
          if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) finishSecretOperation(operation);
        }
      }}><div className="journey-actions"><p>{newRouteIds.length || newRouteGroupIds.length ? t('credentials.createRoutingHint') : formJourneyCopy(locale).draftNoRoutes}</p><Button appearance="primary" type="submit" disabled={!canWrite || Boolean(busy) || Boolean(visibleSecret)}>{busy === 'create-credential' ? t('common.loading') : t('credentials.create')}</Button></div></Form> : <div className="empty">{t('providers.schemaMissing')}</div>}
      </div>
    </CreateJourney>
  </section>{writeTenant && <section className="credential-group-workspace"><Disclosure title={t('groups.credential.title')}><GroupManager kind="credential" token={token} tenant={writeTenant} groups={credentialGroups.groups} resources={values.filter(canManage).map((value) => ({ value: value.key_id, label: value.alias, description: value.key_id }))} onChanged={credentialGroups.load} /></Disclosure></section>}</>;
}

const serviceCredentialStatisticsGroupId = 'statistics-and-requests';
const serviceCredentialScopeGroups = [
  { id: serviceCredentialStatisticsGroupId, title: '统计与请求', titleEn: 'Statistics and requests', scopes: [
    { value: 'metrics:read', label: '读取诊断指标', labelEn: 'Read diagnostic metrics', hint: '读取诊断与健康状态；不包含请求记录或用量分析。', hintEn: 'Read diagnostics and health status, not request records or usage analysis.' },
    { value: 'requests:read', label: '读取请求与用量', labelEn: 'Read requests and usage', hint: '读取请求记录、监控快照和用量分析；不允许写入。', hintEn: 'Read request records, monitoring snapshots, and usage analysis without write access.' },
  ] },
  { id: 'credentials-and-credits', title: '凭据与额度', titleEn: 'Credentials and credits', scopes: [
    { value: 'keys:read', label: '读取客户端凭据', labelEn: 'Read client credentials', hint: '查看客户端凭据及其配置。', hintEn: 'View client credentials and their configuration.' },
    { value: 'keys:write', label: '管理客户端凭据', labelEn: 'Manage client credentials', hint: '创建、更新或撤销客户端凭据。', hintEn: 'Create, update, or revoke client credentials.' },
    { value: 'credits:read', label: '读取额度', labelEn: 'Read credits', hint: '查看额度账户与余额。', hintEn: 'View credit accounts and balances.' },
    { value: 'credits:write', label: '管理额度', labelEn: 'Manage credits', hint: '修改额度与账户余额。', hintEn: 'Modify credits and account balances.' },
    { value: 'entitlements:read', label: '读取授权权益', labelEn: 'Read entitlements', hint: '查看租户授权权益。', hintEn: 'View tenant entitlements.' },
    { value: 'entitlements:write', label: '管理授权权益', labelEn: 'Manage entitlements', hint: '修改租户授权权益。', hintEn: 'Modify tenant entitlements.' },
  ] },
  { id: 'platform-configuration', title: '平台配置', titleEn: 'Platform configuration', scopes: [
    { value: 'providers:read', label: '读取提供商', labelEn: 'Read providers', hint: '查看提供商配置。', hintEn: 'View provider configuration.' },
    { value: 'providers:write', label: '管理提供商', labelEn: 'Manage providers', hint: '创建或修改提供商配置。', hintEn: 'Create or modify provider configuration.' },
    { value: 'plugins:read', label: '读取插件', labelEn: 'Read plugins', hint: '查看插件与其配置。', hintEn: 'View plugins and their configuration.' },
    { value: 'plugins:write', label: '管理插件', labelEn: 'Manage plugins', hint: '修改插件配置或生命周期。', hintEn: 'Modify plugin configuration or lifecycle.' },
    { value: 'routes:read', label: '读取模型路由', labelEn: 'Read model routes', hint: '查看模型路由配置。', hintEn: 'View model route configuration.' },
    { value: 'routes:write', label: '管理模型路由', labelEn: 'Manage model routes', hint: '创建或修改模型路由。', hintEn: 'Create or modify model routes.' },
    { value: 'prices:read', label: '读取价格', labelEn: 'Read prices', hint: '查看模型价格配置。', hintEn: 'View model pricing configuration.' },
    { value: 'prices:write', label: '管理价格', labelEn: 'Manage prices', hint: '修改模型价格配置。', hintEn: 'Modify model pricing configuration.' },
    { value: 'schemas:read', label: '读取配置 Schema', labelEn: 'Read configuration schemas', hint: '读取平台公开的配置 Schema。', hintEn: 'Read configuration schemas exposed by the platform.' },
    { value: 'upstreams:import:write', label: '导入上游账号', labelEn: 'Import upstream accounts', hint: '导入上游账号配置。', hintEn: 'Import upstream account configuration.' },
  ] },
  { id: 'system-and-operations', title: '系统与运维', titleEn: 'System and operations', scopes: [
    { value: 'generations:write', label: '创建生成任务', labelEn: 'Create generation jobs', hint: '提交图片或视频生成任务。', hintEn: 'Submit image or video generation jobs.' },
    { value: 'generations:quarantine:read', label: '读取隔离任务', labelEn: 'Read quarantined jobs', hint: '查看被隔离的生成任务。', hintEn: 'View quarantined generation jobs.' },
    { value: 'generations:reconcile', label: '对账生成任务', labelEn: 'Reconcile generation jobs', hint: '执行生成任务状态对账。', hintEn: 'Reconcile generation job status.' },
    { value: 'filter_assistant:execute', label: '运行筛选助手', labelEn: 'Run filter assistant', hint: '调用模型执行筛选，可能产生计费用量。', hintEn: 'Invoke models for filtering; this may incur billable usage.' },
    { value: 'oauth:write', label: '管理 OAuth 授权', labelEn: 'Manage OAuth authorization', hint: '发起或管理 OAuth 授权流程。', hintEn: 'Start or manage OAuth authorization flows.' },
    { value: 'service_tokens:read', label: '读取服务凭据', labelEn: 'Read service credentials', hint: '列出服务凭据及其权限范围。', hintEn: 'List service credentials and their scopes.' },
    { value: 'service_tokens:write', label: '管理服务凭据', labelEn: 'Manage service credentials', hint: '创建、复制、轮换或暂停服务凭据。', hintEn: 'Create, copy, rotate, or suspend service credentials.' },
    { value: 'tenants:read', label: '读取租户', labelEn: 'Read tenants', hint: '查看租户信息。', hintEn: 'View tenant information.' },
    { value: 'tenants:write', label: '管理租户', labelEn: 'Manage tenants', hint: '创建或修改租户。', hintEn: 'Create or modify tenants.' },
    { value: 'settlements:adjust', label: '调整结算记录', labelEn: 'Adjust settlements', hint: '执行结算记录调整。', hintEn: 'Adjust settlement records.' },
  ] },
] as const;

const serviceCredentialStatisticsScopes = ['metrics:read', 'requests:read'];
const supportedServiceCredentialScopes = new Set<string>(serviceCredentialScopeGroups.flatMap((group) => group.scopes.map((scope) => scope.value)));

export function serviceCredentialCreatePayload(name: string, scopes: string[], tenant: string) {
  const normalizedName = name.trim();
  const uniqueScopes = [...new Set(scopes)];
  if (!tenant || !normalizedName || new TextEncoder().encode(normalizedName).length > 120 || uniqueScopes.length === 0 || uniqueScopes.some((scope) => !supportedServiceCredentialScopes.has(scope))) return undefined;
  return { name: normalizedName, scopes: uniqueScopes, tenant_external_id: tenant };
}

function ServiceCredentialWorkspace({ token, tenant, writeTenant = tenant }: { token: string; tenant: string; writeTenant?: string }) {
  const { locale, t } = useI18n();
  const zh = locale === 'zh-CN';
  const { confirm, confirmationDialog } = useConfirmDialog([token, tenant, writeTenant]);
  const renderScope = useRef({ token, tenant, writeTenant, generation: 0 });
  if (renderScope.current.token !== token || renderScope.current.tenant !== tenant || renderScope.current.writeTenant !== writeTenant) {
    renderScope.current = { token, tenant, writeTenant, generation: renderScope.current.generation + 1 };
  }
  const [values, setValues] = useState<ServiceTokenView[]>([]);
  const [secret, setSecret] = useState<{ value: string; kind: 'issued' | 'revealed'; name?: string; copyFailedInitially?: boolean; displayId: string; scopeGeneration: number }>();
  const [busy, setBusy] = useState('');
  const [message, setMessage] = useState('');
  const [error, setError] = useState('');
  const [createName, setCreateName] = useState('');
  const [createScopes, setCreateScopes] = useState<string[]>([]);
  const [createAttempted, setCreateAttempted] = useState(false);
  const [createOpen, setCreateOpen] = useState(false);
  const loadSequence = useRef(0);
  const secretRequest = useRef<AbortController | undefined>(undefined);
  const secretOperation = useRef<symbol | undefined>(undefined);
  const visibleSecret = secret?.scopeGeneration === renderScope.current.generation ? secret : undefined;
  const secretRef = useRef(visibleSecret);
  secretRef.current = visibleSecret;
  const scopeRef = useRef({ token, tenant, writeTenant });
  scopeRef.current = { token, tenant, writeTenant };
  const ownsSecretScope = (operationToken: string, operationTenant: string, operationWriteTenant: string, operationScopeGeneration: number) =>
    renderScope.current.generation === operationScopeGeneration
    && scopeRef.current.token === operationToken
    && scopeRef.current.tenant === operationTenant
    && scopeRef.current.writeTenant === operationWriteTenant;
  const load = async () => {
    const sequence = ++loadSequence.current;
    const loadToken = token; const loadTenant = tenant;
    if (!loadToken) { setValues([]); return; }
    try {
      const all = await api<ServiceTokenView[]>('/internal/v1/service-tokens', loadToken);
      if (sequence !== loadSequence.current || scopeRef.current.token !== loadToken || scopeRef.current.tenant !== loadTenant) return;
      setValues(loadTenant ? all.filter((value) => value.tenant_external_id === loadTenant) : all); setError('');
    } catch (reason) { if (sequence === loadSequence.current && scopeRef.current.token === loadToken && scopeRef.current.tenant === loadTenant) setError(messageOf(reason, t('common.requestFailed'))); }
  };
  useEffect(() => {
    secretRequest.current?.abort(); secretRequest.current = undefined; secretOperation.current = undefined; secretRef.current = undefined;
    loadSequence.current += 1; setValues([]); setSecret(undefined); setBusy(''); setMessage(''); setError('');
    setCreateName(''); setCreateScopes([]); setCreateAttempted(false); setCreateOpen(false); void load();
    return () => { secretRequest.current?.abort(); };
  }, [token, tenant, writeTenant]);
  const statusFilter = useResourceListStatusFilter('service-credentials', tenant, values, (value) => (value.status ?? 'active') === 'active');
  const canManage = (value: ServiceTokenView) => Boolean(writeTenant) && (!value.tenant_external_id || value.tenant_external_id === writeTenant);
  const writePermissionReason = zh ? '需要服务凭据写入权限才能管理。' : 'Service credential write permission is required to manage.';
  const beginSecretOperation = () => {
    if (secretOperation.current || secretRef.current) return undefined;
    const operation = Symbol('service-credential-secret-operation');
    secretOperation.current = operation;
    return operation;
  };
  const finishSecretOperation = (operation: symbol) => {
    if (secretOperation.current !== operation) return;
    secretOperation.current = undefined;
    setBusy('');
  };
  const showSecret = (value: string, kind: 'issued' | 'revealed', name?: string, copyFailedInitially = false) => {
    const next = { value, kind, name, copyFailedInitially, displayId: crypto.randomUUID(), scopeGeneration: renderScope.current.generation };
    secretRef.current = next;
    setSecret(next);
  };
  const dismissSecret = () => {
    secretRef.current = undefined;
    setSecret(undefined);
  };
  const copyServiceCredential = async (value: ServiceTokenView) => {
    if (!canManage(value) || !token || !value.credential_copy_available || value.status !== 'active') return;
    const operation = beginSecretOperation();
    if (!operation) return;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant; const operationScopeGeneration = renderScope.current.generation;
    const current = () => ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration);
    const controller = new AbortController();
    secretRequest.current?.abort(); secretRequest.current = controller;
    setBusy(`copy-${value.service_id}`); setError(''); setMessage('');
    try {
      const result = await api<{ service_id: string; credential_generation: number; token: string }>(`/internal/v1/service-tokens/${encodeURIComponent(value.service_id)}/copy`, operationToken, { ...secretResponseRequestPolicy, method: 'POST', signal: controller.signal });
      if (!current()) return;
      if (result.service_id !== value.service_id || result.credential_generation !== value.credential_generation) throw new Error(t('common.requestFailed'));
      try {
        await navigator.clipboard.writeText(result.token);
        if (current()) setMessage(t('services.copySuccess', { name: value.name }));
      } catch {
        if (!current()) return;
        showSecret(result.token, 'revealed', value.name, true); setMessage(t('services.copyReady', { name: value.name }));
      }
    } catch (reason) {
      if (!controller.signal.aborted && current()) setError(reason instanceof ApiError && reason.status === 403 ? t('services.copyPermission') : reason instanceof ApiError && reason.status === 404 ? t('services.copyNeedsOriginal') : messageOf(reason, t('common.requestFailed')));
    } finally {
      if (secretRequest.current === controller) secretRequest.current = undefined;
      if (current()) finishSecretOperation(operation);
    }
  };
  const rotateServiceCredential = async (value: ServiceTokenView) => {
    if (!canManage(value) || value.status === 'revoked') return;
    const operation = beginSecretOperation();
    if (!operation) return;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant; const operationScopeGeneration = renderScope.current.generation;
    const controller = new AbortController();
    secretRequest.current?.abort(); secretRequest.current = controller;
    setBusy(`rotate-${value.service_id}`); setError(''); setMessage('');
    try {
      if (!await confirm(`${t('services.rotate')} · ${value.name}\n${value.service_id}`)) return;
      const result = await api<{ token: string }>(`/internal/v1/service-tokens/${value.service_id}/rotate`, operationToken, { ...secretResponseRequestPolicy, method: 'POST', headers: { 'Idempotency-Key': crypto.randomUUID() }, signal: controller.signal });
      if (!ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) return;
      showSecret(result.token, 'issued', value.name); setMessage(t('services.rotated', { name: value.name })); await load();
    } catch (reason) {
      if (!controller.signal.aborted && ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      if (secretRequest.current === controller) secretRequest.current = undefined;
      if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) finishSecretOperation(operation);
    }
  };
  const createServiceCredential = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setError(''); setMessage('');
    setCreateAttempted(true);
    const payload = serviceCredentialCreatePayload(createName, createScopes, writeTenant);
    if (!payload) return;
    const operation = beginSecretOperation();
    if (!operation) return;
    const operationToken = token; const operationTenant = tenant; const operationWriteTenant = writeTenant; const operationScopeGeneration = renderScope.current.generation;
    const controller = new AbortController();
    secretRequest.current?.abort(); secretRequest.current = controller;
    setBusy('create-service-credential');
    try {
      const created = await api<{ token: string }>('/internal/v1/service-tokens', operationToken, { ...secretResponseRequestPolicy, method: 'POST', body: JSON.stringify(payload), signal: controller.signal });
      if (!ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) return;
      showSecret(created.token, 'issued'); setMessage(t('services.created')); await load();
    } catch (reason) {
      if (!controller.signal.aborted && ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) setError(messageOf(reason, t('common.requestFailed')));
    } finally {
      if (secretRequest.current === controller) secretRequest.current = undefined;
      if (ownsSecretScope(operationToken, operationTenant, operationWriteTenant, operationScopeGeneration)) finishSecretOperation(operation);
    }
  };
  return <>{confirmationDialog}{visibleSecret && (visibleSecret.kind === 'issued'
    ? <IssuedCredential key={visibleSecret.displayId} value={visibleSecret.value} filename="service-credential.txt" onDismiss={dismissSecret} message={t('services.issued')} />
    : <RevealedCredential key={visibleSecret.displayId} value={visibleSecret.value} message={t('services.copyReady', { name: visibleSecret.name ?? '' })} copyFailedInitially={visibleSecret.copyFailedInitially} onDismiss={dismissSecret} onCopied={() => setMessage(t('services.copySuccess', { name: visibleSecret.name ?? '' }))} />)}<section className="management-layout">
    <article className="panel"><div className="panel-title"><div><h2>{t('services.title')}</h2><p className="muted">{t('services.description')}</p></div><ResourceListStatusFilterControl filter={statusFilter} inactiveLabel={t('resourceList.inactive')} /></div>{error && <div className="notice error" role="alert">{error}</div>}{message && <div className="notice success" role="status">{message}</div>}<div className="account-list credential-compact-list">{statusFilter.values.length === 0 && <ResourceListStatusEmpty totalCount={statusFilter.totalCount} normalLabel={t('status.active')} empty={t('services.empty')} />}{statusFilter.values.map((value) => {
      const technicalDetails = [
        t('services.identifierHint', { id: value.service_id }),
        t('services.scopesHint', { scopes: value.scopes.join(' · ') || '—' }),
        t('services.tenantHint', { tenant: tenantDisplayName(value.tenant_external_id ?? t('services.globalScope'), locale) }),
      ].join('\n');
      const copyReason = !token || !canManage(value)
        ? t('services.copyPermission')
        : value.status === 'revoked'
          ? t('credentials.copyRevoked')
          : value.status === 'suspended'
            ? t('credentials.copySuspended')
            : !value.credential_copy_available
              ? t('services.copyNeedsOriginal')
              : undefined;
      const manageReason = !canManage(value) ? writePermissionReason : undefined;
      const rotateReason = manageReason ?? (value.status === 'revoked' ? t('credentials.copyRevoked') : undefined);
      const nextStatus = value.status === 'active' ? 'suspended' : 'active';
      const statusLabel = value.status === 'active' ? t('services.suspend') : t('services.resume');
      return <div className="managed-resource credential-compact-row" key={value.service_id}><div className="managed-resource-header"><div><b title={technicalDetails}>{value.name}</b><div className="credential-row-summary"><span className="credential-budget" title={t('services.notBilledHint')}>{t('services.notBilled')}</span><span className="credential-group-summary">{tenantDisplayName(value.tenant_external_id ?? t('services.globalScope'), locale)} · {value.scopes.join(' · ') || t('common.none')}</span></div></div><div className="account-meta"><span className={`status ${value.status === 'active' ? 'ok' : value.status === 'revoked' ? 'bad' : 'pending'}`}>{enumLabel(t, 'status', value.status ?? 'active')}</span><span className="pill">{t('providers.generation')} {formatNumber(value.credential_generation, locale)}</span></div></div><div className="row-actions credential-row-actions"><DetailTooltip content={copyReason ?? t('credentials.copy')}><span tabIndex={copyReason ? 0 : undefined}><Button appearance="secondary" type="button" disabled={Boolean(copyReason) || Boolean(busy) || Boolean(visibleSecret)} onClick={() => void copyServiceCredential(value)}>{busy === `copy-${value.service_id}` ? t('common.loading') : t('credentials.copy')}</Button></span></DetailTooltip><DetailTooltip content={rotateReason ?? t('services.rotate')}><span tabIndex={rotateReason ? 0 : undefined}><Button appearance="secondary" type="button" disabled={Boolean(rotateReason) || Boolean(busy) || Boolean(visibleSecret)} onClick={() => void rotateServiceCredential(value)}>{busy === `rotate-${value.service_id}` ? t('common.loading') : t('services.rotate')}</Button></span></DetailTooltip>{value.status !== 'revoked' && <DetailTooltip content={manageReason ?? statusLabel}><span tabIndex={manageReason ? 0 : undefined}><Button appearance="secondary" type="button" disabled={Boolean(manageReason) || Boolean(busy)} onClick={async () => { setBusy(`status-${value.service_id}`); try { await api(`/internal/v1/service-tokens/${value.service_id}/status`, token, { method: 'PATCH', body: JSON.stringify({ status: nextStatus }) }); if (scopeRef.current.token !== token || scopeRef.current.tenant !== tenant || scopeRef.current.writeTenant !== writeTenant) return; setMessage(t(nextStatus === 'active' ? 'services.resumed' : 'services.suspended', { name: value.name })); await load(); } catch (reason) { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setError(messageOf(reason, t('common.requestFailed'))); } finally { if (scopeRef.current.token === token && scopeRef.current.tenant === tenant && scopeRef.current.writeTenant === writeTenant) setBusy(''); } } }>{busy === `status-${value.service_id}` ? t('common.loading') : statusLabel}</Button></span></DetailTooltip>}</div></div>;
    })}</div></article>
    <section className="panel create-resource"><Disclosure title={zh ? '创建服务凭据表单' : 'Create service credential form'} open={createOpen} onOpenChange={setCreateOpen}><form className="create-resource-body form-panel service-credential-create" noValidate onSubmit={(event) => void createServiceCredential(event)}>
      <p className="muted">{t('services.description')}</p>
      <Field label={zh ? '名称' : 'Name'} required validationState={createAttempted && (!createName.trim() || new TextEncoder().encode(createName.trim()).length > 120) ? 'error' : 'none'} validationMessage={createAttempted && !createName.trim() ? (zh ? '请输入服务凭据名称。' : 'Enter a service credential name.') : createAttempted && new TextEncoder().encode(createName.trim()).length > 120 ? (zh ? '名称不能超过 120 个 UTF-8 字节。' : 'Name must be 120 UTF-8 bytes or fewer.') : undefined}>
        <Input value={createName} onChange={(event) => setCreateName(event.target.value)} placeholder={zh ? '例如：只读统计集成' : 'e.g. Read-only analytics integration'} />
      </Field>
      <Field label={zh ? '租户范围' : 'Tenant scope'} required validationState={!writeTenant && createAttempted ? 'error' : 'none'} validationMessage={!writeTenant && createAttempted ? (zh ? '请先选择有写入权限的租户。' : 'Select a tenant with write access first.') : undefined}>
        <Input readOnly value={writeTenant} placeholder={zh ? '尚未选择租户' : 'No tenant selected'} />
      </Field>
      <fieldset className="service-credential-scopes">
        <legend>{zh ? '权限范围' : 'Permission scopes'} <span aria-hidden="true">*</span></legend>
        <div className="service-credential-scope-groups" style={{ display: 'grid', gap: 20, gridTemplateColumns: 'repeat(auto-fit, minmax(min(100%, 280px), 1fr))' }}>
          {serviceCredentialScopeGroups.map((group) => <section key={group.title} aria-label={zh ? group.title : group.titleEn} style={{ minWidth: 0 }}>
            <h3 style={{ margin: '0 0 10px', fontSize: 14 }}>{zh ? group.title : group.titleEn}</h3>
            {group.id === serviceCredentialStatisticsGroupId && <Button appearance="secondary" type="button" onClick={() => setCreateScopes([...serviceCredentialStatisticsScopes])}>{zh ? '仅选择只读统计权限' : 'Select read-only statistics only'}</Button>}
            <div className="service-credential-scope-options" style={{ display: 'grid', gap: 8, marginTop: 10 }}>
              {group.scopes.map((scope) => <div className="service-credential-scope-option" key={scope.value}>
                <DetailTooltip content={zh ? scope.hint : scope.hintEn}><span tabIndex={0} aria-label={`${scope.value}: ${zh ? scope.hint : scope.hintEn}`}>
                  <Checkbox label={<>{zh ? scope.label : scope.labelEn} <code>{scope.value}</code></>} aria-label={`${zh ? scope.label : scope.labelEn} (${scope.value})`} checked={createScopes.includes(scope.value)} onChange={(_, data) => setCreateScopes((current) => data.checked === true ? [...new Set([...current, scope.value])] : current.filter((value) => value !== scope.value))} />
                </span></DetailTooltip>
              </div>)}
            </div>
          </section>)}
        </div>
        {createAttempted && createScopes.length === 0 && <p className="field-error" role="alert">{zh ? '至少选择一项权限。' : 'Choose at least one permission scope.'}</p>}
      </fieldset>
      <DetailTooltip content={writeTenant ? t('services.create') : writePermissionReason}><span tabIndex={!writeTenant ? 0 : undefined}><Button appearance="primary" type="submit" aria-label={t('services.create')} disabled={!writeTenant || Boolean(busy) || Boolean(visibleSecret)}>{busy === 'create-service-credential' ? t('common.loading') : t('services.create')}</Button></span></DetailTooltip>
    </form></Disclosure></section>
  </section></>;
}


interface OperatorPageProps {
  token: string;
  /** Selected read scope; empty only when no tenant is available. */
  tenant: string;
  /** Explicit target for every create/update action. */
  writeTenant?: string;
}

export function ProvidersPage({ token, tenant, writeTenant, onOpenRequest, onOpenPricing, onOpenProxyGroups }: OperatorPageProps & { onOpenRequest?: (requestId: string) => void; onOpenPricing?: (tenant: string) => void; onOpenProxyGroups?: (accountId?: string) => void }) {
  const { t } = useI18n();
  const savedRefresh = useRef(false);
  const [callerReadFeedback, setCallerReadFeedback] = useState(false);
  // This page needs an acknowledged account-list refresh after OAuth creation.
  // The shared resource hook intentionally preserves its non-throwing semantics.
  const accountRead = useRef<{ scope: string; failed: boolean }>({ scope: '', failed: false });
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async (signal) => {
      const read = { scope: `${token}\0${tenant}`, failed: false };
      accountRead.current = read;
      try {
        const [providers, values] = await Promise.all([
          api<ProviderType[]>('/internal/v1/provider-types', token, { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
          api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token, { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
        ]);
        return { providers, values };
      } catch (reason) { read.failed = true; throw reason; }
    },
    t('common.requestFailed'),
  );
  // Statistics are independent and non-critical. Start them only after the
  // account directory is usable so their aggregation queries cannot contend
  // with the initial account/provider reads for the small control-plane pool.
  const statistics = useOperatorResource(
    Boolean(token) && resource.state.kind === 'ready', `${token}\0${tenant}`,
    async (signal) => {
      const now = Date.now();
      const [availability, windowResult] = await Promise.all([
        api<OperatorMonitoringSnapshot>(recentAvailabilityPath(tenant, now), token, { signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]) })
          .then((availabilitySnapshot) => {
            if (!availabilitySnapshot || !Array.isArray(availabilitySnapshot.top_upstream_models)) throw new Error(t('providers.availabilityUnavailable'));
            return { availabilitySnapshot, availabilityError: undefined };
          })
          .catch((reason) => ({ availabilitySnapshot: undefined, availabilityError: messageOf(reason, t('providers.availabilityUnavailable')) })),
        tenant ? api<UpstreamAvailabilityWindow>(upstreamAvailabilityPath(tenant, now), token, { signal: AbortSignal.any([signal, AbortSignal.timeout(15_000)]) })
          .then((availabilityWindow) => ({ availabilityWindow, windowError: undefined }))
          .catch((reason) => ({ availabilityWindow: undefined, windowError: messageOf(reason, t('providers.availabilityUnavailable')) }))
          : Promise.resolve({ availabilityWindow: undefined, windowError: t('providers.accountWindowSelectTenant') }),
      ]);
      return { ...availability, availabilityWindow: windowResult.availabilityWindow, availabilityError: windowResult.windowError ?? availability.availabilityError };
    },
    t('common.requestFailed'),
  );
  const availability = statistics.state.kind === 'ready' ? statistics.state.value : {
    availabilityError: statistics.state.kind === 'failed' ? statistics.state.message : undefined,
    availabilityLoading: statistics.state.kind === 'idle' || statistics.state.kind === 'loading',
  };
  const accountResource = resource.state.kind === 'ready' && resource.state.refreshError
    ? { ...resource.state, refreshError: callerReadFeedback ? undefined : t(savedRefresh.current ? 'providers.savedListUnavailable' : 'common.requestFailed') }
    : resource.state;
  return <ResourceBoundary resource={accountResource} scopeKey={`${token}\0${tenant}`} onRetry={() => void resource.reload()}>{({ providers, values }) =>
    <UpstreamProviders token={token} tenant={tenant} writeTenant={writeTenant} providers={providers} values={values} {...availability} onOpenRequest={onOpenRequest} onOpenPricing={onOpenPricing} onOpenProxyGroups={onOpenProxyGroups} onReadFeedbackOwnerChange={setCallerReadFeedback} onChanged={async (saved = false, caller = false) => {
      savedRefresh.current = saved;
      setCallerReadFeedback(caller);
      void statistics.reload();
      await resource.reload();
      if (accountRead.current.scope === `${token}\0${tenant}` && accountRead.current.failed) throw new AccountListRefreshError();
    }} />
  }</ResourceBoundary>;
}

export function PricingPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const [schemasRequested, setSchemasRequested] = useState(false);
  const resource = useOperatorResource(
    Boolean(token) && schemasRequested, token,
    (signal) => api<ConfigurationSchemas>('/internal/v1/schemas', token, { signal: AbortSignal.any([signal, AbortSignal.timeout(10_000)]) }),
    t('common.requestFailed'),
  );
  // Schemas are only needed by the manual editor, not the price tables.
  // Keep it mounted while schema discovery completes or fails.
  return <>
    {resource.state.kind === 'failed' && <div className="notice error" role="alert">{resource.state.message}</div>}
    <Pricing key={`${token}\0${tenant}`} token={token} tenant={tenant} writeTenant={writeTenant} schemas={resource.state.kind === 'ready' ? resource.state.value : undefined} schemasLoading={schemasRequested && (resource.state.kind === 'idle' || resource.state.kind === 'loading')} onRequestSchemas={() => setSchemasRequested(true)} />
  </>;
}

export function RoutesPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), `${token}\0${tenant}`,
    async () => {
      const [providers, upstreams] = await Promise.all([
        api<ProviderType[]>('/internal/v1/provider-types', token),
        api<UpstreamAccount[]>(`/internal/v1/upstreams${queryForTenant(tenant)}`, token),
      ]);
      return { providers, upstreams };
    },
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={`${token}\0${tenant}`}>{({ providers, upstreams }) =>
    <RouteWorkspace token={token} tenant={tenant} writeTenant={writeTenant} providers={providers} upstreams={upstreams} />
  }</ResourceBoundary>;
}

export function CredentialsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  const { t } = useI18n();
  const resource = useOperatorResource(
    Boolean(token), token,
    () => api<ConfigurationSchemas>('/internal/v1/schemas', token),
    t('common.requestFailed'),
  );
  return <ResourceBoundary resource={resource.state} scopeKey={token}>{(schemas) =>
    <CredentialWorkspace token={token} tenant={tenant} writeTenant={writeTenant} createSchema={schemas.key_create} policySchema={schemas.key_policy} />
  }</ResourceBoundary>;
}

export function ServiceCredentialsPage({ token, tenant, writeTenant }: OperatorPageProps) {
  return <ServiceCredentialWorkspace token={token} tenant={tenant} writeTenant={writeTenant} />;
}
