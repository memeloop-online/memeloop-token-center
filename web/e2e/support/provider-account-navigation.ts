import type { Page } from 'playwright';

export async function manageProviderAccount(page: Page, accountId?: string) {
  const account = accountId ? page.locator(`[data-upstream-id="${accountId}"]`) : page.locator('.provider-account').first();
  const targetId = accountId ?? await account.getAttribute('data-upstream-id');
  if (!targetId) throw new Error('Account navigation requires an account identity');
  const workspace = page.locator(`[id="provider-details-${targetId}"]`);
  await page.locator('.provider-list:not([hidden]), .provider-detail-workspace').first().waitFor();
  const activeWorkspace = page.locator('.provider-detail-workspace');
  if (await activeWorkspace.isVisible()) {
    if (await activeWorkspace.getAttribute('id') === `provider-details-${targetId}`) return workspace;
    await activeWorkspace.getByRole('button', { name: /^(返回账号列表|Back to account list)$/ }).click();
  }
  await account.locator('.provider-directory-row').getByRole('button', { name: /^(管理账号|Manage account)$/ }).click();
  await workspace.waitFor();
  return workspace;
}

export async function editProviderAccount(page: Page, accountId?: string) {
  const workspace = await manageProviderAccount(page, accountId);
  await workspace.getByRole('button', { name: /^(编辑|Edit)$/ }).click();
}
