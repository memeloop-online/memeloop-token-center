import assert from 'node:assert/strict';
import test from 'node:test';
import { credentialBudgetPresentation } from '../src/operator/credentialListPresentation.js';
import type { KeyView } from '../src/types.js';

const t = (key: string, variables?: Record<string, string | number>) => key === 'credentials.meteredUnlimited' ? 'Unlimited (metered)'
  : key === 'credentials.meteredUnlimitedHint' ? 'Exact usage is recorded.'
    : key === 'credentials.availableBalance' ? `Available: ${variables?.amount}`
      : `Full available balance: ${variables?.amount}`;

function key(enforcement_mode: KeyView['policy']['enforcement_mode'], available_balance: string): KeyView {
  return { key_id: 'key', alias: 'Credential', currency: 'USD', credential_generation: 1, created_at: 0, available_balance, policy: { requests_per_minute: 1, tokens_per_minute: 1, max_concurrency: 1, enforcement_mode, daily_budget: null, weekly_budget: null, lifetime_budget: null } };
}

test('credential budget summaries follow enforcement rather than inferring unlimited from a large prepaid balance', () => {
  const prepaid = credentialBudgetPresentation(key('prepaid', '9223372036854.775807'), 'en', t);
  assert.match(prepaid.text, /^Available:/);
  assert.doesNotMatch(prepaid.text, /9,223,372,036,854/, 'the row uses a compact localized amount');
  assert.match(prepaid.title, /9,223,372,036,854\.775807/, 'the tooltip retains the complete fixed-decimal amount');

  const metered = credentialBudgetPresentation(key('metered_unlimited', '0'), 'en', t);
  assert.equal(metered.text, 'Unlimited (metered)');
  assert.equal(metered.title, 'Exact usage is recorded.');
});
