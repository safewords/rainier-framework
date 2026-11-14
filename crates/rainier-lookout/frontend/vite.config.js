import { fileURLToPath } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

// Built into ../assets/dist with stable names — lookout.js and lookout.css —
// because the Rust crate compiles them in with `include_bytes!`. The shell
// links them with a `?v=` content hash, so the names never need one.
export default defineConfig({
  plugins: [vue(), tailwindcss()],
  // Every URL the app needs comes from the shell's config at runtime, so the
  // build is independent of where the dashboard is mounted.
  base: './',
  build: {
    outDir: fileURLToPath(new URL('../assets/dist', import.meta.url)),
    emptyOutDir: true,
    cssCodeSplit: false,
    modulePreload: false,
    sourcemap: false,
    target: 'es2022',
    rolldownOptions: {
      input: fileURLToPath(new URL('./src/main.js', import.meta.url)),
      output: {
        entryFileNames: 'lookout.js',
        chunkFileNames: 'lookout-[name].js',
        assetFileNames: 'lookout[extname]',
        codeSplitting: false,
      },
    },
  },
})
