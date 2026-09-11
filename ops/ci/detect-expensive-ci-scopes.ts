import { appendFileSync, readFileSync } from 'node:fs';

const SCOPE = 'expensive CI scope detection';
const [eventName = '', changedPathsValue = '', outputValue = ''] = process.argv.slice(2);
if (!['pull_request', 'push'].includes(eventName)) {
  throw new Error(`${SCOPE}: unsupported event ${JSON.stringify(eventName)}`);
}
if (changedPathsValue === '' || outputValue === '') {
  throw new Error(`${SCOPE}: changed-path and output files are required`);
}
const paths = readFileSync(changedPathsValue, 'utf8').split('\n').filter(Boolean);
if (eventName === 'pull_request' && paths.length === 0) {
  throw new Error(`${SCOPE}: a pull request must resolve at least one changed path`);
}

// Fail closed: only presentation/documentation/deployment-only paths and
// non-load test sources are known not to affect the optimized service binary
// exercised by the memory gate. Memory/load harness inputs remain outside this
// allowlist, so every unknown or newly introduced path runs acceptance.
const memorySafe = /^(?:docs\/|web\/|charts\/|openapi\/|tests\/(?!load(?:\/|$))|README\.md$|LICENSE$|\.gitignore$|compose\.yaml$)/;
const memory = eventName === 'push' || paths.some((path) => !memorySafe.test(path));
const pluginInstaller = eventName === 'push' || paths.some((path) => /^(?:\.cargo\/|\.dockerignore$|\.github\/workflows\/ci\.yml$|Cargo\.(?:toml|lock)$|Dockerfile\.plugin-installer$|packaging\/cosign\/|src\/|migrations\/|schemas\/|wit\/|vendor\/|tests\/ops\/plugin-installer-image-contract\.test\.ts$)/.test(path));

appendFileSync(
  outputValue,
  `memory=${String(memory)}\nplugin_installer=${String(pluginInstaller)}\n`,
  'utf8',
);
console.log(JSON.stringify({ event: eventName, changed_path_count: paths.length, memory, plugin_installer: pluginInstaller }));
