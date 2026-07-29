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
            @mouseenter="showTextTooltip($event, p + '\n' + t('click_to_copy', 'Click to copy'))"
            @mouseleave="hideTooltip"
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
            @mouseenter="showTextTooltip($event, b.name + '\n' + t('click_to_copy', 'Click to copy'))"
            @mouseleave="hideTooltip"
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
            @mouseenter="showTextTooltip($event, b.compile_time + '\n' + t('click_to_copy', 'Click to copy'))"
            @mouseleave="hideTooltip"
            @click="copy(b.compile_time)"
          >{{ b.compile_time }}</span>
          ·
          <span class="binary-hash"
            @mouseenter="showTextTooltip($event, b.hash + '\n' + t('click_to_copy', 'Click to copy'))"
            @mouseleave="hideTooltip"
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
        <div class="vtty-terminal" ref="vttyTerminalRef">
          <div v-if="vttyLog.length === 0" class="vtty-spinner">
            <div class="spinner-ring"></div>
          </div>
          <div class="terminal-scroll-bar" v-if="vttyLog.length">
            <span>│ {{ scrollLine }}/{{ vttyLog.length }} lines │ {{ formatTime(vttyFirstTime) }} → {{ formatTime(vttyLastTime) }} │</span>
            <button class="terminal-copy-btn" @click.stop="copyTerminal">Copy</button>
          </div>
        </div>
        <div class="vtty-footer">
          <template v-if="vttyLog.length">{{ t('vtty_connected', 'Connected') }}</template>
          <template v-else>{{ t('vtty_no_output', 'No output yet') }}</template>
        </div>
      </div>
      <div v-if="tooltip" class="malkuth-tooltip" :class="{ 'is-terminal': tooltip.kind === 'terminal' }" :style="tooltipStyle">
        <template v-if="tooltip.kind === 'text'">
          <span v-if="tooltip.content.includes('\n')">
            {{ tooltip.content.substring(0, tooltip.content.lastIndexOf('\n')) }}<br/>
            <i class="tooltip-copy-hint">{{ tooltip.content.substring(tooltip.content.lastIndexOf('\n') + 1) }}</i>
          </span>
          <span v-else>{{ tooltip.content }}</span>
        </template>
        <template v-else-if="tooltip.kind === 'terminal'">
          <div class="malkuth-tooltip-header">
            <span class="malkuth-tooltip-name">{{ tooltip.binaryName }}</span>
          </div>
          <div class="malkuth-tooltip-terminal">
            <div v-if="(tooltip.log || []).length === 0" class="vtty-spinner">
              <div class="spinner-ring"></div>
            </div>
            <pre v-else v-html="(tooltip.log || []).map(l => parseAnsi(l)).join('\n')"></pre>
          </div>
        </template>
      </div>
    </Teleport>
  </div>
</template>

<script setup lang="ts">
import { ref, computed, onMounted, onUnmounted, watch, nextTick } from 'vue'
import { Terminal } from '@xterm/xterm'
import '@xterm/xterm/css/xterm.css'

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
interface TooltipState {
  kind: 'text' | 'terminal'
  el: HTMLElement
  content: string
  binaryName?: string
  log?: string[]
}
const tooltip = ref<TooltipState | null>(null)

let vttyTerminal: Terminal | null = null

const vttyTerminalRef = ref<HTMLElement>()
const vttyFirstTime = ref(0)
const vttyLastTime = ref(0)
const scrollLine = ref(1)

const cardRef = ref<HTMLElement>()

let countdownTimer: any = null
let pollTimer: any = null

function getXtermOptions() {
  return {
    theme: {
      background: '#0c0c18',
      foreground: '#a0a0c0',
      cursor: '#ff8c42',
      cursorAccent: '#0c0c18',
      selectionBackground: '#ff8c4240',
      black: '#0c0c18',
      red: '#cd0000',
      green: '#00cd00',
      yellow: '#cdcd00',
      blue: '#0000ee',
      magenta: '#cd00cd',
      cyan: '#00cdcd',
      white: '#e5e5e5',
      brightBlack: '#666666',
      brightRed: '#ff0000',
      brightGreen: '#00ff00',
      brightYellow: '#ffff00',
      brightBlue: '#5c5cff',
      brightMagenta: '#ff00ff',
      brightCyan: '#00ffff',
      brightWhite: '#ffffff',
    },
    cols: 80,
    rows: 24,
    fontSize: 13,
    fontFamily: '"Cascadia Code", "Fira Code", "JetBrains Mono", "SF Mono", "Consolas", "Courier New", monospace',
    allowProposedApi: false,
    allowTransparency: false,
    disableStdin: true,
    cursorBlink: false,
    scrollback: 10000,
  }
}

function setupXterm(el: HTMLElement) {
  destroyXterm()
  vttyTerminal = new Terminal(getXtermOptions())
  vttyTerminal.open(el)
}

function destroyXterm() {
  if (vttyTerminal) {
    vttyTerminal.dispose()
    vttyTerminal = null
  }
}

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

