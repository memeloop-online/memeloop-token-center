import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const builder = await readFile(new URL('../src/operator/TypedFilterBuilder.tsx', import.meta.url), 'utf8');
const requests = await readFile(new URL('../src/operator/pages/RequestsPage.tsx', import.meta.url), 'utf8');
const settings = await readFile(new URL('../src/operator/pages/SystemSettingsPage.tsx', import.meta.url), 'utf8');

test('professional request filters are schema-backed, previewed, and catalog-bound', () => {
  assert.match(builder, /role="dialog"/);
  assert.match(builder, /\/internal\/v1\/upstream-models/);
  assert.match(builder, /setAssistantPlan/);
  assert.match(builder, /filter\.usePreview/);
  assert.match(requests, /\/internal\/v1\/requests\/query/);
  assert.match(requests, /request-technical-details/);
});

test('system filter assistant configuration persists a route reference rather than a secret', () => {
  assert.match(settings, /FilterAssistantSettings/);
  assert.match(settings, /model_route_id/);
  assert.doesNotMatch(settings, /credential_secret|api[_-]?key|client_secret/i);
});
