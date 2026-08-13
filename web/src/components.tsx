import { useEffect, useState, type ReactNode } from 'react';
import type { RequestView, StatsBucket } from './types';
import { useI18n } from './i18n';

export function Shell({ children, operator = false }: { children: ReactNode; operator?: boolean }) {
  const { locale, setLocale, t } = useI18n();
  const [theme, setTheme] = useState<'dark' | 'light'>(() =>
    document.documentElement.dataset.theme === 'light' ? 'light' : 'dark',
  );
  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem('mtc-theme', theme);
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
        <button className="language-toggle" type="button" onClick={() => setLocale(locale === 'zh-CN' ? 'en' : 'zh-CN')} title={locale === 'zh-CN' ? t('language.en') : t('language.zh')}>
          {locale === 'zh-CN' ? 'EN' : '中'}
        </button>
        <div className="rail-label">{operator ? 'OP' : 'SELF'}</div>
      </aside>
      <main className="main">
        <div className="mobile-controls">
          <button className="theme-toggle" type="button" aria-label={theme === 'dark' ? t('theme.light') : t('theme.dark')} onClick={() => setTheme((current) => current === 'dark' ? 'light' : 'dark')}>{theme === 'dark' ? '☀' : '☾'}</button>
          <button className="language-toggle" type="button" onClick={() => setLocale(locale === 'zh-CN' ? 'en' : 'zh-CN')}>{locale === 'zh-CN' ? 'EN' : '中'}</button>
        </div>
        {children}
      </main>
    </div>
  );
}

export function Metric({ label, value, tone }: { label: string; value: ReactNode; tone?: string }) {
  return (
    <article className={`metric ${tone ?? ''}`}>
      <span>{label}</span>
      <strong>{value}</strong>
    </article>
  );
}

export function Buckets({ values }: { values: StatsBucket[] }) {
  const { t } = useI18n();
  const maximum = Math.max(1, ...values.map((value) => value.requests));
  if (!values.length) return <div className="empty">{t('common.noData')}</div>;
  return (
    <div className="bucket-list">
      {values.map((value) => (
        <div className="bucket" key={value.name}>
          <div>
            <b>{value.name}</b>
            <span>{t('request.count', { count: value.requests })} · {value.input_tokens + value.output_tokens} tokens</span>
          </div>
          <div className="bar"><i style={{ width: `${(value.requests / maximum) * 100}%` }} /></div>
        </div>
      ))}
    </div>
  );
}

export function RequestTable({
  requests,
  onSelect,
}: {
  requests: RequestView[];
  onSelect?: (request: RequestView) => void;
}) {
  const { locale, t } = useI18n();
  if (!requests.length) return <div className="empty">{t('common.noRequests')}</div>;
  return (
    <div className="table-scroll">
      <table>
        <thead><tr><th>{t('request.time')}</th><th>{t('request.model')}</th><th>{t('request.protocol')}</th><th>{t('request.status')}</th><th>{t('request.duration')}</th><th>{t('request.tokens')}</th><th>{t('request.cost')}</th></tr></thead>
        <tbody>
          {requests.map((request) => (
            <tr key={request.request_id} onClick={() => onSelect?.(request)} className={onSelect ? 'clickable' : ''}>
              <td>{new Date(request.created_at).toLocaleString(locale)}</td>
              <td><code>{request.model}</code></td>
              <td>{request.protocol}</td>
              <td><span className={`status ${request.status_code && request.status_code < 400 ? 'ok' : request.status_code ? 'bad' : 'pending'}`}>{request.status_code ?? t('common.running')}</span></td>
              <td>{request.duration_ms ?? '—'} ms</td>
              <td>{request.input_tokens + request.output_tokens}</td>
              <td>{request.cost}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
