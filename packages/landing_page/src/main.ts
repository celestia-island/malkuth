// SPA bootstrap.
//
// Order matters:
//   1. hikari's style entry first — it seeds the token sheets, reset and
//      selection policy; page styles then layer on top.
//   2. the mobile viewport policy before first paint (hikari contract).
//   3. the malkuth theme vars (dark/light) — hikari components read colours
//      from CSS custom properties, so this is the whole theme bridge.
//   4. hikari's own component strings follow the page locale.
import '@celestia-island/hikari/styles'
import './styles/main.scss'

import { createApp } from 'vue'
import { applyViewportPolicy, writeHkRuntime } from '@celestia-island/hikari/runtime'

import { followSystemTheme } from './theme/malkuth'
import LandingPage from './views/LandingPage'

applyViewportPolicy({ allowZoomOut: true })
followSystemTheme()

const init = (window as unknown as { __MALKUTH_INIT__?: { lang?: string } }).__MALKUTH_INIT__
if (init?.lang) {
  // `setLocale` is only re-exported through hikari's barrel (which drags the
  // 3D/wasm surface), so steer the i18n context through the runtime facet
  // instead — the same context, without the barrel.
  try {
    writeHkRuntime('i18n', { type: 'setLocale', locale: init.lang })
  } catch {
    // A locale hikari does not ship simply stays on its English catalogue.
  }
}

createApp(LandingPage).mount('#app')