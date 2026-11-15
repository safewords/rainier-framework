import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'

// The session-replay recorder, as one classic script for rum.js to load:
// assets/dist/rum-replay.js, compiled into the crate beside the dashboard.
// Built after the dashboard, whose build empties the directory first.
export default defineConfig({
  build: {
    outDir: fileURLToPath(new URL('../assets/dist', import.meta.url)),
    emptyOutDir: false,
    sourcemap: false,
    target: 'es2019',
    lib: {
      entry: fileURLToPath(new URL('./src/replay-recorder.js', import.meta.url)),
      name: 'LookoutReplayBundle',
      formats: ['iife'],
      fileName: () => 'rum-replay.js',
    },
  },
})
