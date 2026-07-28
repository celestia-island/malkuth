<template>
  <div class="card" ref="cardRef">
    <img class="logo" :src="'data:image/webp;base64,' + logoBase64" alt="Malkuth" />
    <h1>{{ t('heading', 'Malkuth') }}</h1>
    <p class="tagline">{{ t('tagline', 'This port is managed by the Malkuth process supervisor') }}</p>

    <div class="status" :class="statusClass">
      {{ statusText }}
    </div>

    <template v-if="!showLandingOnly">
      <div class="info-row" v-if="proxyEndpoint">
        <span class="info-label">{{ t('proxy_label', 'Proxy') }}</span>
        <span class="info-value">{{ proxyEndpoint }}</span>
      </div>
      <div class="info-row" v-if="watchPaths.length">
        <span class="info-label">{{ t('watch_label', 'Watching') }}</span>
        <div class="watch-list">
          <HTooltip v-for="p in watchPaths" :key="p"
            :text="`${p}\n${t('click_to_copy', 'Click to copy')}`"
            placement="top" :delay="200" :max-width="'420px'"
          >
            <span class="watch-item" @click="copy(p)">
              <span class="watch-text">{{ p }}</span>
            </span>
          </HTooltip>
        </div>
      </div>
    </template>

    <div class="binaries" v-if="binaries.length">
      <div class="binaries-title">{{ t('binaries_title', 'Supervised Binaries') }}</div>
      <div class="binary-row" v-for="b in binaries" :key="b.name">
        <HTooltip :text="`${b.name}\n${t('click_to_copy', 'Click to copy')}`"
          placement="top" :delay="200" :max-width="'420px'"
        >
          <span class="binary-name" @click="copy(b.name)">
            <span>{{ b.name }}</span>
            <span class="vtty-icon" @click.stop="showBinaryVtty($event, b.name)">
              <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
            </span>
          </span>
        </HTooltip>
        <span class="binary-detail">
          <HTooltip :text="`${b.compile_time}\n${t('click_to_copy', 'Click to copy')}`"
            placement="top" :delay="200" :max-width="'420px'"
          >
            <span class="binary-time" @click="copy(b.compile_time)">{{ b.compile_time }}</span>
          </HTooltip>
          ·
          <HTooltip :text="`${b.hash}\n${t('click_to_copy', 'Click to copy')}`"
            placement="top" :delay="200" :max-width="'420px'"
          >
            <span class="binary-hash" @click="copy(b.hash)">
              <span class="binary-hash-short">{{ b.hash_short }}</span>
            </span>
          </HTooltip>
        </span>
      </div>
    </div>

    <p class="retry-hint" v-if="state === 'landing' || state === 'starting'">
      {{ t('redirect_before', 'Redirecting in') }}
      <span class="countdown">{{ countdown }}</span>
      <span class="countdown-unit">{{ t('redirect_after', 'seconds') }}</span>
    </p>
    <div class="cancel-row">
      <HButton
        v-if="state === 'ready' || state === 'landing' || state === 'starting'"
        variant="ghost"
        size="sm"
        @click="cancelRedirect"
      >
        {{ t('cancel_label', 'Cancel') }}
      </HButton>
      <HButton
        v-if="showRefresh"
        :variant="state === 'ready' || state === 'offline' ? 'outline' : 'ghost'"
        size="sm"
        @click="doRefresh"
      >
        {{ t('refresh_label', 'Refresh Now') }}
      </HButton>
    </div>

    <p class="footer">
      Powered by <a href="https://github.com/celestia-island/malkuth" target="_blank" rel="noopener">Malkuth</a>
    </p>
    <p class="version-line">v{{ version }}</p>

    <Teleport to="body">
      <div class="vtty-backdrop" v-if="vttyVisible" @click="vttyVisible = false" />
      <div class="vtty-panel" v-if="vttyVisible" @click.stop>
        <div class="vtty-title">{{ vttyName }}</div>
        <div class="vtty-screen">
          <div v-if="!vttyLog.length" class="vtty-loading">{{ t('vtty_loading', 'Loading...') }}</div>
          <pre v-else>{{ vttyLog.join('\n') }}</pre>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { useI18n, HTooltip, HButton } from '@celestia-island/hikari'

