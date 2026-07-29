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
          <span v-for="p in watchPaths" :key="p"
            class="watch-item"
            :data-tooltip="p + '\n' + t('click_to_copy', 'Click to copy')"
            @click="copy(p)"
          >
            <span class="watch-text">{{ p }}</span>
          </span>
        </div>
      </div>
    </template>

    <div class="binaries" v-if="binaries.length">
      <div class="binaries-title">{{ t('binaries_title', 'Supervised Binaries') }}</div>
      <div class="binary-row" v-for="b in binaries" :key="b.name">
        <div class="binary-name-cell">
          <span class="binary-name"
            :data-tooltip="b.name + '\n' + t('click_to_copy', 'Click to copy')"
            @click="copy(b.name)"
          >{{ b.name }}</span>
          <span class="vtty-icon"
            @click.stop="showBinaryVtty($event, b.name)"
            @mouseenter="hoverVttyIcon($event, b.name)"
            @mouseleave="hoverVttyLeave"
          >
            <svg xmlns="http://www.w3.org/2000/svg" width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="2" y="3" width="20" height="14" rx="2" ry="2"/><line x1="8" y1="21" x2="16" y2="21"/><line x1="12" y1="17" x2="12" y2="21"/></svg>
          </span>
        </div>
        <span class="binary-detail">
          <span class="binary-time"
            :data-tooltip="b.compile_time + '\n' + t('click_to_copy', 'Click to copy')"
            @click="copy(b.compile_time)"
          >{{ b.compile_time }}</span>
          ·
          <span class="binary-hash"
            :data-tooltip="b.hash + '\n' + t('click_to_copy', 'Click to copy')"
            @click="copy(b.hash)"
          >
            <span class="binary-hash-short">{{ b.hash_short }}</span>
          </span>
        </span>
      </div>
    </div>

    <p class="retry-hint" v-if="state === 'landing' || state === 'starting'">
      {{ t('redirect_before', 'Redirecting in') }}
      <span class="countdown">{{ countdown }}</span>
      <span class="countdown-unit">{{ t('redirect_after', 'seconds') }}</span>
    </p>
    <div class="cancel-row">
      <button
        v-if="state === 'ready' || state === 'landing' || state === 'starting'"
        class="btn btn-ghost btn-sm"
        @click="cancelRedirect"
      >
        {{ t('cancel_label', 'Cancel') }}
      </button>
      <button
        v-if="showRefresh"
        class="btn btn-sm btn-primary"
        @click="doRefresh"
      >
        {{ t('refresh_label', 'Refresh Now') }}
      </button>
    </div>

    <p class="footer">
      Powered by <a href="https://github.com/celestia-island/malkuth" target="_blank" rel="noopener">Malkuth</a>
    </p>
    <p class="version-line">v{{ version }}</p>

    <Teleport to="body">
      <div class="vtty-backdrop" v-if="vttyVisible" @click="vttyVisible = false" />
      <div class="vtty-panel" v-if="vttyVisible" @click.stop>
        <div class="vtty-header">
          <span class="vtty-name">{{ vttyName }}</span>
          <button class="vtty-close" @click="vttyVisible = false" :aria-label="t('vtty_close', 'Close')">
            <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>
          </button>
        </div>
        <div class="vtty-terminal">
          <div v-if="vttyLog.length === 0" class="vtty-spinner">
            <div class="spinner-ring"></div>
          </div>
          <pre v-else>{{ vttyLog.join('\n') }}</pre>
        </div>
        <div class="vtty-footer">
          <template v-if="vttyLog.length">{{ t('vtty_connected', 'Connected') }}</template>
          <template v-else>{{ t('vtty_no_output', 'No output yet') }}</template>
        </div>
      </div>
      <div v-show="hoveredBinary" class="vtty-tooltip" :style="hoverTooltipStyle">
        <div class="vtty-tooltip-header">
          <span class="vtty-tooltip-name">{{ hoveredBinary }}</span>
        </div>
        <div class="vtty-tooltip-terminal">
          <div v-if="hoverLog.length === 0" class="vtty-spinner">
            <div class="spinner-ring"></div>
          </div>
          <pre v-else>{{ hoverLog.join('\n') }}</pre>
        </div>
      </div>
    </Teleport>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted } from 'vue'

