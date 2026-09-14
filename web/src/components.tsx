import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { Popover, PopoverSurface, PopoverTrigger, PortalMountNodeProvider } from '@fluentui/react-components';
import { CopyButton } from './CopyButton.js';
import type { RequestView, StatsBucket } from './types.js';
import { useI18n } from './i18n.js';
import { formatCurrency, formatCurrencyDisplay, formatDurationDisplay, formatMetricDisplay, formatMetricNumber, formatMilliseconds, formatNumber } from './format.js';
import { useAnchoredPopover } from './useAnchoredPopover.js';
import { DetailTooltip } from './design-system';
import { RequestStatus } from './RequestStatus';
import { averageRequestOutputTps, generationRequestOutputTps, nonCachedRequestInput, requestCredentialLabel, requestIsPending } from './requestTablePresentation';

export function Shell({ children, operator = false }: { children: ReactNode; operator?: boolean }) {
  const { locale, setLocale, t } = useI18n();
  const [theme, setTheme] = useState<'dark' | 'light'>(() =>
    document.documentElement.dataset.theme === 'light' ? 'light' : 'dark',
  );
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem('mtc-theme', theme);
    document.querySelector<HTMLMetaElement>('meta[name="theme-color"]')?.setAttribute('content', theme === 'light' ? '#f4f7f5' : '#071014');
  }, [theme]);
  return (
    <div className="app-shell">
      <aside className="rail">
        <div className="brand-mark"><img src="/ui-assets/token-center-icon-32.png" alt="Memeloop Token Center" /></div>
        <div className="rail-line" />
        <button
          className="theme-toggle"
          type="button"
          aria-label={theme === 'dark' ? t('theme.light') : t('theme.dark')}
          title={theme === 'dark' ? t('theme.light') : t('theme.dark')}
          onClick={() => setTheme((current) => current === 'dark' ? 'light' : 'dark')}
        >
          {theme === 'dark' ? '☀' : '☾'}
        </button>
        <button className="language-toggle" type="button" aria-label={locale === 'zh-CN' ? t('language.en') : t('language.zh')} onClick={() => setLocale(locale === 'zh-CN' ? 'en' : 'zh-CN')} title={locale === 'zh-CN' ? t('language.en') : t('language.zh')}>
          {locale === 'zh-CN' ? 'EN' : '中'}
        </button>
        <div className="rail-label">{t(operator ? 'shell.operator' : 'shell.selfService')}</div>
      </aside>
      <main className="main">
        <div className="mobile-controls">
          <button className="theme-toggle" type="button" aria-label={theme === 'dark' ? t('theme.light') : t('theme.dark')} onClick={() => setTheme((current) => current === 'dark' ? 'light' : 'dark')}>{theme === 'dark' ? '☀' : '☾'}</button>
          <button className="language-toggle" type="button" aria-label={locale === 'zh-CN' ? t('language.en') : t('language.zh')} onClick={() => setLocale(locale === 'zh-CN' ? 'en' : 'zh-CN')}>{locale === 'zh-CN' ? 'EN' : '中'}</button>
        </div>
        {children}
      </main>
    </div>
  );
}

export function Metric({ label, labelContent, value, tone }: { label: string; labelContent?: ReactNode; value: ReactNode; tone?: string }) {
  return (
    <article className={`metric ${tone ?? ''}`}>
      <span className="metric-label">{labelContent ?? label}</span>
      <strong className="metric-value">{value}</strong>
    </article>
  );
}

export function NumberMetric({
  label,
  value,
  tone,
  showCompact = true,
}: {
  label: string;
  value: number | null | undefined;
  tone?: string;
  showCompact?: boolean;
}) {
  const { locale } = useI18n();
  const formatted = formatMetricNumber(value, locale);
  return <Metric label={label} tone={tone} value={
    <span className={`metric-number${showCompact && formatted.compact ? ' has-compact' : ''}`}>
      <span className="metric-exact" title={formatted.text}>{showCompact && formatted.compact ? formatted.compact : formatted.text}</span>
    </span>
  } />;
}