const { t } = useI18n()

const state = ref<'landing' | 'building' | 'ready' | 'offline' | 'starting'>('landing')
const countdown = ref(3)
const redirectAttempts = ref(0)
const statusMessage = ref('')
const showRefresh = ref(false)
const showLandingOnly = ref(false)
const version = ref('0.2.4')
const logoBase64 = ref('')
const proxyEndpoint = ref('')
const watchPaths = ref<string[]>([])
const binaries = ref<any[]>([])
const vttyName = ref('')
const vttyLog = ref<string[]>([])
const vttyVisible = ref(false)

const cardRef = ref<HTMLElement>()

let countdownTimer: any = null
let pollTimer: any = null

const statusClass = computed(() => {
  if (state.value === 'ready') return 'status--ready'
  if (state.value === 'offline') return 'status--working'
  if (state.value === 'building') return 'status--working'
  return 'status--landing'
})
const statusText = computed(() => {
  if (state.value === 'ready') return t('status_ready', 'All services running.')
  if (state.value === 'building') return t('status_building', 'Building')
  if (state.value === 'offline') return t('status_offline', 'Service is offline')
  return t('status_landing', 'Redirecting shortly')
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
  countdownTimer = setInterval(() => {
    countdown.value--
    if (countdown.value <= 0) {
      clearInterval(countdownTimer)
      const attempts = redirectAttempts.value + 1
      localStorage.setItem('__malkuth_redirect_attempts', String(attempts))
      document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
      location.reload()
    }
  }, 1000)
}

function loadInit() {
  const init = (window as any).__MALKUTH_INIT__
  if (!init) return
  logoBase64.value = init.logo_base64 || ''
  proxyEndpoint.value = init.proxy_endpoint || ''
  watchPaths.value = init.watch_paths || []
  binaries.value = init.binaries || []
  version.value = init.version || '0.2.4'
  const s = init.state || 'landing'
  state.value = s as any
  statusMessage.value = init.message || ''

  const stored = parseInt(localStorage.getItem('__malkuth_redirect_attempts') || '0', 10)
  redirectAttempts.value = stored

  if (stored >= 3) {
    state.value = 'offline'
    showRefresh.value = true
    localStorage.removeItem('__malkuth_redirect_attempts')
    return
  }

  if (s === 'ready') {
    localStorage.removeItem('__malkuth_redirect_attempts')
    startCountdown()
  } else if (s === 'building') {
    document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
    showRefresh.value = true
    pollTimer = setInterval(probe, 2000)
  } else if (s === 'offline') {
    showRefresh.value = true
  } else {
    startCountdown()
  }
}

onMounted(() => {
  loadInit()
  const card = cardRef.value
  if (card) {
    card.addEventListener('animationend', () => {
      card.style.clipPath = 'none'
      card.style.overflow = 'visible'
    }, { once: true })
  }
})

onUnmounted(() => {
  clearInterval(countdownTimer)
  clearInterval(pollTimer)
})

function copy(text: string) {
  navigator.clipboard?.writeText(text).catch(() => {})
  toast(text)
}

function toast(_msg: string) {
  let el = document.getElementById('globalToast')
  if (!el) { el = document.createElement('div'); el.id = 'globalToast'; el.className = 'toast'; document.body.appendChild(el) }
  el.textContent = t('copied_msg', 'Copied to clipboard')
  el.classList.add('show')
  clearTimeout((el as any)._timer);
  (el as any)._timer = setTimeout(() => el!.classList.remove('show'), 2000)
}

function showBinaryVtty(ev: MouseEvent, name: string) {
  vttyName.value = name
  vttyLog.value = []
  vttyVisible.value = true
  probe()
}

function cancelRedirect() {
  clearInterval(countdownTimer)
  clearInterval(pollTimer)
  showRefresh.value = true
  state.value = 'building'
  document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
}

function doRefresh() {
  document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
  location.reload()
}
</script>
