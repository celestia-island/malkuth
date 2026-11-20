// Malkuth's brand theme, expressed as a hikari ThemeSchemeTokens pair.
//
// The palette is NOT invented here: every slot starts from hikari's shipped
// default preset and only the brand-bearing slots (primary / focusedBorder /
// accent) plus the page floors are overridden with malkuth's own orange
// identity (the colour the landing page has carried since the first version).
// Deriving the rest from `stockDefaultPreset` keeps the semantics (success /
// error / warning / info, on-solid inks, selection slots) on hikari's own
// contract instead of a second hand-rolled table.
//
// Three surfaces consume this module:
//   1. the SPA (src/main.ts) — applies the vars at runtime and follows
//      `prefers-color-scheme`;
//   2. the prerendered fallback page — scripts/publish.mjs snapshots the same
//      vars into hikari-tokens.json, which build.rs embeds as Rust consts;
//   3. the 503 restart page — proxy.rs renders that snapshot inline.
// One definition, three consumers: a colour change here cannot leave either
// server-rendered surface behind.
import {
  stockDefaultPreset,
  tokensToCSSVars,
  type ThemeSchemeTokens,
} from '@celestia-island/hikari/theme/presets'

export type ThemeMode = 'dark' | 'light'

const rgb = (r: number, g: number, b: number) => ({ r, g, b })

/**
 * Dark scheme: malkuth's near-black page floor, hikari's surface/semantic
 * slots, malkuth orange as the brand accent.
 */
export const malkuthDark: ThemeSchemeTokens = {
  ...stockDefaultPreset.dark,
  primary: rgb(255, 140, 66),
  focusedBorder: rgb(255, 140, 66),
  accent: rgb(255, 179, 122),
  background: rgb(16, 20, 24),
  surface: rgb(26, 31, 40),
  text: rgb(232, 234, 240),
  muted: rgb(154, 164, 178),
  border: rgb(96, 106, 120),
  selectedBackground: rgb(55, 65, 85),
  selectedText: rgb(240, 244, 250),
  statusBarBackground: rgb(22, 27, 36),
}

/**
 * Light scheme: the historical warm paper floor (#f5f5f0, the same value
 * hikari's own light preset uses), a darker orange measured at 4.03:1
 * on the paper floor (up from master's 1.76:1 countdown accent), and hikari's light semantic slots.
 */
export const malkuthLight: ThemeSchemeTokens = {
  ...stockDefaultPreset.light,
  primary: rgb(194, 90, 25),
  focusedBorder: rgb(194, 90, 25),
  accent: rgb(214, 109, 25),
  background: rgb(245, 245, 240),
  surface: rgb(255, 255, 255),
  text: rgb(51, 51, 64),
  muted: rgb(100, 100, 120),
  border: rgb(170, 170, 180),
  selectedBackground: rgb(224, 224, 232),
  selectedText: rgb(40, 40, 52),
  statusBarBackground: rgb(236, 236, 232),
}

export function tokensFor(mode: ThemeMode): ThemeSchemeTokens {
  return mode === 'dark' ? malkuthDark : malkuthLight
}

/** The complete CSS custom-property map for one scheme (L0 triplets + aliases). */
export function cssVarsFor(mode: ThemeMode): Record<string, string> {
  return tokensToCSSVars(tokensFor(mode))
}

/**
 * Snapshot consumed by the Rust side (build.rs -> design_tokens.rs).
 * Deliberately the SAME cssVarsFor() output the SPA applies at runtime.
 */
export function designTokens(): { dark: Record<string, string>; light: Record<string, string> } {
  return { dark: cssVarsFor('dark'), light: cssVarsFor('light') }
}

/**
 * Apply one scheme to the document root. Hikari's components read their
 * colours from these CSS custom properties, so this is the whole theme
 * bridge: no theme engine, no pinia store, no `initTheme()` (whose 'system'
 * mode means solar time-of-day, not prefers-color-scheme).
 */
export function applyTheme(mode: ThemeMode, root: HTMLElement = document.documentElement): void {
  for (const [name, value] of Object.entries(cssVarsFor(mode))) {
    root.style.setProperty(name, value)
  }
  root.dataset.mode = mode
  root.style.colorScheme = mode
}

/** Follow the OS scheme for as long as the page lives; returns an unsubscribe. */
export function followSystemTheme(): () => void {
  const query = window.matchMedia('(prefers-color-scheme: dark)')
  const apply = () => applyTheme(query.matches ? 'dark' : 'light')
  apply()
  query.addEventListener('change', apply)
  return () => query.removeEventListener('change', apply)
}
