export default {
  paths: ['e2e/features/**/*.feature'],
  import: [
    'e2e/tsx-register.js',
    'e2e/support/world.ts',
    'e2e/support/hooks.ts',
    'e2e/step_definitions/**/*.ts',
  ],
  format: ['progress'],
  formatOptions: { includeAttachments: false },
  order: 'defined',
  parallel: 0,
  publish: false,
  retry: 0,
  strict: true,
};
