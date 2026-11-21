// The malkuth information / landing page.
//
// Rendered by TWO entries from ONE component tree:
//   src/main.ts        -> createApp(<LandingPage />).mount('#app')   (SPA)
//   src/ssr/prerender  -> renderToString(<LandingPage static />)     (build-time
//                         prerender embedded in the binary as the no-JS fallback)
//
// The static render swaps every per-request value for a `{{MALKUTH_*}}`
// marker that src/info_page.rs substitutes; the layout, classes and hikari
// components are literally the same ones the SPA renders, so the fallback
// cannot drift away from the real page.
//
// Everything visual comes from hikari: the card/button/badge/copy-chip/
// terminal/popover/toast components plus the CSS custom properties produced
// by src/theme/malkuth.ts. The hand-written part is page layout only.
import { computed, defineComponent, onBeforeUnmount, onMounted, ref } from 'vue'

import HkBadge, { type BadgeVariant } from '@celestia-island/hikari/components/HkBadge.tsx'
import HkButton from '@celestia-island/hikari/components/HkButton.tsx'
import HkCard from '@celestia-island/hikari/components/HkCard.tsx'
import HkDivider from '@celestia-island/hikari/components/HkDivider.tsx'
import HkIconButton from '@celestia-island/hikari/components/HkIconButton.tsx'
import HkLogo from '@celestia-island/hikari/components/HkLogo.tsx'
import HkSpinner from '@celestia-island/hikari/components/HkSpinner.tsx'
import { HkStatusRow } from '@celestia-island/hikari/components/HkStatusRow.tsx'
import HkToast from '@celestia-island/hikari/components/HkToast.tsx'
import { HkLogWindow, useClipboardWithToast, useToast, setLocale, type HLogTab } from '@celestia-island/hikari'

// The payload these types describe is produced by `src/info_page.rs`
// (`build_spa_init`) — that Rust builder is the single source of truth. This
// crate has no ts-rs feature (the generated-bindings lane does not exist here
// yet), so the declaration is the consumer side of one contract: change the
// Rust builder and this interface together. `main.ts` imports these types
// rather than re-declaring a narrower copy.
/** Live state of the supervised backend, as the Rust side reports it. */
export type PageState = 'landing' | 'building' | 'starting' | 'ready' | 'offline'

export interface BinaryInfo {
  name: string
  path: string
  compile_time: string
  hash: string
  hash_short: string
}

/** `window.__MALKUTH_INIT__` — built server-side by `build_spa_init`. */
export interface MalkuthInit {
  state: PageState
  message: string
  binaries: BinaryInfo[]
  watch_paths: string[]
  proxy_endpoint: string | null
  version: string
  epoch: string
  logo_base64: string
  serve: boolean
  lang: string
  dir: 'ltr' | 'rtl'
  i18n: Record<string, string>
  /** Client beats; the server owns the numbers so both surfaces agree. */
  timing?: { countdown_s?: number; probe_interval_ms?: number }
  /** Build-token cookie contract (name + lifetime) as the server defines it. */
  nonce?: { cookie?: string; max_age_s?: number; path?: string }
}

interface ProbeReply {
  state?: PageState
  message?: string
  epoch?: string
  serve?: boolean
  vttys?: { name: string; log: string[] }[]
}

/** English fallbacks: only used when the server payload arrives incomplete. */
const FALLBACK: Record<string, string> = {
  title: 'Malkuth — Service Supervisor',
  heading: 'Malkuth',
  tagline: 'This port is managed by the Malkuth process supervisor',
  status_ready: 'All services are running normally.',
  status_building: 'The service is currently being rebuilt. Please wait a moment',
  status_starting: 'The service is starting up',
  status_landing: 'Redirecting shortly',
  status_offline: 'The service is temporarily unavailable',
  proxy_label: 'Proxy',
  watch_label: 'Watching',
  binaries_title: 'Supervised Binaries',
  redirect_before: 'Redirecting in',
  redirect_after: 'seconds',
  cancel_label: 'Cancel',
  refresh_label: 'Refresh Now',
  copied_msg: 'Copied to clipboard',

  vtty_lines: 'lines',
  vtty_first_output: 'First:',
  vtty_last_output: 'Last:',
  vtty_click_to_pin: 'Click to pin',
  vtty_pinned: 'Pinned',
  vtty_waiting: 'Connected, waiting for output',
  footer: 'Powered by Malkuth — a composable service-supervision toolkit for Rust',
}

