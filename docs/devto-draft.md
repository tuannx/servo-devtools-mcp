# DRAFT — do NOT post without owner approval

# I built the missing MCP server for Servo (chrome-devtools-mcp, but for a Rust browser engine)

Every AI browser agent today drives Chromium through CDP — `chrome-devtools-mcp`,
`playwright-mcp`, `browser-use`, all of them. Servo, the Rust browser engine,
has no CDP at all. Its WebDriver is a thin testing subset with no accessibility
tree. So there was simply no way for an agent to *see* a Servo page.

**servo-devtools-mcp** closes that gap: an MCP server that talks to the Servo
engine directly through its public WebView API, exposing 17 tools over stdio to
any MCP client (Claude Code, Cursor, VS Code).

## What it does

- **Navigation**: `navigate`, `reload`, `go_back`, `go_forward`, `get_state`
- **Input**: `click`, `type_text`, `key_press`, `scroll`
- **Observation**: `screenshot` (real PNGs), `a11y_snapshot` (AccessKit tree —
  the agent's DOM), `dom_snapshot`, `evaluate_js`
- **Debugging**: `console_messages`, `network_requests`, `cookies` get/set,
  `resize_viewport`

The interesting engineering bits:

1. **Servo's `Servo`/`WebView` are `!Send`** (Rc-based), so the engine lives on
   a dedicated OS thread and MCP handlers talk to it over an mpsc channel —
   the same pattern as Servo's own headless test harness.
2. **Screenshots stalled for a full debugging session.** Root cause: the engine
   loop never called `WebView::paint()`, so the screenshot state machine never
   advanced. The embedder must drive repaints, exactly like servoshell does.
3. **No upstream patches needed.** Console messages come from the public
   `WebViewDelegate::show_console_message` callback; cookies from the public
   `SiteDataManager` API; network requests from the public
   `load_web_resource` delegate hook. One honest gap: Servo doesn't expose
   *response* status/headers to embedders, so `network_requests` is a
   request log — and `performance_trace` doesn't exist because Servo has no
   tracing API at all.

## Status

P0/P1/P2 verified end-to-end on a headless Linux VM: engine boots, renders
frames, a11y tree flows, form interaction works (click → type → onclick),
cookies round-trip between JS and the tool. CI is green.

One structural note: this can never be on crates.io while the `servo` crate
itself isn't published there (crates.io forbids git dependencies). Install via
`cargo install --git`.

Repo: https://github.com/tuannx/servo-devtools-mcp — MIT. Independent project,
not affiliated with the Servo project.
