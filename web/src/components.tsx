import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import { CopyButton } from './CopyButton.js';
import type { RequestView, StatsBucket } from './types.js';
import { useI18n } from './i18n.js';
import { formatCurrency, formatMetricNumber, formatMilliseconds, formatNumber } from './format.js';

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
    <span className="metric-number">
      <span className="metric-exact">{formatted.text}</span>
      {showCompact && formatted.compact && <small className="metric-compact" aria-hidden="true">{formatted.compact}</small>}
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
            ? <button className="bucket-heading" type="button" onClick={() => onSelect(value)} aria-label={t('request.filterBy', { name: value.name })}><b>{value.name}</b><span>{t('request.count', { count: formatNumber(value.requests, locale) })} · {formatNumber(value.input_tokens + value.output_tokens, locale)} {t('request.tokenUnit')}</span></button>
            : <div className="bucket-heading"><b>{value.name}</b><span>{t('request.count', { count: formatNumber(value.requests, locale) })} · {formatNumber(value.input_tokens + value.output_tokens, locale)} {t('request.tokenUnit')}</span></div>}
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

function RequestTokenSummary({ request }: { request: RequestView }) {
  const { locale, t } = useI18n();
  const breakdown = requestTokenBreakdown(request);
  return <small>{breakdown
    ? t('request.tokenBreakdown', { input: formatNumber(breakdown.input, locale), cached: formatNumber(breakdown.cached, locale), cacheWrite: formatNumber(breakdown.cacheWrite, locale), output: formatNumber(breakdown.output, locale) })
    : <>{t('usage.inputTokens')}: {formatNumber(request.input_tokens, locale)} · {t('usage.outputTokens')}: {formatNumber(request.output_tokens, locale)}
      {request.cached_input_tokens !== undefined && <> · {t('usage.cachedTokens')}: {formatNumber(request.cached_input_tokens, locale)}</>}
      {request.cache_write_tokens !== undefined && <> · {t('usage.cacheWriteTokens')}: {formatNumber(request.cache_write_tokens, locale)}</>}
    </>}</small>;
}

