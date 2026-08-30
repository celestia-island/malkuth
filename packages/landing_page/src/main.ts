import { createApp } from 'vue'
import LandingPage from './views/LandingPage'
import { applyViewportPolicy } from './mobileViewport'

// Mobile UX contract (hikari #325 sibling): normalize the viewport meta
// before first paint so phones never refuse pinch zoom. The tap-highlight
// reset ships via styles/main.scss.
applyViewportPolicy({ allowZoomOut: true })

createApp(LandingPage).mount('#app')
