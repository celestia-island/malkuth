import { defineConfig, type Plugin } from 'vite'
import vue from '@vitejs/plugin-vue'
import vueJsx from '@vitejs/plugin-vue-jsx'
import { resolve } from 'path'
import { readFileSync, writeFileSync } from 'fs'

function inlinePlugin(): Plugin {
  let outDir = ''
  return {
    name: 'inline-single-file',
    apply: 'build',
    configResolved(config) {
      outDir = config.build.outDir
    },
    closeBundle() {
      const htmlPath = resolve(outDir, 'index.html')
      let html = readFileSync(htmlPath, 'utf-8')

      // Inline JS bundles
      html = html.replace(
        /<script\b[^>]*\bsrc="([^"]+)"[^>]*>\s*<\/script>/gi,
        (match, srcAttr) => {
          const rel = srcAttr.replace(/^\//, '')
          const jsPath = resolve(outDir, rel)
          try {
            const code = readFileSync(jsPath, 'utf-8')
            return `<script type="module">${code}</script>`
          } catch {
            return match
          }
        }
      )

      // Inline CSS
      html = html.replace(
        /<link\b[^>]*\bhref="([^"]+\.css)"[^>]*\s*\/?>/gi,
        (match, hrefAttr) => {
          const rel = hrefAttr.replace(/^\//, '')
          const cssPath = resolve(outDir, rel)
          try {
            const css = readFileSync(cssPath, 'utf-8')
            return `<style>${css}</style>`
          } catch {
            return match
          }
        }
      )

      // Remove modulepreload links
      html = html.replace(
        /<link\b[^>]*\brel="modulepreload"[^>]*\s*\/?>/gi,
        ''
      )

      writeFileSync(htmlPath, html)
    },
  }
}

export default defineConfig({
  plugins: [vue(), vueJsx(), inlinePlugin()],
  base: '/',
  resolve: {
    alias: {
      '@': resolve(__dirname, 'src'),
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
