import { defineConfig } from 'vitest/config';

export default defineConfig({
  test: {
    environment: 'node',
    exclude: ['tests/e2e/**', 'node_modules/**'],
    coverage: {
      provider: 'v8',
      reporter: ['text', 'json-summary'],
      include: ['src/**/*.ts'],
      exclude: ['src/background.ts', 'src/popup.ts', 'src/sidepanel.ts'],
      thresholds: { lines: 90, statements: 90 }
    }
  }
});
