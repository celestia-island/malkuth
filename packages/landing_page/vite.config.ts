import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import vueJsx from '@vitejs/plugin-vue-jsx'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('.', import.meta.url))

// SPA pass. Writes ONLY into the staging directory: scripts/publish.mjs is
// the single writer of the artifacts build.rs embeds, and it runs only after
// every vite pass has exited 0. The previous in-tree `closeBundle` inlining
// rewrote dist even on a failed build, which is exactly the trap this shape
// removes.
export default defineConfig({
  plugins: [vue(), vueJsx()],
  base: '/',
  // hikari ships raw TSX sources (its exports point at src/*.tsx), so the
  // dev-server pre-bundler must leave it alone — it cannot parse JSX.
  optimizeDeps: { exclude: ['@celestia-island/hikari'] },
  build: {
    outDir: resolve(root, '../../target/landing_page/.stage/spa'),
    emptyOutDir: true,
    target: 'es2020',
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        manualChunks: undefined,
        codeSplitting: false,
        entryFileNames: 'assets/index.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/[name].[ext]',
      },
    },
  },
})