export function Buckets({ values, onSelect }: { values: StatsBucket[]; onSelect?: (value: StatsBucket) => void }) {
  const { locale, t } = useI18n();
  const maximum = Math.max(1, ...values.map((value) => value.requests));
  if (!values.length) return <div className="empty">{t('common.noData')}</div>;
  return (
    <div className="bucket-list">
      {values.map((value) => (
        <div className="bucket" key={value.name}>
          {onSelect
            ? <button className="bucket-heading" type="button" onClick={() => onSelect(value)} aria-label={t('request.filterBy', { name: value.name })}><b>{value.name}</b><span>{t('request.count', { count: formatMetricDisplay(value.requests, locale).text })} · {formatMetricDisplay(value.input_tokens + value.output_tokens, locale).text} {t('request.tokenUnit')}</span></button>
            : <div className="bucket-heading"><b>{value.name}</b><span>{t('request.count', { count: formatMetricDisplay(value.requests, locale).text })} · {formatMetricDisplay(value.input_tokens + value.output_tokens, locale).text} {t('request.tokenUnit')}</span></div>}
          <div className="bar"><i style={{ width: `${(value.requests / maximum) * 100}%` }} /></div>
        </div>
      ))}
    </div>
  );
}

function requestTokenBreakdown(request: RequestView) {
  const cachedInputTokens = request.cached_input_tokens;
  const cacheWriteTokens = request.cache_write_tokens;
  if (cachedInputTokens === undefined || cacheWriteTokens === undefined) return undefined;
  return {
    input: Math.max(0, request.input_tokens - cachedInputTokens - cacheWriteTokens),
    cached: cachedInputTokens,
    cacheWrite: cacheWriteTokens,
    output: request.output_tokens,
  };
}

function requestTokenDetails(request: RequestView, locale: Parameters<typeof formatNumber>[1], t: ReturnType<typeof useI18n>['t']) {
  const breakdown = requestTokenBreakdown(request);
  if (breakdown) return t('request.tokenBreakdown', { input: formatNumber(breakdown.input, locale), cached: formatNumber(breakdown.cached, locale), cacheWrite: formatNumber(breakdown.cacheWrite, locale), output: formatNumber(breakdown.output, locale) });
  return [
    `${t('usage.inputTokens')}: ${formatNumber(request.input_tokens, locale)}`,
    `${t('usage.outputTokens')}: ${formatNumber(request.output_tokens, locale)}`,
    request.cached_input_tokens !== undefined ? `${t('usage.cachedTokens')}: ${formatNumber(request.cached_input_tokens, locale)}` : '',
    request.cache_write_tokens !== undefined ? `${t('usage.cacheWriteTokens')}: ${formatNumber(request.cache_write_tokens, locale)}` : '',
  ].filter(Boolean).join(' · ');
}

function RequestTokenSummary({ request }: { request: RequestView }) {
  const { locale, t } = useI18n();
  const pending = requestIsPending(request);
  const tokenDisplay = formatMetricDisplay(request.input_tokens + request.output_tokens, locale);
  const tokenDetails = pending ? t('request.pendingUsage') : requestTokenDetails(request, locale, t);
  const uncachedInput = nonCachedRequestInput(request);
  return <><DetailTooltip content={tokenDetails}><span className="request-value-info request-token-total" aria-label={pending ? t('request.pendingUsage') : `${t('request.totalTokens')} ${tokenDisplay.text} (${tokenDetails})`} tabIndex={0}>{pending ? t('common.running') : <>{t('request.totalTokens')} <span>{tokenDisplay.text}</span></>}</span></DetailTooltip>
    {pending ? <DetailTooltip content={t('request.pendingUsage')}><span className="request-token-pending" tabIndex={0}>{locale === 'zh-CN' ? '用量待结算' : 'Usage pending settlement'}</span></DetailTooltip> : <span className="request-token-primary"><span>{t('request.uncachedInput')} <b>{uncachedInput === null ? <DetailTooltip content={t('request.cacheMissing')}><span tabIndex={0}>{t('request.usageUnknown')}</span></DetailTooltip> : formatMetricDisplay(uncachedInput, locale).text}</b></span><span>{t('request.outputTokens')} <b>{formatMetricDisplay(request.output_tokens, locale).text}</b></span></span>}
  </>;
}

