//! Bridge between the async MCP world and Servo's single-threaded engine.
//!
//! Servo's `Servo` / `WebView` types are `!Send` (Rc-based), so the engine
//! lives on a dedicated OS thread ("servo thread"). MCP tool handlers talk to
//! it through an mpsc command channel plus oneshot replies.
//!
//! Event-loop pattern follows servo's own headless test harness
//! (`components/servo/tests/common/mod.rs` @ pinned rev): a
//! `SoftwareRenderingContext`, `ServoBuilder::default()` (which already carries
//! a `DefaultEventLoopWaker`), and `spin_event_loop()` pumped in a loop.
//!
//! P1 note: Servo's screenshot pipeline only completes when the embedder
//! drives repaints (`WebView::paint()` -> `Painter::render()` runs
//! `maybe_take_screenshots`). servoshell does this from its window loop; our
//! P0 loop never painted, which is why screenshots stalled forever. The run
//! loop below paints at a throttled cadence, and the screenshot handler paints
//! every poll iteration.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Context;
use dpi::PhysicalSize;
use euclid::{Point2D, Vector2D};
use image::ImageFormat;
use servo::{
    accesskit, CSSPixel, ConsoleLogLevel, CookieSource, InputEvent, JSValue,
    JavaScriptEvaluationError, Key, KeyState, KeyboardEvent, LoadStatus, MouseButton,
    MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NamedKey, Preferences, RenderingContext,
    Scroll, Servo, ServoBuilder, SoftwareRenderingContext, WebResourceLoad, WebView,
    WebViewBuilder, WebViewDelegate, WebViewPoint, WebViewVector,
};
use url::Url;

/// How long a single tool call waits for the engine before giving up.
const TOOL_TIMEOUT: Duration = Duration::from_secs(45);

/// Headless viewport for the engine.
const VIEWPORT: PhysicalSize<u32> = PhysicalSize {
    width: 1280,
    height: 800,
};

/// How often the run loop repaints the webview when idle. Drives the
/// screenshot state machine and keeps hit-testing data fresh for clicks.
const PAINT_INTERVAL: Duration = Duration::from_millis(100);

/// Delegate that records what the engine tells the embedder.
#[derive(Default)]
struct McpWebViewDelegate {
    tree_updates: RefCell<Vec<accesskit::TreeUpdate>>,
    console: RefCell<Vec<(String, String)>>,
    /// Request log. Servo only exposes the *request* side to the embedder
    /// (`WebResourceLoad::request()`); response status/headers live behind the
    /// crate-private responder, so `network_requests` is a request log, not a
    /// full CDP-style Network log. No upstream patch needed.
    network: RefCell<Vec<String>>,
}

impl WebViewDelegate for McpWebViewDelegate {
    fn notify_accessibility_tree_update(&self, _webview: WebView, update: accesskit::TreeUpdate) {
        self.tree_updates.borrow_mut().push(update);
    }

    fn show_console_message(&self, _webview: WebView, level: ConsoleLogLevel, message: String) {
        self.console
            .borrow_mut()
            .push((format!("{level:?}"), message));
    }

    fn load_web_resource(&self, _webview: WebView, load: WebResourceLoad) {
        let req = load.request();
        let record = serde_json::json!({
            "url": req.url.to_string(),
            "method": req.method.to_string(),
            "destination": format!("{:?}", req.destination),
            "is_main_frame": req.is_for_main_frame,
            "is_redirect": req.is_redirect,
        });
        self.network.borrow_mut().push(record.to_string());
    }
}