/** Placeholders the Rust side rewrites in the prerendered fallback page. */
export const MARKERS = {
  logo: '{{MALKUTH_LOGO_SRC}}',
  heading: '{{MALKUTH_HEADING}}',
  tagline: '{{MALKUTH_TAGLINE}}',
  statusBadge: '{{MALKUTH_STATUS_BADGE}}',
  taskRow: '{{MALKUTH_TASK_ROW}}',
  proxyRow: '{{MALKUTH_PROXY_ROW}}',
  watchRow: '{{MALKUTH_WATCH_ROW}}',
  binariesBlock: '{{MALKUTH_BINARIES_BLOCK}}',
  retryBlock: '{{MALKUTH_RETRY_BLOCK}}',
  actionBlock: '{{MALKUTH_ACTION_BLOCK}}',
  footer: '{{MALKUTH_FOOTER}}',
  version: '{{MALKUTH_VERSION_LINE}}',
} as const

const BADGE_BY_STATE: Record<PageState, BadgeVariant> = {
  ready: 'success',
  landing: 'warning',
  building: 'warning',
  starting: 'warning',
  offline: 'error',
}

const BINARY_ICON = () => (
  <svg
    xmlns="http://www.w3.org/2000/svg"
    width="16"
    height="16"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <path d="M21 8a2 2 0 0 0-1-1.73l-7-4a2 2 0 0 0-2 0l-7 4A2 2 0 0 0 3 8v8a2 2 0 0 0 1 1.73l7 4a2 2 0 0 0 2 0l7-4A2 2 0 0 0 21 16z" />
    <path d="m3.3 7 8.7 5 8.7-5" />
    <path d="M12 22V12" />
  </svg>
)

const TERMINAL_ICON = () => (
  <svg
    xmlns="http://www.w3.org/2000/svg"
    width="14"
    height="14"
    viewBox="0 0 24 24"
    fill="none"
    stroke="currentColor"
    stroke-width="2"
    stroke-linecap="round"
    stroke-linejoin="round"
    aria-hidden="true"
  >
    <rect x="2" y="3" width="20" height="14" rx="2" ry="2" />
    <line x1="8" y1="21" x2="16" y2="21" />
    <line x1="12" y1="17" x2="12" y2="21" />
  </svg>
)

