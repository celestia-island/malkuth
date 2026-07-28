import '@celestia-island/plana-ui/tokens.scss'
import './styles/main.scss'
import './i18n'
import { createApp } from 'vue'
import { createRouter, createWebHistory } from 'vue-router'
import { initTheme } from '@celestia-island/hikari'
import App from './App.vue'
import LandingPage from './views/LandingPage.vue'

localStorage.setItem('hikari-theme', 'tokyonight')
initTheme()

const routes = [
  { path: '/', component: LandingPage },
]

const router = createRouter({
  history: createWebHistory(),
  routes,
})

createApp(App).use(router).mount('#app')
