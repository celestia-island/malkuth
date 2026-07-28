<template>
  <div class="card" ref="cardRef">
    <img class="logo" :src="'data:image/webp;base64,' + logoBase64" alt="Malkuth" />
    <h1>{{ t('heading', 'Malkuth') }}</h1>
    <p class="tagline">{{ t('tagline', 'This port is managed by the Malkuth process supervisor') }}</p>

    <div class="status" :class="statusClass">
      {{ statusMessage || t('status_landing', 'Redirecting shortly') }}
    </div>

    <template v-if="!showLandingOnly">
      <div class="info-row" v-if="proxyEndpoint">
        <span class="info-label">{{ t('proxy_label', 'Proxy') }}</span>
        <span class="info-value">{{ proxyEndpoint }}</span>
      </div>
      <div class="info-row" v-if="watchPaths.length">
        <span class="info-label">{{ t('watch_label', 'Watching') }}</span>
        <div class="watch-list">
          <span class="watch-item" v-for="p in watchPaths" :key="p"
                :data-path="p" @click="copy(p)"
                @mouseenter="showWatchTooltip($event, p)" @mouseleave="hidePortal">
            <span class="watch-text">{{ p }}</span>
            <span class="tooltip"><span class="tooltip-path">{{ p }}</span><span class="tooltip-hint">{{ t('click_to_copy', 'Click to copy') }}</span></span>
          </span>
        </div>
      </div>
    </template>

    <div class="binaries" v-if="binaries.length">
      <div class="binaries-title">{{ t('binaries_title', 'Supervised Binaries') }}</div>
      <div class="binary-row" v-for="b in binaries" :key="b.name">
        <span class="binary-name" :data-copy="b.name" @click="copy(b.name)"
              @mouseenter="(e: MouseEvent) => showVtty(e, b.name)" @mouseleave="hideVtty">
          <span>{{ b.name }}</span>
          <span class="vtty-icon">
            <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
          </span>
          <span class="binary-name-full">{{ b.name }}<br><span class="tooltip-hint">{{ t('click_to_copy', 'Click to copy') }}</span></span>
        </span>
        <span class="binary-detail">
          <span class="binary-time" :data-copy="b.compile_time"
                @click="copy(b.compile_time)"
                @mouseenter="(e: MouseEvent) => showPortalTooltip(e, b.compile_time)" @mouseleave="scheduleHidePortal">
            {{ b.compile_time }}
            <span class="binary-time-full">{{ b.compile_time }}<br><span class="tooltip-hint">{{ t('click_to_copy', 'Click to copy') }}</span></span>
          </span>
          ·
          <span class="binary-hash" :data-copy="b.hash"
                @click="copy(b.hash)"
                @mouseenter="(e: MouseEvent) => showPortalTooltip(e, b.hash)" @mouseleave="scheduleHidePortal">
            <span class="binary-hash-short">{{ b.hash_short }}</span>
            <span class="binary-hash-full">{{ b.hash }}<br><span class="tooltip-hint">{{ t('click_to_copy', 'Click to copy') }}</span></span>
          </span>
        </span>
      </div>
    </div>

    <p class="retry-hint" id="retryHint" v-if="state === 'landing' || state === 'starting'">
      {{ t('redirect_before', 'Redirecting in') }}
      <span class="countdown" id="countdown" style="--countdown-color: #ffa500">{{ countdown }}</span>
      <span class="countdown-unit" id="countdownUnit">{{ t('redirect_after', 'seconds') }}</span>
    </p>
    <div class="cancel-row">
      <button class="cancel-btn" id="cancelBtn" v-if="showRefresh" @click="doRefresh"
              :class="{ 'cancel-btn--refresh': state === 'ready' || state === 'offline' }">
        {{ t('refresh_label', 'Refresh Now') }}
      </button>
    </div>

    <p class="footer">
      Powered by <a href="https://github.com/celestia-island/malkuth" target="_blank" rel="noopener">Malkuth</a>
    </p>
    <p class="version-line">v{{ version }}</p>

    <div class="vtty-tooltip" v-if="vttyVisible" :style="vttyStyle">
      <div class="vtty-title">{{ vttyName }}</div>
      <div class="vtty-screen">
        <div v-if="!vttyLog.length" class="vtty-loading">Loading...</div>
        <pre v-else>{{ vttyLog.join('\n') }}</pre>
      </div>
    </div>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'
import { useI18n } from '@celestia-island/hikari'

const { t } = useI18n()

