import assert from 'node:assert/strict';
import { appendFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

// Dispatch selects only a reviewed package, never an executable, path, registry,
// command or installer. Every execution-sensitive value comes from this table.
const packages = {
  'model-guard': {
    id: 'mtc-model-guard',
    crate: 'plugin-sources/model-guard',
    wasm: 'mtc_model_guard.wasm',
    source: 'ghcr.io/memeloop-online/mtc-model-guard',
    hostTest: 'first_party_model_guard',
  },
  'preferred-account': {
    id: 'mtc-preferred-account',
    crate: 'plugin-sources/preferred-account',
    wasm: 'mtc_preferred_account.wasm',
    source: 'ghcr.io/memeloop-online/mtc-preferred-account',
    hostTest: 'first_party_preferred_account',
  },
} as const;

export function firstPartyPackage(name: unknown) {
  assert(typeof name === 'string' && Object.hasOwn(packages, name), 'unknown first-party package');
  return packages[name as keyof typeof packages];
}

export function packageEnvironment(name: unknown): string {
  const item = firstPartyPackage(name);
  return `PLUGIN_ID=${item.id}\nPLUGIN_CRATE=${item.crate}\nPLUGIN_WASM=${item.wasm}\nPLUGIN_SOURCE=${item.source}\nPLUGIN_HOST_TEST=${item.hostTest}\n`;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const environment = packageEnvironment(process.env.REQUESTED_PLUGIN_PACKAGE);
  assert(process.env.GITHUB_ENV, 'GitHub environment output required');
  appendFileSync(process.env.GITHUB_ENV, environment);
}
