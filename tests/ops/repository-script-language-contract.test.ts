import assert from 'node:assert/strict';
import { existsSync, readFileSync } from 'node:fs';
import { extname, resolve } from 'node:path';
import test from 'node:test';
import { repository, run } from './contract-helpers.ts';

test('all tracked repository scripts use TypeScript and Node 24', () => {
  const tracked = run('git', ['ls-files', '-z', '--cached', '--others', '--exclude-standard']).split('\0').filter(Boolean);
  const present = tracked.filter((path) => existsSync(resolve(repository, path)));
  const forbidden = present.filter((path) => ['.py', '.pyi', '.pyc', '.sh', '.cjs'].includes(extname(path)) || path.includes('__pycache__/'));
  assert.deepEqual(forbidden, [], `forbidden tracked script files:\n${forbidden.join('\n')}`);
  const candidates = present.filter((path) => !path.startsWith('vendor/') && !path.endsWith('package-lock.json') && !path.endsWith('Cargo.lock'));
  const badShebangs: string[] = [];
  const badCalls: string[] = [];
  const badModuleOrEval: string[] = [];
  const commonJsCall = new RegExp(`${['requ', 'ire'].join('')}\\s*\\(`, 'u');
  const nodeEvalCommand = new RegExp(`\\bnode(?:js)?\\s+(?:${['--ev', 'al'].join('')}|-${'e'}|--print|-p)\\b`, 'u');
  const nodeEvalArgument = new RegExp(`['\"](?:-${'e'}|${['--ev', 'al'].join('')}|--print)['\"]`, 'u');
  for (const path of candidates) {
    let body: string;
    try { body = readFileSync(resolve(repository, path), 'utf8'); } catch { continue; }
    if (/^#!.*(?:python|\b(?:ba|z|k)?sh\b)/m.test(body)) badShebangs.push(path);
    if (/(?:spawn|spawnSync|execFile|execFileSync|run|rejected)\s*\(\s*['"](?:python\d*|bash|sh)['"]/.test(body) ||
        /^\s*(?:run|shell):\s*(?:python\d*|bash|sh)\s*$/m.test(body) || /shell\s*:\s*true/u.test(body) ||
        /import\s*\{[^}]*\bexec(?:Sync)?\b[^}]*\}\s*from\s*['"]node:child_process['"]/su.test(body)) badCalls.push(path);
    if (commonJsCall.test(body) || nodeEvalCommand.test(body) || nodeEvalArgument.test(body)) badModuleOrEval.push(path);
  }
  assert.deepEqual(badShebangs, [], `forbidden Python/shell shebangs:\n${badShebangs.join('\n')}`);
  assert.deepEqual(badCalls, [], `forbidden Python/shell process calls:\n${badCalls.join('\n')}`);
  assert.deepEqual(badModuleOrEval, [], `forbidden CommonJS or Node eval execution:\n${badModuleOrEval.join('\n')}`);
  assert.equal(process.versions.node.split('.')[0], '24', `repository contracts require Node 24, got ${process.version}`);
});
