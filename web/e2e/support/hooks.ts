import { After, AfterAll, Before, BeforeAll, setDefaultTimeout } from '@cucumber/cucumber';
import { runtime } from './runtime.js';
import type { DogfoodWorld } from './world.js';

setDefaultTimeout(60_000);

BeforeAll({ timeout: 360_000 }, async () => {
  await runtime.start();
});

Before(async function (this: DogfoodWorld) {
  await this.createBrowserContext();
});

After(async function (this: DogfoodWorld) {
  await this.context?.close();
  this.page = undefined;
  this.context = undefined;
});

AfterAll({ timeout: 30_000 }, async () => {
  await runtime.stop();
});
