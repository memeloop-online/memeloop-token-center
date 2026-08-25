import assert from 'node:assert/strict';
import test from 'node:test';

import { clearRememberedCredential, readRememberedCredential, rememberCredential } from '../src/credentialStorage.js';

class MemoryStorage {
  private readonly values = new Map<string, string>();
  get length() { return this.values.size; }
  clear() { this.values.clear(); }
  getItem(key: string) { return this.values.get(key) ?? null; }
  key(index: number) { return [...this.values.keys()][index] ?? null; }
  removeItem(key: string) { this.values.delete(key); }
  setItem(key: string, value: string) { this.values.set(key, value); }
}

test('operator and self-service credentials persist in isolated browser keys', () => {
  const storage = new MemoryStorage() as Storage;
  rememberCredential('operator', '  mts_operator  ', storage);
  rememberCredential('self', '  mtk_client  ', storage);

  assert.equal(readRememberedCredential('operator', storage), 'mts_operator');
  assert.equal(readRememberedCredential('self', storage), 'mtk_client');

  clearRememberedCredential('operator', storage);
  assert.equal(readRememberedCredential('operator', storage), '');
  assert.equal(readRememberedCredential('self', storage), 'mtk_client');
});

test('empty values and unavailable storage never break login state', () => {
  const storage = new MemoryStorage() as Storage;
  rememberCredential('operator', '   ', storage);
  assert.equal(storage.length, 0);

  const blocked = {
    getItem() { throw new Error('blocked'); },
    setItem() { throw new Error('blocked'); },
    removeItem() { throw new Error('blocked'); },
  } as unknown as Storage;
  assert.equal(readRememberedCredential('self', blocked), '');
  assert.doesNotThrow(() => rememberCredential('self', 'mtk_client', blocked));
  assert.doesNotThrow(() => clearRememberedCredential('self', blocked));
});