const state = ref<'landing' | 'building' | 'ready' | 'offline' | 'starting'>('landing')
const countdown = ref(3)
const statusMessage = ref('')
const showRefresh = ref(false)
const showLandingOnly = ref(false)
const version = ref('0.2.4')
const logoBase64 = ref('')
const proxyEndpoint = ref('')
const watchPaths = ref<string[]>([])
const binaries = ref<any[]>([])

const vttyVisible = ref(false)
const vttyName = ref('')
const vttyLog = ref<string[]>([])
const vttyStyle = ref<Record<string, string>>({})

const cardRef = ref<HTMLElement>()

let countdownTimer: any = null
let pollTimer: any = null
let vttyPollTimer: any = null
let portalEl: HTMLElement | null = null
let portalHideTimer: any = null

const statusClass = computed(() => {
  if (state.value === 'ready') return 'status--ready'
  if (state.value === 'offline') return 'status--working'
  if (state.value === 'building') return 'status--working'
  return 'status--landing'
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
      document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
      location.reload()
    }
  }, 1000)
}

// Init data from server-injected script
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

  if (s === 'ready') {
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
  // Accordion animation cleanup
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
  clearInterval(vttyPollTimer)
})

// Portal tooltip helpers (same as original template)
function getPortal() {
  if (!portalEl) { portalEl = document.createElement('div'); document.body.appendChild(portalEl) }
  return portalEl
}
function scheduleHidePortal() {
  portalHideTimer = setTimeout(hidePortal, 150)
}
function showPortalTooltip(ev: MouseEvent, text: string) {
  hidePortal()
  clearTimeout(portalHideTimer)
  const p = getPortal()
  const tip = document.createElement('div')
  tip.className = 'portal-tooltip'
  tip.innerHTML = text + '<br><span class="tooltip-hint" style="cursor:pointer" onclick="navigator.clipboard?.writeText(\'' + text.replace(/'/g, '\\\'') + '\')">' + t('click_to_copy', 'Click to copy') + '</span>'
  p.appendChild(tip)
  const rect = (ev.target as HTMLElement).getBoundingClientRect()
  const tw = tip.offsetWidth
  let left = rect.left + rect.width / 2 - tw / 2
  if (left < 8) left = 8
  if (left + tw > window.innerWidth - 8) left = window.innerWidth - tw - 8
  tip.style.left = Math.max(8, left) + 'px'
  let top = rect.top - tip.offsetHeight - 8
  if (top < 8) {
    top = rect.bottom + 8
  }
  tip.style.top = top + 'px'
  tip.addEventListener('mouseenter', () => clearTimeout(portalHideTimer))
  tip.addEventListener('mouseleave', () => { portalHideTimer = setTimeout(hidePortal, 150) })
}
function hidePortal() {
  if (portalEl) { portalEl.innerHTML = '' }
}
function showWatchTooltip(ev: MouseEvent, path: string) {
  const el = ev.currentTarget as HTMLElement
  const tt = el.querySelector('.tooltip') as HTMLElement
  if (tt) { tt.style.opacity = '1'; tt.style.visibility = 'visible'; tt.style.transform = 'translateX(-50%) translateY(-4px)' }
}
function copy(text: string) {
  navigator.clipboard?.writeText(text).catch(() => {})
  toast(text)
}
function toast(msg: string) {
  let el = document.getElementById('globalToast')
  if (!el) { el = document.createElement('div'); el.id = 'globalToast'; el.className = 'toast'; document.body.appendChild(el) }
  el.textContent = t('copied_msg', 'Copied to clipboard')
  el.classList.add('show')
  clearTimeout((el as any)._timer);
  (el as any)._timer = setTimeout(() => el!.classList.remove('show'), 2000)
}

function showVtty(ev: MouseEvent, name: string) {
  vttyName.value = name
  vttyVisible.value = true
  const mw = 740
  let left = ev.clientX
  if (left + mw > window.innerWidth - 16) left = window.innerWidth - mw - 16
  if (left < 8) left = 8
  vttyStyle.value = { left: left + 'px', top: Math.min(ev.clientY + 16, window.innerHeight - 420) + 'px' }
  probe()
  clearInterval(vttyPollTimer)
  vttyPollTimer = setInterval(probe, 2000)
}
function hideVtty() { vttyVisible.value = false; clearInterval(vttyPollTimer) }
function doRefresh() {
  document.cookie = '__malkuth_nonce=1; max-age=1800; path=/'
  location.reload()
}
</script>
