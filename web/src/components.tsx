import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
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

export function Metric({ label, value, tone }: { label: string; value: ReactNode; tone?: string }) {
  return (
    <article className={`metric ${tone ?? ''}`}>
      <span className="metric-label">{label}</span>
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
  return <small>{requestTokenDetails(request, locale, t)}</small>;
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
  const sessionMetadata = [context?.task_kind, context?.agent_id].filter(Boolean).join(' · ');

  return <div className="request-diagnostics request-detail-surface">
    <span><b>{t('request.request')}</b><RequestIdentifier requestId={request.request_id} /></span>
    <span><b>{t('request.receivedAt')}</b>{new Date(request.created_at).toLocaleString(locale)}</span>
    <span><b>{t('request.completedAt')}</b>{request.completed_at === null || request.completed_at === undefined ? '—' : new Date(request.completed_at).toLocaleString(locale)}</span>
    <span><b>{t('request.status')}</b><RequestStatus request={request} /></span>
    <span><b>{t('request.protocol')}</b>{request.protocol}</span>
    <span><b>{t('request.duration')}</b>{formatMilliseconds(request.duration_ms, locale)}</span>
    <span><RequestOutputRate request={request} /></span>
    <span><b>{t('request.upstreamId')}</b>{upstreamName && <small>{upstreamName}</small>}{request.upstream_account_id ?? '—'}</span>
    <span><b>{t('request.routeId')}</b>{request.route_id ?? '—'}</span>
    <span><b>{t('request.tokens')}</b>{pending ? <DetailTooltip content={t('request.pendingUsage')}><span tabIndex={0}>{locale === 'zh-CN' ? '待结算' : 'Awaiting settlement'}</span></DetailTooltip> : <>{formatNumber(request.input_tokens + request.output_tokens, locale)}<RequestTokenSummary request={request} /></>}
    </span>
    <span><b>{t('request.cost')}</b>{pending ? <DetailTooltip content={t('request.pendingUsage')}><span tabIndex={0}>{locale === 'zh-CN' ? '待结算' : 'Awaiting settlement'}</span></DetailTooltip> : currencyForRequest ? formatCurrency(request.cost, currencyForRequest, locale) : '—'}</span>
    <span><b>{t('request.error')}</b>{request.error_code ?? '—'}</span>
    {context && <span><b>{t('request.session')}</b>
      {context.association === 'confirmed'
        ? context.session_id && onOpenSession
          ? <button type="button" className="table-link request-diagnostic-session" onClick={() => onOpenSession(context.session_id!)}>{sessionLabel}</button>
          : <span>{sessionLabel}</span>
        : <span className="request-session-unlinked">{t('sessions.unlinkedRequests')}</span>}
      {context.association === 'confirmed' && context.session_id && <small><code className="break-anywhere">{context.session_id}</code>{sessionMetadata && ` · ${sessionMetadata}`}</small>}
      {context.association === 'unlinked' && sessionMetadata && <small>{sessionMetadata}</small>}
    </span>}
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
    total: t('request.totalTokens'), input: t('request.uncachedInput'), output: t('request.outputTokens'),
    unknown: t('request.usageUnknown'), tpsHint: `${t('request.generationTpsHint')} ${t('request.averageTpsHint')}`,
    cacheMissing: t('request.cacheMissing'), pendingUsage: t('request.pendingUsage'),
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
            const tokenDisplay = formatMetricDisplay(request.input_tokens + request.output_tokens, locale);
            const tokenDetails = pending ? copy.pendingUsage : requestTokenDetails(request, locale, t);
            const duration = formatDurationDisplay(request.duration_ms, locale);
            const uncachedInput = nonCachedRequestInput(request);
            const credential = requestCredentialLabel(request, credentialAlias);
            const credentialLabel = 'label' in credential ? credential.label : t(credential.key);
            const credentialDetails = request.credential_identity ? `${request.credential_identity.key_id} · ${request.credential_identity.principal_external_id}` : credentialLabel;
            const upstreamName = request.upstream_account_id ? upstreamNames?.get(request.upstream_account_id) : undefined;
            return <tr key={request.request_id}>
              <td className="request-time-cell"><time>{new Date(request.created_at).toLocaleString(locale)}</time><RequestIdentifier requestId={request.request_id} compact /></td>
              <td className="request-credential-cell"><DetailTooltip content={credentialDetails}><strong tabIndex={0}>{credentialLabel}</strong></DetailTooltip></td>
              <td className="request-model-cell"><DetailTooltip content={technicalSummary}><span className="request-routing-info" tabIndex={0}><code>{request.model}</code>{upstreamName && <small className="request-upstream-name">{upstreamName}</small>}</span></DetailTooltip></td>
              <td className="request-token-cell"><DetailTooltip content={tokenDetails}><span className="request-value-info request-token-total" aria-label={pending ? copy.pendingUsage : `${copy.total} ${tokenDisplay.text} (${tokenDetails})`} tabIndex={0}>{pending ? t('common.running') : <>{copy.total} <span>{tokenDisplay.text}</span></>}</span></DetailTooltip>
                {pending ? <DetailTooltip content={copy.pendingUsage}><span className="request-token-pending" tabIndex={0}>{locale === 'zh-CN' ? '用量待结算' : 'Usage pending settlement'}</span></DetailTooltip> : <span className="request-token-primary"><span>{copy.input} <b>{uncachedInput === null ? <DetailTooltip content={copy.cacheMissing}><span tabIndex={0}>{copy.unknown}</span></DetailTooltip> : formatMetricDisplay(uncachedInput, locale).text}</b></span><span>{copy.output} <b>{formatMetricDisplay(request.output_tokens, locale).text}</b></span></span>}
              </td>
              <td className="request-cost-cell"><span className="request-value-info" title={cost.title} aria-label={cost.title ? `${cost.text} (${cost.title})` : undefined} tabIndex={cost.title ? 0 : undefined}>{cost.text}</span></td>
              {showsSession && <td className="request-session-cell">
                {!context
                  ? '—'
                  : context.association === 'confirmed'
                    ? context.session_id && onOpenSession
                      ? <button type="button" className="table-link" onClick={() => onOpenSession(context.session_id!)}>{sessionLabel}</button>
                      : <span className="request-session-name">{sessionLabel}</span>
                    : <span className="request-session-unlinked">{t('sessions.unlinkedRequests')}</span>}
                {sessionMeta && <RequestSessionMetadata value={sessionMeta} />}
              </td>}
              <td><RequestStatus request={request} />{request.error_code && <span className="visually-hidden">{request.error_code}</span>}</td>
              <td><span className="request-duration-info" title={[duration.title, durationSummary].filter(Boolean).join(' · ') || undefined} aria-label={[duration.text, duration.title, durationSummary].filter(Boolean).join(' · ') || undefined} tabIndex={duration.title || durationSummary ? 0 : undefined}>{duration.text}</span></td>
              <td className="request-tps-cell"><RequestOutputRate request={request} /></td>
              {onSelect && <td><button className="secondary table-action" type="button" onClick={() => onSelect(request)} aria-label={t('request.openDetail', { model: request.model })}>{t('request.inspect')}</button></td>}
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
  const previousFocus = useRef<HTMLElement | null>(null);
  const onCloseRef = useRef(onClose);
  onCloseRef.current = onClose;
  useEffect(() => {
    previousFocus.current = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    drawerRef.current?.focus();
    const previousOverflow = document.body.style.overflow;
    document.body.style.overflow = 'hidden';
    const restoredBackground: Array<{ element: HTMLElement; inert: boolean; ariaHidden: string | null }> = [];
    let branch: HTMLElement | null = drawerRef.current?.parentElement ?? null;
    while (branch?.parentElement) {
      const parent: HTMLElement = branch.parentElement;
      for (const sibling of Array.from(parent.children)) {
        if (!(sibling instanceof HTMLElement) || sibling === branch || sibling.tagName === 'SCRIPT') continue;
        restoredBackground.push({ element: sibling, inert: sibling.inert, ariaHidden: sibling.getAttribute('aria-hidden') });
        sibling.inert = true;
        sibling.setAttribute('aria-hidden', 'true');
      }
      branch = parent;
      if (parent === document.body) break;
    }
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
      document.body.style.overflow = previousOverflow;
      for (const { element, inert, ariaHidden } of restoredBackground.reverse()) {
        element.inert = inert;
        if (ariaHidden === null) element.removeAttribute('aria-hidden');
        else element.setAttribute('aria-hidden', ariaHidden);
      }
      if (previousFocus.current?.isConnected) previousFocus.current.focus();
    };
  }, []);
  return <div className="drawer-backdrop" onMouseDown={(event) => { if (event.currentTarget === event.target) onCloseRef.current(); }}>
    <aside className="drawer" ref={drawerRef} role="dialog" aria-modal="true" aria-labelledby={titleId} tabIndex={-1}>
      <button className="close" type="button" onClick={() => onCloseRef.current()} aria-label={t('common.close')}>×</button>
      <span className="eyebrow">{eyebrow}</span>
      <h2 id={titleId}>{title}</h2>
      {children}
    </aside>
  </div>;
}
