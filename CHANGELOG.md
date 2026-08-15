# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.2.16] - 2026-08-15

### Fixed

- Keep the previous build token when the epoch probe's body read times out instead of hashing the partial body, which could flip the token between probe cycles on backends whose document delivery time jitters around the read budget.

## [0.2.15] - 2026-08-15

### Added

- Bind the serve-mode landing interstitial to a per-build token derived from the backend's served document, so every client sees the landing page exactly once per rebuild (first visits included) instead of once per cookie lifetime.
- Forward non-document API/XHR traffic from previous-build sessions through the serve door while their document loads are intercepted, keeping stale SPAs functional across a rebuild until their next navigation.

### Changed

- Extend the `__malkuth_nonce` cookie lifetime from 30 minutes to 7 days; re-showing the landing page is now driven by build-token mismatch rather than cookie expiry.

## [0.2.14] - 2026-08-14

### Added

- Add automatic database backups with rolling retention and optional age encryption.

### Fixed

- Restart supervised pods when the watched binary changes despite a failing build.
- Remove the partial dump file when the age binary cannot be spawned.

## [0.2.13] - 2026-08-05

### Added

- Tunnel WebSocket upgrades through the serve proxy.
- Add GitHub-hosted CI fallback to remove the self-hosted single point.

### Changed

- Isolate CI cargo caches per runner via CARGO_HOME.

### Fixed

- Route all HTTP methods through the serve proxy front door.

## [0.2.12] - 2026-08-03

### Fixed

- Restart supervised pods when the watched binary itself changes.

## [0.2.11] - 2026-08-02

### Changed

- Route compute CI to local self-hosted runner.
- Update tera requirement to ^2 and sha2 to ^0.11.

### Fixed

- Pass through backend error responses instead of masking them as offline.

### Removed

- Retire per-repo PLAN.md in favor of root PLAN.

## [0.2.10] - 2026-08-01

### Fixed

- Probe backend readiness via `/readyz` with starting state and edge-case hardening.

## [0.2.9] - 2026-08-01

### Changed

- Unify npm specs to caret-star and upgrade to latest series.

## [0.2.8] - 2026-07-31

### Fixed

- Show `offline` state instead of redirect countdown when backend is unreachable.

## [0.2.7] - 2026-07-31 (untagged)

### Fixed

- Catch npm publish race by treating already-published as success.

## [0.2.6] - 2026-07-30

### Changed

- Disable ligatures and add font smoothing to xterm for consistent selection text.
- Refactor landing page to TSX with CSS variables, BEM nesting, zero hardcoded colors, and terminal theme fixes.

### Fixed

- Add overflow hidden and border radius to terminal tooltip to prevent corner bleed.
- Fix terminal light theme mismatch by hardcoding dark One Half theme.

## [0.2.5] - 2026-07-30 (untagged)

### Changed

- Bump Cargo.toml and landing page version to 0.2.5.

## [0.2.4] - 2026-07-30 (untagged)

### Added

- Add `--serve` reverse-proxy landing page with poll-based probe and VTTY tooltip.
- Capture supervised process runtime stdout/stderr for VTTY terminal display.
- Add ANSI parsing, custom scroll indicator, monospace font, and sticky copy button to VTTY terminal.
- Add landing_page Vue SPA via include_dir monorepo packages.
- Overhaul malkuth landing page with orange theme, unified TS-anchored tooltips, VTTY terminal panel, and standalone single-file HTML.
- Overhaul malkuth landing page with unified VTTY terminal tooltip, nonce-based proxy routing, One Half theme, and xterm.js rendering.

### Changed

- Unify locale codes to BCP 47 format.
- Switch kou from local path to crates.io dependency.

### Fixed

- Auto-scroll terminal to bottom and fix footer to sit below content instead of floating.
- Use calc height for xterm to ensure footer sits below without overlap.
- Set explicit 20em xterm height and 28px footer to prevent terminal text overlapping status bar.
- Add overflow hidden to terminal and xterm wrapper to prevent canvas overflow.
- Adjust terminal width, footer layout to flex, styled scrollbar, auto-scroll, log truncation.

## [0.2.3] - 2026-07-28

