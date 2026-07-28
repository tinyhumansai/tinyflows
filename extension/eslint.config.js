import eslint from '@eslint/js';
import tseslint from 'typescript-eslint';

export default tseslint.config(
  { ignores: ['dist', 'artifacts', 'coverage', 'node_modules'] },
  eslint.configs.recommended,
  ...tseslint.configs.recommended,
  {
    files: ['**/*.ts'],
    rules: {
      '@typescript-eslint/consistent-type-imports': 'error',
      '@typescript-eslint/no-explicit-any': 'off'
    }
  },
  {
    files: ['scripts/*.mjs'],
    languageOptions: { globals: { process: 'readonly', Buffer: 'readonly', console: 'readonly' } }
  },
  {
    files: ['tests/e2e/*.ts'],
    rules: { 'no-empty-pattern': 'off' }
  }
);
