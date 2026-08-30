import { useEffect, useRef, useState, type KeyboardEvent, type ReactNode } from 'react';
import { useI18n, type Locale } from '../i18n';
import { appHref, type AppRouteKey, type AppSurface } from './routes';
import { RouteErrorBoundary, type RouteErrorCopy } from './RouteErrorBoundary';

interface NavigationItem {
  route: AppRouteKey;
  label: string;
  icon: IconName;
  primary?: boolean;
}

interface NavigationSection {
  label: string;
  items: NavigationItem[];
}

type IconName = 'overview' | 'requests' | 'sessions' | 'usage' | 'generations' | 'generate'
  | 'providers' | 'routes' | 'pricing' | 'credentials' | 'service-credentials' | 'plugins';

const labels = {
  'zh-CN': {
    portal: '个人中心', operator: '管理中心', skip: '跳到主要内容', menu: '打开导航', close: '关闭导航',
    collapse: '收起侧栏', expand: '展开侧栏', appearance: '外观与语言', workspace: '工作区',
    errorEyebrow: '页面加载失败', errorTitle: '暂时无法显示此页面', errorDescription: '页面模块未能完成加载。你可以先重试；如果问题仍然存在，请刷新页面。', errorRetry: '重试', errorRefresh: '刷新页面',
    monitoring: '监控', traffic: '流量配置', identity: '身份与权限', system: '系统', creation: '多模态',
    overview: '总览', requests: '请求', sessions: '会话', usage: '用量分析', generations: '生成任务', generate: '创建任务',
    providers: '上游服务', routes: '模型路由', pricing: '模型计费', credentials: '客户端凭据',
    'service-credentials': '服务凭据', plugins: '插件',
  },
  en: {
    portal: 'Portal', operator: 'Operator', skip: 'Skip to main content', menu: 'Open navigation', close: 'Close navigation',
    collapse: 'Collapse sidebar', expand: 'Expand sidebar', appearance: 'Appearance and language', workspace: 'Workspace',
    errorEyebrow: 'Page load failed', errorTitle: 'This page cannot be displayed', errorDescription: 'A page module did not finish loading. Try again, or refresh the page if the problem continues.', errorRetry: 'Try again', errorRefresh: 'Refresh page',
    monitoring: 'Monitoring', traffic: 'Traffic configuration', identity: 'Identity and access', system: 'System', creation: 'Multimodal',
    overview: 'Overview', requests: 'Requests', sessions: 'Sessions', usage: 'Usage', generations: 'Generation jobs', generate: 'Create task',
    providers: 'Upstream services', routes: 'Model routes', pricing: 'Model pricing', credentials: 'Client credentials',
    'service-credentials': 'Service credentials', plugins: 'Plugins',
  },
} as const;

function label(locale: Locale, key: keyof typeof labels.en) {
  return labels[locale][key];
}

function navigation(surface: AppSurface, locale: Locale): NavigationSection[] {
  const item = (route: AppRouteKey, icon: IconName = route as IconName, primary = false): NavigationItem => ({
    route,
    icon,
    primary,
    label: label(locale, route as keyof typeof labels.en),
  });
  if (surface === 'portal') return [
    { label: label(locale, 'workspace'), items: [item('overview'), item('requests'), item('sessions'), item('usage')] },
    { label: label(locale, 'creation'), items: [item('generations'), item('generate', 'generate', true)] },
  ];
  return [
    { label: label(locale, 'monitoring'), items: [item('overview'), item('requests'), item('sessions'), item('usage'), item('generations')] },
    { label: label(locale, 'traffic'), items: [item('providers'), item('routes'), item('pricing')] },
    { label: label(locale, 'identity'), items: [item('credentials'), item('service-credentials')] },
    { label: label(locale, 'system'), items: [item('plugins')] },
  ];
}