function RequestIdentifier({ requestId, compact = false }: { requestId: string; compact?: boolean }) {
  const { t } = useI18n();
  return <span className={`request-id-control${compact ? ' compact' : ''}`} title={compact ? requestId : undefined} aria-label={compact ? `${t('request.request')}: ${requestId}` : undefined}>
    <code title={requestId}>{requestId}</code>
    <CopyButton value={requestId} label={t('common.copy')} />
  </span>;
}

function recordedCurrency(request: RequestView, fallbackCurrency?: string) {
  // A received null is historical data: generation and archive-only rows do
  // not inherit today's credential currency. Older streams predate the field
  // and alone may use the local credential as a presentation fallback.
  return request.currency === undefined ? fallbackCurrency : request.currency;
}

function RequestSessionMetadata({ value }: { value: string }) {
  const { t } = useI18n();
  const id = useId();
  const [open, setOpen] = useState(false);
  const { anchor, panel, position } = useAnchoredPopover(open);
  const close = (restoreFocus = false) => {
    setOpen(false);
    if (restoreFocus) anchor.current?.focus();
  };

  return <>
    <button ref={anchor} type="button" className="secondary request-session-metadata-trigger" aria-haspopup="dialog" aria-expanded={open} aria-controls={`${id}-popover`} onClick={() => open ? close() : setOpen(true)}>{t('request.sessionMetadata')}</button>
    {open && <section ref={panel} id={`${id}-popover`} className="request-session-metadata-popover" popover="auto" style={position} role="dialog" aria-modal="false" aria-label={t('request.sessionMetadata')} onKeyDown={(event) => {
      if (event.key !== 'Escape') return;
      event.preventDefault();
      event.stopPropagation();
      close(true);
    }} onToggle={(event) => {
      if (event.target === event.currentTarget && event.newState === 'closed') setOpen(false);
    }}>
      <code>{value}</code>
      <CopyButton value={value} label={t('common.copy')} />
    </section>}
  </>;
}

/**
 * The raw-request API deliberately exposes only fields that were durably
 * recorded for the request. Keep its diagnostics in one surface so the self
 * portal and operator request drawer cannot drift or turn absent telemetry
 * into inferred values.
 */
function RequestOutputRate({ request }: { request: RequestView }) {
  const { locale, t } = useI18n();
  const generation = generationRequestOutputTps(request);
  const rate = generation ?? averageRequestOutputTps(request);
  const label = t(generation === null ? 'request.averageTps' : 'request.generationTps');
  const explanation = t(generation === null ? 'request.averageTpsHint' : 'request.generationTpsHint');
  const first = request.first_output_ms;
  const wait = typeof first === 'number' && Number.isFinite(first) && first >= 0
    ? `${t('request.firstOutputWait')}: ${formatMilliseconds(first, locale)}` : t('request.firstOutputMissing');
  const hint = `${rate === null ? t(requestIsPending(request) ? 'request.tpsRunning' : 'request.tpsMissing') : explanation} ${wait}`;
  return <DetailTooltip content={hint}><span tabIndex={0} aria-label={`${label}: ${rate === null ? t('request.usageUnknown') : formatNumber(rate, locale, 2)}. ${hint}`}><small>{label}</small> {rate === null ? '—' : formatNumber(rate, locale, 2)}</span></DetailTooltip>;
}

