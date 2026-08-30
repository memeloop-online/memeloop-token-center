import assert from 'node:assert/strict';
import type { Locator, Page } from 'playwright';

export type AppSurface = 'operator' | 'portal';

export type OperatorRoute = 'overview' | 'requests' | 'sessions' | 'usage' | 'generations'
  | 'providers' | 'routes' | 'pricing' | 'credentials' | 'service-credentials' | 'plugins';
export type PortalRoute = 'overview' | 'requests' | 'sessions' | 'usage' | 'generations' | 'generate';

export async function openAppRoute(
  page: Page,
  surface: 'operator',
  route: OperatorRoute,
): Promise<void>;
export async function openAppRoute(
  page: Page,
  surface: 'portal',
  route: PortalRoute,
): Promise<void>;
export async function openAppRoute(
  page: Page,
  surface: AppSurface,
  route: OperatorRoute | PortalRoute,
): Promise<void> {
  const href = `/${surface}?view=${route}`;
  const link = page.locator(`.app-navigation a[href="${href}"]`);
  await link.waitFor({ state: 'attached', timeout: 10_000 });

  if ((page.viewportSize()?.width ?? 1280) < 900) {
    const menu = page.locator('.app-mobile-menu');
    if (await menu.getAttribute('aria-expanded') !== 'true') await menu.click();
  }

  await link.click();
  await page.locator(`.app-main-content[data-surface="${surface}"][data-route="${route}"]`)
    .waitFor({ state: 'visible', timeout: 10_000 });
  await link.waitFor({ state: 'attached' });
  assert.equal(await link.getAttribute('aria-current'), 'page');
  const url = new URL(page.url());
  assert.equal(url.pathname, `/${surface}`);
  assert.equal(url.searchParams.get('view'), route);
}

export async function openUsageDimension(page: Page, label: string): Promise<void> {
  const dimensions = page.getByRole('tab', { name: /^(维度分析|Dimensions)$/, exact: false });
  if (await dimensions.getAttribute('aria-selected') !== 'true') await dimensions.click();
  const button = page.locator('.usage-dimension-picker').getByRole('button', { name: label, exact: true });
  await button.click();
  assert.equal(await button.getAttribute('aria-pressed'), 'true');
}

export function appPreferenceControls(page: Page): Locator {
  return (page.viewportSize()?.width ?? 1280) < 900
    ? page.locator('.app-mobile-preferences')
    : page.locator('.app-sidebar-footer');
}
