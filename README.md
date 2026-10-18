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

## Status: P1 (MVP)

| Milestone | State |
|---|---|
| P0 — prove a11y tree + JS eval + navigation from an independent crate | ✅ **PASS** (verified 2026-10-18, see evidence below) |
| P1 — 15 tools MVP | ✅ **PASS** (verified 2026-10-18, see evidence below) |
| P2 — debugging parity subset | planned |
| P3 — crates.io publish + dev.to write-up | planned |

Full plan: [`servo-devtools-mcp-plan-2026-10-17.md`](../) (in the author's notes repo).

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

## MCP tools (P1 — 15 tools)

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

**Environment notes** (from the 2026-10-18 bring-up on a headless Linux VM):
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
