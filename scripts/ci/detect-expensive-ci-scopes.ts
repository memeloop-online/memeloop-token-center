import { appendFileSync, readFileSync } from 'node:fs';
import { execFileSync } from 'node:child_process';

const SCOPE = 'expensive CI scope detection';
const [eventName = '', changedPathsValue = '', outputValue = ''] = process.argv.slice(2);
if (!['pull_request', 'push'].includes(eventName)) {
  throw new Error(`${SCOPE}: unsupported event ${JSON.stringify(eventName)}`);
}
if (changedPathsValue === '' || outputValue === '') {
  throw new Error(`${SCOPE}: changed-path and output files are required`);
}
type Change = { status: string; paths: string[] };

function checkedOutMergeDiff(): { input: string; base: string } | undefined {
  const checkout = process.env.GITHUB_SHA ?? '';
  const head = process.env.PR_HEAD_SHA ?? '';
  if (![checkout, head].every((sha) => /^[0-9a-f]{40}$/i.test(sha))) return undefined;
  const git = (...args: string[]): string => execFileSync('git', ['--no-replace-objects', ...args], {
    encoding: 'utf8',
    stdio: ['ignore', 'pipe', 'pipe'],
    maxBuffer: 16 * 1024 * 1024,
  });
  try {
    if (git('rev-parse', '--verify', 'HEAD').trim() !== checkout.toLowerCase()) return undefined;
    const [commit, base, parentHead, ...extra] = git('rev-list', '--parents', '-n', '1', checkout).trim().split(/\s+/);
    if (commit !== checkout.toLowerCase() || !base || !/^[0-9a-f]{40}$/.test(base)
      || parentHead !== head.toLowerCase() || extra.length !== 0) return undefined;
    // Event base.sha can lag the base actually used to build this checkout.
    // Compare the verified merge tree with its real first parent, never a
    // guessed branch/ref or a stale event base that includes unrelated work.
    const input = git('diff', '--name-status', '--find-renames=100%', '--diff-filter=ACDMRT', '-z', base, checkout);
    return input === '' ? undefined : { input, base };
  } catch {
    return undefined;
  }
}

function checkedOutPushDiff(): { input: string; base: string } | undefined {
  const checkout = process.env.GITHUB_SHA ?? '';
  const before = process.env.GITHUB_EVENT_BEFORE ?? '';
  const after = process.env.GITHUB_EVENT_AFTER ?? '';
  if (![checkout, before, after].every((sha) => /^[0-9a-f]{40}$/i.test(sha)) || after.toLowerCase() !== checkout.toLowerCase()) return undefined;
  const git = (...args: string[]): string => execFileSync('git', ['--no-replace-objects', ...args], { encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 16 * 1024 * 1024 });
  try {
    if (git('rev-parse', '--verify', 'HEAD').trim() !== checkout.toLowerCase()) return undefined;
    git('rev-parse', '--verify', `${before}^{commit}`);
    const input = git('diff', '--name-status', '--find-renames=100%', '--diff-filter=ACDMRT', '-z', before, checkout);
    return input === '' ? undefined : { input, base: before };
  } catch { return undefined; }
}

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

const verifiedMergeMode = changedPathsValue === '--verified-merge';
const merge = verifiedMergeMode ? (eventName === 'pull_request' ? checkedOutMergeDiff() : checkedOutPushDiff()) : undefined;
const forceFull = verifiedMergeMode && merge === undefined;
if (forceFull) console.warn(`${SCOPE}: merge checkout could not be verified; running all gates`);
const changes = parseNameStatus(verifiedMergeMode ? merge?.input ?? '' : readFileSync(changedPathsValue, 'utf8'));
const paths = changes.flatMap(({ paths }) => paths);
if (eventName === 'pull_request' && !forceFull && changes.length === 0) {
  throw new Error(`${SCOPE}: a pull request must resolve at least one changed path`);
}

// Fail closed: only presentation/documentation/deployment-only paths and
// non-load test sources are known not to affect the optimized service binary
// exercised by the memory gate. Memory/load harness inputs remain outside this
// allowlist, so every unknown or newly introduced path runs acceptance.
const memorySafe = /^(?:docs\/|web\/|charts\/|openapi\/|tests\/(?!load(?:\/|$))|README\.md$|LICENSE$|\.gitignore$|compose\.yaml$)/;
const fullCoverage = forceFull || (eventName === 'push' && !verifiedMergeMode);
const memory = eventName === 'push' || fullCoverage || paths.some((path) => !memorySafe.test(path));
// Both sides of rename/copy records participate, so a move to or from any
// executable path stays full. Documentation and the browser application have
// no service-binary inputs; treating that combined presentation-only surface
// as one scope keeps a UI change accompanied by its design documentation from
// rebuilding Rust, replaying migrations, or running the memory harness.
// Static operator-contract tests neither build nor execute the service; their
// packaging contract job remains mandatory, while production source, workflow,
// and CI-script changes remain full coverage.
const presentationOnly = changes.length > 0
  && changes.every(({ paths }) => paths.every((path) => /^(?:docs|web)\//.test(path)));
const memoryAcceptance = fullCoverage || (eventName === 'push' ? !presentationOnly : memory);
const staticContractsOnly = changes.length > 0 && changes.every(({ paths }) => paths.every((path) => path.startsWith('tests/ops/')));
const rust = fullCoverage || !(presentationOnly || staticContractsOnly);
const web = fullCoverage || !staticContractsOnly;
const migration = fullCoverage || !(presentationOnly || staticContractsOnly);
const pluginInstaller = fullCoverage || paths.some((path) => /^(?:\.cargo\/|\.dockerignore$|\.github\/workflows\/ci\.yml$|Cargo\.(?:toml|lock)$|Dockerfile\.plugin-installer$|packaging\/cosign\/|src\/|migrations\/|schemas\/|wit\/|vendor\/|tests\/ops\/plugin-installer-image-contract\.test\.ts$)/.test(path));

appendFileSync(
  outputValue,
  `rust=${String(rust)}\nweb=${String(web)}\nmigration=${String(migration)}\nmemory=${String(memory)}\nmemory_acceptance=${String(memoryAcceptance)}\nplugin_installer=${String(pluginInstaller)}\n`,
  'utf8',
);
console.log(JSON.stringify({ event: eventName, comparison_base: merge?.base, force_full: forceFull, change_count: changes.length, presentation_only: presentationOnly, static_contracts_only: staticContractsOnly, rust, web, migration, memory, memory_acceptance: memoryAcceptance, plugin_installer: pluginInstaller }));