const messages: Record<string, Record<string, string>> = {
  en: {
    heading: 'Malkuth',
    tagline: 'This port is managed by the Malkuth process supervisor',
    status_landing: 'Redirecting shortly',
    status_building: 'The service is currently being rebuilt. Please wait a moment',
    status_starting: 'The service is starting up',
    status_ready: 'All services are running normally.',
    status_offline: 'Service is offline',
    proxy_label: 'Proxy',
    watch_label: 'Watching',
    binaries_title: 'Supervised Binaries',
    redirect_before: 'Redirecting in',
    redirect_after: 'seconds',
    cancel_label: 'Cancel',
    refresh_label: 'Refresh Now',
    vtty_loading: 'Loading...',
    vtty_no_output: 'No output yet',
    vtty_connected: 'Connected',
    vtty_close: 'Close',
    click_to_copy: 'Click to copy',
    copied_msg: 'Copied to clipboard',
  },
  zhs: {
    heading: 'Malkuth',
    tagline: '此端口由 Malkuth 进程管理器接管',
    status_landing: '即将跳转',
    status_building: '服务正在重新构建中，请稍候',
    status_starting: '服务正在启动中',
    status_ready: '所有服务运行正常。',
    status_offline: '服务已离线',
    proxy_label: '代理',
    watch_label: '监听',
    binaries_title: '受监管二进制',
    redirect_before: '将在',
    redirect_after: '秒后跳转',
    cancel_label: '取消跳转',
    refresh_label: '立即刷新',
    vtty_loading: '加载中...',
    vtty_no_output: '暂无输出',
    vtty_connected: '已连接',
    vtty_close: '关闭',
    click_to_copy: '点击以复制',
    copied_msg: '已复制到剪贴板',
  },
}

function resolveLocale(): string {
  const full = (navigator.language || 'en').toLowerCase()
  if (full.startsWith('zh-cn') || full.startsWith('zh-sg') || full.startsWith('zh-my')) return 'zhs'
  if (full.startsWith('zh-tw') || full.startsWith('zh-hk') || full.startsWith('zh-mo')) return 'zht'
  if (full.split('-')[0] === 'zh') return 'zhs'
  return 'en'
}

function t(key: string, fallback: string): string {
  const lang = resolveLocale()
  return messages[lang]?.[key] || messages.en?.[key] || fallback
}

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
const hoveredBinary = ref<string | null>(null)
const hoverLog = ref<string[]>([])
const hoverTooltipStyle = ref<Record<string, string>>({})

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

function showBinaryVtty(_ev: MouseEvent, name: string) {
  vttyName.value = name
  vttyLog.value = []
  vttyVisible.value = true
  probe()
}

const hoverCache: Record<string, string[]> = {}
let hoverTimer: any = null
let hoverIconEl: HTMLElement | null = null

function hoverVttyIcon(ev: MouseEvent, name: string) {
  clearTimeout(hoverTimer)
  hoverIconEl = ev.currentTarget as HTMLElement
  hoveredBinary.value = name

  const rect = hoverIconEl.getBoundingClientRect()
  const roomAbove = rect.top > 420
  hoverTooltipStyle.value = {
    position: 'fixed',
    top: roomAbove ? (rect.top - 16) + 'px' : (rect.bottom + 8) + 'px',
    left: (rect.left + rect.width / 2) + 'px',
    transform: roomAbove ? 'translate(-50%, -100%)' : 'translate(-50%, 0)',
    display: 'flex',
  }

  if (hoverCache[name]) {
    hoverLog.value = hoverCache[name]
  } else {
    hoverLog.value = []
    fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
      .then(r => r.json())
      .then(d => {
        const logs = d.vttys?.[0]?.log || []
        hoverCache[name] = logs
        if (hoveredBinary.value === name) hoverLog.value = logs
      }).catch(() => {})
  }
}

function hoverVttyLeave() {
  hoverTimer = setTimeout(() => { hoveredBinary.value = null }, 200)
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
