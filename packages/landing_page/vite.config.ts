import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import { resolve } from 'path'

export default defineConfig({
  plugins: [vue()],
  base: '/',
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
      '@celestia-island/hikari': resolve(__dirname, '../../../hikari/packages/vue'),
    },
  },
  build: {
    outDir: resolve(__dirname, '../../target/landing_page'),
    emptyOutDir: true,
    cssCodeSplit: false,
    rollupOptions: {
      output: {
        manualChunks: undefined,
        inlineDynamicImports: true,
        assetFileNames: 'assets/[name].[ext]',
        entryFileNames: 'assets/index.js',
      },
    },
  },
})
