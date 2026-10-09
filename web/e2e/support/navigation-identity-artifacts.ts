import assert from 'node:assert/strict';
import { createHash } from 'node:crypto';
import { mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import type { Page } from 'playwright';

type Surface = 'proxy-groups' | 'identity' | 'identity-service-credentials';

const root = fileURLToPath(new URL('../../e2e-artifacts/ui-system/navigation-identity/', import.meta.url));
const ownerSha = '0980bd68e12a615cc7d280295d4c90ea576c5168';
const manifests = new Map<Surface, Array<{
  file: string;
  sha256: string;
  scenario: string;
  locale: string;
  viewport: { width: number; height: number };
  route: string | null;
  theme: 'light';
  assertions: 'passed';
}>>();

export async function seedNavigationIdentity(page: Page, locale: string, preserveLocale = false) {
  await page.addInitScript({ content: `(({ locale, preserveLocale }) => {
    if (!preserveLocale || !localStorage.getItem('mtc-locale')) localStorage.setItem('mtc-locale', locale);
    localStorage.setItem('mtc-theme', 'light');
    const applyTheme = () => {
      if (!document.documentElement) return false;
      document.documentElement.dataset.theme = 'light';
      return true;
    };
    if (!applyTheme()) {
      const observer = new MutationObserver(() => { if (applyTheme()) observer.disconnect(); });
      observer.observe(document, { childList: true });
    }
  })(${JSON.stringify({ locale, preserveLocale })});` });
}

export async function captureNavigationIdentity(page: Page, surface: Surface, scenario: string, locale: string) {
  const directory = `${root}/${surface === 'proxy-groups' ? 'proxy-groups' : 'identity'}`;
  await mkdir(directory, { recursive: true });
  await page.emulateMedia({ colorScheme: 'light' });
  assert.equal(await page.evaluate(() => document.documentElement.dataset.theme), 'light', 'artifact theme must match the actual product theme');
  const entries = manifests.get(surface) ?? [];
  manifests.set(surface, entries);
  for (const viewport of [{ width: 1440, height: 1000 }, { width: 390, height: 844 }]) {
    await page.setViewportSize(viewport);
    const layout = await page.evaluate(() => ({ width: document.documentElement.clientWidth, scroll: document.documentElement.scrollWidth }));
    assert.ok(layout.scroll <= layout.width, `${surface} ${scenario} ${locale} ${viewport.width}px must not overflow`);
    const file = `${surface}--${scenario}--${locale}--${viewport.width}x${viewport.height}--light.png`;
    assert.equal(entries.some(entry => entry.file === file), false, `artifact must have a single writer: ${file}`);
    const bytes = await page.screenshot({ path: `${directory}/${file}`, fullPage: true, animations: 'disabled' });
    assert.ok(bytes.byteLength > 0);
    entries.push({ file, sha256: createHash('sha256').update(bytes).digest('hex'), scenario, locale, viewport,
      route: new URL(page.url()).searchParams.get('view'), theme: 'light', assertions: 'passed' });
    await writeFile(`${directory}/${surface}--manifest.json`, `${JSON.stringify({
      integrated_sha: process.env.GITHUB_SHA ?? null,
      owner_sha: process.env.MTC_NAVIGATION_IDENTITY_OWNER_SHA ?? ownerSha,
      surface,
      evidence: 'synthetic fixture with real product components; not production acceptance',
      screenshots: entries,
    }, null, 2)}\n`);
  }
}
