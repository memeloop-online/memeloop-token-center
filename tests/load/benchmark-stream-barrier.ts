export type StreamStartBarrierFailure = "timeout" | "client_disconnected";

export interface StreamStartBarrierEvidence {
  required: number;
  admitted: number;
  released: boolean;
  failure: StreamStartBarrierFailure | null;
}

/**
 * A one-shot gate for the memory stream phase. It prevents a slow CI runner
 * from serializing otherwise concurrent client requests before the mock starts
 * emitting any response bytes.
 */
export class StreamStartBarrier {
  private readonly required: number;
  private admitted = 0;
  private released = false;
  private failure: StreamStartBarrierFailure | null = null;
  private readonly waiters = new Set<(released: boolean) => void>();
  private readonly timer: ReturnType<typeof setTimeout>;

  constructor(required: number, timeoutMs: number) {
    if (!Number.isInteger(required) || required < 1) throw new RangeError("stream start barrier required count must be a positive integer");
    if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) throw new RangeError("stream start barrier timeout must be positive");
    this.required = required;
    this.timer = setTimeout(() => this.release("timeout"), timeoutMs);
  }

  arrive(signal?: AbortSignal): Promise<boolean> {
    if (this.released) return Promise.resolve(this.failure === null);
    this.admitted += 1;
    const abort = (): void => this.release("client_disconnected");
    if (signal?.aborted) {
      abort();
      return Promise.resolve(false);
    }
    const waiting = new Promise<boolean>((resolve) => this.waiters.add(resolve));
    signal?.addEventListener("abort", abort, { once: true });
    if (this.admitted >= this.required) this.release();
    return waiting.finally(() => signal?.removeEventListener("abort", abort));
  }

  evidence(): StreamStartBarrierEvidence {
    return {
      required: this.required,
      admitted: this.admitted,
      released: this.released,
      failure: this.failure,
    };
  }

  private release(failure: StreamStartBarrierFailure | null = null): void {
    if (this.released) return;
    this.released = true;
    this.failure = failure;
    clearTimeout(this.timer);
    const passed = failure === null;
    for (const resolve of this.waiters) resolve(passed);
    this.waiters.clear();
  }
}
