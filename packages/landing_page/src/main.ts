// SPA bootstrap.
//
// Order matters:
//   1. hikari's style entry first — it seeds the token sheets, reset and
//      selection policy; page styles then layer on top.
//   2. the mobile viewport policy before first paint (hikari contract).
//   3. the malkuth theme vars (dark/light) — hikari components read colours
//      from CSS custom properties, so this is the whole theme bridge.
//   4. hikari's own component strings follow the page locale.
//   5. the app is built before its error-reporting hook is installed, so
//      the hook reads the server's error island and paints the family
//      landing for a navigation that was answered with a failure.
import '@celestia-island/hikari/styles'
import './styles/main.scss'

import { createApp } from 'vue'
import { applyViewportPolicy, createErrorReporting, setLocale } from '@celestia-island/hikari'

import { followSystemTheme } from './theme/malkuth'
import LandingPage, { type MalkuthInit } from './views/LandingPage'

applyViewportPolicy({ allowZoomOut: true })
followSystemTheme()

const init = (window as unknown as { __MALKUTH_INIT__?: MalkuthInit }).__MALKUTH_INIT__
if (init?.lang) {
  // Unknown locales stay on the English catalogue — safe to ignore.
  void setLocale(init.lang)
}

const app = createApp(LandingPage)
// Installed BEFORE mount: the plugin settles the server's verdict on this
// document (the `hk-server-error` island the info door writes into a refused
// navigation) and covers everything the page itself fails to catch.
app.use(createErrorReporting())
app.mount('#app')
