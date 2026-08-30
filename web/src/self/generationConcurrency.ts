export class GenerationActionRegistry {
  readonly #controllers = new Map<string, AbortController>();

  has(key: string) {
    return this.#controllers.has(key);
  }

  begin(key: string) {
    if (this.#controllers.has(key)) return undefined;
    const controller = new AbortController();
    this.#controllers.set(key, controller);
    return controller;
  }

  finish(key: string, controller: AbortController) {
    if (this.#controllers.get(key) === controller) this.#controllers.delete(key);
  }

  abortAll() {
    for (const controller of this.#controllers.values()) controller.abort();
    this.#controllers.clear();
  }
}

export function startCompletionPolling(
  run: () => Promise<void>,
  delayMs: number,
  setTimer: (callback: () => void, delay: number) => number = window.setTimeout,
  clearTimer: (timer: number) => void = window.clearTimeout,
) {
  let stopped = false;
  let timer = setTimer(poll, delayMs);
  async function poll() {
    await run();
    if (!stopped) timer = setTimer(poll, delayMs);
  }
  return () => {
    stopped = true;
    clearTimer(timer);
  };
}