### Added

- Add `--serve` reverse-proxy mode to info page.

## [0.2.2] - 2026-07-28

### Added

- Add `--serve` reverse-proxy mode to info page.

## [0.2.1] - 2026-07-28

### Changed

- Upgrade toml to ^1, bump setup-python to v7 and fix all clippy warnings.
- Translate MCP Server Deployment section for all 8 languages.

### Fixed

- Fix Windows clippy and lagrange deploy branch.

## [0.2.0] - 2026-07-28

### Added

- Add rich info page with i18n, landing mode, binary info and portal tooltips.

## [0.1.6] - 2026-07-28

### Added

- Conditionally restart only when build produces output changes.

## [0.1.5] - 2026-07-28

### Added

- Add daemon subcommand with TOML config, PID guard, and SIGHUP reload.
- Add supervisor restart gate: drain worker auth, WS/IPC proxy, and working dir.
- Add self-update fork exec zero-downtime restart with inherited listener fd.
- Add `--build` and `--debounce` for pre-restart build commands.
- Update docs for `--build` and `--debounce` across all locales.

### Changed

- Add cfg guards for ws and ipc proxy modules.

### Fixed

- Fix self-update fd closure to use close-on-exec instead of blind close loop.
- Close authorization gate: validate drain requests against approved proposal registry.
- Fix WS proxy routing: add path-worker mapping for service-based backend selection.
- Remove unused Duration import in ipc_proxy.
- Fix pre-existing compilation warnings before v0.1.5 release.
- Suppress dead_code warning for format_time.

## [0.1.4] - 2026-07-19

### Added

- Add replica and leader-elector backends for HA deployment.
- Add `--singleton` flag for exclusive instance locking per proxy port.
- Add HTTP_PROXY/HTTPS_PROXY/ALL_PROXY env var support to MCP probe's HTTP client.

### Changed

- Final sync of dev into master before dev retirement.
- Migrate arona references to plana.
- Consolidate dependabot dependency updates.

## [0.1.3] - 2026-07-17

### Added

- Add service supervision toolkit with lifecycle management and rolling updates.

### Changed

- Apply cargo fmt across workspace.
- Enable npm Trusted Publishing with Node 22 and npm CLI upgrade.

## [0.1.2] - 2026-07-10

### Added

- Add cross-compilation toolchains for ARM64 and musl Linux targets.

## [0.1.1] - 2026-07-09

### Added

- Expand the npm platform matrix to six targets covering ARM and musl.

## [0.1.0] - 2026-07-09

### Added

- Add service supervision and process lifecycle management.
- Add in-process MCP server and publish to npm as a precompiled npx package.

### Changed

- Refactor supervision onto tokio, flatten to one crate, and harden for publish.
- Drive the pages CNAME from config and move to the docs subdomain.
- Drop `--locked` from npm-release build to let Cargo resolve the lock file.

[Unreleased]: https://github.com/celestia-island/malkuth/compare/v0.2.12...HEAD
[0.2.12]: https://github.com/celestia-island/malkuth/compare/v0.2.10...v0.2.12
[0.2.10]: https://github.com/celestia-island/malkuth/compare/v0.2.9...v0.2.10
[0.2.9]: https://github.com/celestia-island/malkuth/compare/v0.2.8...v0.2.9
[0.2.8]: https://github.com/celestia-island/malkuth/compare/v0.2.6...v0.2.8
[0.2.6]: https://github.com/celestia-island/malkuth/compare/v0.2.3...v0.2.6
[0.2.3]: https://github.com/celestia-island/malkuth/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/celestia-island/malkuth/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/celestia-island/malkuth/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/celestia-island/malkuth/compare/v0.1.6...v0.2.0
[0.1.6]: https://github.com/celestia-island/malkuth/compare/v0.1.5...v0.1.6
[0.1.5]: https://github.com/celestia-island/malkuth/compare/v0.1.4...v0.1.5
[0.1.4]: https://github.com/celestia-island/malkuth/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/celestia-island/malkuth/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/celestia-island/malkuth/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/celestia-island/malkuth/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/celestia-island/malkuth/releases/tag/v0.1.0
