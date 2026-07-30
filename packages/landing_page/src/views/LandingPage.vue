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
            :class="{ 'is-pinned': tooltipPinned && pinnedBinaryName === b.name }"
            @mouseenter="hoverVttyBadge($event, b.name)"
            @mouseleave="hoverVttyLeave"
            @click.stop="togglePin($event, b.name)"
          >{{ b.name }}</span>
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
      <div
        v-if="tooltip"
        class="malkuth-tooltip"
        :class="{ 'is-terminal': tooltip.kind === 'terminal', 'is-pinned': tooltipPinned }"
        :style="tooltipStyle"
        @mouseenter="clearHideTimer"
        @mouseleave="hoverTooltipLeave"
      >
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
            <button class="malkuth-tooltip-header-copy" @click.stop="copyBinaryName" :title="t('copy_name', 'Copy name')">
              <svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
            </button>
          </div>
          <div class="malkuth-tooltip-terminal">
            <div class="malkuth-tooltip-xterm" ref="tooltipTerminalRef"></div>
            <div v-if="(tooltip.log || []).length === 0" class="vtty-spinner">
              <div class="spinner-ring"></div>
            </div>
          </div>
          <div class="malkuth-tooltip-footer" v-if="(tooltip.log || []).length">
            <span class="footer-pin-area" @click.stop="togglePinFromFooter">
              <svg xmlns="http://www.w3.org/2000/svg" width="12" height="12" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><line x1="12" y1="17" x2="12" y2="22"/><path d="M5 17h14v-1.76a2 2 0 0 0-1.11-1.79l-1.78-.9A2 2 0 0 1 15 10.7V5h1a2 2 0 0 0 2-2H6a2 2 0 0 0 2 2h1v5.7a2 2 0 0 1-1.11 1.79l-1.78.9A2 2 0 0 0 5 15.24V17z"/></svg>
              <span>{{ tooltipPinned ? t('vtty_pinned', 'Pinned') : t('vtty_click_to_pin', 'Click to pin') }}</span>
            </span>
            <span class="footer-info">{{ tooltipScrollLine }}/{{ (tooltip.log || []).length }} {{ t('vtty_lines', 'lines') }}  {{ t('vtty_first_output', 'First:') }} {{ formatTime(tooltipFirstTime) }}  {{ t('vtty_last_output', 'Last:') }} {{ formatTime(tooltipLastTime) }}</span>
            <button class="terminal-copy-btn" @click.stop="copyTooltipTerminal">
              <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="9" y="9" width="13" height="13" rx="2" ry="2"/><path d="M5 15H4a2 2 0 0 1-2-2V4a2 2 0 0 1 2-2h9a2 2 0 0 1 2 2v1"/></svg>
            </button>
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
    vtty_click_to_pin: 'Click to pin',
    vtty_pinned: 'Pinned',
    vtty_lines: 'lines',
    vtty_first_output: 'First:',
    vtty_last_output: 'Last:',
    copy_name: 'Copy name',
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
    vtty_click_to_pin: '点击固定',
    vtty_pinned: '已固定',
    vtty_lines: '行',
    vtty_first_output: '首次:',
    vtty_last_output: '末次:',
    copy_name: '复制名称',
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

const tooltipPinned = ref(false)
const pinnedBinaryName = ref<string | null>(null)
const tooltipFirstTime = ref(0)
const tooltipLastTime = ref(0)
const tooltipScrollLine = ref(1)

interface TooltipState {
  kind: 'text' | 'terminal'
  el: HTMLElement
  content: string
  binaryName?: string
  log?: string[]
}
const tooltip = ref<TooltipState | null>(null)

let tooltipTerminal: Terminal | null = null
const tooltipTerminalRef = ref<HTMLElement>()

const cardRef = ref<HTMLElement>()

let countdownTimer: any = null
let pollTimer: any = null

