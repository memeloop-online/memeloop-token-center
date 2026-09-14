import assert from 'node:assert/strict';
import { appendFileSync, readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { resolve } from 'node:path';

const repository = 'ghcr.io/memeloop-online/memeloop-token-center-plugin-installer';

export function installerEnvironment(record: unknown): string {
  assert(record && typeof record === 'object' && !Array.isArray(record), 'invalid installer trust record');
  const value = record as Record<string, unknown>;
  assert.deepEqual(Object.keys(value).sort(), ['digest', 'format_version', 'repository', 'source_revision', 'status']);
  assert.equal(value.format_version, 1);
  assert.equal(value.repository, repository, 'installer repository must be the reviewed first-party source');
  assert.equal(value.status, 'ready', 'plugin release unavailable: installer release must first be pinned by a reviewed master change');
  assert(typeof value.digest === 'string' && /^sha256:[a-f0-9]{64}$/.test(value.digest), 'reviewed installer digest required');
  assert(typeof value.source_revision === 'string' && /^[a-f0-9]{40}$/.test(value.source_revision), 'reviewed source revision required');
  return `INSTALLER_SOURCE=${repository}\nINSTALLER_DIGEST=${value.digest}\nINSTALLER_SOURCE_REVISION=${value.source_revision}\n`;
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  // No CLI path, dispatch input, registry tag or environment-provided installer
  // reference can override this master-reviewed source file.
  const record = JSON.parse(readFileSync(new URL('../../.github/first-party-plugin-installer-trust.json', import.meta.url), 'utf8'));
  const environment = installerEnvironment(record);
  assert(process.env.GITHUB_ENV, 'GitHub environment output required');
  appendFileSync(process.env.GITHUB_ENV, environment);
}