enum BridgeCommand {
    Navigate {
        url: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Screenshot {
        reply: tokio::sync::oneshot::Sender<Result<Vec<u8>, String>>,
    },
    A11ySnapshot {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    ConsoleMessages {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Reload {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    GoBack {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    GoForward {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    GetState {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Click {
        x: f32,
        y: f32,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    TypeText {
        text: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    KeyPress {
        key: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Scroll {
        x: f32,
        y: f32,
        dx: f32,
        dy: f32,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    DomSnapshot {
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    EvaluateJs {
        script: String,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    ResizeViewport {
        width: u32,
        height: u32,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    NetworkRequests {
        clear: bool,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
    Cookies {
        action: String,
        url: Option<String>,
        name: Option<String>,
        value: Option<String>,
        reply: tokio::sync::oneshot::Sender<Result<String, String>>,
    },
}

struct ServoEngine {
    servo: Servo,
    rendering_context: Rc<SoftwareRenderingContext>,
    webview: Option<WebView>,
    delegate: Rc<McpWebViewDelegate>,
    rx: Receiver<BridgeCommand>,
    last_paint: Cell<Instant>,
}

impl ServoEngine {
    fn new(rx: Receiver<BridgeCommand>) -> anyhow::Result<Self> {
        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(VIEWPORT)
                .map_err(|e| anyhow::anyhow!("SoftwareRenderingContext::new failed: {e:?}"))?,
        );
        rendering_context
            .make_current()
            .map_err(|e| anyhow::anyhow!("rendering context make_current failed: {e:?}"))?;

        // Sandboxed/CI networks often require an egress proxy; feed it
        // through preferences like servo's headless test does.
        let mut preferences = Preferences::default();
        // Accessibility tree updates only flow when this pref is on; without
        // it `set_accessibility_active()` is a no-op (see webview.rs).
        preferences.accessibility_enabled = true;
        if let Ok(proxy) = std::env::var("https_proxy").or_else(|_| std::env::var("HTTPS_PROXY")) {
            preferences.network_https_proxy_uri = proxy;
        }
        if let Ok(proxy) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY")) {
            preferences.network_http_proxy_uri = proxy;
        }

        let servo = ServoBuilder::default().preferences(preferences).build();
        let delegate = Rc::new(McpWebViewDelegate::default());

        Ok(Self {
            servo,
            rendering_context,
            webview: None,
            delegate,
            rx,
            last_paint: Cell::new(Instant::now()),
        })
    }

    /// Get the live webview, creating it on first navigation. The first
    /// navigation goes through `WebViewBuilder::url()` because `WebView::load()`
    /// races with the async `NewWebView` constellation message on a fresh
    /// webview (the `LoadUrl` is dropped if it arrives first).
    fn ensure_webview(&mut self, first_url: Url) -> &WebView {
        if self.webview.is_none() {
            let webview = WebViewBuilder::new(&self.servo, self.rendering_context.clone())
                .delegate(self.delegate.clone())
                .url(first_url)
                .build();
            self.webview = Some(webview);
        }
        self.webview.as_ref().unwrap()
    }

    fn require_webview(&self) -> Result<&WebView, String> {
        self.webview
            .as_ref()
            .ok_or_else(|| "no page loaded yet; call navigate first".to_string())
    }

    fn pump_until(&self, mut done: impl FnMut() -> bool, timeout: Duration) -> bool {
        let start = Instant::now();
        while !done() {
            if start.elapsed() > timeout {
                return false;
            }
            self.servo.spin_event_loop();
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// Drive one compositor pass, like servoshell's `repaint_webviews`.
    /// Required for screenshots, and keeps hit-test data fresh for clicks.
    fn paint(&self) {
        if let Some(webview) = self.webview.as_ref() {
            webview.paint();
            self.last_paint.set(Instant::now());
        }
    }

    fn wait_for_load(&self, webview: &WebView, timeout: Duration) -> Result<(), String> {
        if !self.pump_until(|| webview.load_status() == LoadStatus::Complete, timeout) {
            return Err("timed out waiting for LoadStatus::Complete".into());
        }
        Ok(())
    }

    fn css_point(x: f32, y: f32) -> WebViewPoint {
        WebViewPoint::from(Point2D::<f32, CSSPixel>::new(x, y))
    }

    /// Evaluate JS and wait for the callback, pumping the event loop.
    fn eval_js(&self, webview: &WebView, script: String) -> Result<JSValue, String> {
        let (tx, rx) = mpsc::channel::<Result<JSValue, JavaScriptEvaluationError>>();
        webview.evaluate_javascript(script, move |result| {
            let _ = tx.send(result);
        });
        let start = Instant::now();
        loop {
            match rx.try_recv() {
                Ok(Ok(value)) => return Ok(value),
                Ok(Err(e)) => return Err(format!("javascript evaluation failed: {e:?}")),
                Err(mpsc::TryRecvError::Disconnected) => {
                    return Err("javascript evaluator channel disconnected".into())
                }
                Err(mpsc::TryRecvError::Empty) => {
                    if start.elapsed() > TOOL_TIMEOUT {
                        return Err("timed out waiting for javascript evaluation".into());
                    }
                    self.servo.spin_event_loop();
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    fn send_key(&self, webview: &WebView, key: Key) {
        webview.notify_input_event(InputEvent::Keyboard(KeyboardEvent::from_state_and_key(
            KeyState::Down,
            key.clone(),
        )));
        webview.notify_input_event(InputEvent::Keyboard(KeyboardEvent::from_state_and_key(
            KeyState::Up,
            key,
        )));
    }

    fn handle(&mut self, cmd: BridgeCommand) {
        match cmd {
            BridgeCommand::Navigate { url, reply } => {
                let result = (|| -> Result<String, String> {
                    let parsed = Url::parse(&url).map_err(|e| format!("bad url: {e}"))?;
                    if self.webview.is_none() {
                        // First navigation: go through the builder to avoid the
                        // NewWebView/LoadUrl race on a fresh webview.
                        self.ensure_webview(parsed.clone());
                    } else {
                        self.webview.as_ref().unwrap().load(parsed.clone());
                    }
                    let webview = self.webview.as_ref().unwrap();
                    // Wait for the URL to actually change (covers redirect
                    // chains), then for the load to complete.
                    if !self.pump_until(|| webview.url() == Some(parsed.clone()), TOOL_TIMEOUT) {
                        return Err("navigation timed out waiting for URL change".into());
                    }
                    self.wait_for_load(webview, TOOL_TIMEOUT)?;
                    // One paint so the page is composited before tools run.
                    self.paint();
                    Ok(webview.url().map(|u| u.to_string()).unwrap_or_default())
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::Screenshot { reply } => {
                let result = (|| -> Result<Vec<u8>, String> {
                    let webview = self.require_webview()?;
                    let (tx, rx) = mpsc::channel();
                    webview.take_screenshot(None, move |result| {
                        let _ = tx.send(result.map_err(|e| format!("screenshot failed: {e:?}")));
                    });
                    let mut outcome: Result<Vec<u8>, String> =
                        Err("screenshot callback never fired".into());
                    let start = Instant::now();
                    while start.elapsed() < TOOL_TIMEOUT {
                        // Drive the compositor: the screenshot state machine only
                        // advances when the embedder repaints.
                        self.paint();
                        match rx.try_recv() {
                            Ok(img_result) => {
                                outcome = img_result.and_then(|rgba| {
                                    let mut cursor = std::io::Cursor::new(Vec::new());
                                    rgba.write_to(&mut cursor, ImageFormat::Png)
                                        .map_err(|e| format!("png encode failed: {e}"))?;
                                    Ok(cursor.into_inner())
                                });
                                break;
                            }
                            Err(mpsc::TryRecvError::Empty) => {
                                self.servo.spin_event_loop();
                                std::thread::sleep(Duration::from_millis(2));
                            }
                            Err(mpsc::TryRecvError::Disconnected) => {
                                outcome = Err("screenshot channel disconnected".into());
                                break;
                            }
                        }
                    }
                    outcome
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::A11ySnapshot { reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    webview.set_accessibility_active(true);
                    // Servo sends the tree as a sequence of incremental updates
                    // (its own test waits for 3). Collect a few before returning.
                    self.pump_until(
                        || self.delegate.tree_updates.borrow().len() >= 3,
                        Duration::from_secs(10),
                    );
                    // Drain whatever arrived so the next call sees fresh updates.
                    let updates: Vec<String> = self
                        .delegate
                        .tree_updates
                        .borrow_mut()
                        .drain(..)
                        .map(|u| {
                            serde_json::to_string(&u)
                                .unwrap_or_else(|e| format!("{{\"serialize_error\":\"{e}\"}}"))
                        })
                        .collect();
                    if updates.is_empty() {
                        Err("timed out waiting for accessibility tree update".into())
                    } else {
                        Ok(format!("[{}]", updates.join(",")))
                    }
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::ConsoleMessages { reply } => {
                // Give the engine a beat to flush any pending console traffic.
                self.pump_until(|| false, Duration::from_millis(500));
                let messages = self.delegate.console.borrow();
                let result = serde_json::to_string(
                    &messages
                        .iter()
                        .map(|(level, text)| serde_json::json!({"level": level, "message": text}))
                        .collect::<Vec<_>>(),
                )
                .map_err(|e| format!("console serialize failed: {e}"));
                let _ = reply.send(result);
            }
            BridgeCommand::Reload { reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    webview.reload();
                    self.wait_for_load(webview, TOOL_TIMEOUT)?;
                    self.paint();
                    Ok(webview.url().map(|u| u.to_string()).unwrap_or_default())
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::GoBack { reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    webview.go_back(1);
                    self.wait_for_load(webview, TOOL_TIMEOUT)?;
                    self.paint();
                    Ok(webview.url().map(|u| u.to_string()).unwrap_or_default())
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::GoForward { reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    webview.go_forward(1);
                    self.wait_for_load(webview, TOOL_TIMEOUT)?;
                    self.paint();
                    Ok(webview.url().map(|u| u.to_string()).unwrap_or_default())
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::GetState { reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    let state = serde_json::json!({
                        "url": webview.url().map(|u| u.to_string()).unwrap_or_default(),
                        "title": webview.page_title().unwrap_or_default(),
                        "load_status": format!("{:?}", webview.load_status()),
                    });
                    serde_json::to_string(&state).map_err(|e| format!("state serialize: {e}"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::Click { x, y, reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    let point = Self::css_point(x, y);
                    // A real click: move, press, release.
                    webview.notify_input_event(InputEvent::MouseMove(MouseMoveEvent::new(point)));
                    webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                        MouseButtonAction::Down,
                        MouseButton::Primary,
                        point,
                    )));
                    webview.notify_input_event(InputEvent::MouseButton(MouseButtonEvent::new(
                        MouseButtonAction::Up,
                        MouseButton::Primary,
                        point,
                    )));
                    self.paint();
                    // Let the events dispatch through script.
                    self.pump_until(|| false, Duration::from_millis(300));
                    Ok(format!("clicked ({x}, {y})"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::TypeText { text, reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    for ch in text.chars() {
                        self.send_key(webview, Key::Character(ch.to_string()));
                    }
                    self.pump_until(|| false, Duration::from_millis(300));
                    Ok(format!("typed {} chars", text.chars().count()))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::KeyPress { key, reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    let key = parse_key(&key)?;
                    self.send_key(webview, key.clone());
                    self.pump_until(|| false, Duration::from_millis(200));
                    Ok(format!("pressed {key:?}"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::Scroll {
                x,
                y,
                dx,
                dy,
                reply,
            } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    let delta = WebViewVector::from(Vector2D::<f32, CSSPixel>::new(dx, dy));
                    webview.notify_scroll_event(Scroll::Delta(delta), Self::css_point(x, y));
                    self.paint();
                    self.pump_until(|| false, Duration::from_millis(300));
                    Ok(format!("scrolled by ({dx}, {dy}) at ({x}, {y})"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::DomSnapshot { reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    match self.eval_js(webview, "document.documentElement.outerHTML".into())? {
                        JSValue::String(html) => Ok(html),
                        other => Err(format!("unexpected JSValue: {other:?}")),
                    }
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::EvaluateJs { script, reply } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    let value = self.eval_js(webview, script)?;
                    serde_json::to_string(&value).map_err(|e| format!("jsvalue serialize: {e}"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::ResizeViewport {
                width,
                height,
                reply,
            } => {
                let result = (|| -> Result<String, String> {
                    let webview = self.require_webview()?;
                    webview.resize(PhysicalSize::new(width, height));
                    self.paint();
                    Ok(format!("viewport resized to {width}x{height}"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::NetworkRequests { clear, reply } => {
                let result = (|| -> Result<String, String> {
                    // Pump briefly so in-flight requests land in the log.
                    self.pump_until(|| false, Duration::from_millis(300));
                    let mut log = self.delegate.network.borrow_mut();
                    let entries: Vec<String> = if clear {
                        log.drain(..).collect()
                    } else {
                        log.clone()
                    };
                    let parsed: Vec<serde_json::Value> = entries
                        .iter()
                        .map(|e| serde_json::from_str(e).unwrap_or(serde_json::Value::Null))
                        .collect();
                    serde_json::to_string(&serde_json::json!({
                        "requests": parsed,
                        "count": parsed.len(),
                        "note": "request log only — Servo does not expose response status/headers to the embedder without an upstream patch",
                    }))
                    .map_err(|e| format!("network log serialize: {e}"))
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::Cookies {
                action,
                url,
                name,
                value,
                reply,
            } => {
                let result = (|| -> Result<String, String> {
                    let page_url = match url {
                        Some(u) => u,
                        None => self
                            .require_webview()?
                            .url()
                            .map(|u| u.to_string())
                            .unwrap_or_default(),
                    };
                    let parsed =
                        Url::parse(&page_url).map_err(|e| format!("bad url for cookies: {e}"))?;
                    let manager = self.servo.site_data_manager();
                    match action.as_str() {
                        "get" => {
                            let cookies = manager.cookies_for_url(parsed, CookieSource::HTTP);
                            serde_json::to_string(
                                &cookies
                                    .iter()
                                    .map(|c| {
                                        serde_json::json!({
                                            "name": c.name(),
                                            "value": c.value(),
                                            "domain": c.domain(),
                                            "path": c.path(),
                                        })
                                    })
                                    .collect::<Vec<_>>(),
                            )
                            .map_err(|e| format!("cookies serialize: {e}"))
                        }
                        "set" => {
                            let name = name.ok_or("set requires a cookie name")?;
                            let value = value.unwrap_or_default();
                            let cookie = cookie::Cookie::new(name.clone(), value).into_owned();
                            manager.set_cookie_for_url(parsed, cookie, None);
                            Ok(format!("set cookie {name}"))
                        }
                        other => Err(format!("unknown cookies action '{other}'; use get or set")),
                    }
                })();
                let _ = reply.send(result);
            }
        }
    }

    fn run(mut self) {
        loop {
            while let Ok(cmd) = self.rx.try_recv() {
                self.handle(cmd);
            }
            self.servo.spin_event_loop();
            // Drive the compositor at a throttled cadence (mirrors servoshell's
            // repaint loop). Without this, screenshots stall and hit-testing
            // data goes stale.
            if self.webview.is_some() && self.last_paint.get().elapsed() >= PAINT_INTERVAL {
                self.paint();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

/// Parse a key name into a [`Key`]. Accepts common special-key names
/// (Enter, Tab, Escape, Backspace, Delete, arrows, Home, End, PageUp,
/// PageDown, F1–F12) or a single character.
fn parse_key(name: &str) -> Result<Key, String> {
    let named = match name.to_lowercase().as_str() {
        "enter" => NamedKey::Enter,
        "tab" => NamedKey::Tab,
        "escape" | "esc" => NamedKey::Escape,
        "backspace" => NamedKey::Backspace,
        "delete" | "del" => NamedKey::Delete,
        "arrowleft" | "left" => NamedKey::ArrowLeft,
        "arrowright" | "right" => NamedKey::ArrowRight,
        "arrowup" | "up" => NamedKey::ArrowUp,
        "arrowdown" | "down" => NamedKey::ArrowDown,
        "home" => NamedKey::Home,
        "end" => NamedKey::End,
        "pageup" => NamedKey::PageUp,
        "pagedown" => NamedKey::PageDown,
        "f1" => NamedKey::F1,
        "f2" => NamedKey::F2,
        "f3" => NamedKey::F3,
        "f4" => NamedKey::F4,
        "f5" => NamedKey::F5,
        "f6" => NamedKey::F6,
        "f7" => NamedKey::F7,
        "f8" => NamedKey::F8,
        "f9" => NamedKey::F9,
        "f10" => NamedKey::F10,
        "f11" => NamedKey::F11,
        "f12" => NamedKey::F12,
        _ => {
            return if name.chars().count() == 1 {
                Ok(Key::Character(name.to_string()))
            } else {
                Err(format!("unknown key: {name}"))
            }
        }
    };
    Ok(Key::Named(named))
}

/// Async-safe handle to the Servo engine. Cheaply cloneable; share across tools.
#[derive(Clone)]
pub struct ServoBridge {
    tx: Sender<BridgeCommand>,
}

impl ServoBridge {
    /// Spawn the engine thread. Returns once the thread is launched (the engine
    /// itself initializes inside the thread).
    pub fn spawn() -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("servo-engine".into())
            .spawn(move || match ServoEngine::new(rx) {
                Ok(engine) => engine.run(),
                Err(e) => eprintln!("[servo-devtools-mcp] engine failed to start: {e:?}"),
            })
            .context("failed to spawn servo engine thread")?;
        Ok(Self { tx })
    }

    async fn call<T>(
        &self,
        send: impl FnOnce(tokio::sync::oneshot::Sender<Result<T, String>>) -> BridgeCommand,
    ) -> Result<T, String> {
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(send(reply_tx))
            .map_err(|_| "servo engine thread is gone".to_string())?;
        reply_rx
            .await
            .map_err(|_| "servo engine dropped the reply".to_string())?
    }

    /// Load `url` and wait for `LoadStatus::Complete`. Returns the final URL.
    pub async fn navigate(&self, url: String) -> Result<String, String> {
        self.call(|reply| BridgeCommand::Navigate { url, reply })
            .await
    }

    /// Capture the current viewport as PNG bytes.
    pub async fn screenshot(&self) -> Result<Vec<u8>, String> {
        self.call(|reply| BridgeCommand::Screenshot { reply }).await
    }

    /// Enable accessibility and return the latest AccessKit `TreeUpdate` as JSON.
    pub async fn a11y_snapshot(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::A11ySnapshot { reply })
            .await
    }

    /// Return console messages captured so far as a JSON array.
    pub async fn console_messages(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::ConsoleMessages { reply })
            .await
    }

    /// Reload the current page and wait for it to finish loading.
    pub async fn reload(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::Reload { reply }).await
    }

    /// Go back one step in history.
    pub async fn go_back(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::GoBack { reply }).await
    }

    /// Go forward one step in history.
    pub async fn go_forward(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::GoForward { reply }).await
    }

    /// Return `{url, title, load_status}` of the current page.
    pub async fn get_state(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::GetState { reply }).await
    }

    /// Click at CSS-pixel coordinates (x, y).
    pub async fn click(&self, x: f32, y: f32) -> Result<String, String> {
        self.call(|reply| BridgeCommand::Click { x, y, reply })
            .await
    }

    /// Type text into the focused element, character by character.
    pub async fn type_text(&self, text: String) -> Result<String, String> {
        self.call(|reply| BridgeCommand::TypeText { text, reply })
            .await
    }

    /// Press a key (Enter, Tab, Escape, arrows, F1–F12, or a single char).
    pub async fn key_press(&self, key: String) -> Result<String, String> {
        self.call(|reply| BridgeCommand::KeyPress { key, reply })
            .await
    }

    /// Scroll by (dx, dy) CSS pixels at point (x, y).
    pub async fn scroll(&self, x: f32, y: f32, dx: f32, dy: f32) -> Result<String, String> {
        self.call(|reply| BridgeCommand::Scroll {
            x,
            y,
            dx,
            dy,
            reply,
        })
        .await
    }

    /// Return the page's serialized DOM (`document.documentElement.outerHTML`).
    pub async fn dom_snapshot(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::DomSnapshot { reply })
            .await
    }

    /// Evaluate JavaScript and return the result as JSON.
    pub async fn evaluate_js(&self, script: String) -> Result<String, String> {
        self.call(|reply| BridgeCommand::EvaluateJs { script, reply })
            .await
    }

    /// Resize the viewport.
    pub async fn resize_viewport(&self, width: u32, height: u32) -> Result<String, String> {
        self.call(|reply| BridgeCommand::ResizeViewport {
            width,
            height,
            reply,
        })
        .await
    }

    /// Return the recorded network request log as JSON.
    /// Request-only: Servo does not expose response status/headers to the
    /// embedder without an upstream patch.
    pub async fn network_requests(&self, clear: bool) -> Result<String, String> {
        self.call(|reply| BridgeCommand::NetworkRequests { clear, reply })
            .await
    }

    /// Cookie store access via Servo's public SiteDataManager API.
    /// action "get" lists cookies for `url` (defaults to the current page URL);
    /// action "set" stores `name`=`value` for `url`.
    pub async fn cookies(
        &self,
        action: String,
        url: Option<String>,
        name: Option<String>,
        value: Option<String>,
    ) -> Result<String, String> {
        self.call(|reply| BridgeCommand::Cookies {
            action,
            url,
            name,
            value,
            reply,
        })
        .await
    }
}