function RequestIdentifier({ requestId, compact = false }: { requestId: string; compact?: boolean }) {
  const { t } = useI18n();
  return <span className={`request-id-control${compact ? ' compact' : ''}`}>
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

/**
 * The raw-request API deliberately exposes only fields that were durably
 * recorded for the request. Keep its diagnostics in one surface so the self
 * portal and operator request drawer cannot drift or turn absent telemetry
 * into inferred values.
 */
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
  const successful = request.status_code !== null && request.status_code < 400;
  const currencyForRequest = recordedCurrency(request, currency);
  const context = request.session_context;
  const sessionLabel = context?.session_name ?? t('sessions.reportedNameMissing');
  const sessionMetadata = [context?.task_kind, context?.agent_id].filter(Boolean).join(' · ');

  return <div className="request-diagnostics">
    <span><b>{t('request.request')}</b><RequestIdentifier requestId={request.request_id} /></span>
    <span><b>{t('request.receivedAt')}</b>{new Date(request.created_at).toLocaleString(locale)}</span>
    <span><b>{t('request.completedAt')}</b>{request.completed_at === null || request.completed_at === undefined ? '—' : new Date(request.completed_at).toLocaleString(locale)}</span>
    <span><b>{t('request.status')}</b><i className={`status ${successful ? 'ok' : request.status_code ? 'bad' : 'pending'}`}>{request.status_code ?? t('common.running')}</i></span>
    <span><b>{t('request.protocol')}</b>{request.protocol}</span>
    <span><b>{t('request.duration')}</b>{formatMilliseconds(request.duration_ms, locale)}</span>
    <span><b>{t('request.upstreamId')}</b>{upstreamName && <small>{upstreamName}</small>}{request.upstream_account_id ?? '—'}</span>
    <span><b>{t('request.routeId')}</b>{request.route_id ?? '—'}</span>
    <span><b>{t('request.tokens')}</b>{formatNumber(request.input_tokens + request.output_tokens, locale)}
      <RequestTokenSummary request={request} />
    </span>
    <span><b>{t('request.cost')}</b>{currencyForRequest ? formatCurrency(request.cost, currencyForRequest, locale) : '—'}</span>
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
  showRoutingDetails = false,
  upstreamNames,
}: {
  requests: RequestView[];
  onSelect?: (request: RequestView) => void;
  onOpenSession?: (sessionId: string) => void;
  currency?: string;
  showRoutingDetails?: boolean;
  upstreamNames?: ReadonlyMap<string, string>;
}) {
  const { locale, t } = useI18n();
  if (!requests.length) return <div className="empty">{t('common.noRequests')}</div>;
  const showsSession = requests.some((request) => request.session_context !== undefined);
  return (
    <div className="table-scroll request-table-scroll" role="region" aria-label={t('request.table')} tabIndex={0}>
      <table className="request-table">
        <thead><tr><th>{t('request.receivedAt')}</th>{showRoutingDetails && <th>{t('request.completedAt')}</th>}<th>{t('request.model')}</th>{showsSession && <th>{t('request.session')}</th>}<th>{t('request.protocol')}</th>{showRoutingDetails && <><th>{t('request.upstreamId')}</th><th>{t('request.routeId')}</th></>}<th>{t('request.status')}</th><th>{t('request.duration')}</th><th>{t('request.tokens')}</th><th>{t('request.cost')}</th><th>{t('request.error')}</th>{onSelect && <th><span className="visually-hidden">{t('request.actions')}</span></th>}</tr></thead>
        <tbody>
          {requests.map((request) => {
            const context = request.session_context;
            const sessionLabel = context?.session_name
              ?? (context?.association === 'confirmed' ? t('sessions.reportedNameMissing') : t('sessions.unlinkedRequests'));
            const sessionMeta = [context?.task_kind, context?.agent_id].filter(Boolean).join(' · ');
            const currencyForRequest = recordedCurrency(request, currency);
            return <tr key={request.request_id}>
              <td className="request-time-cell"><time>{new Date(request.created_at).toLocaleString(locale)}</time><RequestIdentifier requestId={request.request_id} compact /></td>
              {showRoutingDetails && <td className="request-completed-cell">{request.completed_at == null ? '—' : <time>{new Date(request.completed_at).toLocaleString(locale)}</time>}</td>}
              <td className="request-model-cell"><code>{request.model}</code></td>
              {showsSession && <td className="request-session-cell">
                {!context
                  ? '—'
                  : context.association === 'confirmed'
                    ? context.session_id && onOpenSession
                      ? <button type="button" className="table-link" onClick={() => onOpenSession(context.session_id!)}>{sessionLabel}</button>
                      : <span className="request-session-name">{sessionLabel}</span>
                    : <span className="request-session-unlinked">{t('sessions.unlinkedRequests')}</span>}
                {sessionMeta && <details className="request-session-metadata"><summary>{t('request.sessionMetadata')}</summary><small>{sessionMeta}</small></details>}
              </td>}
              <td>{request.protocol}</td>
              {showRoutingDetails && <><td className="request-upstream-cell">{request.upstream_account_id && upstreamNames?.get(request.upstream_account_id) && <span>{upstreamNames.get(request.upstream_account_id)}</span>}<code>{request.upstream_account_id ?? '—'}</code></td><td className="request-route-cell"><code>{request.route_id ?? '—'}</code></td></>}
              <td><span className={`status ${request.status_code && request.status_code < 400 ? 'ok' : request.status_code ? 'bad' : 'pending'}`}>{request.status_code ?? t('common.running')}</span></td>
              <td>{request.duration_ms === null ? '—' : `${formatNumber(request.duration_ms, locale, 2)} ms`}</td>
              <td className="request-token-cell"><span>{formatNumber(request.input_tokens + request.output_tokens, locale)}</span><RequestTokenSummary request={request} /></td>
              <td>{currencyForRequest ? formatCurrency(request.cost, currencyForRequest, locale) : '—'}</td>
              <td>{request.error_code ? <code className="error-code">{request.error_code}</code> : '—'}</td>
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
