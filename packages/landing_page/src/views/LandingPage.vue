<template>
  <div class="landing">
    <HCard class="landing-card">
      <template #header>
        <div class="header-content">
          <HLogo size="lg" :src="logoBase64 || undefined" alt="Malkuth" />
          <h1 class="header-title">{{ t('heading', 'Malkuth') }}</h1>
          <p class="tagline">{{ t('tagline') }}</p>
        </div>
      </template>

      <HBadge :variant="statusVariant" class="landing-status">
        {{ statusMessage }}
      </HBadge>

      <section class="info" v-if="!showLandingOnly">
        <div class="info-row" v-if="proxyEndpoint">
          <span class="info-label">{{ t('proxy_label', 'Proxy') }}</span>
          <span class="info-value">{{ proxyEndpoint }}</span>
        </div>
        <div class="info-row" v-if="watchPaths.length">
          <span class="info-label">{{ t('watch_label', 'Watching') }}</span>
          <div class="watch-list">
            <span class="watch-item" v-for="p in watchPaths" :key="p"
              :data-path="p" @click="copy(p)">
              <span class="watch-text">{{ p }}</span>
            </span>
          </div>
        </div>
      </section>

      <section class="binaries" v-if="binaries.length">
        <div class="binaries-title">{{ t('binaries_title', 'Supervised Binaries') }}</div>
        <div class="binary-row" v-for="b in binaries" :key="b.name">
          <HTooltip :text="'Click to copy: ' + b.name" placement="top">
            <span class="binary-name" @click="copy(b.name)" @mouseenter="showVtty($event, b.name)" @mouseleave="hideVtty">
              {{ b.name }}
              <span class="vtty-icon">
                <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
              </span>
            </span>
          </HTooltip>
          <span class="binary-detail">
            <span class="binary-time" @click="copy(b.compile_time)">{{ b.compile_time }}</span>
            · <span class="binary-hash" @click="copy(b.hash)">{{ b.hash_short }}</span>
          </span>
        </div>
      </section>

      <div class="retry-hint" v-if="state === 'landing' || state === 'starting'">
        {{ t('redirect_before', 'Redirecting in') }}
        <span class="countdown" :style="{ color: '#ffa500' }">{{ countdown }}</span>
        <span class="countdown-unit">{{ t('redirect_after', 'seconds') }}</span>
      </div>

      <HButton v-if="showRefresh" variant="outline" block @click="doRefresh">
        {{ t('refresh_label', 'Refresh Now') }}
      </HButton>

      <template #footer>
        <footer class="footer">
          <a href="https://github.com/celestia-island/malkuth" target="_blank">Malkuth</a>
        </footer>
        <p class="version-line">v{{ version }}</p>
      </template>
    </HCard>

    <div class="vtty-tooltip" v-if="vttyVisible" :style="vttyStyle">
      <div class="vtty-title">{{ vttyName }}</div>
      <div class="vtty-screen">
        <div v-if="!vttyLog.length" class="vtty-loading">
          <HSpinner size="sm" tone="primary" />
        </div>
        <pre v-else>{{ vttyLog.join('\n') }}</pre>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, onMounted, onUnmounted, computed } from 'vue'
import { HButton, HCard, HBadge, HLogo, HTooltip, HSpinner, useI18n, type BadgeVariant } from '@celestia-island/hikari'

const { t } = useI18n()

const state = ref<'landing' | 'building' | 'ready' | 'offline' | 'starting'>('landing')
const countdown = ref(3)
const statusMessage = ref('')
const showRefresh = ref(false)
const version = ref('0.2.4')
const logoBase64 = ref('')
const proxyEndpoint = ref('')
const watchPaths = ref<string[]>([])
const binaries = ref<any[]>([])
const showLandingOnly = ref(false)

const vttyVisible = ref(false)
const vttyName = ref('')
const vttyLog = ref<string[]>([])
const vttyStyle = ref({})

let timer: any = null
let pollTimer: any = null

const statusVariant = computed<BadgeVariant>(() => {
  const map: Record<string, BadgeVariant> = {
    landing: 'warning',
    building: 'primary',
    ready: 'success',
    offline: 'error',
    starting: 'warning',
  }
  return map[state.value] || 'default'
})

function probe() {
  fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
    .then(r => r.json())
    .then(d => {
      statusMessage.value = d.message || ''
      if (d.state === 'ready') {
        document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
        location.reload()
      } else if (d.state === 'offline') {
        state.value = 'offline'
        showRefresh.value = true
        clearInterval(pollTimer)
      } else {
        state.value = d.state
      }
      if (d.vttys?.length) {
        vttyLog.value = d.vttys[0].log || []
      }
    }).catch(() => {})
}

function startCountdown() {
  countdown.value = 3
  timer = setInterval(() => {
    countdown.value--
    if (countdown.value <= 0) {
      clearInterval(timer)
      document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
      location.reload()
    }
  }, 1000)
}

function showVtty(ev: MouseEvent, name: string) {
  vttyName.value = name
  vttyVisible.value = true
  const mw = 840
  let left = ev.clientX
  if (left + mw > window.innerWidth - 16) {
    left = window.innerWidth - mw - 16
  }
  if (left < 8) left = 8
  vttyStyle.value = {
    left: left + 'px',
    top: Math.min(ev.clientY + 16, window.innerHeight - 500) + 'px',
  }
  probe()
}

function hideVtty() { vttyVisible.value = false }

function copy(text: string) {
  navigator.clipboard?.writeText(text)
}

function doRefresh() {
  document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
  location.reload()
}

onMounted(() => {
  const init = (window as any).__MALKUTH_INIT__ || { state: 'landing', countdown: 3 }
  state.value = init.state
  statusMessage.value = init.message || ''

  if (init.state === 'ready') {
    startCountdown()
  } else if (init.state === 'building') {
    document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
    showRefresh.value = true
    pollTimer = setInterval(probe, 2000)
  } else if (init.state === 'offline') {
    showRefresh.value = true
  } else {
    startCountdown()
  }
})

onUnmounted(() => {
  clearInterval(timer)
  clearInterval(pollTimer)
})
</script>
