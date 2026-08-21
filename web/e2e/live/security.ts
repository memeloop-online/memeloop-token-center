import assert from 'node:assert/strict';
import { constants } from 'node:fs';
import { lstat, open } from 'node:fs/promises';

export const readOnlyMethods = new Set(['GET', 'HEAD', 'OPTIONS']);

export function isReadOnlyMethod(method: string): boolean {
  return readOnlyMethods.has(method.toUpperCase());
}

export function assertReadOnlyMethod(method: string): void {
  assert.ok(isReadOnlyMethod(method), `live read-only guard rejected HTTP ${method.toUpperCase()}`);
}

export function assertSecureLiveURL(name: string, url: URL): void {
  assert.equal(url.protocol, 'https:', `${name} must use HTTPS`);
  assert.equal(url.username, '', `${name} must not contain credentials`);
  assert.equal(url.password, '', `${name} must not contain credentials`);
  assert.equal(url.hash, '', `${name} must not contain a fragment`);
}

export function isAllowedLiveDestination(requestURL: string, allowedOrigins: ReadonlySet<string>): boolean {
  try {
    const url = new URL(requestURL);
    return url.protocol === 'https:'
      && url.username === ''
      && url.password === ''
      && allowedOrigins.has(url.origin);
  } catch {
    return false;
  }
}

export function urlContainsCredential(requestURL: string, credentials: readonly string[]): boolean {
  try {
    const url = new URL(requestURL);
    const exposedValues = [url.pathname, ...url.searchParams.values()];
    return credentials.some((credential) => credential.length > 0
      && exposedValues.some((value) => value.includes(credential)));
  } catch {
    return true;
  }
}

export interface CredentialFile {
  credential: string;
  expectedKeyId?: string;
}

export async function readCredentialFile(path: string): Promise<CredentialFile> {
  assert.ok(path, 'a credential file path is required');
  const before = await lstat(path);
  assert.ok(!before.isSymbolicLink(), 'credential file must not be a symbolic link');
  assert.ok(before.isFile(), 'credential file must be a regular file');
  assert.equal(before.mode & 0o777, 0o600, 'credential file permissions must be exactly 0600');
  if (typeof process.getuid === 'function') {
    assert.equal(before.uid, process.getuid(), 'credential file must be owned by the current user');
  }

  const handle = await open(path, constants.O_RDONLY | (constants.O_NOFOLLOW ?? 0));
  try {
    const after = await handle.stat();
    assert.equal(after.dev, before.dev, 'credential file changed while it was opened');
    assert.equal(after.ino, before.ino, 'credential file changed while it was opened');
    assert.ok(after.size > 0 && after.size <= 65_536, 'credential file has an invalid size');
    const content = (await handle.readFile({ encoding: 'utf8' })).trim();
    assert.ok(content, 'credential file must not be empty');
    return parseCredential(content);
  } finally {
    await handle.close();
  }
}

function parseCredential(content: string): CredentialFile {
  if (!content.startsWith('{')) return { credential: content };
  const value: unknown = JSON.parse(content);
  assert.ok(value && typeof value === 'object' && !Array.isArray(value), 'credential JSON must be an object');
  const record = value as Record<string, unknown>;
  const credential = [record.key, record.token, record.credential]
    .find((candidate): candidate is string => typeof candidate === 'string' && candidate.trim().length > 0);
  assert.ok(credential, 'credential JSON must contain key, token, or credential');
  const expectedKeyId = [record.expected_key_id, record.key_id]
    .find((candidate): candidate is string => typeof candidate === 'string' && candidate.trim().length > 0);
  return { credential: credential.trim(), expectedKeyId: expectedKeyId?.trim() };
}
