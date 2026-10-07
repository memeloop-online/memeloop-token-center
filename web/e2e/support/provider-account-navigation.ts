import type { Page } from 'playwright';

export async function manageProviderAccount(page: Page, accountId?: string) {
  const account = accountId ? page.locator(`[data-upstream-id="${accountId}"]`) : page.locator('.provider-account').first();
  const manage = account.locator('.provider-directory-row').getByRole('button', { name: /^(管理账号|Manage account)$/ });
  if (await manage.getAttribute('aria-expanded') !== 'true') await manage.click();
  const workspace = account.locator('.provider-detail-workspace');
  await workspace.waitFor();
  return workspace;
}

export async function editProviderAccount(page: Page, accountId?: string) {
  const workspace = await manageProviderAccount(page, accountId);
  await workspace.getByRole('button', { name: /^(编辑|Edit)$/ }).click();
}
