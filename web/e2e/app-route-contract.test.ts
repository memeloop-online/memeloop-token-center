import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

import {
  appHref,
  operatorRouteKeys,
  portalRouteKeys,
  readAppLocation,
} from '../src/app/routes.js';

test('portal and operator expose the complete product route sets', () => {
  assert.deepEqual(portalRouteKeys, ['overview', 'requests', 'sessions', 'usage', 'generations', 'generate']);
  assert.deepEqual(operatorRouteKeys, ['overview', 'requests', 'sessions', 'usage', 'generations', 'providers', 'routes', 'pricing', 'credentials', 'service-credentials', 'plugins']);
});

test('legacy entry URLs resolve to stable defaults and query routes survive refresh', () => {
  assert.deepEqual(readAppLocation(new URL('https://example.test/portal')), { surface: 'portal', route: 'overview' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator')), { surface: 'operator', route: 'overview' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/portal?view=sessions')), { surface: 'portal', route: 'sessions' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator?view=service-credentials')), { surface: 'operator', route: 'service-credentials' });
  assert.deepEqual(readAppLocation(new URL('https://example.test/operator?view=unknown')), { surface: 'operator', route: 'overview' });
});

test('navigation URLs use exact server document paths and never contain credentials', () => {
  assert.equal(appHref('portal', 'generate'), '/portal?view=generate');
  assert.equal(appHref('operator', 'pricing'), '/operator?view=pricing');
  assert.throws(() => appHref('portal', 'providers'));
  for (const route of portalRouteKeys) assert.doesNotMatch(appHref('portal', route), /token|secret|credential=/i);
  for (const route of operatorRouteKeys) assert.doesNotMatch(appHref('operator', route), /token|secret|credential=/i);
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

test('drawer keeps a stable close callback, traps focus, and restores background state', async () => {
  const source = await readFile(new URL('../src/components.tsx', import.meta.url), 'utf8');
  const drawer = source.slice(source.indexOf('export function DrawerFrame'));
  assert.match(drawer, /const onCloseRef = useRef\(onClose\)/);
  assert.match(drawer, /sibling\.inert = true/);
  assert.match(drawer, /element\.inert = inert/);
  assert.match(drawer, /previousFocus\.current\?\.isConnected/);
  assert.match(drawer, /document\.addEventListener\('keydown', keydown\)/);
  assert.match(drawer, /\}, \[\]\);/);
  assert.doesNotMatch(drawer, /\}, \[onClose\]\);/);
});
