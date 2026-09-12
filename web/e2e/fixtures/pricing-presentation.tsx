import React from 'react';
import { createRoot } from 'react-dom/client';
import { I18nProvider } from '../../src/i18n';
import { CredentialPolicySummary } from '../../src/operator/CredentialPolicySummary';
import { PriceSource } from '../../src/operator/PriceSource';
import '../../src/styles.css';
import '../../src/operator/operator.css';
import '../../src/modelPicker.css';

const policy = { requests_per_minute: 4294967295, tokens_per_minute: 9007199254740991, max_concurrency: 4294967295, daily_budget: null, weekly_budget: '1234567', lifetime_budget: null };
createRoot(document.getElementById('root')!).render(<I18nProvider><main style={{ padding: 16 }}>
  <section aria-label="Unlimited credential"><CredentialPolicySummary currency="USD" policy={{ ...policy, enforcement_mode: 'metered_unlimited' }} /></section>
  <section aria-label="Prepaid credential"><CredentialPolicySummary currency="USD" policy={{ ...policy, enforcement_mode: 'prepaid' }} /></section>
  <PriceSource source={'cpamp:import-run/' + 'a'.repeat(160)} />
  <PriceSource source="copied:old-price" />
  <PriceSource source="models.dev" />
</main></I18nProvider>);
