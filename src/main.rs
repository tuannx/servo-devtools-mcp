//! servo-devtools-mcp — MCP server exposing Servo browser-engine control.
//!
//! "chrome-devtools-mcp for Servo". stdio transport; point any MCP client
//! (Claude Code, Cursor, VS Code) at this binary.

mod bridge;

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_handler, tool_router,
    ErrorData as McpError, ServerHandler, ServiceExt,
};
use tokio::sync::Mutex;

use bridge::ServoBridge;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NavigateParams {
    /// URL to load (http/https/data:).
    url: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ClickParams {
    /// X coordinate in CSS pixels.
    x: f32,
    /// Y coordinate in CSS pixels.
    y: f32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct TypeTextParams {
    /// Text to type into the focused element.
    text: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct KeyPressParams {
    /// Key name: Enter, Tab, Escape, Backspace, Delete, ArrowLeft/Right/Up/Down,
    /// Home, End, PageUp, PageDown, F1-F12, or a single character.
    key: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ScrollParams {
    /// X coordinate in CSS pixels.
    x: f32,
    /// Y coordinate in CSS pixels.
    y: f32,
    /// Horizontal scroll delta in CSS pixels.
    dx: f32,
    /// Vertical scroll delta in CSS pixels.
    dy: f32,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct EvaluateJsParams {
    /// JavaScript to evaluate in the page.
    script: String,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct ResizeViewportParams {
    /// Viewport width in pixels.
    width: u32,
    /// Viewport height in pixels.
    height: u32,
}

#[derive(Clone)]
struct ServoDevtools {
    bridge: Arc<Mutex<Option<ServoBridge>>>,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

impl ServoDevtools {
    fn new() -> Self {
        Self {
            bridge: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    async fn engine(&self) -> Result<ServoBridge, McpError> {
        let mut guard = self.bridge.lock().await;
        if guard.is_none() {
            *guard = Some(ServoBridge::spawn().map_err(|e| {
                McpError::internal_error(format!("failed to start servo engine: {e}"), None)
            })?);
        }
        Ok(guard.clone().expect("engine just spawned"))
    }

    fn ok_text(text: String) -> Result<CallToolResult, McpError> {
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    fn err_text(message: String) -> McpError {
        McpError::internal_error(message, None)
    }
}

#[tool_router]
impl ServoDevtools {
    #[tool(
        name = "navigate",
        description = "Load a URL in the Servo engine and wait for the page to finish loading. Returns the final URL."
    )]
    async fn navigate(
        &self,
        params: Parameters<NavigateParams>,
    ) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.navigate(params.0.url).await {
            Ok(final_url) => Self::ok_text(format!("loaded: {final_url}")),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "screenshot",
        description = "Capture the current Servo viewport as a PNG image."
    )]
    async fn screenshot(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.screenshot().await {
            Ok(png) => {
                use base64::Engine as _;
                let b64 = base64::engine::general_purpose::STANDARD.encode(&png);
                Ok(CallToolResult::success(vec![ContentBlock::image(
                    b64,
                    "image/png",
                )]))
            }
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "a11y_snapshot",
        description = "Enable accessibility in the Servo engine and return the latest AccessKit accessibility tree as JSON. This is the agent's DOM: roles, names, and structure of the page."
    )]
    async fn a11y_snapshot(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.a11y_snapshot().await {
            Ok(json) => Self::ok_text(json),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "console_messages",
        description = "Return console messages logged by page content so far, as a JSON array of {level, message}."
    )]
    async fn console_messages(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.console_messages().await {
            Ok(json) => Self::ok_text(json),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "reload",
        description = "Reload the current page and wait for it to finish loading. Returns the final URL."
    )]
    async fn reload(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.reload().await {
            Ok(url) => Self::ok_text(format!("reloaded: {url}")),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "go_back",
        description = "Go back one step in the page history. Returns the final URL."
    )]
    async fn go_back(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.go_back().await {
            Ok(url) => Self::ok_text(format!("went back to: {url}")),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "go_forward",
        description = "Go forward one step in the page history. Returns the final URL."
    )]
    async fn go_forward(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.go_forward().await {
            Ok(url) => Self::ok_text(format!("went forward to: {url}")),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "get_state",
        description = "Return the current page state as JSON: {url, title, load_status}."
    )]
    async fn get_state(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.get_state().await {
            Ok(json) => Self::ok_text(json),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "click",
        description = "Click at CSS-pixel coordinates (x, y): mouse move, button down, button up."
    )]
    async fn click(&self, params: Parameters<ClickParams>) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.click(params.0.x, params.0.y).await {
            Ok(msg) => Self::ok_text(msg),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "type_text",
        description = "Type text into the focused element, character by character."
    )]
    async fn type_text(
        &self,
        params: Parameters<TypeTextParams>,
    ) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.type_text(params.0.text).await {
            Ok(msg) => Self::ok_text(msg),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "key_press",
        description = "Press a key: Enter, Tab, Escape, Backspace, Delete, ArrowLeft/Right/Up/Down, Home, End, PageUp, PageDown, F1-F12, or a single character."
    )]
    async fn key_press(
        &self,
        params: Parameters<KeyPressParams>,
    ) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.key_press(params.0.key).await {
            Ok(msg) => Self::ok_text(msg),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "scroll",
        description = "Scroll by (dx, dy) CSS pixels at point (x, y)."
    )]
    async fn scroll(&self, params: Parameters<ScrollParams>) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine
            .scroll(params.0.x, params.0.y, params.0.dx, params.0.dy)
            .await
        {
            Ok(msg) => Self::ok_text(msg),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "dom_snapshot",
        description = "Return the page's serialized DOM (document.documentElement.outerHTML)."
    )]
    async fn dom_snapshot(&self) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.dom_snapshot().await {
            Ok(html) => Self::ok_text(html),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "evaluate_js",
        description = "Evaluate JavaScript in the page and return the result as JSON."
    )]
    async fn evaluate_js(
        &self,
        params: Parameters<EvaluateJsParams>,
    ) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine.evaluate_js(params.0.script).await {
            Ok(json) => Self::ok_text(json),
            Err(e) => Err(Self::err_text(e)),
        }
    }

    #[tool(
        name = "resize_viewport",
        description = "Resize the viewport to width x height pixels."
    )]
    async fn resize_viewport(
        &self,
        params: Parameters<ResizeViewportParams>,
    ) -> Result<CallToolResult, McpError> {
        let engine = self.engine().await?;
        match engine
            .resize_viewport(params.0.width, params.0.height)
            .await
        {
            Ok(msg) => Self::ok_text(msg),
            Err(e) => Err(Self::err_text(e)),
        }
    }
}

#[tool_handler]
impl ServerHandler for ServoDevtools {}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let transport = (tokio::io::stdin(), tokio::io::stdout());
    let service = ServoDevtools::new();
    let server = service.serve(transport).await?;
    server.waiting().await?;
    Ok(())
}
