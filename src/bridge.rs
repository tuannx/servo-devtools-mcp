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
    accesskit, ConsoleLogLevel, LoadStatus, Preferences, Servo, ServoBuilder,
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
    fn notify_accessibility_tree_update(
        &self,
        _webview: WebView,
        update: accesskit::TreeUpdate,
    ) {
        self.tree_updates.borrow_mut().push(update);
    }

    fn show_console_message(
        &self,
        _webview: WebView,
        level: ConsoleLogLevel,
        message: String,
    ) {
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
    webview: WebView,
    delegate: Rc<McpWebViewDelegate>,
    rx: Receiver<BridgeCommand>,
}

impl ServoEngine {
    fn new(rx: Receiver<BridgeCommand>) -> anyhow::Result<Self> {
        let rendering_context = Rc::new(
            SoftwareRenderingContext::new(VIEWPORT).map_err(|e| {
                anyhow::anyhow!("SoftwareRenderingContext::new failed: {e:?}")
            })?,
        );
        rendering_context
            .make_current()
            .map_err(|e| anyhow::anyhow!("rendering context make_current failed: {e:?}"))?;

        // Proxy env vars break localhost-only test traffic; the servo test
        // harness clears them, so do we.
        let mut preferences = Preferences::default();
        preferences.network_http_proxy_uri = String::new();
        preferences.network_https_proxy_uri = String::new();

        let servo = ServoBuilder::default().preferences(preferences).build();
        let delegate = Rc::new(McpWebViewDelegate::default());
        let webview = WebViewBuilder::new(&servo, rendering_context)
            .delegate(delegate.clone())
            .build();

        Ok(Self {
            servo,
            webview,
            delegate,
            rx,
        })
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

    fn handle(&self, cmd: BridgeCommand) {
        match cmd {
            BridgeCommand::Navigate { url, reply } => {
                let result = (|| -> Result<String, String> {
                    let parsed = Url::parse(&url).map_err(|e| format!("bad url: {e}"))?;
                    self.webview.load(parsed);
                    if !self.pump_until(
                        || self.webview.load_status() == LoadStatus::Complete,
                        TOOL_TIMEOUT,
                    ) {
                        return Err("navigation timed out waiting for LoadStatus::Complete".into());
                    }
                    Ok(self
                        .webview
                        .url()
                        .map(|u| u.to_string())
                        .unwrap_or_default())
                })();
                let _ = reply.send(result);
            }
            BridgeCommand::Screenshot { reply } => {
                let (tx, rx) = mpsc::channel();
                self.webview.take_screenshot(None, move |result| {
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
                self.webview.set_accessibility_active(true);
                let got_update = self.pump_until(
                    || !self.delegate.tree_updates.borrow().is_empty(),
                    TOOL_TIMEOUT,
                );
                let result = if got_update {
                    let updates = self.delegate.tree_updates.borrow();
                    match updates.last() {
                        Some(update) => serde_json::to_string(update)
                            .map_err(|e| format!("tree serialize failed: {e}")),
                        None => Err("no tree update received".into()),
                    }
                } else {
                    Err("timed out waiting for accessibility tree update".into())
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
                        .map(|(level, text)| {
                            serde_json::json!({"level": level, "message": text})
                        })
                        .collect::<Vec<_>>(),
                )
                .map_err(|e| format!("console serialize failed: {e}"));
                let _ = reply.send(result);
            }
        }
    }

    fn run(self) {
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
        self.call(|reply| BridgeCommand::Navigate { url, reply }).await
    }

    /// Capture the current viewport as PNG bytes.
    pub async fn screenshot(&self) -> Result<Vec<u8>, String> {
        self.call(|reply| BridgeCommand::Screenshot { reply }).await
    }

    /// Enable accessibility and return the latest AccessKit `TreeUpdate` as JSON.
    pub async fn a11y_snapshot(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::A11ySnapshot { reply }).await
    }

    /// Return console messages captured so far as a JSON array.
    pub async fn console_messages(&self) -> Result<String, String> {
        self.call(|reply| BridgeCommand::ConsoleMessages { reply })
            .await
    }
}
