import { defineConfig } from 'vitest/config'
import path from 'path'

export default defineConfig({
  resolve: {
    alias: {
      src: path.resolve(import.meta.dirname, './src'),
    },
  },
  test: {
    dir: './src',
    environment: 'jsdom',
    pool: 'forks',
    isolate: true,
    setupFiles: ['./tests/env-setup.ts', './tests/ipc-setup.ts'],
    fsModuleCache: true,
  },
})
