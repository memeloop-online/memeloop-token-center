import type { RefreshClock } from './requestRefresh.js';

export const requestOverflowReconcileDelayMs = 250;
export const requestOverflowReconcileCooldownMs = 30_000;

export interface ReconciliationClock extends RefreshClock {
  now: () => number;
}

/**
 * Edge-triggered, sticky overflow reconciliation.
 *
 * Repeated overflow signals collapse into one dirty bit. A signal received
 * while the authoritative query is in flight survives as one trailing pass,
 * and completed passes are separated by a hard cooldown.
 */
export class RequestOverflowReconciliation {
  private activeTicket?: number;
  private blocked = false;
  private dirty = false;
  private disposed = false;
  private lastFinishedAt?: number;
  private nextTicket = 0;
  private resumeImmediately = false;
  private timer?: number;

  constructor(
    private clock: ReconciliationClock,
    private start: (ticket: number) => void,
    private delayMs = requestOverflowReconcileDelayMs,
    private cooldownMs = requestOverflowReconcileCooldownMs,
  ) {}

  get needsReconcile() {
    return this.dirty || this.timer !== undefined || this.activeTicket !== undefined;
  }

  signal() {
    if (this.disposed) return;
    const edge = !this.dirty;
    this.dirty = true;
    if (edge) this.arm();
  }

  setBlocked(blocked: boolean) {
    if (this.disposed || this.blocked === blocked) return;
    this.blocked = blocked;
    if (blocked) this.cancelTimer();
    else this.arm();
  }

  /** Settle the exact pass which was started. Stale/aborted passes are ignored. */
  finish(ticket: number, succeeded: boolean) {
    if (this.disposed || this.activeTicket !== ticket) return;
    this.activeTicket = undefined;
    this.lastFinishedAt = this.clock.now();
    if (!succeeded) this.dirty = true;
    this.arm();
  }

  /**
   * The page claimed a ticket, then became ineligible before it could issue
   * the authoritative query (for example while a foreground load, pause, or
   * filter change committed). Keep the edge and wait for the page to reopen
   * the lane. This did not make a query, so it must not consume the success
   * cooldown; the reopened lane receives one immediate catch-up pass.
   */
  defer(ticket: number) {
    if (this.disposed || this.activeTicket !== ticket) return;
    this.activeTicket = undefined;
    this.dirty = true;
    this.resumeImmediately = true;
    this.setBlocked(true);
  }

  /**
   * Cancel this scope's work. When preserveDirty is true an interrupted pass
   * remains sticky, while the existing cooldown boundary is retained.
   */
  reset(preserveDirty = false, resetCooldown = false) {
    if (this.disposed) return;
    const interrupted = this.needsReconcile;
    this.cancelTimer();
    this.activeTicket = undefined;
    this.dirty = preserveDirty && interrupted;
    if (!this.dirty || resetCooldown) this.resumeImmediately = false;
    if (resetCooldown) this.lastFinishedAt = undefined;
    this.arm();
  }

  dispose() {
    if (this.disposed) return;
    this.cancelTimer();
    this.activeTicket = undefined;
    this.dirty = false;
    this.resumeImmediately = false;
    this.disposed = true;
  }

  private arm() {
    if (this.disposed || this.blocked || !this.dirty || this.timer !== undefined || this.activeTicket !== undefined) return;
    const now = this.clock.now();
    const cooldownDelay = this.lastFinishedAt === undefined ? 0 : this.lastFinishedAt + this.cooldownMs - now;
    const delay = this.resumeImmediately ? 0 : Math.max(this.delayMs, cooldownDelay, 0);
    this.timer = this.clock.schedule(() => {
      this.timer = undefined;
      if (this.disposed || this.blocked || !this.dirty || this.activeTicket !== undefined) {
        this.arm();
        return;
      }
      this.dirty = false;
      this.resumeImmediately = false;
      const ticket = ++this.nextTicket;
      this.activeTicket = ticket;
      this.start(ticket);
    }, delay);
  }

  private cancelTimer() {
    if (this.timer !== undefined) this.clock.cancel(this.timer);
    this.timer = undefined;
  }
}
