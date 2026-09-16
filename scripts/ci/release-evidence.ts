import { spawnSync } from 'node:child_process';
import { closeSync, lstatSync, openSync, realpathSync, writeFileSync } from 'node:fs';
import { resolve } from 'node:path';

export type Json = null | boolean | number | string | Json[] | { [key: string]: Json };
export type JsonObject = { [key: string]: Json };

export function fail(scope: string, message: string): never {
  throw new Error(`${scope}: ${message}`);
}

export function requireDigest(value: string, scope: string, label: string): string {
  if (!/^sha256:[0-9a-f]{64}$/.test(value)) fail(scope, `${label} must be a lowercase sha256 digest`);
  return value;
}

export function requireRevision(value: string | undefined, scope: string): string {
  if (value === undefined || !/^[0-9a-f]{40}$/.test(value)) fail(scope, 'GITHUB_SHA must be an exact lowercase 40-hex revision');
  return value;
}

export function requireCanonicalDirectory(path: string, scope: string, label: string): string {
  let metadata;
  try { metadata = lstatSync(path); } catch { fail(scope, `${label} does not exist`); }
  if (!metadata.isDirectory() || metadata.isSymbolicLink() || realpathSync(path) !== resolve(path)) {
    fail(scope, `${label} must be a canonical non-symlink directory`);
  }
  return path;
}

export function parseObject(payload: string, scope: string, label: string): JsonObject {
  let value: Json;
  try { value = JSON.parse(payload) as Json; } catch { fail(scope, `${label} is invalid JSON`); }
  if (value === null || Array.isArray(value) || typeof value !== 'object') fail(scope, `${label} must be a JSON object`);
  return value as JsonObject;
}

export function writeExclusive(path: string, payload: string, scope: string): void {
  let descriptor = -1;
  try {
    descriptor = openSync(path, 'wx', 0o600);
    writeFileSync(descriptor, payload, 'utf8');
  } catch {
    fail(scope, `refused to overwrite or create evidence at ${path}`);
  } finally { if (descriptor >= 0) closeSync(descriptor); }
}

export function run(command: string, args: string[], scope: string, label: string): string {
  const result = spawnSync(command, args, {
    encoding: 'utf8',
    shell: false,
    stdio: ['ignore', 'pipe', 'pipe'],
    maxBuffer: 128 * 1024 * 1024,
  });
  if (result.status !== 0) fail(scope, `${label} failed`);
  return result.stdout;
}
