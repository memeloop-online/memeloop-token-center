import assert from 'node:assert/strict';
import test from 'node:test';
import {
  ARCHIVE_RESERVATION_TTL_MS,
  validateArchiveBudgetRollout,
  type ArchiveRolloutObservation,
} from '../../scripts/deploy/archive-budget-rollout.ts';
import { read } from './contract-helpers.ts';

const now = 2_000_000;
const version = (schemaVersion: number) => ({ schemaVersion, replicas: 1, readyReplicas: 1 });
function snapshot(schema = 105): ArchiveRolloutObservation {
  return {
    observedAtMillis: now,
    roles: { worker: version(schema), gateway: version(schema), control: version(schema) },
    reservationCount: 0,
    lastReservationProducerStoppedAtMillis: null,
  };
}

test('v106 upgrade stages a ready reclaimer before either producer', () => {
  const target = { worker: version(106), gateway: version(106), control: version(106) };
  assert.deepEqual(validateArchiveBudgetRollout('upgrade', snapshot(), target, now), {
    stages: [{ roles: ['worker'], waitForReady: true }, { roles: ['gateway', 'control'], waitForReady: true }],
    retainReclaimer: true,
  });
  assert.throws(() => validateArchiveBudgetRollout('upgrade', snapshot(), { gateway: version(106) }, now), /reclaimer/);
  const live = snapshot(106);
  live.roles.worker.readyReplicas = 0;
  assert.throws(() => validateArchiveBudgetRollout('upgrade', live, { gateway: version(106) }, now), /available/);
});

test('rollback preserves the installed reclaimer and rejects whole-service downgrade', () => {
  const live = snapshot(106);
  assert.deepEqual(validateArchiveBudgetRollout('rollback', live, { gateway: version(105), control: version(105) }, now), {
    stages: [{ roles: ['gateway', 'control'], waitForReady: true }], retainReclaimer: true,
  });
  assert.throws(() => validateArchiveBudgetRollout('rollback', live, { worker: version(105) }, now), /retirement/);
  assert.throws(() => validateArchiveBudgetRollout('rollback', live, { worker: version(106) }, now), /gateway\/control only/);
});

test('reclaimer retirement requires stopped producers, elapsed TTL, fresh zero-count evidence', () => {
  const live = snapshot(105);
  live.roles.worker = version(106);
  const target = { worker: version(105) };
  assert.throws(() => validateArchiveBudgetRollout('retire-reclaimer', live, target, now), /StoppedAtMillis/);
  live.lastReservationProducerStoppedAtMillis = now - ARCHIVE_RESERVATION_TTL_MS + 1;
  assert.throws(() => validateArchiveBudgetRollout('retire-reclaimer', live, target, now), /TTL/);
  live.lastReservationProducerStoppedAtMillis -= 1;
  live.reservationCount = 1;
  assert.throws(() => validateArchiveBudgetRollout('retire-reclaimer', live, target, now), /zero/);
  live.reservationCount = null;
  assert.throws(() => validateArchiveBudgetRollout('retire-reclaimer', live, target, now), /reservationCount/);
  live.reservationCount = 0;
  assert.deepEqual(validateArchiveBudgetRollout('retire-reclaimer', live, target, now), {
    stages: [{ roles: ['worker'], waitForReady: true }], retainReclaimer: false,
  });
  assert.throws(() => validateArchiveBudgetRollout('retire-reclaimer', live, target, now + 30_001), /refreshed/);
  live.roles.gateway = version(106);
  assert.throws(() => validateArchiveBudgetRollout('retire-reclaimer', live, target, now), /still running/);
});

test('rollout retirement TTL stays aligned with durable reservation expiry', () => {
  const rust = read('src/db/archive_spool/reservations.rs');
  const expression = /const\s+RESERVATION_TTL\s*:\s*i64\s*=\s*([\d_\s*]+);/.exec(rust)?.[1];
  assert.ok(expression, 'the worker TTL must remain readable by the rollout contract');
  const rustTtl = expression.split('*').map(term => Number(term.replaceAll('_', '').trim())).reduce((total, factor) => total * factor, 1);
  assert.equal(ARCHIVE_RESERVATION_TTL_MS, rustTtl);
});
