import { After, AfterAll, Before, BeforeAll, setDefaultTimeout } from '@cucumber/cucumber';
import { liveRuntime } from './runtime.js';
import type { LiveWorld } from './world.js';

setDefaultTimeout(90_000);

BeforeAll({ timeout: 60_000 }, async () => {
  await liveRuntime.start();
});

Before({ tags: '@live and @readonly' }, async function (this: LiveWorld) {
  await this.createBrowserContext();
});

After({ tags: '@live and @readonly' }, async function (this: LiveWorld) {
  try {
    await this.assertReadOnlyAndClean();
  } finally {
    await this.context?.close();
    this.page = undefined;
    this.context = undefined;
  }
});

AfterAll({ timeout: 30_000 }, async () => {
  await liveRuntime.stop();
});
