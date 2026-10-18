//! P0 spike — the single pass/fail gate for servo-devtools-mcp.
//!
//! In an *independent* crate (no Servo workspace), prove that we can:
//!   1. boot Servo headless via `SoftwareRenderingContext`,
//!   2. load a page and receive an AccessKit `TreeUpdate` through our
//!      `WebViewDelegate`,
//!   3. take a screenshot and save it as PNG,
//!   4. evaluate JavaScript and read the result.
//!
//! Pass: prints the a11y tree root role + node count, writes
//! `p0-screenshot.png`, prints `document.title`, exits 0.
//! Fail: prints which step failed and exits non-zero.
//!
//! Run: `cargo run --example p0_spike`
//! (First build compiles the whole Servo engine — expect 1–3h.)

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use image::ImageFormat;
use servo::{
    accesskit, LoadStatus, Preferences, RenderingContext, Servo, ServoBuilder,
    SoftwareRenderingContext, WebView, WebViewBuilder, WebViewDelegate,
};
use url::Url;

#[derive(Default)]
struct SpikeDelegate {
    tree_updates: RefCell<Vec<accesskit::TreeUpdate>>,
}

impl WebViewDelegate for SpikeDelegate {
    fn notify_accessibility_tree_update(
        &self,
        _webview: WebView,
        update: accesskit::TreeUpdate,
    ) {
        self.tree_updates.borrow_mut().push(update);
    }
}

fn fail(step: &str, detail: String) -> ! {
    eprintln!("P0 FAIL [{step}]: {detail}");
    std::process::exit(1);
}

fn main() {
    // Enable servo's internal debug logs (e.g. screenshot readiness).
    let _ = env_logger::try_init();
    // --- 1. boot headless -------------------------------------------------
    let rendering_context = Rc::new(
        SoftwareRenderingContext::new(PhysicalSize {
            width: 1280,
            height: 800,
        })
        .unwrap_or_else(|e| fail("boot", format!("SoftwareRenderingContext::new: {e:?}"))),
    );
    if rendering_context.make_current().is_err() {
        fail("boot", "rendering context make_current failed".into());
    }

    let mut preferences = Preferences::default();
    // Accessibility tree updates only flow when this pref is on; without
    // it `set_accessibility_active()` is a no-op (see webview.rs).
    preferences.accessibility_enabled = true;
    // This sandbox (and many CI nets) requires an egress proxy; feed it
    // through preferences like servo's headless test does.
    if let Ok(proxy) = std::env::var("https_proxy").or_else(|_| std::env::var("HTTPS_PROXY")) {
        preferences.network_https_proxy_uri = proxy;
    }
    if let Ok(proxy) = std::env::var("http_proxy").or_else(|_| std::env::var("HTTP_PROXY")) {
        preferences.network_http_proxy_uri = proxy;
    }
    let servo: Servo = ServoBuilder::default().preferences(preferences).build();

    let delegate = Rc::new(SpikeDelegate::default());
    let webview = WebViewBuilder::new(&servo, rendering_context)
        .delegate(delegate.clone())
        .url(Url::parse("data:text/html,<h1 id=t>Hello from Servo</h1>").unwrap())
        .build();
    println!("P0 [boot]: engine up, webview created");

    let pump_until = |done: &dyn Fn() -> bool, timeout: Duration| -> bool {
        let start = Instant::now();
        while !done() {
            if start.elapsed() > timeout {
                return false;
            }
            servo.spin_event_loop();
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    };

    // --- 2. a11y tree ------------------------------------------------------
    webview.set_accessibility_active(true);
    if !pump_until(
        &|| {
            webview.load_status() == LoadStatus::Complete
                && !delegate.tree_updates.borrow().is_empty()
        },
        Duration::from_secs(45),
    ) {
        fail(
            "a11y",
            "timed out waiting for load-complete + accessibility tree update".into(),
        );
    }
    let updates = delegate.tree_updates.borrow();
    let latest = updates.last().unwrap();
    let tree_json = serde_json::to_string(latest)
        .unwrap_or_else(|e| fail("a11y", format!("tree serialize failed: {e}")));
    println!(
        "P0 [a11y]: PASS — received {} tree update(s); latest tree JSON bytes: {}",
        updates.len(),
        tree_json.len()
    );
    // Print the root node's role as the human-readable proof.
    let v: serde_json::Value =
        serde_json::from_str(&tree_json).unwrap_or(serde_json::Value::Null);
    println!(
        "P0 [a11y]: tree root sample: {}",
        v.get("tree")
            .and_then(|t| t.get("root"))
            .map(|r| r.to_string())
            .unwrap_or_else(|| "(unparsed)".into())
    );

    // --- 3. javascript eval ------------------------------------------------
    println!("P0 [js-eval]: evaluating...");
    let (jtx, jrx) = mpsc::channel();
    webview.evaluate_javascript("document.title", move |result| {
        let _ = jtx.send(result.map(|v| format!("{v:?}")).map_err(|e| format!("{e:?}")));
    });
    let title = {
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(60) {
                fail("js-eval", "timed out waiting for js callback".into());
            }
            match jrx.try_recv() {
                Ok(r) => break r,
                Err(mpsc::TryRecvError::Empty) => {
                    servo.spin_event_loop();
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    fail("js-eval", "callback channel disconnected".into())
                }
            }
        }
    };
    match title {
        Ok(t) => println!("P0 [js-eval]: PASS — document.title = {t}"),
        Err(e) => fail("js-eval", e),
    }


    // --- 4. screenshot -----------------------------------------------------
    println!("P0 [screenshot]: requesting...");
    let (tx, rx) = mpsc::channel();
    webview.take_screenshot(None, move |result| {
        let _ = tx.send(result.map_err(|e| format!("{e:?}")));
    });
    let png: Vec<u8> = {
        let start = Instant::now();
        loop {
            if start.elapsed() > Duration::from_secs(170) {
                fail("screenshot", "timed out waiting for screenshot callback".into());
            }
            match rx.try_recv() {
                Ok(Ok(rgba)) => {
                    let mut cursor = std::io::Cursor::new(Vec::new());
                    if let Err(e) = rgba.write_to(&mut cursor, ImageFormat::Png) {
                        fail("screenshot", format!("png encode: {e}"));
                    }
                    break cursor.into_inner();
                }
                Ok(Err(e)) => fail("screenshot", e),
                Err(mpsc::TryRecvError::Empty) => {
                    servo.spin_event_loop();
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    fail("screenshot", "callback channel disconnected".into())
                }
            }
        }
    };
    std::fs::write("p0-screenshot.png", &png)
        .unwrap_or_else(|e| fail("screenshot", format!("write png: {e}")));
    println!("P0 [screenshot]: PASS — wrote p0-screenshot.png ({} bytes)", png.len());

    println!("P0 GATE: PASS");
}
