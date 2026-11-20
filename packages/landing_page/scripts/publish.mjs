#!/usr/bin/env node
// publish.mjs — the ONLY writer of the artifact directory that build.rs
// embeds (`target/landing_page/`).
//
// Runs after both vite passes exited 0. Writes:
//   index.html          SPA, JS+CSS inlined (served to browsers)
//   fallback.html       SSR-prerendered page whose per-request values are
//                       {{MALKUTH_*}} markers (src/info_page.rs substitutes)
//   hikari-tokens.json  the exact CSS custom-property maps the SPA applies,
//                       plus the compiled spinner rules the 503 page reuses
//   build-stamp.json    the commit marker (absent => build.rs refuses to embed)
import { createHash } from 'node:crypto'
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from 'node:fs'
import { dirname, join, relative, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { extractSpinnerCss, rootBlock, stripComments } from './css-rules.mjs'

const app = resolve(dirname(fileURLToPath(import.meta.url)), '..')
const outDir = resolve(app, '../../target/landing_page')
const stageSpa = resolve(outDir, '.stage/spa')
const stageSsr = resolve(outDir, '.stage/ssr/prerender.mjs')

const sha256 = (buf) => createHash('sha256').update(buf).digest('hex')

/** Escape a JS/CSS payload being inlined into an HTML element. */
const escapeForScript = (js) => js.replace(/<\/script/gi, '<\\/script')
const escapeForStyle = (css) => css.replace(/<\/style/gi, '<\\/style')

function readStagedSpa() {
  const htmlPath = join(stageSpa, 'index.html')
  if (!existsSync(htmlPath)) throw new Error(`publish: missing ${htmlPath}`)
  const assetsDir = join(stageSpa, 'assets')
  const files = readdirSync(assetsDir)
  const jsName = files.find((f) => f.endsWith('.js'))
  const cssName = files.find((f) => f.endsWith('.css'))
  if (!jsName) throw new Error('publish: no JS asset staged')
  if (!cssName) throw new Error('publish: no CSS asset staged')
  return {
    html: readFileSync(htmlPath, 'utf8'),
    js: readFileSync(join(assetsDir, jsName), 'utf8'),
    css: readFileSync(join(assetsDir, cssName), 'utf8'),
    jsName,
    cssName,
  }
}

function inlineSpa({ html, js, css }) {
  let out = html
  out = out.replace(
    /<script\b[^>]*\bsrc="[^"]+"[^>]*>\s*<\/script>/gi,
    () => `<script type="module">${escapeForScript(js)}</script>`,
  )
  out = out.replace(
    /<link\b[^>]*\bhref="[^"]+\.css"[^>]*\/?>/gi,
    () => `<style>${escapeForStyle(css)}</style>`,
  )
  out = out.replace(/<link\b[^>]*\brel="modulepreload"[^>]*\/?>/gi, '')
  return out
}

/**
 * The no-JS fallback shell. `lang`/`dir`/`title` are markers too: the same
 * file is served for all 11 locales, and src/info_page.rs substitutes the
 * per-request strings (it already owns the catalogue in i18n/all.json).
 */
function fallbackShell(body, css, tokensCss) {
  return `<!DOCTYPE html>
<html lang="{{MALKUTH_LANG}}" dir="{{MALKUTH_DIR}}">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1.0" />
<meta name="robots" content="noindex" />
<title>{{MALKUTH_TITLE}}</title>
{{MALKUTH_AUTO_REFRESH}}
<style>${escapeForStyle(css)}${escapeForStyle(tokensCss)}</style>
</head>
<body>
<div id="app">${body}</div>
</body>
</html>
`
}

