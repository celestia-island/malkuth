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
import { applyViewportPolicy, setLocale } from '@celestia-island/hikari'

import { followSystemTheme } from './theme/malkuth'
import LandingPage, { type MalkuthInit } from './views/LandingPage'

applyViewportPolicy({ allowZoomOut: true })
followSystemTheme()

const init = (window as unknown as { __MALKUTH_INIT__?: MalkuthInit }).__MALKUTH_INIT__
if (init?.lang) {
  // Unknown locales stay on the English catalogue — safe to ignore.
  void setLocale(init.lang)
}

createApp(LandingPage).mount('#app')
