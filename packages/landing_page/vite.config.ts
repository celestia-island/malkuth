import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import vueJsx from '@vitejs/plugin-vue-jsx'
import { resolve } from 'path'

export default defineConfig({
  plugins: [vue(), vueJsx()],
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
  },
})