/** Technical values remain available on touch and keyboard without occupying a column. */
function RequestMetadata({ label, fields }: { label: string; fields: [string, string | null | undefined][] }) {
  const { locale, t } = useI18n();
  const [open, setOpen] = useState(false);
  const recorded = fields.filter((field): field is [string, string] => Boolean(field[1]));
  if (!recorded.length) return <span>{label}</span>;
  return <Popover positioning="below-start" trapFocus open={open} onOpenChange={(_, data) => setOpen(data.open)}>
    <PopoverTrigger disableButtonEnhancement><button type="button" className="table-link request-metadata-trigger" onKeyDown={(event) => { if (event.key === 'Escape' && open) event.stopPropagation(); }} aria-label={`${label} · ${locale === 'zh-CN' ? '详细信息' : 'Details'}`}>{label}</button></PopoverTrigger>
    <PopoverSurface className="request-metadata-surface" aria-label={label} onKeyDown={(event) => { if (event.key === 'Escape') event.stopPropagation(); }}>
      <dl>{recorded.map(([name, value]) => <div key={name}><dt>{name}</dt><dd><code>{value}</code><CopyButton value={value} label={`${t('common.copy')} ${name}`} /></dd></div>)}</dl>
    </PopoverSurface>
  </Popover>;
}

function RequestCompaction({ request }: { request: RequestView }) {
  const { locale } = useI18n();
  if (request.compaction !== true) return null;
  return <DetailTooltip content={locale === 'zh-CN' ? '客户端明确标记了本次请求用于上下文压缩。' : 'The client explicitly marked this request as context compaction.'}><span className="request-compaction" tabIndex={0}>{locale === 'zh-CN' ? '上下文压缩' : 'Context compaction'}</span></DetailTooltip>;
}

export function RequestDiagnostics({
  request,
  currency,
  onOpenSession,
  upstreamName,
}: {
  request: RequestView;
  currency?: string;
  onOpenSession?: (sessionId: string) => void;
  upstreamName?: string;
}) {
  const { locale, t } = useI18n();
  const pending = requestIsPending(request);
  const currencyForRequest = recordedCurrency(request, currency);
  const context = request.session_context;
  const sessionLabel = context?.session_name ?? t('sessions.reportedNameMissing');
  const zh = locale === 'zh-CN';
  const missing = zh ? '未记录' : 'Not recorded';
  const credential = requestCredentialLabel(request, undefined);
  const credentialLabel = 'label' in credential ? credential.label : t(credential.key);
  const duration = formatDurationDisplay(request.duration_ms, locale);
  const timingDetails = `${t('request.receivedAt')}: ${new Date(request.created_at).toLocaleString(locale)} · ${t('request.completedAt')}: ${request.completed_at == null ? (pending ? (zh ? '尚未结束' : 'Still running') : missing) : new Date(request.completed_at).toLocaleString(locale)} · ${t('request.duration')}: ${duration.title ?? missing}`;
  const cost = currencyForRequest ? formatCurrencyDisplay(request.cost, currencyForRequest, locale) : { text: missing };
  const settlement = <DetailTooltip content={t('request.pendingUsage')}><span tabIndex={0}>{zh ? '待结算' : 'Awaiting settlement'}</span></DetailTooltip>;

  return <div className="request-diagnostics request-detail-surface request-detail-summary">
    <section className="request-detail-group request-detail-primary" aria-label={zh ? '模型与用量' : 'Model and usage'}>
      <div className="request-detail-wide"><b>{t('request.model')}</b><RequestMetadata label={request.model} fields={[[t('request.routeId'), request.route_id], [t('request.protocol'), request.protocol]]} /><RequestCompaction request={request} /></div>
      <div className="request-detail-wide"><b>{zh ? '凭据' : 'Credential'}</b><RequestMetadata label={credentialLabel} fields={[[zh ? '凭据 ID' : 'Credential ID', request.credential_identity?.key_id], [zh ? '主体' : 'Principal', request.credential_identity?.principal_external_id], [zh ? '租户' : 'Tenant', request.credential_identity?.tenant_external_id]]} /></div>
      <div className="request-token-cell"><b>{t('request.tokens')}</b><RequestTokenSummary request={request} /></div>
      <div><b>{t('request.cost')}</b>{pending ? settlement : <DetailTooltip content={cost.title ?? missing}><span tabIndex={0}>{cost.text}</span></DetailTooltip>}</div>
    </section>
    <section className="request-detail-group" aria-label={zh ? '交付与性能' : 'Delivery and performance'}>
      <div><b>{zh ? '最终上游' : 'Final upstream'}</b><RequestMetadata label={upstreamName || (request.upstream_account_id ? (zh ? '未命名上游' : 'Unnamed upstream') : missing)} fields={[[t('request.upstreamId'), request.upstream_account_id]]} /></div>
      <div><b>{t('request.status')}</b><RequestStatus request={request} /></div>
      <div><b>{t('request.request')}</b><RequestMetadata label={zh ? '记录标识' : 'Record identifiers'} fields={[[zh ? '请求 ID' : 'Request ID', request.request_id]]} /></div>
      {request.error_code && <div className="request-detail-wide"><b>{t('request.error')}</b>{request.error_code}</div>}
      <div><b>{t('request.duration')}</b><DetailTooltip content={timingDetails}><span className="request-detail-timing" tabIndex={0}>{duration.text === '—' ? missing : duration.text}</span></DetailTooltip></div>
      <div><RequestOutputRate request={request} /></div>
    </section>
    {context && <section className="request-detail-group request-detail-session" aria-label={t('request.session')}><div className="request-detail-wide"><b>{t('request.session')}</b>
      {context.association === 'confirmed'
        ? context.session_id && onOpenSession
          ? <button type="button" className="table-link request-diagnostic-session" onClick={() => onOpenSession(context.session_id!)}>{sessionLabel}</button>
          : <span>{sessionLabel}</span>
        : <span className="request-session-unlinked">{t('sessions.unlinkedRequests')}</span>}
      <RequestMetadata label={zh ? '会话信息' : 'Session details'} fields={[[zh ? '会话 ID' : 'Session ID', context.session_id], [zh ? '任务类型' : 'Task kind', context.task_kind], [zh ? '代理 ID' : 'Agent ID', context.agent_id]]} />
    </div></section>}
  </div>;
}