function NavIcon({ name }: { name: IconName }) {
  const paths: Record<IconName, ReactNode> = {
    overview: <><rect x="3" y="3" width="7" height="7" rx="1" /><rect x="14" y="3" width="7" height="7" rx="1" /><rect x="3" y="14" width="7" height="7" rx="1" /><rect x="14" y="14" width="7" height="7" rx="1" /></>,
    requests: <><path d="M4 7h16M4 12h11M4 17h8" /><path d="m17 15 3 3-3 3" /></>,
    sessions: <><circle cx="8" cy="8" r="3" /><circle cx="17" cy="16" r="3" /><path d="M10.5 9.7 14.5 14M5.5 10.5 4 18h10" /></>,
    usage: <><path d="M4 19V9M10 19V4M16 19v-7M22 19H2" /></>,
    generations: <><path d="M4 5h16v14H4z" /><path d="m7 15 3-3 3 3 3-4 2 3" /><circle cx="9" cy="9" r="1" /></>,
    generate: <><path d="M12 3v18M3 12h18" /><path d="m17 4 .5 1.5L19 6l-1.5.5L17 8l-.5-1.5L15 6l1.5-.5z" /></>,
    providers: <><path d="M12 2v7M12 15v7M4.2 6.5l6.1 3.5M13.7 14l6.1 3.5M19.8 6.5 13.7 10M10.3 14l-6.1 3.5" /><circle cx="12" cy="12" r="3" /></>,
    routes: <><path d="M4 5h7a4 4 0 0 1 4 4v10M20 5h-2a3 3 0 0 0-3 3" /><path d="m11 16 4 4 4-4" /></>,
    pricing: <><path d="M12 3v18M17 7.5C17 5.6 14.8 4 12 4S7 5.6 7 7.5 9.2 11 12 11s5 1.6 5 3.5S14.8 18 12 18s-5-1.6-5-3.5" /></>,
    credentials: <><circle cx="9" cy="8" r="4" /><path d="M3 21v-2a6 6 0 0 1 12 0v2M16 11h5M19 8v6" /></>,
    'service-credentials': <><rect x="3" y="5" width="18" height="14" rx="2" /><path d="M7 10h5M7 14h8M17 9v6" /></>,
    plugins: <><path d="M8 3v5H3v8h5v5h8v-5h5V8h-5V3z" /></>,
  };
  return <svg aria-hidden="true" className="app-nav-icon" fill="none" viewBox="0 0 24 24" stroke="currentColor" strokeWidth="1.7" strokeLinecap="round" strokeLinejoin="round">{paths[name]}</svg>;
}

