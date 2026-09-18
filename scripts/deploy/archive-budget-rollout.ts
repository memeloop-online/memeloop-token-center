import { readFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';

export const ARCHIVE_RESERVATION_SCHEMA = 106;
export const ARCHIVE_RESERVATION_TTL_MS = 10 * 60 * 1000;
const OBSERVATION_MAX_AGE_MS = 30_000;
const roles = ['worker', 'gateway', 'control'] as const;
type Role = typeof roles[number];
type Version = { schemaVersion: number; replicas: number };
// Live replicas include terminating Pods; readyReplicas counts only Pods whose
// actual image supports schemaVersion, not old replicas during a rolling update.
type LiveVersion = Version & { readyReplicas: number };
export type ArchiveRolloutObservation = {
  observedAtMillis: number;
  roles: Record<Role, LiveVersion>;
  /** Live database COUNT(*), not a cached operator quota/metrics response. */
  reservationCount: number | null;
  /** Last v106+ producer stopped; supplied by the deployment controller. */
  lastReservationProducerStoppedAtMillis: number | null;
};
export type ArchiveRolloutTarget = Partial<Record<Role, Version>>;
export type ArchiveRolloutOperation = 'upgrade' | 'rollback' | 'retire-reclaimer';
export type ArchiveRolloutPlan = {
  stages: Array<{ roles: Role[]; waitForReady: boolean }>;
  retainReclaimer: boolean;
};

function integer(value: unknown, label: string): asserts value is number {
  if (!Number.isSafeInteger(value) || Number(value) < 0) throw new Error(`${label} must be a nonnegative integer`);
}

function validateVersion(value: Version, label: string): void {
  if (!value || typeof value !== 'object') throw new Error(`${label} is required`);
  integer(value.schemaVersion, `${label}.schemaVersion`);
  integer(value.replicas, `${label}.replicas`);
}

function reclaims(version: Version): boolean {
  return version.schemaVersion >= ARCHIVE_RESERVATION_SCHEMA && version.replicas >= 1;
}

/**
 * GitOps calls this on live image/schema evidence and the exact desired role
 * changes before applying them. The returned stages are executed in order;
 * worker readiness is the boundary, not merely successful manifest submission.
 */
export function validateArchiveBudgetRollout(
  operation: ArchiveRolloutOperation,
  observed: ArchiveRolloutObservation,
  desired: ArchiveRolloutTarget,
  nowMillis = Date.now(),
): ArchiveRolloutPlan {
  if (!['upgrade', 'rollback', 'retire-reclaimer'].includes(operation)) throw new Error('unknown rollout operation');
  integer(nowMillis, 'nowMillis');
  integer(observed.observedAtMillis, 'observedAtMillis');
  if (observed.observedAtMillis > nowMillis || nowMillis - observed.observedAtMillis > OBSERVATION_MAX_AGE_MS) {
    throw new Error('live rollout observation must be refreshed');
  }
  for (const role of roles) {
    validateVersion(observed.roles[role], `live.${role}`);
    integer(observed.roles[role].readyReplicas, `live.${role}.readyReplicas`);
    if (desired[role] !== undefined) validateVersion(desired[role]!, `desired.${role}`);
  }
  if (Object.keys(desired).some(role => !roles.includes(role as Role))) throw new Error('unsupported role in rollout target');
  const worker = desired.worker ?? observed.roles.worker;
  const producers = ['gateway', 'control'] as const;
  const hasNewProducer = producers.some(role => reclaims(desired[role] ?? observed.roles[role]));
  const removesReclaimer = reclaims(observed.roles.worker) && !reclaims(worker);

  if (operation === 'retire-reclaimer') {
    if (!removesReclaimer || producers.some(role => desired[role] !== undefined)) {
      throw new Error('reclaimer retirement changes only the worker after producer rollback');
    }
    if (hasNewProducer || producers.some(role => reclaims(observed.roles[role]))) {
      throw new Error('reservation producers are still running');
    }
    const stopped = observed.lastReservationProducerStoppedAtMillis;
    integer(stopped, 'lastReservationProducerStoppedAtMillis');
    if (stopped > observed.observedAtMillis || observed.observedAtMillis - stopped < ARCHIVE_RESERVATION_TTL_MS) {
      throw new Error('retain the reclaimer until the reservation TTL has elapsed after the last producer stopped');
    }
    integer(observed.reservationCount, 'reservationCount');
    if (observed.reservationCount !== 0) throw new Error('retain the reclaimer until the live reservation count is zero');
    return { stages: [{ roles: ['worker'], waitForReady: true }], retainReclaimer: false };
  }

  if (removesReclaimer) throw new Error('worker downgrade requires a separate verified reclaimer retirement');
  if (operation === 'rollback' && desired.worker !== undefined) {
    throw new Error('rollback changes gateway/control only; retain the installed worker image');
  }
  if (hasNewProducer && !reclaims(worker)) throw new Error('upgrade the v106 reclaimer before reservation producers');
  const stages: ArchiveRolloutPlan['stages'] = [];
  if (desired.worker !== undefined) stages.push({ roles: ['worker'], waitForReady: true });
  if (hasNewProducer && desired.worker === undefined && observed.roles.worker.readyReplicas < 1) {
    throw new Error('an available v106 reclaimer is required before producer rollout');
  }
  const changedProducers = producers.filter(role => desired[role] !== undefined);
  if (changedProducers.length) stages.push({ roles: changedProducers, waitForReady: true });
  return { stages, retainReclaimer: reclaims(worker) };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [operation, observedFile, desiredFile] = process.argv.slice(2);
  if (!operation || !observedFile || !desiredFile || process.argv.length !== 5) {
    throw new Error('usage: archive-budget-rollout.ts <upgrade|rollback|retire-reclaimer> <live-observation.json> <desired-roles.json>');
  }
  const observed = JSON.parse(readFileSync(observedFile, 'utf8')) as ArchiveRolloutObservation;
  const desired = JSON.parse(readFileSync(desiredFile, 'utf8')) as ArchiveRolloutTarget;
  console.log(JSON.stringify(validateArchiveBudgetRollout(operation as ArchiveRolloutOperation, observed, desired)));
}
