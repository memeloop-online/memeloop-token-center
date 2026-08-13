import { defineConfig, devices } from '@playwright/test';

const baseURL = process.env.MTC_E2E_BASE_URL ?? 'http://127.0.0.1:41739';

export default defineConfig({
  testDir: './e2e',
  fullyParallel: false,
  workers: 1,
  // The serial suite provisions state through the public API. Retrying against
  // the same running service would reuse those resources and hide the original
  // failure behind expected uniqueness conflicts.
  retries: 0,
  reporter: process.env.CI ? [['github'], ['list']] : 'list',
  timeout: 45_000,
  expect: { timeout: 10_000 },
  use: {
    baseURL,
    trace: 'retain-on-failure',
    screenshot: 'only-on-failure',
    video: 'retain-on-failure',
  },
  projects: [
    {
      name: 'chromium',
      use: { ...devices['Desktop Chrome'] },
    },
  ],
  webServer: {
    command: 'node e2e/server.mjs',
    url: `${baseURL}/healthz`,
    reuseExistingServer: false,
    // CI performs an explicit cold build first, but allow enough startup time for
    // slower shared runners and cache misses without conflating build latency
    // with a browser assertion failure.
    timeout: 300_000,
  },
});