export function RequestTable({
  requests,
  onSelect,
  onOpenSession,
  currency,
  credentialAlias,
  upstreamNames,
}: {
  requests: RequestView[];
  onSelect?: (request: RequestView) => void;
  onOpenSession?: (sessionId: string) => void;
  currency?: string;
  credentialAlias?: string;
  upstreamNames?: ReadonlyMap<string, string>;
}) {
  const { locale, t } = useI18n();
  if (!requests.length) return <div className="empty">{t('common.noRequests')}</div>;
  const showsSession = requests.some((request) => request.session_context !== undefined);
  const copy = {
    tpsHint: `${t('request.generationTpsHint')} ${t('request.averageTpsHint')}`,
    pendingUsage: t('request.pendingUsage'),
  };
  return (
    <div className="table-scroll request-table-scroll" role="region" aria-label={t('request.table')} tabIndex={0}>
      <table className="request-table">
        <thead><tr><th>{t('request.receivedAt')}</th><th>{t('self.credential')}</th><th>{t('request.model')}</th><th>{t('request.tokens')}</th><th>{t('request.cost')}</th>{showsSession && <th>{t('request.session')}</th>}<th>{t('request.status')}</th><th>{t('request.duration')}</th><th><DetailTooltip content={copy.tpsHint}><span tabIndex={0}>TPS</span></DetailTooltip></th>{onSelect && <th><span className="visually-hidden">{t('request.actions')}</span></th>}</tr></thead>
        <tbody>
          {requests.map((request) => {
            const context = request.session_context;
            const sessionLabel = context?.session_name
              ?? (context?.association === 'confirmed' ? t('sessions.reportedNameMissing') : t('sessions.unlinkedRequests'));
            const sessionMeta = [context?.task_kind, context?.agent_id].filter(Boolean).join(' · ');
            const currencyForRequest = recordedCurrency(request, currency);
            const technicalSummary = [
              `${t('request.protocol')}: ${request.protocol}`,
              request.upstream_account_id
                ? `${t('request.upstreamId')}: ${upstreamNames?.get(request.upstream_account_id) ? `${upstreamNames.get(request.upstream_account_id)} (${request.upstream_account_id})` : request.upstream_account_id}`
                : '',
              request.route_id ? `${t('request.routeId')}: ${request.route_id}` : '',
            ].filter(Boolean).join(' · ');
            const durationSummary = request.completed_at != null
              ? `${t('request.completedAt')}: ${new Date(request.completed_at).toLocaleString(locale)}`
              : '';
            const pending = requestIsPending(request);
            const cost = pending ? { text: '—', title: copy.pendingUsage } : currencyForRequest ? formatCurrencyDisplay(request.cost, currencyForRequest, locale) : { text: '—' };
            const duration = formatDurationDisplay(request.duration_ms, locale);
            const credential = requestCredentialLabel(request, credentialAlias);
            const credentialLabel = 'label' in credential ? credential.label : t(credential.key);
            const credentialDetails = request.credential_identity ? `${request.credential_identity.key_id} · ${request.credential_identity.principal_external_id}` : credentialLabel;
            const upstreamName = request.upstream_account_id ? upstreamNames?.get(request.upstream_account_id) : undefined;
            return <tr key={request.request_id}>
              <td className="request-time-cell" data-label={t('request.receivedAt')}><time>{new Date(request.created_at).toLocaleString(locale)}</time><RequestIdentifier requestId={request.request_id} compact /></td>
              <td className="request-credential-cell" data-label={t('self.credential')}><DetailTooltip content={credentialDetails}><strong tabIndex={0}>{credentialLabel}</strong></DetailTooltip></td>
              <td className="request-model-cell"><DetailTooltip content={technicalSummary}><span className="request-routing-info" tabIndex={0}><code>{request.model}</code>{upstreamName && <small className="request-upstream-name">{upstreamName}</small>}</span></DetailTooltip><RequestCompaction request={request} /></td>
              <td className="request-token-cell" data-label={t('request.tokens')}><RequestTokenSummary request={request} /></td>
              <td className="request-cost-cell" data-label={t('request.cost')}><span className="request-value-info" title={cost.title} aria-label={cost.title ? `${cost.text} (${cost.title})` : undefined} tabIndex={cost.title ? 0 : undefined}>{cost.text}</span></td>
              {showsSession && <td className="request-session-cell" data-label={t('request.session')}>
                {!context
                  ? '—'
                  : context.association === 'confirmed'
                    ? context.session_id && onOpenSession
                      ? <button type="button" className="table-link" onClick={() => onOpenSession(context.session_id!)}>{sessionLabel}</button>
                      : <span className="request-session-name">{sessionLabel}</span>
                    : <span className="request-session-unlinked">{t('sessions.unlinkedRequests')}</span>}
                {sessionMeta && <RequestSessionMetadata value={sessionMeta} />}
              </td>}
              <td className="request-status-cell" data-label={t('request.status')}><RequestStatus request={request} />{request.error_code && <span className="visually-hidden">{request.error_code}</span>}</td>
              <td className="request-duration-cell" data-label={t('request.duration')}><span className="request-duration-info" title={[duration.title, durationSummary].filter(Boolean).join(' · ') || undefined} aria-label={[duration.text, duration.title, durationSummary].filter(Boolean).join(' · ') || undefined} tabIndex={duration.title || durationSummary ? 0 : undefined}>{duration.text}</span></td>
              <td className="request-tps-cell" data-label="TPS"><RequestOutputRate request={request} /></td>
              {onSelect && <td className="request-actions-cell"><button className="secondary table-action" type="button" onClick={() => onSelect(request)} aria-label={t('request.openDetail', { model: request.model })}>{t('request.inspect')}</button></td>}
            </tr>
          })}
        </tbody>
      </table>
    </div>
  );
}

