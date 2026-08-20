export default {
  paths: ['e2e/live/features/**/*.feature'],
  import: [
    'e2e/tsx-register.js',
    'e2e/live/support/world.ts',
    'e2e/live/support/hooks.ts',
    'e2e/live/step_definitions/**/*.ts',
  ],
  tags: '@live and @readonly',
  format: ['progress'],
  formatOptions: { includeAttachments: false },
  order: 'defined',
  parallel: 0,
  publish: false,
  retry: 0,
  strict: true,
};