/** sha256 over every build input, so the stamp can be audited later. */
function inputsFingerprint() {
  const files = [
    'index.html',
    'package.json',
    'pnpm-lock.yaml',
    'tsconfig.json',
    'vite.config.ts',
    'vite.ssr.config.ts',
  ]
  const entries = []
  const walk = (dir) => {
    for (const name of readdirSync(dir).sort()) {
      const path = join(dir, name)
      if (statSync(path).isDirectory()) walk(path)
      else entries.push([relative(app, path), sha256(readFileSync(path))])
    }
  }
  walk(resolve(app, 'src'))
  walk(resolve(app, 'scripts'))
  for (const file of files) {
    if (existsSync(resolve(app, file))) {
      entries.push([file, sha256(readFileSync(resolve(app, file)))])
    }
  }
  entries.sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
  return {
    hash: sha256(entries.map(([path, hash]) => `${path}:${hash}`).join('\n')),
    files: Object.fromEntries(entries),
  }
}

const pkgVersion = (name) =>
  JSON.parse(readFileSync(resolve(app, `node_modules/${name}/package.json`), 'utf8')).version

// ------------------------------------------------------------------- main

const staged = readStagedSpa()

// 1. SPA single file.
const indexHtml = inlineSpa(staged)

// 2. SSR-prerendered fallback markup (markers intact) + the theme snapshot.
if (!existsSync(stageSsr)) throw new Error(`publish: missing ${stageSsr} (SSR pass not run)`)
const { renderPrerenderBody, designTokens } = await import(`${stageSsr}?v=${Date.now()}`)
const body = await renderPrerenderBody()
const tokens = designTokens()

const tokensCss = [
  rootBlock(tokens.dark),
  `@media (prefers-color-scheme: light){${rootBlock(tokens.light)}}`,
].join('')

// Vite leaves a bookkeeping comment (`/*$vite$:1*/`) at the top of the emitted
// stylesheet; the served page does not need it.
const fallbackHtml = fallbackShell(body, stripComments(staged.css), tokensCss)
const spinnerCss = extractSpinnerCss(staged.css)

const tokenSnapshot = {
  schema: 1,
  package: '@celestia-island/hikari',
  hikari_version: pkgVersion('@celestia-island/hikari'),
  dark: tokens.dark,
  light: tokens.light,
  tokens_css: tokensCss,
  spinner_css: spinnerCss,
}

mkdirSync(outDir, { recursive: true })
const stampPath = join(outDir, 'build-stamp.json')
rmSync(stampPath, { force: true }) // invalidate BEFORE touching artifacts

const tmp = (path) => `${path}.tmp-${process.pid}`
function put(name, contents) {
  const target = join(outDir, name)
  writeFileSync(tmp(target), contents)
  renameSync(tmp(target), target)
  return { bytes: Buffer.byteLength(contents), sha256: sha256(contents) }
}

const artifacts = {
  'index.html': put('index.html', indexHtml),
  'fallback.html': put('fallback.html', fallbackHtml),
  'hikari-tokens.json': put('hikari-tokens.json', JSON.stringify(tokenSnapshot, null, 2) + '\n'),
}

// 3. stamp LAST — the commit marker build.rs requires.
const stamp = {
  ok: true,
  schema: 1,
  builtAt: new Date().toISOString(),
  pipeline: 'scripts/build.mjs -> scripts/publish.mjs',
  hikari: tokenSnapshot.hikari_version,
  vue: pkgVersion('vue'),
  vite: pkgVersion('vite'),
  inputs: inputsFingerprint(),
  artifacts,
}
writeFileSync(tmp(stampPath), JSON.stringify(stamp, null, 2) + '\n')
renameSync(tmp(stampPath), stampPath)

console.log(
  [
    `published -> ${relative(app, outDir)}`,
    `  index.html          ${String(artifacts['index.html'].bytes).padStart(9)} B  (js ${Buffer.byteLength(staged.js)} + css ${Buffer.byteLength(staged.css)} inlined)`,
    `  fallback.html       ${String(artifacts['fallback.html'].bytes).padStart(9)} B  (ssr body ${Buffer.byteLength(body)} B)`,
    `  hikari-tokens.json  ${String(artifacts['hikari-tokens.json'].bytes).padStart(9)} B  (spinner css ${Buffer.byteLength(spinnerCss)} B)`,
    `  build-stamp.json    ${String(Buffer.byteLength(JSON.stringify(stamp))).padStart(9)} B  inputs ${stamp.inputs.hash.slice(0, 12)}`,
  ].join('\n'),
)
