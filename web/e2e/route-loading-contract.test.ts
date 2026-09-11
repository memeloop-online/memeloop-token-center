import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import test from 'node:test';

const source = readFileSync(new URL('../src/operator/pages/ManagementPages.tsx', import.meta.url), 'utf8');
const workspace = source.slice(source.indexOf('function RouteWorkspace('), source.indexOf('function CredentialWorkspace('));
const primaryLoad = workspace.slice(workspace.indexOf('const load ='), workspace.indexOf('const searchCredential ='));

test('route list has a one-request critical path', () => {
  assert.match(primaryLoad, /apiRead<ModelRouteView\[\]>\(`\/internal\/v1\/model-routes/);
  assert.doesNotMatch(primaryLoad, /\/internal\/v1\/keys/);
  assert.doesNotMatch(primaryLoad, /Promise\.all/);
});

test('route credentials remain fresh but load only for an opened editor', () => {
  assert.match(workspace, /credentialsRequested/);
  assert.match(workspace, /setCredentialsRequested\(true\);\s*setEditing\(route\)/);
  assert.match(workspace, /onToggle=\{\(event\) => \{ if \(event\.currentTarget\.open\) setCredentialsRequested\(true\); \}\}/);
  assert.match(workspace, /credentialLoadAbort\.current\?\.abort\(\)/);
  assert.match(workspace, /apiRead<KeyView\[\]>\(`\/internal\/v1\/keys/);
});
