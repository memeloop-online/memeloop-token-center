import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';
import test from 'node:test';

const [surface, replayView, replayStyles, operatorSessions, selfSessions, i18n] = await Promise.all([
  readFile(new URL('../src/SessionViews.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/sessionReplayViews.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/sessionReplay.css', import.meta.url), 'utf8'),
  readFile(new URL('../src/operator/SessionMonitor.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/self/SessionsPage.tsx', import.meta.url), 'utf8'),
  readFile(new URL('../src/i18n.tsx', import.meta.url), 'utf8'),
]);

test('sessions surface supplies a scoped archive reader without moving credentials into the replay component', () => {
  assert.match(surface, /loadReplayArchive\?: SessionReplayArchiveLoader/);
  assert.match(surface, /<SessionReplayPanel detail=\{detail\} loadArchiveDetail=\{loadReplayArchive\}/);
  assert.match(operatorSessions, /requestArchivePath\(tenant, request\.request_id\)/);
  assert.match(operatorSessions, /tenant_external_id/);
  assert.match(operatorSessions, /loadReplayArchive=\{loadReplayArchive\}/);
  assert.match(selfSessions, /`\/self\/v1\/requests\/\$\{encodeURIComponent\(request\.request_id\)\}`/);
  assert.match(selfSessions, /loadReplayArchive=\{loadReplayArchive\}/);
  assert.doesNotMatch(replayView, /credential|Authorization|Bearer/);
});

test('replay reads are bounded, abortable, and accept only an exact request and session match', () => {
  assert.match(replayView, /SESSION_REPLAY_MAX_REQUESTS/);
  assert.match(replayView, /REPLAY_ARCHIVE_CONCURRENCY = 4/);
  assert.match(replayView, /const controller = new AbortController\(\)/);
  assert.match(replayView, /candidate\.request_id !== request\.request_id \|\| candidate\.session_context\?\.session_id !== detail\.session_id/);
  assert.match(replayView, /controller\.abort\(\)/);
  assert.match(replayView, /projectSessionReplay\(detail\.session_id, archiveDetails\)/);
  assert.match(replayView, /outside_session/);
  assert.doesNotMatch(replayView, /dangerouslySetInnerHTML|\beval\s*\(/);
});

test('replay provides localized user-turn navigation, compact archive state, and responsive dark/light presentation', () => {
  assert.match(replayView, /title=\{fullText\}/);
  assert.match(replayView, /scrollIntoView\(\{ block: 'center', behavior: 'smooth' \}\)/);
  assert.match(replayView, /useI18n/);
  assert.match(replayView, /sessionReplay\.toolCall/);
  assert.match(replayView, /sessionReplay\.toolResult/);
  assert.match(replayView, /sessionReplay\.archiveUnknown/);
  assert.match(replayView, /sessionReplay\.pairing\.\$\{item\.pairing\}/);
  assert.doesNotMatch(replayView, /ARCHIVE UNKNOWN|ARCHIVE INCOMPLETE|TOOL CALL|TOOL RESULT/);
  for (const key of ['sessionReplay.title', 'sessionReplay.userTurns', 'sessionReplay.toolCall', 'sessionReplay.toolResult', 'sessionReplay.archiveUnavailableBoth', 'sessionReplay.expand', 'sessionReplay.pairing.paired'] as const) {
    assert.match(i18n, new RegExp(`'${key.replaceAll('.', '\\.')}':`));
  }
  assert.match(replayView, /REPLAY_EXPAND_TEXT_LENGTH/);
  assert.match(replayView, /session-replay-preview/);
  assert.match(replayView, /archiveBodies = 2/);
  assert.match(replayView, /<ArchiveText kind="message"/);
  assert.match(replayStyles, /--replay-surface/);
  assert.match(replayStyles, /font: 14px\/1\.65 Inter/);
  assert.match(replayStyles, /\.session-replay-body\.tool \{ font-family: ui-monospace/);
  assert.doesNotMatch(replayStyles, /max-height: 260px/);
  assert.match(replayStyles, /:root\[data-theme='light'\] \.session-replay/);
  assert.match(replayStyles, /@media \(min-width: 1440px\)/);
  assert.match(replayStyles, /@media \(max-width: 768px\)/);
  assert.match(replayStyles, /@media \(max-width: 320px\)/);
});
