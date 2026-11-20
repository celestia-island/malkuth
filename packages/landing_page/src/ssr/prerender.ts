// SSR entry — bundled by `vite build --config vite.ssr.config.ts` into
// target/landing_page/.stage/ssr/prerender.mjs and imported by
// scripts/publish.mjs at build time.
//
// It exports two things:
//   * renderPrerenderBody() — the static page markup, with every per-request
//     value left as a `{{MALKUTH_*}}` marker for src/info_page.rs;
//   * designTokens()       — the exact CSS custom-property maps the SPA
//     applies at runtime, snapshotted for the Rust-embedded 503 page.
//
// No DOM exists here. The tree renders in `static` mode precisely so the
// build never needs one: overlays/toasts are not rendered, interactions are
// plain text, and the layout still comes from the same hikari components.
import { createSSRApp, h } from 'vue'
import { renderToString } from 'vue/server-renderer'

import { designTokens } from '../theme/malkuth'
import LandingPage from '../views/LandingPage'

export async function renderPrerenderBody(): Promise<string> {
  const app = createSSRApp({ render: () => h(LandingPage, { static: true }) })
  return await renderToString(app)
}

export { designTokens }