export function AppShell({ surface, route, onNavigate, children }: {
  surface: AppSurface;
  route: AppRouteKey;
  onNavigate: (route: AppRouteKey) => void;
  children: ReactNode;
}) {
  const { locale, setLocale, t } = useI18n();
  const [collapsed, setCollapsed] = useState(() => localStorage.getItem('mtc-sidebar-collapsed') === 'true');
  const [mobileOpen, setMobileOpen] = useState(false);
  const [theme, setTheme] = useState<'dark' | 'light'>(() => document.documentElement.dataset.theme === 'light' ? 'light' : 'dark');
  const navigationSections = navigation(surface, locale);
  const navigationItems = navigationSections.flatMap((section) => section.items);
  const navRefs = useRef<Array<HTMLAnchorElement | null>>([]);
  const sidebarRef = useRef<HTMLElement>(null);
  const mobileMenuRef = useRef<HTMLButtonElement>(null);
  const mobileCloseRef = useRef<HTMLButtonElement>(null);
  const restoreMobileFocus = useRef(true);
  const previousRoute = useRef(route);
  const activeItem = navigationItems.find((item) => item.route === route) ?? navigationItems[0];
  const routeErrorCopy: RouteErrorCopy = {
    eyebrow: label(locale, 'errorEyebrow'),
    title: label(locale, 'errorTitle'),
    description: label(locale, 'errorDescription'),
    retry: label(locale, 'errorRetry'),
    refresh: label(locale, 'errorRefresh'),
  };

  useEffect(() => {
    document.documentElement.dataset.theme = theme;
    localStorage.setItem('mtc-theme', theme);
    document.querySelector<HTMLMetaElement>('meta[name="theme-color"]')?.setAttribute('content', theme === 'light' ? '#f4f7f5' : '#071014');
  }, [theme]);
  useEffect(() => {
    const mobileViewport = window.matchMedia('(max-width: 900px)');
    const closeWhenDesktop = (event: MediaQueryListEvent) => { if (!event.matches) setMobileOpen(false); };
    mobileViewport.addEventListener('change', closeWhenDesktop);
    return () => mobileViewport.removeEventListener('change', closeWhenDesktop);
  }, []);
  useEffect(() => {
    if (!mobileOpen) return;
    const opener = document.activeElement instanceof HTMLElement ? document.activeElement : mobileMenuRef.current;
    const productApp = sidebarRef.current?.closest<HTMLElement>('.product-app');
    const stage = productApp?.querySelector<HTMLElement>(':scope > .app-stage');
    const skipLink = productApp?.querySelector<HTMLElement>(':scope > .app-skip-link');
    const previousOverflow = document.body.style.overflow;
    const stageWasInert = stage?.inert ?? false;
    const skipWasInert = skipLink?.inert ?? false;
    if (stage) stage.inert = true;
    if (skipLink) skipLink.inert = true;
    document.body.style.overflow = 'hidden';
    const focusFrame = requestAnimationFrame(() => mobileCloseRef.current?.focus());
    const keydown = (event: globalThis.KeyboardEvent) => {
      if (event.key === 'Escape') {
        event.preventDefault();
        setMobileOpen(false);
        return;
      }
      if (event.key !== 'Tab' || !sidebarRef.current) return;
      const focusable = Array.from(sidebarRef.current.querySelectorAll<HTMLElement>('button:not([disabled]), a[href], [tabindex]:not([tabindex="-1"])'))
        .filter((element) => !element.inert && element.getAttribute('aria-hidden') !== 'true' && element.getClientRects().length > 0);
      if (!focusable.length) {
        event.preventDefault();
        sidebarRef.current.focus();
        return;
      }
      const first = focusable[0];
      const last = focusable[focusable.length - 1];
      if (event.shiftKey && (document.activeElement === first || !sidebarRef.current.contains(document.activeElement))) {
        event.preventDefault();
        last.focus();
      } else if (!event.shiftKey && (document.activeElement === last || !sidebarRef.current.contains(document.activeElement))) {
        event.preventDefault();
        first.focus();
      }
    };
    document.addEventListener('keydown', keydown);
    return () => {
      cancelAnimationFrame(focusFrame);
      document.removeEventListener('keydown', keydown);
      if (stage) stage.inert = stageWasInert;
      if (skipLink) skipLink.inert = skipWasInert;
      document.body.style.overflow = previousOverflow;
      if (restoreMobileFocus.current && opener?.isConnected) opener.focus();
      restoreMobileFocus.current = true;
    };
  }, [mobileOpen]);
  useEffect(() => {
    document.title = `${activeItem.label} · Token Center`;
    if (previousRoute.current === route) return;
    previousRoute.current = route;
    setMobileOpen(false);
  }, [activeItem.label, route]);

  const changeCollapsed = () => setCollapsed((current) => {
    localStorage.setItem('mtc-sidebar-collapsed', String(!current));
    return !current;
  });
  const changeTheme = () => setTheme((current) => current === 'dark' ? 'light' : 'dark');
  const changeLocale = () => setLocale(locale === 'zh-CN' ? 'en' : 'zh-CN');
  const navigate = (nextRoute: AppRouteKey) => {
    restoreMobileFocus.current = false;
    setMobileOpen(false);
    onNavigate(nextRoute);
    requestAnimationFrame(() => {
      window.scrollTo({ top: 0, left: 0, behavior: 'auto' });
      document.getElementById('app-main-content')?.focus({ preventScroll: true });
    });
  };
  const onNavigationKeyDown = (event: KeyboardEvent<HTMLAnchorElement>, index: number) => {
    let next = index;
    if (event.key === 'ArrowDown' || event.key === 'ArrowRight') next = (index + 1) % navigationItems.length;
    else if (event.key === 'ArrowUp' || event.key === 'ArrowLeft') next = (index - 1 + navigationItems.length) % navigationItems.length;
    else if (event.key === 'Home') next = 0;
    else if (event.key === 'End') next = navigationItems.length - 1;
    else return;
    event.preventDefault();
    navRefs.current[next]?.focus();
  };

  let itemIndex = 0;
  return <div className={`product-app ${collapsed ? 'is-sidebar-collapsed' : ''} ${mobileOpen ? 'is-mobile-nav-open' : ''}`}>
    <a className="app-skip-link" href="#app-main-content">{label(locale, 'skip')}</a>
    {mobileOpen && <div className="app-nav-backdrop" aria-hidden="true" onMouseDown={() => setMobileOpen(false)} />}
    <aside className="app-sidebar" id="app-sidebar" aria-label={label(locale, surface)} aria-modal={mobileOpen ? true : undefined} role={mobileOpen ? 'dialog' : undefined} ref={sidebarRef} tabIndex={mobileOpen ? -1 : undefined}>
      <div className="app-brand">
        <img src="/ui-assets/token-center-icon-32.png" alt="" />
        <span><b>Token Center</b><small>{label(locale, surface)}</small></span>
        <button className="app-mobile-close" ref={mobileCloseRef} type="button" aria-label={label(locale, 'close')} onClick={() => setMobileOpen(false)}>×</button>
      </div>
      <nav className="app-navigation" aria-label={label(locale, surface)}>
        {navigationSections.map((section) => <section className="app-nav-section" key={section.label}>
          <h2>{section.label}</h2>
          {section.items.map((item) => {
            const index = itemIndex++;
            const selected = item.route === route;
            return <a
              href={appHref(surface, item.route)}
              className={`app-nav-item ${item.primary ? 'is-primary' : ''}`}
              aria-current={selected ? 'page' : undefined}
              aria-label={item.label}
              key={item.route}
              ref={(node) => { navRefs.current[index] = node; }}
              title={item.label}
              onClick={(event) => {
                if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
                event.preventDefault();
                navigate(item.route);
              }}
              onKeyDown={(event) => onNavigationKeyDown(event, index)}
            ><NavIcon name={item.icon} /><span>{item.label}</span></a>;
          })}
        </section>)}
      </nav>
      <div className="app-sidebar-footer" aria-label={label(locale, 'appearance')}>
        <button type="button" onClick={changeTheme} aria-label={theme === 'dark' ? t('theme.light') : t('theme.dark')} title={theme === 'dark' ? t('theme.light') : t('theme.dark')}><span aria-hidden="true">{theme === 'dark' ? '☀' : '☾'}</span><b>{theme === 'dark' ? t('theme.light') : t('theme.dark')}</b></button>
        <button type="button" onClick={changeLocale} aria-label={locale === 'zh-CN' ? t('language.en') : t('language.zh')} title={locale === 'zh-CN' ? t('language.en') : t('language.zh')}><span aria-hidden="true">{locale === 'zh-CN' ? 'EN' : '中'}</span><b>{locale === 'zh-CN' ? t('language.en') : t('language.zh')}</b></button>
        <button className="app-sidebar-collapse" type="button" aria-expanded={!collapsed} onClick={changeCollapsed} aria-label={collapsed ? label(locale, 'expand') : label(locale, 'collapse')} title={collapsed ? label(locale, 'expand') : label(locale, 'collapse')}><span aria-hidden="true">{collapsed ? '›' : '‹'}</span><b>{collapsed ? label(locale, 'expand') : label(locale, 'collapse')}</b></button>
      </div>
    </aside>
    <div className="app-stage">
      <header className="app-context-bar">
        <button className="app-mobile-menu" ref={mobileMenuRef} type="button" aria-controls="app-sidebar" aria-expanded={mobileOpen} aria-label={mobileOpen ? label(locale, 'close') : label(locale, 'menu')} onClick={() => setMobileOpen((current) => !current)}><span aria-hidden="true">{mobileOpen ? '×' : '☰'}</span></button>
        <div className="app-breadcrumb"><small>{label(locale, surface)}</small><strong>{activeItem.label}</strong></div>
        <div className="app-mobile-preferences">
          <button type="button" onClick={changeTheme} aria-label={theme === 'dark' ? t('theme.light') : t('theme.dark')}>{theme === 'dark' ? '☀' : '☾'}</button>
          <button type="button" onClick={changeLocale} aria-label={locale === 'zh-CN' ? t('language.en') : t('language.zh')}>{locale === 'zh-CN' ? 'EN' : '中'}</button>
        </div>
      </header>
      <p className="app-route-announcement" aria-live="polite" aria-atomic="true">{activeItem.label}</p>
      <main className="app-main-content" id="app-main-content" tabIndex={-1} data-surface={surface} data-route={route}>
        <RouteErrorBoundary copy={routeErrorCopy} resetKey={`${surface}:${route}`}>{children}</RouteErrorBoundary>
      </main>
    </div>
  </div>;
}
