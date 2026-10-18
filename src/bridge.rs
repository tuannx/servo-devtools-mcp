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

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use anyhow::Context;
use dpi::PhysicalSize;
use image::ImageFormat;
use servo::{
    accesskit, ConsoleLogLevel, LoadStatus, Preferences, RenderingContext, Servo, ServoBuilder,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
};
use url::Url;

/// How long a single tool call waits for the engine before giving up.
const TOOL_TIMEOUT: Duration = Duration::from_secs(45);

/// Headless viewport for the P0 spike.
const VIEWPORT: PhysicalSize<u32> = PhysicalSize {
    width: 1280,
    height: 800,
};

/// Delegate that records what the engine tells the embedder.
#[derive(Default)]
struct McpWebViewDelegate {
    tree_updates: RefCell<Vec<accesskit::TreeUpdate>>,
    console: RefCell<Vec<(String, String)>>,
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
}

struct ServoEngine {
    servo: Servo,
    rendering_context: Rc<SoftwareRenderingContext>,
    webview: Option<WebView>,
    delegate: Rc<McpWebViewDelegate>,
    rx: Receiver<BridgeCommand>,
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
                    if !self.pump_until(
                        || webview.load_status() == LoadStatus::Complete,
                        TOOL_TIMEOUT,
                    ) {
                        return Err("navigation timed out waiting for LoadStatus::Complete".into());
                    }
                    Ok(webview.url().map(|u| u.to_string()).unwrap_or_default())
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::Screenshot { reply } => {
                let Some(webview) = self.webview.as_ref() else {
                    let _ = reply.send(Err("no page loaded yet; call navigate first".into()));
                    return;
                };
                let (tx, rx) = mpsc::channel();
                webview.take_screenshot(None, move |result| {
                    let _ = tx.send(result.map_err(|e| format!("screenshot failed: {e:?}")));
                });
                let mut outcome: Result<Vec<u8>, String> =
                    Err("screenshot callback never fired".into());
                let start = Instant::now();
                while start.elapsed() < TOOL_TIMEOUT {
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
                let _ = reply.send(outcome);
            }
            BridgeCommand::A11ySnapshot { reply } => {
                let Some(webview) = self.webview.as_ref() else {
                    let _ = reply.send(Err("no page loaded yet; call navigate first".into()));
                    return;
                };
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
                let result = if updates.is_empty() {
                    Err("timed out waiting for accessibility tree update".into())
                } else {
                    Ok(format!("[{}]", updates.join(",")))
                };
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
        }
    }

    fn run(mut self) {
        loop {
            while let Ok(cmd) = self.rx.try_recv() {
                self.handle(cmd);
            }
            self.servo.spin_event_loop();
            std::thread::sleep(Duration::from_millis(2));
        }
    }
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
}
