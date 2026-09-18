import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

function operatorUiPackages() {
  const resolvedId = '\0mtc-operator-ui-packages';
  const packageNames = (process.env.MTC_OPERATOR_UI_PACKAGES ?? '')
    .split(',')
    .map((value) => value.trim())
    .filter(Boolean);
  const packageName = /^(?:@[a-z0-9][a-z0-9._-]*\/[a-z0-9][a-z0-9._-]*|[a-z0-9][a-z0-9._-]*)$/u;
  for (const value of packageNames) {
    if (!packageName.test(value)) throw new Error(`Invalid MTC_OPERATOR_UI_PACKAGES entry: ${value}`);
  }
  return {
    name: 'mtc-operator-ui-packages',
    resolveId(id: string) {
      return packageNames.length > 0 && id.endsWith('/plugins/trustedOperatorUiPackages.js') ? resolvedId : undefined;
    },
    load(id: string) {
      if (id !== resolvedId) return undefined;
      const imports = packageNames.map((value, index) => `import { operatorUiPackage as package${index} } from ${JSON.stringify(value)};`);
      return `${imports.join('\n')}\nexport default [${packageNames.map((_, index) => `package${index}`).join(',')}];`;
    },
  };
}

export default defineConfig({
  base: '/ui-assets/',
  plugins: [operatorUiPackages(), react()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: true,
  },
  server: {
    proxy: {
      '/self': 'http://127.0.0.1:8080',
      '/internal': 'http://127.0.0.1:8080',
    },
  },
});
