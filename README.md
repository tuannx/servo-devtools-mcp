# servo-devtools-mcp

**chrome-devtools-mcp for Servo** — an MCP server that exposes the [Servo](https://servo.org/)
browser engine's control surface to AI agents: navigate, screenshot, accessibility-tree
snapshot, console messages. stdio transport; works with Claude Code, Cursor, VS Code,
and any MCP client.

This is a **greenfield** project: no MCP server for Servo existed before
(verified via GitHub + web search, 2026-10-16). All existing browser MCPs
(`chrome-devtools-mcp`, `playwright-mcp`, `browser-use`) drive Chromium via CDP.
Servo has no CDP, so this server talks to the engine directly through Servo's
public **WebView API** (`components/servo`).

**Independent project.** This repository is not part of the Servo project and is
not affiliated with it. It depends on Servo only as a library (git dependency,
see "Pinned Servo revision" below). No code is copied from Servo; the crate's
own files are MIT.

## Status: P2 done (17 tools)

| Milestone | State |
|---|---|
| P0 — prove a11y tree + JS eval + navigation from an independent crate | ✅ **PASS** (verified 2026-10-18, see evidence below) |
| P1 — 15 tools MVP | ✅ **PASS** (verified 2026-10-18, see evidence below) |
| P2 — network + cookies via public API; performance_trace verdict | ✅ **DONE** (verified 2026-10-18, see evidence below) |
| P3 — publish + write-up | 🟡 **PREPPED, not published** — `cargo publish --dry-run` proves crates.io publish is currently *impossible* (see "Publishing" below); dev.to draft is written but **not posted** (drafts-only) |

Full plan: [`servo-devtools-mcp-plan-2026-10-17.md`](../) (in the author's notes repo).

### P2 — evidence (2026-10-18, this VM)

End-to-end MCP stdio smoke test (`/tmp/p2_smoke.py`), 12/12 checks green:

- **`network_requests` — implemented with public API only.** The delegate
  implements `WebViewDelegate::load_web_resource()` (public, default no-op),
  which Servo calls for every resource load. Each record:
  `{url, method, destination, is_main_frame, is_redirect}`. Verified: navigating
  `https://example.com` logs 4 requests, main-frame flag correct on the document
  request. **Honest limitation:** request log only — response status/headers
  live behind the crate-private `WebResourceLoad` responder, so exposing them
  would need an upstream patch, which is out of scope (project policy: no
  contributions to servo/servo).
- **`cookies` — implemented with public API only.** `get` lists cookies via
  `Servo::site_data_manager().cookies_for_url(url, CookieSource::HTTP)`
  (public method, verified at the pinned rev); `set` via
  `set_cookie_for_url(url, Cookie, None)`. Verified round-trips:
  JS-set cookie readable by the tool, tool-set cookie visible to
  `document.cookie`.
- **`performance_trace` — NOT SUPPORTED, verified.** There is no
  tracing/profiling/performance API on any public embedder type: nothing on
  `WebView`, `WebViewDelegate`, or `Servo` at the pinned rev (grep over
  `components/servo/{webview,webview_delegate,servo}.rs`). The only related
  surface is internal logging (`servo_tracing` re-export, `DiagnosticsLogging`
  opts) — no trace-capture API. Would need an upstream patch: out of scope.

### P1 — evidence (2026-10-18, this VM)

### P1 — evidence (2026-10-18, this VM)

End-to-end MCP stdio smoke test (`/tmp/p1_smoke.py`), 21/21 checks green:

- **screenshot — FIXED.** Root cause of the P0 stall: the engine loop never
  called `WebView::paint()`, so `Painter::render()` never ran and the
  screenshot state machine (`ScreenshotTaker`) never advanced past readiness —
  exactly like servoshell, the embedder must drive repaints. Fix: the servo
  thread now paints at a 100 ms cadence, and the screenshot handler paints
  every poll iteration. Verified: real 1280×800 PNG with page content
  (background #336699, white text — 853 distinct colors, not a blank frame).
- **console_messages — verified end-to-end.** Page `console.log`/`warn`/`error`
  all captured via the public `WebViewDelegate::show_console_message` callback
  (no Servo patch needed — the earlier plan draft was wrong on this).
- **Form interaction loop works.** Click into an `<input>`, `type_text "Tony"`,
  click a `<button>` → `evaluate_js` confirms the page's `onclick` ran
  (`"clicked:Tony"`). Hit-testing works because the run loop keeps the
  compositor fresh.
- **Navigation suite:** `navigate` (data: + https via egress proxy —
  example.com → "Example Domain"), `reload`, `go_back`, `go_forward`,
  `get_state` (`{url, title, load_status}`), `resize_viewport` 800×600.
- **Observation suite:** `dom_snapshot` (outerHTML), `evaluate_js` (JSON
  results, incl. `getBoundingClientRect`), `scroll`, `key_press` (Enter;
  unknown key names return a clean error).
- `cargo check --all-targets --locked` + `cargo fmt --check`: PASS.

### P0 gate — evidence (2026-10-18, this VM)

`cargo run --example p0_spike` and live MCP `tools/call` smoke tests:

- `P0 [a11y]: PASS — received 3 tree update(s)` — delegate receives AccessKit
  `TreeUpdate`s; `a11y_snapshot` returns a real tree (`heading`/"Hello from Servo",
  `paragraph`, `textRun`, `rootWebArea` with bounds).
- `P0 [js-eval]: PASS — document.title = String("")` — `evaluate_javascript` works.
- `navigate` via MCP: `loaded: data:text/html,...` — first navigation goes through
  `WebViewBuilder::url()` (avoids a `NewWebView`/`LoadUrl` race on fresh webviews).
- `cargo check --all-targets --locked`: PASS.

### Known issues

- None currently blocking. Screenshot and console capture — the two P0
  unknowns — are both resolved (see P1 evidence). Remaining P1 caveats:
  `type_text` sends per-character key events (no IME); `scroll` is a raw wheel
  delta with no smooth-scroll animation; `a11y_snapshot` drains the update
  buffer each call (by design — next call sees fresh updates).

## MCP tools (P2 — 17 tools)

| Tool | Description |
|---|---|
| `navigate` | Load a URL, wait for `LoadStatus::Complete`, return the final URL |
| `reload` | Reload the page, wait for load, return the final URL |
| `go_back` / `go_forward` | History traversal, return the final URL |
| `get_state` | `{url, title, load_status}` as JSON |
| `click` | Click at CSS-pixel (x, y): mouse move + button down/up |
| `type_text` | Type text into the focused element, character by character |
| `key_press` | Enter, Tab, Escape, Backspace, Delete, arrows, Home/End, PageUp/Down, F1–F12, or a single char |
| `scroll` | Scroll by (dx, dy) CSS pixels at point (x, y) |
| `screenshot` | Capture the viewport as PNG (returned base64) |
| `a11y_snapshot` | Enable accessibility, return the latest AccessKit `TreeUpdate` as JSON — the agent's DOM |
| `dom_snapshot` | `document.documentElement.outerHTML` |
| `evaluate_js` | Evaluate JavaScript, return the result as JSON |
| `console_messages` | Console messages logged by page content so far, as JSON |
| `resize_viewport` | Resize the viewport to width × height pixels |
| `network_requests` | Request log `{url, method, destination, is_main_frame, is_redirect}`; optional `clear`. Request-only — no response status/headers (Servo limitation, see P2 evidence) |
| `cookies` | `get` lists cookies for a URL as `{name, value, domain, path}`; `set` stores `name=value` |

### Parity vs chrome-devtools-mcp (honest)

`chrome-devtools-mcp` (Google, ~34 tools over CDP) is the reference. Mapping:

| chrome-devtools-mcp area | servo-devtools-mcp | Notes |
|---|---|---|
| Navigation (navigate, reload, back/forward) | ✅ `navigate`, `reload`, `go_back`, `go_forward`, `get_state` | |
| Input (click, type, keys, scroll) | ✅ `click`, `type_text`, `key_press`, `scroll` | per-char typing, no IME |
| Observation (screenshot, a11y/DOM snapshot) | ✅ `screenshot`, `a11y_snapshot`, `dom_snapshot` | a11y via AccessKit (Servo-native) |
| JS evaluation | ✅ `evaluate_js` | |
| Console | ✅ `console_messages` | via public delegate callback |
| Cookies | ✅ `cookies` get/set | via public `SiteDataManager` API |
| Network requests | ⚠️ `network_requests` (request log only) | response status/headers need an upstream patch |
| Performance/tracing | ❌ not supported | no public API exists in Servo; CDP has none here either |
| Viewport | ✅ `resize_viewport` | |

### Client config (Claude Code)

```json
{
  "mcpServers": {
    "servo-devtools": {
      "command": "/path/to/servo-devtools-mcp/target/release/servo-devtools-mcp"
    }
  }
}
```

## Build

**Warning: the first build compiles the entire Servo engine — expect 1–3 hours
and ~15 GB of disk.** Requirements: Rust ≥ 1.97, Python 3, a C++ toolchain,
and the system libraries Servo needs (see [Servo's build docs](https://servo.org/)).

```sh
cargo build --release
```

### Install on macOS

Steps follow [Servo's own build docs](https://book.servo.org/building/building.html)
(verified 2026-10-18); differences from Linux are called out.

```sh
# 1. Xcode Command Line Tools + Homebrew
xcode-select --install
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# 2. Native build deps (from Servo's own Brewfile — cmake + pkg-config only)
brew install cmake pkg-config

# 3. Rust (needs >= 1.97 — this crate sets rust-version = "1.97")
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
# restart your shell so cargo is on PATH

# 4. Build (first build compiles the whole Servo engine: 1–3 h, ~15 GB disk)
git clone https://github.com/tuannx/servo-devtools-mcp
cd servo-devtools-mcp
cargo build --release
# binary: ./target/release/servo-devtools-mcp
```

macOS vs Linux notes:
- **No fontconfig needed on macOS** — Servo's fonts component uses CoreText
  there (verified in `components/fonts/Cargo.toml` at the pinned rev; fontconfig
  is Linux/FreeBSD-only).
- **No EGL/Mesa needed on macOS** — the `libegl1`/`libegl-mesa0` requirement
  and `XDG_RUNTIME_DIR` are Linux-only (`SoftwareRenderingContext` uses the
  platform GL on macOS).
- Media (audio/video playback) would need GStreamer
  (`etc/install_macos_gstreamer.sh` in the Servo tree); the MCP tools here
  don't use media, so it's optional.
- Apple Silicon: builds natively on aarch64; no Rosetta needed.

> Not build-verified on Mac hardware from this Linux dev environment — the
> steps above are Servo's official macOS build prerequisites plus this crate's
> own `cargo build --release`. If a step fails on your Mac, file an issue.

Claude Code client config (macOS paths):

```json
{
  "mcpServers": {
    "servo-devtools": {
      "command": "/Users/you/servo-devtools-mcp/target/release/servo-devtools-mcp"
    }
  }
}
```

### Environment notes (Linux)

From the 2026-10-18 bring-up on a headless Linux VM:
- `SoftwareRenderingContext` needs `libEGL` (Mesa): `apt install libegl1 libegl-mesa0`,
  plus a valid `XDG_RUNTIME_DIR`.
- Sandboxed networks need the egress proxy in `Preferences`
  (`network_*_proxy_uri`), like Servo's headless test does.
- Accessibility needs `preferences.accessibility_enabled = true`, otherwise
  `set_accessibility_active()` is a silent no-op.

### Pinned Servo revision

The `servo` crate is not on crates.io, so this crate depends on it via git,
pinned to a commit whose API surface was read and verified:

```
rev = 23564ef62e2d898dee75d161bf62cb4d5cb89daf   # servo/servo main, 2026-10-18
```

Files verified at that commit: `components/servo/lib.rs`, `webview.rs`,
`webview_delegate.rs`, `servo.rs`, `components/servo/tests/{accessibility.rs,
common/mod.rs}`. When bumping the pin, re-check them — Servo's WebView API churns.

### Publishing

**crates.io: currently impossible by design.** `cargo publish --dry-run`
(2026-10-18) fails with:

```
error: failed to verify manifest ...
  all dependencies must have a version requirement specified when publishing.
  dependency `servo` does not specify a version
```

crates.io forbids git dependencies, and the `servo` crate is not published on
crates.io — so this crate can never be published there while it depends on
Servo by git rev. This is a structural property, not a metadata bug. If Servo
ever publishes the `servo` crate, flip the dependency to a version requirement
and re-run the dry-run.

Distribution until then: **GitHub releases + `cargo install --git`**:

```sh
cargo install --git https://github.com/tuannx/servo-devtools-mcp
```

Publish checklist (for the owner, when the blocker lifts):
1. `cargo login` with the owner's crates.io API token (owner-only step).
2. Bump `version` in `Cargo.toml`.
3. `cargo publish --dry-run` — must pass.
4. Tag `git tag vX.Y.Z && git push --tags`; cut a GitHub release with the
   release binary attached.
5. Post the dev.to write-up (`docs/devto-draft.md` — draft only, not posted).

## Architecture

```
┌──────────────────────────────────────────────────┐
│ servo-devtools-mcp (this crate, MIT)             │
│  ┌──────────┐   ┌──────────────────────────────┐  │
│  │ rmcp     │   │  servo-bridge                │  │
│  │ MCP      │◄─►│  - dedicated servo thread    │  │
│  │ server   │   │    (Servo/WebView are !Send)  │  │
│  │ (stdio)  │   │  - WebViewDelegate impl      │  │
│  └──────────┘   │    (a11y TreeUpdate, console)│  │
│                 └──────────────┬───────────────┘  │
└────────────────────────────────┼────────────────┘
                                 │ git dependency (pinned rev)
                                 ▼
                    servo/servo — components/servo (libservo)
```

Servo's `Servo`/`WebView` are `!Send` (Rc-based), so the engine lives on one
dedicated OS thread. Tool handlers send commands over an mpsc channel and await
oneshot replies. The headless rendering path is `SoftwareRenderingContext` —
no GPU, no window.

Design notes:

- **WebView API, not WebDriver.** Servo ships a WebDriver server
  (`ports/servoshell/webdriver.rs`), but it is a thin protocol subset with no
  accessibility tree. Kept as a possible fallback transport only.
- **Console messages need no upstream patch.** `WebViewDelegate::show_console_message`
  is already public (default no-op) — this repo just implements it. (An earlier
  plan draft assumed a patch would be needed; reading the source proved otherwise.)
- **License.** Servo is MPL-2.0 (file-level copyleft). Depending on it as a
  library is fine; this crate's own files stay MIT. No Servo code is vendored.

## License

MIT — see [LICENSE](LICENSE).
