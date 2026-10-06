import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { appendFileSync, readFileSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { performance } from 'node:perf_hooks';
import { setTimeout as delay } from 'node:timers/promises';

assert.equal(process.env.GITHUB_ACTIONS, 'true');
const directory = process.env.COLLECTOR_FIXTURE_DIRECTORY!;
const plan = JSON.parse(readFileSync(join(directory, 'plan.json'), 'utf8'));
const statePath = join(directory, 'state.json');
const state = JSON.parse(readFileSync(statePath, 'utf8'));
const step = plan[state.index++];
assert.ok(step, 'Unexpected collector API invocation');
assert.ok(process.argv.includes(step.requestTimeout ?? '--request-timeout=15s'));
for (const token of step.contains) assert.ok(process.argv.includes(token), `Expected fixture argument ${token}`);
const started = performance.now();
const response = step.reply;
writeFileSync(statePath, JSON.stringify(state));
appendFileSync(join(directory, 'calls.jsonl'), JSON.stringify({ index: state.index, event: 'start', observedAt: new Date().toISOString() }) + '\n');
await delay(step.delayMs ?? 0);
if (process.env.COLLECTOR_LEASE_CONTAINER && step.leaseOperation) {
  const command = process.argv.slice(process.argv.indexOf('--') + 1);
  assert.ok(step.contains.includes('exec'));
  assert.ok(['publish', 'revoke'].includes(step.leaseOperation));
  if (step.leaseOperation === 'publish') {
    execFileSync('docker', ['exec', process.env.COLLECTOR_LEASE_CONTAINER, '/bin/sh', '-ec', 'kill -0 "$(cat /tmp/supervisor.pid)"'], { timeout: 10_000, stdio: 'pipe' });
  }
  execFileSync('docker', ['exec', process.env.COLLECTOR_LEASE_CONTAINER, ...command], { timeout: 10_000, stdio: 'pipe' });
}
appendFileSync(join(directory, 'calls.jsonl'), JSON.stringify({ index: state.index, event: 'end', durationMs: performance.now() - started }) + '\n');
if (step.stderr) process.stderr.write(step.stderr);
process.stdout.write(typeof response === 'string' ? response : JSON.stringify(response ?? {}));
process.exitCode = step.exitCode ?? 0;
