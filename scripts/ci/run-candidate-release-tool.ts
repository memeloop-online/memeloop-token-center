import { spawnSync } from 'node:child_process';
const [tool = '', ...args] = process.argv.slice(2);
if (!['verify-published-image', 'verify-ghcr-release'].includes(tool)
    || !/^[0-9a-f]{40}$/.test(process.env.CANDIDATE_SHA ?? '')) {
  throw new Error('candidate release tool: invalid trusted tool or revision');
}
// Existing verifiers consume GITHUB_SHA. Set it only for this trusted child;
// workflow identity and Actions checkout remain anchored to the master SHA.
const result = spawnSync(process.execPath, [`scripts/ci/${tool}.ts`, ...args], {
  env: { ...process.env, GITHUB_SHA: process.env.CANDIDATE_SHA },
  stdio: 'inherit', shell: false,
});
if (result.status !== 0) process.exit(result.status ?? 1);
