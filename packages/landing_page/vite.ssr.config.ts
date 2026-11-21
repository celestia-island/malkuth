import { defineConfig } from 'vite'
import vueJsx from '@vitejs/plugin-vue-jsx'
import { resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = fileURLToPath(new URL('.', import.meta.url))

// SSR pass: bundles the prerender entry for Node so scripts/publish.mjs can
// `import()` it and render the fallback page at build time.
//
// `noExternal: true` is required: hikari is distributed as raw .tsx source,
// which Node cannot import.
export default defineConfig({
  plugins: [vueJsx()],
  ssr: { noExternal: true },
  build: {
    ssr: resolve(root, 'src/ssr/prerender.ts'),
    outDir: resolve(root, '../../target/landing_page/.stage/ssr'),
    emptyOutDir: true,
    target: 'node20',
    minify: false,
    cssCodeSplit: false,
    rollupOptions: {
      output: { entryFileNames: 'prerender.mjs', format: 'es' },
    },
  },
})
