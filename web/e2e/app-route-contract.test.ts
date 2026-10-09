import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  appHref,
  isSameAppLocation,
  operatorRouteKeys,
  portalRouteKeys,
  readAppLocation,
} from '../src/app/routes.js';
import { readProxyGroupNavigationContext, withProxyGroupNavigationContext } from '../src/app/proxyGroupNavigation.js';

test('portal and operator expose the complete product route sets', () => {
  assert.deepEqual(portalRouteKeys, ['overview', 'requests', 'sessions', 'usage', 'generations', 'generate']);
  assert.deepEqual(operatorRouteKeys, ['overview', 'requests', 'sessions', 'usage', 'generations', 'providers', 'proxy-groups', 'routes', 'pricing', 'tenants', 'credentials', 'service-credentials', 'plugins', 'system-settings']);
});

test('legacy entry URLs resolve to stable defaults and query routes survive refresh', () => {
  assert.deepEqual(readAppLocation(new URL('https://example.test/portal')), { surface: 'portal', route: 'overview' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator')), { surface: 'operator', route: 'overview' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/portal?view=sessions')), { surface: 'portal', route: 'sessions' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator?view=service-credentials')), { surface: 'operator', route: 'service-credentials' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator?view=tenants')), { surface: 'operator', route: 'tenants' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator?view=proxy-groups')), { surface: 'operator', route: 'proxy-groups' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator?view=unknown')), { surface: 'operator', route: 'overview' });
});

test('navigation URLs use exact server document paths and never contain credentials', () => {
  assert.equal(appHref('portal', 'generate'), '/portal?view=generate');
  assert.equal(appHref('operator', 'pricing'), '/operator?view=pricing');
  assert.equal(appHref('operator', 'tenants'), '/operator?view=tenants');
  assert.equal(appHref('operator', 'proxy-groups'), '/operator?view=proxy-groups');
  assert.throws(() => appHref('portal', 'providers'));
  for (const route of portalRouteKeys) assert.doesNotMatch(appHref('portal', route), /token|secret|credential=/i);
  for (const route of operatorRouteKeys) assert.doesNotMatch(appHref('operator', route), /token|secret|credential=/i);
});

test('proxy group deep links preserve bounded account and tenant context without introducing another route implementation', () => {
  const href = withProxyGroupNavigationContext(appHref('operator', 'proxy-groups'), { accountId: 'account/one', tenant: 'research team' });
  assert.equal(href, '/operator?view=proxy-groups&account=account%2Fone&tenant=research+team');
  assert.deepEqual(readProxyGroupNavigationContext(new URL(href, 'https://example.test').search), { accountId: 'account/one', tenant: 'research team' });
  assert.equal(withProxyGroupNavigationContext(appHref('operator', 'providers'), { accountId: 'account/one', tenant: 'research team' }), '/operator?view=providers');
  assert.deepEqual(readProxyGroupNavigationContext('?view=providers&account=account&tenant=tenant'), {});
  assert.deepEqual(readProxyGroupNavigationContext(`?view=proxy-groups&account=${'x'.repeat(201)}&tenant=${'y'.repeat(201)}`), {});
});

test('same-route proxy locations distinguish account and tenant history entries', () => {
  const current = readAppLocation(new URL('https://example.test/operator?view=proxy-groups&account=one&tenant=north'));
  for (const query of ['account=two&tenant=north', 'account=one&tenant=south', '']) {
    const next = readAppLocation(new URL(`https://example.test/operator?view=proxy-groups&${query}`));
    assert.equal(isSameAppLocation(current, next), false, 'context changes must not be discarded as same-route navigation');
    assert.deepEqual(readAppLocation(new URL(appHref(next.surface, next.route, next.context), 'https://example.test')), next);
  }
});

test('production entry forwards the live location context to Operator', async () => {
  const source = await readFile(new URL('../src/main.tsx', import.meta.url), 'utf8');
  assert.match(source, /const\s*\{[^}]*\bcontext\b[^}]*\}\s*=\s*useAppLocation\(\)/, 'the production entry must subscribe to context changes');
  assert.match(source, /<Operator\b[^>]*\bnavigationContext=\{context\}/, 'Operator must receive current history context instead of relying on fixture-only wiring');
  assert.match(source, /<Operator\b[^>]*\bonRouteChange=\{navigate\}/);
});

test('application shell preserves native links and provides modal mobile navigation', async () => {
  const source = await readFile(new URL('../src/app/AppShell.tsx', import.meta.url), 'utf8');
  const styles = await readFile(new URL('../src/app-shell.css', import.meta.url), 'utf8');
  assert.match(source, /aria-current=\{selected \? 'page'/);
  assert.doesNotMatch(source, /tabIndex=\{selected \? 0 : -1\}/);
  assert.match(source, /event\.metaKey \|\| event\.ctrlKey/);
  assert.match(source, /event\.key === 'ArrowDown'/);
  assert.match(source, /event\.key === 'Home'/);
  assert.match(source, /stage\.inert = true/);
  assert.match(source, /mobileCloseRef\.current\?\.focus/);
  assert.match(source, /window\.scrollTo\(\{ top: 0/);
  const routeEffect = source.slice(source.indexOf('document.title ='), source.indexOf('const changeCollapsed'));
  const clickNavigation = source.slice(source.indexOf('const navigate ='), source.indexOf('const onNavigationKeyDown'));
  assert.doesNotMatch(routeEffect, /scrollTo/);
  assert.match(clickNavigation, /scrollTo/);
  assert.match(source, /Skip to main content/);
  assert.match(styles, /min-height: 44px/);
  assert.match(styles, /\.drawer-backdrop \{ z-index: 80; \}/);
});

// Drawer isolation, cleanup, focus containment and live-update stability are
// exercised through the real DOM in request-lifecycle-browser-contract.test.ts.