export function DrawerFrame({
  title,
  eyebrow,
  onClose,
  children,
}: {
  title: string;
  eyebrow: string;
  onClose: () => void;
  children: ReactNode;
}) {
  const { t } = useI18n();
  const titleId = useId();
  const drawerRef = useRef<HTMLElement>(null);
  // Owned floating content must stay inside the modal subtree. The default
  // body portal is background content and is intentionally made inert below.
  const [portalMountNode, setPortalMountNode] = useState<HTMLDivElement | null>(null);
  const previousFocus = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  useEffect(() => {
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    drawerRef.current?.focus();
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    const restoredBackground = new Map<HTMLElement, { inert: boolean; ariaHidden: string | null }>();
    const branches: Array<{ parent: HTMLElement; branch: HTMLElement }> = [];
    let branch: HTMLElement | null = drawerRef.current?.parentElement ?? null;
    while (branch?.parentElement) {
      const parent: HTMLElement = branch.parentElement;
      branches.push({ parent, branch });
      branch = parent;
      if (parent === document.body) break;
    }
    const restore = (element: HTMLElement, previous: { inert: boolean; ariaHidden: string | null }) => {
      element.inert = previous.inert;
      if (previous.ariaHidden === null) element.removeAttribute('aria-hidden');
      else element.setAttribute('aria-hidden', previous.ariaHidden);
    };
    const isolateBackground = () => {
      const siblings = new Set<HTMLElement>();
      for (const { parent, branch } of branches) for (const sibling of Array.from(parent.children)) {
        if (sibling instanceof HTMLElement && sibling !== branch && sibling.tagName !== 'SCRIPT') siblings.add(sibling);
      }
      for (const [element, previous] of restoredBackground) if (!siblings.has(element)) {
        restore(element, previous);
        restoredBackground.delete(element);
      }
      for (const element of siblings) if (!restoredBackground.has(element)) {
        restoredBackground.set(element, { inert: element.inert, ariaHidden: element.getAttribute('aria-hidden') });
        element.inert = true;
        element.setAttribute('aria-hidden', 'true');
      }
    };
    isolateBackground();
    // Only direct sibling insertions/removals matter: descendants inherit inert.
    // No attribute observation, and drawer-owned portals are never siblings here.
    const backgroundObserver = new MutationObserver(isolateBackground);
    for (const { parent } of branches) backgroundObserver.observe(parent, { childList: true });
    const keydown = (event: KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        onCloseRef.current();
        return;
      }
      if (event.key !== 'Tab' || !drawerRef.current) return;
      const focusable = Array.from(drawerRef.current.querySelectorAll<HTMLElement>('button:not([disabled]), a[href], input:not([disabled]), select:not([disabled]), textarea:not([disabled]), [contenteditable="true"], [tabindex]:not([tabindex="-1"])'))
        .filter((element) => !element.inert && element.getAttribute('aria-hidden') !== 'true' && element.getClientRects().length > 0);
      if (!focusable.length) {
        event.preventDefault();
        drawerRef.current.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && (document.activeElement === first || !drawerRef.current.contains(document.activeElement))) { event.preventDefault(); last.focus(); }
      else if (!event.shiftKey && (document.activeElement === last || !drawerRef.current.contains(document.activeElement))) { event.preventDefault(); first.focus(); }
    };
    document.addEventListener('keydown', keydown);
    return () => {
      document.removeEventListener('keydown', keydown);
      backgroundObserver.disconnect();
      document.body.style.overflow = previousOverflow;
      for (const [element, previous] of restoredBackground) restore(element, previous);
      restoredBackground.clear();
      if (previousFocus.current?.isConnected) previousFocus.current.focus();
    };
  }, []);
  return <div className="drawer-backdrop" onMouseDown={(event) => { if (event.currentTarget === event.target) onCloseRef.current(); }}>
    <aside className="drawer" ref={drawerRef} role="dialog" aria-modal="true" aria-labelledby={titleId} tabIndex={-1}>
      <button className="close" type="button" onClick={() => onCloseRef.current()} aria-label={t('common.close')}>×</button>
      <span className="eyebrow">{eyebrow}</span>
      <h2 id={titleId}>{title}</h2>
      <div ref={setPortalMountNode} className="drawer-owned-portals" />
      {portalMountNode && <PortalMountNodeProvider value={portalMountNode}>{children}</PortalMountNodeProvider>}
    </aside>
  </div>;
}
