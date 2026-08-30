import { useEffect, useId, useRef, useState, type ReactNode } from 'react';
import type { RequestView, StatsBucket } from './types.js';
import { useI18n } from './i18n.js';
import { formatCurrency, formatMetricNumber, formatNumber } from './format.js';

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

export function RequestTable({
  requests,
  onSelect,
  onOpenSession,
  currency,
}: {
  requests: RequestView[];
  onSelect?: (request: RequestView) => void;
  onOpenSession?: (sessionId: string) => void;
  currency?: string;
}) {
  const { locale, t } = useI18n();
  if (!requests.length) return <div className="empty">{t('common.noRequests')}</div>;
  const showsSession = requests.some((request) => request.session_context !== undefined);
  return (
    <div className="table-scroll">
      <table>
        <thead><tr><th>{t('request.time')}</th><th>{t('request.model')}</th>{showsSession && <th>{t('request.session')}</th>}<th>{t('request.protocol')}</th><th>{t('request.status')}</th><th>{t('request.duration')}</th><th>{t('request.tokens')}</th><th>{t('request.cost')}</th><th>{t('request.error')}</th>{onSelect && <th><span className="visually-hidden">{t('request.actions')}</span></th>}</tr></thead>
        <tbody>
          {requests.map((request) => {
            const context = request.session_context;
            const sessionLabel = context?.session_name
              ?? (context?.association === 'confirmed' ? t('sessions.reportedNameMissing') : t('sessions.unlinkedRequests'));
            const sessionMeta = [context?.task_kind, context?.agent_id].filter(Boolean).join(' · ');
            return <tr key={request.request_id}>
              <td>{new Date(request.created_at).toLocaleString(locale)}</td>
              <td><code>{request.model}</code></td>
              {showsSession && <td className="request-session-cell">
                {!context
                  ? '—'
                  : context.association === 'confirmed'
                    ? context.session_id && onOpenSession
                      ? <button type="button" className="table-link" onClick={() => onOpenSession(context.session_id!)}>{sessionLabel}</button>
                      : <span className="request-session-name">{sessionLabel}</span>
                    : <span className="request-session-unlinked">{t('sessions.unlinkedRequests')}</span>}
                {sessionMeta && <small>{sessionMeta}</small>}
              </td>}
              <td>{request.protocol}</td>
              <td><span className={`status ${request.status_code && request.status_code < 400 ? 'ok' : request.status_code ? 'bad' : 'pending'}`}>{request.status_code ?? t('common.running')}</span></td>
              <td>{request.duration_ms === null ? '—' : `${formatNumber(request.duration_ms, locale, 2)} ms`}</td>
              <td>{formatNumber(request.input_tokens + request.output_tokens, locale)}</td>
              <td>{request.currency || currency ? formatCurrency(request.cost, request.currency ?? currency ?? '', locale) : '—'}</td>
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