const tooltipStyle = computed(() => {
  if (!tooltip.value) return {}
  const rect = tooltip.value.el.getBoundingClientRect()
  return {
    left: (rect.left + rect.width / 2) + 'px',
    top: (rect.top - 12) + 'px',
    transform: 'translate(-50%, -100%)',
  }
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
        const newLog = d.vttys[0].log || []
        if (!vttyFirstTime.value && newLog.length > 0) {
          vttyFirstTime.value = Date.now()
        }
        if (newLog.length > 0) {
          vttyLastTime.value = Date.now()
        }
        vttyLog.value = newLog
        nextTick(updateScroll)
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

  const nonce = parseInt(getCookie('__malkuth_nonce') || '0', 10)
  if (nonce >= 1) {
    showRefresh.value = true
    return
  }

  if (s === 'ready' || s === 'landing') {
    startCountdown()
  } else if (s === 'building') {
    showRefresh.value = true
    pollTimer = setInterval(probe, 2000)
  } else {
    showRefresh.value = true
  }
}

function getCookie(name: string): string | null {
  const match = document.cookie.match(new RegExp('(^|; )' + name + '=([^;]*)'))
  return match ? decodeURIComponent(match[2]) : null
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

watch(vttyLog, () => {
  nextTick(() => {
    if (vttyTerminal) {
      vttyTerminal.reset()
      vttyTerminal.write(vttyLog.value.join('\r\n') + '\r\n')
    }
    updateScroll()
  })
})

watch(vttyVisible, (val) => {
  if (!val) destroyXterm()
})

onUnmounted(() => {
  clearInterval(countdownTimer)
  clearInterval(pollTimer)
  destroyXterm()
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
  vttyFirstTime.value = 0
  vttyLastTime.value = 0
  scrollLine.value = 1
  vttyVisible.value = true
  cancelRedirect()
  nextTick(() => {
    const el = vttyTerminalRef.value
    if (el) setupXterm(el)
    probe()
  })
}

function showTextTooltip(ev: MouseEvent, content: string) {
  tooltip.value = {
    kind: 'text',
    el: ev.currentTarget as HTMLElement,
    content,
  }
}

let hideTimer: any = null

function hideTooltip() {
  if (tooltip.value?.kind === 'text') {
    tooltip.value = null
    return
  }
  clearTimeout(hideTimer)
  hideTimer = setTimeout(() => {
    tooltip.value = null
  }, 200)
}

const hoverCache: Record<string, string[]> = {}

function hoverVttyIcon(ev: MouseEvent, name: string) {
  clearTimeout(hideTimer)
  const el = ev.currentTarget as HTMLElement
  tooltip.value = {
    kind: 'terminal',
    el,
    content: '',
    binaryName: name,
    log: hoverCache[name] || [],
  }

  if (!hoverCache[name]) {
    fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
      .then(r => r.json())
      .then(d => {
        const logs = d.vttys?.[0]?.log || []
        hoverCache[name] = logs
        if (tooltip.value?.kind === 'terminal' && tooltip.value.binaryName === name) {
          tooltip.value = { ...tooltip.value, log: logs }
        }
      }).catch(() => {})
  }
}

function hoverVttyLeave() {
  clearTimeout(hideTimer)
  hideTimer = setTimeout(() => {
    tooltip.value = null
  }, 200)
}

function cancelRedirect() {
  clearInterval(countdownTimer)
  clearInterval(pollTimer)
  showRefresh.value = true
  state.value = 'building'
}

function doRefresh() {
  document.cookie = '__malkuth_nonce=; max-age=0; path=/'
  location.reload()
}

function parseAnsi(text: string): string {
  const sgrMap: Record<number, string> = {
    0: '',
    1: 'font-weight:bold',
    31: 'color:#cd0000', 32: 'color:#00cd00', 33: 'color:#cdcd00',
    34: 'color:#0000ee', 35: 'color:#cd00cd', 36: 'color:#00cdcd',
    37: 'color:#e5e5e5',
    90: 'color:#666666', 91: 'color:#ff0000', 92: 'color:#00ff00',
    93: 'color:#ffff00', 94: 'color:#5c5cff', 95: 'color:#ff00ff',
    96: 'color:#00ffff', 97: 'color:#ffffff',
  }
  let out = ''
  let i = 0
  let spanOpen = false
  while (i < text.length) {
    if (text[i] === '\x1b' && i + 1 < text.length && text[i + 1] === '[') {
      let j = i + 2
      while (j < text.length && text[j] !== 'm') j++
      if (j === text.length) { i++; continue }
      const codeStr = text.substring(i + 2, j)
      const params = codeStr.split(';').map(Number)
      i = j + 1
      if (spanOpen) { out += '</span>'; spanOpen = false }
      if (params.includes(0)) continue
      let style = ''
      for (const p of params) {
        if (sgrMap[p]) style += sgrMap[p] + ';'
      }
      if (style) {
        out += '<span style="' + style.slice(0, -1) + '">'
        spanOpen = true
      }
      continue
    }
    if (text.startsWith('\x1b[K', i)) { i += 3; continue }
    if (text[i] === '<') { out += '&lt;'; i++; continue }
    if (text[i] === '>') { out += '&gt;'; i++; continue }
    if (text[i] === '&') { out += '&amp;'; i++; continue }
    out += text[i]
    i++
  }
  if (spanOpen) out += '</span>'
  return out
}

function updateScroll() {
  if (vttyTerminal) {
    const buffer = vttyTerminal.buffer.active
    const total = vttyLog.value.length || buffer.length
    const viewportY = buffer.viewportY
    const baseY = buffer.baseY
    const offset = Math.min(baseY + Math.max(0, viewportY), total - 1)
    scrollLine.value = Math.max(1, offset + 1)
  } else {
    scrollLine.value = 1
  }
}

function formatTime(ts: number): string {
  if (!ts) return '--:--:--'
  return new Date(ts).toTimeString().slice(0, 8)
}

function copyTerminal() {
  const text = vttyLog.value.join('\n')
  navigator.clipboard?.writeText(text).then(() => {
    toast(t('copied_msg', 'Copied to clipboard'))
  }).catch(() => {})
}
</script>
