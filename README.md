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

## Status: P0 (spike)

| Milestone | State |
|---|---|
| P0 — prove a11y tree + screenshot + JS eval from an independent crate | 🚧 code written, API surface verified against pinned Servo source; **runtime gate not yet passed on this machine** (see below) |
| P1 — 12 tools MVP | planned |
| P2 — debugging parity subset | planned |
| P3 — crates.io publish + dev.to write-up | planned |

Full plan: [`servo-devtools-mcp-plan-2026-10-17.md`](../) (in the author's notes repo).

### P0 gate

Run the spike binary:

```sh
cargo run --example p0_spike
```

Pass = prints the accessibility tree root role + node count, writes
`p0-screenshot.png`, prints `document.title`, exits 0. Fail = names the step
and exits non-zero.

The spike mirrors Servo's own headless test harness
(`components/servo/tests/common/mod.rs` @ pinned rev): `SoftwareRenderingContext`
+ `ServoBuilder::default()` + `WebViewBuilder::new(&servo, rc)`, pumping
`spin_event_loop()`.

## MCP tools (P0)

| Tool | Description |
|---|---|
| `navigate` | Load a URL, wait for `LoadStatus::Complete`, return the final URL |
| `screenshot` | Capture the viewport as PNG (returned base64) |
| `a11y_snapshot` | Enable accessibility, return the latest AccessKit `TreeUpdate` as JSON — the agent's DOM |
| `console_messages` | Console messages logged by page content so far, as JSON |

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
