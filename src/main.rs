//! servo-devtools-mcp — MCP server exposing Servo browser-engine control.
//!
//! "chrome-devtools-mcp for Servo". stdio transport; point any MCP client
//! (Claude Code, Cursor, VS Code) at this binary.

mod bridge;

use std::sync::Arc;

use rmcp::{
    handler::server::wrapper::Parameters, model::*, schemars, tool, tool_handler,
    tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use tokio::sync::Mutex;

use bridge::ServoBridge;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
struct NavigateParams {
    /// URL to load (http/https/data:).
    url: String,
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
