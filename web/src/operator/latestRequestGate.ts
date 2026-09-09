export interface LatestRequest {
  signal: AbortSignal;
  isCurrent: () => boolean;
}

/** Owns one request lane: starting B aborts A, and invalidating a scope rejects both. */
export class LatestRequestGate {
  private sequence = 0;
  private controller?: AbortController;

  begin(): LatestRequest {
    this.controller?.abort();
    const controller = new AbortController();
    const sequence = ++this.sequence;
    this.controller = controller;
    return {
      signal: controller.signal,
      isCurrent: () => sequence === this.sequence && !controller.signal.aborted,
    };
  }

  invalidate() {
    this.sequence += 1;
    this.controller?.abort();
    this.controller = undefined;
  }
}