export default defineComponent({
  name: 'MalkuthPage',
  props: {
    /**
     * Build-time prerender mode: no DOM, no interactions, every per-request
     * value is a `{{MALKUTH_*}}` marker.
     */
    static: { type: Boolean, default: false },
  },
  setup(props) {
    // ── Server payload ────────────────────────────────────────────────
    const init: MalkuthInit | null = props.static
      ? null
      : ((window as unknown as { __MALKUTH_INIT__?: MalkuthInit }).__MALKUTH_INIT__ ?? null)

    const t = (key: string, fallback?: string): string =>
      init?.i18n?.[key] ?? FALLBACK[key] ?? fallback ?? key

    const state = ref<PageState>(init?.state ?? 'landing')
    const version = ref(init?.version ?? '')
    const logoBase64 = ref(init?.logo_base64 ?? '')
    const proxyEndpoint = ref(init?.proxy_endpoint ?? '')
    const watchPaths = ref<string[]>(init?.watch_paths ?? [])
    const binaries = ref<BinaryInfo[]>(init?.binaries ?? [])
    const statusMessage = ref(init?.message ?? '')

    // Countdown / recovery state, ported 1:1 from the pre-hikari page.
    const countdown = ref(3)
    const showRefresh = ref(false)
    const redirectAttempted = ref(false)
    let canProxy = init?.serve !== false
    let initEpoch = init?.epoch ?? '1'

    // ── Terminal preview ──────────────────────────────────────────────
    const terminalOpen = ref(false)
    const terminalBinary = ref<string>('')
    const terminalLog = ref<string[]>([])
    const terminalFirstAt = ref(0)
    const terminalLastAt = ref(0)
    const logCache = new Map<string, string[]>()
    const toast = useToast()
    // One copy path for the whole page: hikari's hook carries the
    // `<textarea>`+execCommand fallback that non-secure (plain http) doors
    // need, plus both toasts.
    const clipboard = useClipboardWithToast(
      toast,
      () => t('copied_msg'),
      () => t('copy_fail_msg'),
    )

    const logoUrl = computed(() =>
      logoBase64.value ? `data:image/webp;base64,${logoBase64.value}` : '',
    )
    const badgeVariant = computed<BadgeVariant>(() => BADGE_BY_STATE[state.value] ?? 'warning')
    const statusText = computed(() => {
      switch (state.value) {
        case 'ready':
          return t('status_ready')
        case 'building':
          return t('status_building')
        case 'starting':
          return t('status_starting')
        case 'offline':
          return t('status_offline')
        default:
          return t('status_landing')
      }
    })
    // `ready` and `landing` both arm the 3s redirect (loadInit starts the same
    // countdown), so both must offer the cancel affordance; only `landing`
    // renders the numeric hint (master behaviour).
    const showCountdown = computed(
      () => state.value === 'landing' && !redirectAttempted.value,
    )
    const canCancelRedirect = computed(
      () =>
        (state.value === 'ready' || state.value === 'landing') &&
        !redirectAttempted.value,
    )

    const cookieName = init?.nonce?.cookie ?? '__malkuth_nonce'
    const cookieMaxAge = init?.nonce?.max_age_s ?? 604800
    const cookiePath = init?.nonce?.path ?? '/'
    const countdownStart = init?.timing?.countdown_s ?? 3
    const probeInterval = init?.timing?.probe_interval_ms ?? 2000

    function setNonceCookie() {
      document.cookie = `${cookieName}=${initEpoch}; max-age=${cookieMaxAge}; path=${cookiePath}`
    }

    function getCookie(name: string): string | null {
      const match = document.cookie.match(new RegExp('(^|; )' + name + '=([^;]*)'))
      return match ? decodeURIComponent(match[2]) : null
    }

    let countdownTimer: ReturnType<typeof setInterval> | null = null
    let pollTimer: ReturnType<typeof setInterval> | null = null

    function stopTimers() {
      if (countdownTimer) clearInterval(countdownTimer)
      if (pollTimer) clearInterval(pollTimer)
      countdownTimer = null
      pollTimer = null
    }

    function startCountdown() {
      countdown.value = countdownStart
      countdownTimer = setInterval(() => {
        countdown.value -= 1
        if (countdown.value <= 0) {
          stopTimers()
          setNonceCookie()
          location.reload()
        }
      }, 1000)
    }

    function startPolling() {
      if (pollTimer) return
      pollTimer = setInterval(probe, probeInterval)
    }

    function probe() {
      fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
        .then((r) => r.json())
        .then((d: ProbeReply) => {
          statusMessage.value = d.message ?? ''
          if (d.epoch) initEpoch = d.epoch
          if (d.serve !== undefined) canProxy = d.serve !== false
          if (d.state === 'ready') {
            // A non-proxy door cannot turn a reload into an entry into the
            // backend — reloading there would loop on a static page.
            if (!canProxy) {
              state.value = 'ready'
              return
            }
            setNonceCookie()
            location.reload()
            return
          }
          state.value = (d.state as PageState) ?? state.value
          if (d.state === 'offline') showRefresh.value = true
        })
        .catch(() => {})
    }

    function loadInit() {
      const loaded = init
      if (!loaded) return
      state.value = loaded.state ?? 'landing'

      if (loaded.state === 'offline' || loaded.state === 'starting') {
        showRefresh.value = true
        startPolling()
        return
      }
      if (getCookie(cookieName) === initEpoch) {
        // The cookie already names the served build yet this page rendered:
        // a redirect was attempted (or cancelled) and the backend was not
        // reachable. Offer the manual action and let polling recover.
        redirectAttempted.value = true
        showRefresh.value = true
        if (canProxy) startPolling()
        return
      }
      if (loaded.state === 'ready' || loaded.state === 'landing') {
        startCountdown()
      } else {
        showRefresh.value = true
        startPolling()
      }
    }

    function cancelRedirect() {
      stopTimers()
      redirectAttempted.value = true
      showRefresh.value = true
    }

    function doRefresh() {
      // Re-enter the fresh build: stamp the current build token before
      // reloading so the next document loads straight from the backend.
      fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
        .then((r) => r.json())
        .then((d: ProbeReply) => {
          if (d?.epoch) initEpoch = d.epoch
        })
        .catch(() => {})
        .finally(() => {
          setNonceCookie()
          location.reload()
        })
    }

    function fetchLog(name: string) {
      const cached = logCache.get(name)
      if (cached) terminalLog.value = cached
      fetch('/', { headers: { 'X-Malkuth-Probe': '1' } })
        .then((r) => r.json())
        .then((d: ProbeReply) => {
          const lines = (d.vttys?.[0]?.log ?? []).slice(-100)
          logCache.set(name, lines)
          if (terminalBinary.value !== name) return
          terminalLog.value = lines
          const now = Date.now()
          if (lines.length && !terminalFirstAt.value) terminalFirstAt.value = now
          if (lines.length) terminalLastAt.value = now
        })
        .catch(() => {})
    }

    function openTerminal(name: string) {
      terminalBinary.value = name
      terminalLog.value = logCache.get(name) ?? []
      terminalOpen.value = true
      fetchLog(name)
    }

    function closeTerminal() {
      terminalOpen.value = false
      terminalBinary.value = ''
      terminalLog.value = []
    }

    /**
     * Split a `host:port` / `branch:hash` value at its LAST separator, so the
     * caller can render the two halves as separate blocks. Returns a single
     * segment when the value carries no separator.
     */
    function splitLast(value: string, sep: string): string[] {
      const at = value.lastIndexOf(sep)
      // The separator stays with the head, matching the fallback renderer, so
      // the rendered text still reads as one value (`0.0.0.0:` + `3097`)
      // rather than two tokens that run together when copied.
      const tail = at > 0 ? value.slice(at + sep.length) : ''
      return at > 0 && tail ? [value.slice(0, at + sep.length), tail] : [value]
    }

    /** Split a path into its directory prefix and file name. */
    function splitPath(value: string): string[] {
      const at = value.lastIndexOf('/')
      const tail = value.slice(at + 1)
      return at > 0 && tail ? [value.slice(0, at + 1), tail] : [value]
    }

    const PROXY_ARROW = /\s*(?:→|->)\s*/

    onMounted(() => {
      if (props.static) return
      void setLocale(init?.lang ?? 'en')
      document.documentElement.lang = init?.lang ?? 'en'
      document.documentElement.dir = init?.dir ?? 'ltr'
      if (init?.i18n?.title) document.title = init.i18n.title
      loadInit()
    })

    onBeforeUnmount(stopTimers)

    // ── Shared pieces ─────────────────────────────────────────────────
    const head = (heading: string, tagline: string, logo: string) => (
      <header class="mk-head">
        <HkLogo src={logo} alt="Malkuth" size="lg" class="mk-logo" />
        <h1 class="mk-title">{heading}</h1>
        <p class="mk-tagline">{tagline}</p>
      </header>
    )

    const staticTree = () => (
      <div class="mk-page">
        <HkCard class="mk-card">
          {head(MARKERS.heading, MARKERS.tagline, MARKERS.logo)}
          <div class="mk-status">{MARKERS.statusBadge}</div>
          <div class="mk-pairs">
            {MARKERS.taskRow}
            {MARKERS.proxyRow}
            {MARKERS.watchRow}
          </div>
          {MARKERS.binariesBlock}
          {MARKERS.retryBlock}
          {MARKERS.actionBlock}
          <footer class="mk-footer">
            <span class="mk-footer-link">{MARKERS.footer}</span>
            <span class="mk-version">{MARKERS.version}</span>
          </footer>
        </HkCard>
      </div>
    )

    /**
     * One body, two blocks: a label block and a value block in a single frame.
     * The copyable form is a real <button> so the affordance keeps a keyboard
     * path, focus ring and an accessible name.
     */
    const pairTag = (label: string, segments: string[], copyValue?: string) => {
      const body = segments.map((part, i) =>
        part === '→' ? (
          <span class="mk-seg-arrow" aria-hidden="true">
            {part}
          </span>
        ) : (
          <span
            class={[
              'mk-seg',
              i === 0 && segments.length > 1 ? 'mk-seg--muted' : 'mk-seg--accent',
            ]}
          >
            {part}
          </span>
        ),
      )
      return (
        <span class="mk-pair">
          {label ? <span class="mk-pair__label">{label}</span> : null}
          {copyValue === undefined ? (
            <span class="mk-pair__value">{body}</span>
          ) : (
            <button
              type="button"
              class="mk-pair__value"
              title={copyValue}
              aria-label={label ? `${label}: ${copyValue}` : copyValue}
              onClick={() => void clipboard.copy(copyValue)}
            >
              {body}
            </button>
          )}
        </span>
      )
    }

    /** `0.0.0.0:3097 → 3009-3009` becomes [0.0.0.0][3097] → [3009-3009]. */
    const proxySegments = (endpoint: string): string[] => {
      const [public_side, backend_side] = endpoint.split(PROXY_ARROW)
      const left = splitLast(public_side ?? '', ':')
      const segments = [...left]
      if (backend_side) {
        segments.push('→', ...splitLast(backend_side, ':'))
      }
      return segments
    }

    /** hikari's own log viewer (tabs + pause/autoscroll/copy/clear on the
     *  HScrollContainer overlay rail) — replaces the xterm popover wholesale:
     *  the landing page is a read-only log viewer, not a terminal emulator. */
    const logTabs = computed<HLogTab[]>(() => [
      { key: terminalBinary.value || 'log', title: terminalBinary.value || 'log', lines: terminalLog.value },
    ])

    const terminalPopup = () => (
      <HkLogWindow
        modelValue={terminalOpen.value}
        onUpdate:modelValue={(v: boolean) => {
          if (!v) closeTerminal()
        }}
        tabs={logTabs.value}
        title={terminalBinary.value || undefined}
        height="55vh"
      />
    )

    const liveTree = () => (
      <div class="mk-page">
        <HkCard class="mk-card">
          {head('Malkuth', t('tagline'), logoUrl.value)}
          <div class="mk-status" aria-live="polite">
            <HkBadge variant={badgeVariant.value} dot pulse={state.value !== 'ready'}>
              {statusText.value}
            </HkBadge>
            {(state.value === 'building' || state.value === 'starting') && (
              <HkSpinner size="sm" />
            )}
          </div>

          <div class="mk-pairs">
            {proxyEndpoint.value
              ? pairTag(t('proxy_label'), proxySegments(proxyEndpoint.value), proxyEndpoint.value)
              : null}
            {watchPaths.value.map((path) =>
              pairTag(t('watch_label'), splitPath(path), path),
            )}
          </div>

          {binaries.value.length ? (
            <section class="mk-binaries">
              <h2 class="mk-section-title">{t('binaries_title')}</h2>
              <div class="mk-binary-list">
                {binaries.value.map((binary) => (
                  <HkStatusRow
                    key={binary.name}
                    tone="info"
                    title={binary.name}
                    status={binary.compile_time}
                  >
                    {{
                      icon: () => BINARY_ICON(),
                      action: () => (
                        <div class="mk-binary-actions">
                          {pairTag(
                            '',
                            splitLast(binary.hash_short, ':'),
                            binary.hash,
                          )}
                          <HkIconButton
                            size={24}
                            aria-label={t('vtty_open')}
                            onClick={() => openTerminal(binary.name)}
                          >
                            {TERMINAL_ICON()}
                          </HkIconButton>
                        </div>
                      ),
                    }}
                  </HkStatusRow>
                ))}
              </div>
            </section>
          ) : null}

          <HkDivider spacing="md" />

          {showCountdown.value ? (
            <p class="mk-retry">
              <span>{t('redirect_before')}</span>
              <strong class="mk-countdown">{countdown.value}</strong>
              <span>{t('redirect_after')}</span>
            </p>
          ) : null}

          <div class="mk-actions">
            {canCancelRedirect.value ? (
              <HkButton variant="outline" size="sm" onClick={cancelRedirect}>
                {t('cancel_label')}
              </HkButton>
            ) : null}
            {showRefresh.value ? (
              <HkButton variant="primary" size="sm" onClick={doRefresh}>
                {t('refresh_label')}
              </HkButton>
            ) : null}
          </div>

          <footer class="mk-footer">
            <span class="mk-footer-link">{t('footer')}</span>
            <span class="mk-version">v{version.value}</span>
          </footer>
          {statusMessage.value ? <span class="mk-visually-hidden">{statusMessage.value}</span> : null}
        </HkCard>

        {terminalPopup()}
        <HkToast />
      </div>
    )

    return () => (props.static ? staticTree() : liveTree())
  },
})
