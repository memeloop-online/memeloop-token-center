import { appendFileSync, readFileSync } from 'node:fs';

const SCOPE = 'expensive CI scope detection';
const [eventName = '', changedPathsValue = '', outputValue = ''] = process.argv.slice(2);
if (!['pull_request', 'push'].includes(eventName)) {
  throw new Error(`${SCOPE}: unsupported event ${JSON.stringify(eventName)}`);
}
if (changedPathsValue === '' || outputValue === '') {
  throw new Error(`${SCOPE}: changed-path and output files are required`);
}
type Change = { status: string; paths: string[] };

function parseNameStatus(value: string): Change[] {
  if (value === '') return [];
  const fields = value.split('\0');
  if (fields.at(-1) !== '') throw new Error(`${SCOPE}: git name-status input must be NUL terminated`);
  fields.pop();
  const changes: Change[] = [];
  for (let index = 0; index < fields.length;) {
    const status = fields[index++]!;
    if (!/^(?:[ACDMRT]|[RC][0-9]{1,3})$/.test(status)) throw new Error(`${SCOPE}: unsupported git diff status ${JSON.stringify(status)}`);
    const pathCount = /^[RC]/.test(status) ? 2 : 1;
    const paths = fields.slice(index, index + pathCount);
    index += pathCount;
    if (paths.length !== pathCount || paths.some((path) => path === '')) throw new Error(`${SCOPE}: ${status} change has an incomplete path set`);
    changes.push({ status, paths });
  }
  return changes;
}

const changes = parseNameStatus(readFileSync(changedPathsValue, 'utf8'));
const paths = changes.flatMap(({ paths }) => paths);
if (eventName === 'pull_request' && changes.length === 0) {
  throw new Error(`${SCOPE}: a pull request must resolve at least one changed path`);
}

// Fail closed: only presentation/documentation/deployment-only paths and
// non-load test sources are known not to affect the optimized service binary
// exercised by the memory gate. Memory/load harness inputs remain outside this
// allowlist, so every unknown or newly introduced path runs acceptance.
const memorySafe = /^(?:docs\/|web\/|charts\/|openapi\/|tests\/(?!load(?:\/|$))|README\.md$|LICENSE$|\.gitignore$|compose\.yaml$)/;
const memory = eventName === 'push' || paths.some((path) => !memorySafe.test(path));
// Rust and migration jobs have no non-web contract that is safe to skip. Both
// sides of rename/copy records participate, so boundary crossings stay full.
const webOnly = changes.length > 0 && changes.every(({ paths }) => paths.every((path) => path.startsWith('web/')));
const rust = eventName === 'push' || !webOnly;
const migration = eventName === 'push' || !webOnly;
const pluginInstaller = eventName === 'push' || paths.some((path) => /^(?:\.cargo\/|\.dockerignore$|\.github\/workflows\/ci\.yml$|Cargo\.(?:toml|lock)$|Dockerfile\.plugin-installer$|packaging\/cosign\/|src\/|migrations\/|schemas\/|wit\/|vendor\/|tests\/ops\/plugin-installer-image-contract\.test\.ts$)/.test(path));

appendFileSync(
  outputValue,
  `rust=${String(rust)}\nmigration=${String(migration)}\nmemory=${String(memory)}\nplugin_installer=${String(pluginInstaller)}\n`,
  'utf8',
);
console.log(JSON.stringify({ event: eventName, change_count: changes.length, web_only: webOnly, rust, migration, memory, plugin_installer: pluginInstaller }));
