#!/usr/bin/env node
// build.mjs — the single entry point `cargo build` reaches through build.rs
// (`pnpm build`): two vite passes, then exactly one publisher.
//
// Nothing outside target/landing_page/.stage/ is written until both passes
// have exited 0, and scripts/publish.mjs deletes the build stamp BEFORE it
// touches artifacts. build.rs refuses to embed a directory without a stamp,
// so a half-written or failed build can never be mistaken for a good one.
import { spawnSync } from 'node:child_process'
import { existsSync, readdirSync, readFileSync, rmSync, statSync } from 'node:fs'
import { dirname, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

const app = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const vite = resolve(app, 'node_modules/.bin/vite')
const outDir = resolve(app, '../../target/landing_page')

function step(label, file, args) {
  console.log(`\n── ${label} ${'─'.repeat(Math.max(0, 56 - label.length))}`)
  const res = spawnSync(file, args, { cwd: app, stdio: 'inherit' })
  if (res.error) {
    console.error(`build: could not run ${file}: ${res.error.message}`)
    process.exit(3)
  }
  if (res.status !== 0) {
    console.error(`build: ${label} FAILED (exit ${res.status}) — publishing nothing`)
    process.exit(res.status ?? 1)
  }
}

if (!existsSync(vite)) {
  console.error(`build: ${relative(app, vite)} missing — run \`pnpm install\` first`)
  process.exit(4)
}

// Staging is disposable. The artifact directory is NOT cleaned here: a failed
// build must leave the previous good artifacts (and stamp) intact so the
// Rust-side freshness guard sees a real error instead of a truncated dir.
rmSync(resolve(outDir, '.stage'), { recursive: true, force: true })

step('vite build (SPA)', vite, ['build'])
step('vite build (SSR prerender)', vite, ['build', '--config', 'vite.ssr.config.ts'])
step('publish (inline + prerender + tokens)', process.execPath, [
  resolve(app, 'scripts/publish.mjs'),
])

console.log('\nbuild: ok')
for (const name of readdirSync(outDir).sort()) {
  if (name.startsWith('.')) continue
  const path = resolve(outDir, name)
  if (!statSync(path).isFile()) continue
  const bytes = readFileSync(path).length
  console.log(`  ${name.padEnd(22)} ${String(bytes).padStart(9)} B`)
}