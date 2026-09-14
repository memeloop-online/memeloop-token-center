import assert from 'node:assert/strict';
import type { Locator } from 'playwright';

/** Re-entering a workspace must reveal the field, not toggle it closed. */
export async function openIndividualAuthorization(editor: Locator, kind: 'credential' | 'model') {
  const disclosure = editor.getByRole('button', { name: kind === 'credential' ? /^单独授权凭据/ : /^单独授权模型/ });
  await disclosure.waitFor({ state: 'visible' });
  if (await disclosure.getAttribute('aria-expanded') === 'false') await disclosure.click();
  await editor.getByRole('combobox', { name: kind === 'credential' ? '授权给具体凭据' : '具体路由', exact: true }).waitFor({ state: 'visible' });
  assert.equal(await disclosure.getAttribute('aria-expanded'), 'true');
}
