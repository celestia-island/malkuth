#!/usr/bin/env node
// Minimal brace-aware CSS walker shared by publish.mjs (spinner extraction).
// Deliberately tiny: the landing page only ever needs to pull a handful of
// compiled rules out of the stylesheet vite just emitted, so this is not a
// general CSS parser.

/** Strip /* *\/ comments without touching string literals. */
export function stripComments(css) {
  let out = ''
  let i = 0
  let quote = null
  while (i < css.length) {
    const c = css[i]
    if (quote) {
      out += c
      if (c === '\\') out += css[++i] ?? ''
      else if (c === quote) quote = null
      i++
      continue
    }
    if (c === '"' || c === "'") {
      quote = c
      out += c
      i++
      continue
    }
    if (c === '/' && css[i + 1] === '*') {
      const end = css.indexOf('*/', i + 2)
      i = end === -1 ? css.length : end + 2
      out += ' '
      continue
    }
    out += c
    i++
  }
  return out
}

/**
 * Yield every leaf rule as `{ selector, body, atRules }`, where `atRules`
 * carries the enclosing `@media`/`@supports` preludes (outermost first).
 */
export function* walkRules(css, offset = 0, atRules = []) {
  let i = offset
  const len = css.length
  while (i < len) {
    let prelude = ''
    let quote = null
    while (i < len) {
      const c = css[i]
      if (quote) {
        prelude += c
        if (c === '\\') prelude += css[++i] ?? ''
        else if (c === quote) quote = null
        i++
        continue
      }
      if (c === '"' || c === "'") {
        quote = c
        prelude += c
        i++
        continue
      }
      if (c === '{' || c === '}' || c === ';') break
      prelude += c
      i++
    }
    const term = css[i]
    if (term === '}') {
      i++
      return
    }
    if (term === ';') {
      i++
      continue
    }
    if (term !== '{') return

    let depth = 1
    let j = i + 1
    quote = null
    while (j < len && depth > 0) {
      const c = css[j]
      if (quote) {
        if (c === '\\') j++
        else if (c === quote) quote = null
      } else if (c === '"' || c === "'") quote = c
      else if (c === '{') depth++
      else if (c === '}') depth--
      j++
    }
    const body = css.slice(i + 1, j - 1)
    const sel = prelude.trim()
    if (sel.startsWith('@')) {
      yield* walkRules(css, i + 1, [...atRules, sel])
    } else {
      yield { selector: sel, body, atRules }
    }
    i = j
  }
}

/**
 * The compiled rules a static page needs to render hikari's spinner:
 * every `.hk-spinner*` rule plus the rotation keyframes. Values are taken
 * verbatim from the stylesheet the SPA ships, so the 503 page cannot drift
 * from the component.
 */
/** Extract one at-rule block verbatim (`@keyframes …{…}`), braces balanced. */
function atRuleBlock(css, prelude) {
  const at = css.indexOf(prelude)
  if (at === -1) return null
  const open = css.indexOf('{', at)
  if (open === -1) return null
  let depth = 0
  for (let i = open; i < css.length; i++) {
    if (css[i] === '{') depth++
    else if (css[i] === '}') {
      depth--
      if (depth === 0) return css.slice(at, i + 1)
    }
  }
  return null
}

export function extractSpinnerCss(css) {
  const flat = stripComments(css)
  const parts = []
  for (const rule of walkRules(flat)) {
    const text = `${rule.selector.trim()}{${rule.body.trim()}}`
    const wrapped = rule.atRules.reduceRight(
      (inner, at) => `${at.trim()}{${inner}}`,
      text,
    )
    if (rule.selector.includes('.hk-spinner')) parts.push(wrapped)
  }
  // Keyframes are at-rules with a block: the leaf walker descends into them
  // (`to{…}`), so they have to be lifted out verbatim.
  const keyframes = atRuleBlock(flat, '@keyframes hk-spinner-rotate')
  if (!keyframes) {
    throw new Error('no @keyframes hk-spinner-rotate in the compiled CSS — the 503 page would render a frozen arc')
  }
  parts.push(keyframes)
  if (!parts.length) throw new Error('no .hk-spinner rules found in the compiled CSS')
  return parts.join('')
}

/** `:root{...}` for one custom-property map. */
export function rootBlock(decls) {
  const body = Object.entries(decls)
    .map(([name, value]) => `${name}:${value}`)
    .join(';')
  return `:root{${body}}`
}
