import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  base: '/ui-assets/',
  plugins: [react()],
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