function getXtermOptions() {
  return {
    theme: {
      background: '#282c34',
      foreground: '#dcdfe4',
      cursor: '#528bff',
      cursorAccent: '#282c34',
      selectionBackground: '#528bff40',
      black: '#282c34',
      red: '#e06c75',
      green: '#98c379',
      yellow: '#e5c07b',
      blue: '#61afef',
      magenta: '#c678dd',
      cyan: '#56b6c2',
      white: '#dcdfe4',
      brightBlack: '#5c6370',
      brightRed: '#e06c75',
      brightGreen: '#98c379',
      brightYellow: '#e5c07b',
      brightBlue: '#61afef',
      brightMagenta: '#c678dd',
      brightCyan: '#56b6c2',
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
    cursorStyle: 'block' as const,
    scrollback: 10000,
  }
}

function setupTooltipXterm(el: HTMLElement) {
  disposeTooltipXterm()
  tooltipTerminal = new Terminal(getXtermOptions())
  tooltipTerminal.open(el)
  tooltipTerminal.onScroll(() => {
    if (tooltipTerminal) {
      tooltipScrollLine.value = tooltipTerminal.buffer.active.viewportY + 1
    }
  })
}

function disposeTooltipXterm() {
  if (tooltipTerminal) {
    tooltipTerminal.dispose()
    tooltipTerminal = null
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

watch(tooltip, (newVal) => {
  if (newVal?.kind === 'terminal') {
    nextTick(() => {
      const el = tooltipTerminalRef.value
      if (el && !tooltipTerminal) {
        setupTooltipXterm(el)
      }
      if (tooltipTerminal && newVal.log?.length) {
        tooltipTerminal.reset()
        tooltipTerminal.write(newVal.log.join('\r\n') + '\r\n')
        tooltipTerminal.scrollToBottom()
        tooltipScrollLine.value = tooltipTerminal.buffer.active.viewportY + 1
      }
    })
  } else if (!newVal || newVal.kind === 'text') {
    disposeTooltipXterm()
  }
})

onUnmounted(() => {
  clearInterval(countdownTimer)
  clearInterval(pollTimer)
  disposeTooltipXterm()
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

function showTextTooltip(ev: MouseEvent, content: string) {
  if (tooltipPinned.value) return
  tooltip.value = {
    kind: 'text',
    el: ev.currentTarget as HTMLElement,
    content,
  }
}

let hideTimer: any = null

function hideTooltip() {
  if (tooltipPinned.value) return
  if (tooltip.value?.kind === 'text') {
    tooltip.value = null
    return
  }
  clearTimeout(hideTimer)
  hideTimer = setTimeout(() => {
    if (tooltipPinned.value) return
    disposeTooltipXterm()
    tooltip.value = null
  }, 200)
}

function clearHideTimer() {
  clearTimeout(hideTimer)
}

function hoverTooltipLeave() {
  if (tooltipPinned.value) return
  clearTimeout(hideTimer)
  hideTimer = setTimeout(() => {
    disposeTooltipXterm()
    tooltip.value = null
  }, 200)
}

const hoverCache: Record<string, string[]> = {}

function hoverVttyBadge(ev: MouseEvent, name: string) {
  clearTimeout(hideTimer)
  const el = ev.currentTarget as HTMLElement
  tooltip.value = {
    kind: 'terminal',
    el,
    content: '',
    binaryName: name,
    log: hoverCache[name] || [],
  }

  nextTick(() => {
    const terminalEl = tooltipTerminalRef.value
    if (terminalEl && !tooltipTerminal) {
      setupTooltipXterm(terminalEl)
    }
    if (tooltipTerminal && hoverCache[name]?.length) {
      tooltipTerminal.reset()
      tooltipTerminal.write(hoverCache[name].join('\r\n') + '\r\n')
        tooltipTerminal.scrollToBottom()
      tooltipScrollLine.value = tooltipTerminal.buffer.active.viewportY + 1
    }
  })

  if (!hoverCache[name]) {
    fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
      .then(r => r.json())
      .then(d => {
        const logs = (d.vttys?.[0]?.log || []).slice(-100)
        hoverCache[name] = logs
        if (!tooltipFirstTime.value && logs.length > 0) {
          tooltipFirstTime.value = Date.now()
        }
        if (logs.length > 0) {
          tooltipLastTime.value = Date.now()
        }
        if (tooltip.value?.kind === 'terminal' && tooltip.value.binaryName === name) {
          tooltip.value = { ...tooltip.value, log: logs }
          if (tooltipTerminal) {
            tooltipTerminal.reset()
            tooltipTerminal.write(logs.join('\r\n') + '\r\n')
        tooltipTerminal.scrollToBottom()
            tooltipScrollLine.value = tooltipTerminal.buffer.active.viewportY + 1
          }
        }
      }).catch(() => {})
  }
}

function hoverVttyLeave() {
  clearTimeout(hideTimer)
  if (tooltipPinned.value) return
  hideTimer = setTimeout(() => {
    disposeTooltipXterm()
    tooltip.value = null
  }, 200)
}

function togglePin(ev: MouseEvent, name: string) {
  if (tooltipPinned.value && pinnedBinaryName.value === name) {
    unpinTooltip()
    return
  }
  pinnedBinaryName.value = name
  tooltipPinned.value = true
  cancelRedirect()
  clearTimeout(hideTimer)
  hoverVttyBadge(ev, name)
}

function unpinTooltip() {
  tooltipPinned.value = false
  pinnedBinaryName.value = null
  disposeTooltipXterm()
  tooltip.value = null
}

function togglePinFromFooter() {
  if (tooltipPinned.value) {
    unpinTooltip()
  } else {
    pinnedBinaryName.value = tooltip.value?.binaryName || null
    tooltipPinned.value = true
    cancelRedirect()
  }
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

function formatTime(ts: number): string {
  if (!ts) return '--:--:--'
  return new Date(ts).toTimeString().slice(0, 8)
}

function copyTooltipTerminal() {
  if (!tooltipTerminal) return
  const buffer = tooltipTerminal.buffer.active
  const lines: string[] = []
  for (let i = 0; i < buffer.length; i++) {
    const line = buffer.getLine(i)
    if (line) lines.push(line.translateToString())
  }
  navigator.clipboard?.writeText(lines.join('\n')).then(() => {
    toast(t('copied_msg', 'Copied to clipboard'))
  }).catch(() => {})
}

function copyBinaryName() {
  if (!tooltip.value?.binaryName) return
  navigator.clipboard?.writeText(tooltip.value.binaryName).then(() => {
    toast(t('copied_msg', 'Copied to clipboard'))
  }).catch(() => {})
}

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
    }).catch(() => {})
}
</script>
