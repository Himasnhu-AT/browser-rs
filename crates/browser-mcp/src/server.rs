//! The MCP server implementation over the shared engine.
//!
//! [`BrowserMcpServer`] is an `rmcp` [`ServerHandler`] that holds a
//! [`ToolInvoker`] and does nothing else: it lists the shared catalogue,
//! forwards a call to the invoker, and translates the answer. Every decision an
//! agent can observe — what the tools are called, what their fields mean, how
//! much text comes back — is made in [`browser_server::mcp`], so the stdio
//! transport and the `/mcp` HTTP transport cannot drift apart.

use std::sync::Arc;

use rmcp::model::{
    CallToolRequestParams, CallToolResult, Implementation, ListToolsResult, PaginatedRequestParams,
    ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{RequestContext, RoleServer, RunningService};
use rmcp::{ErrorData, ServerHandler, ServiceExt};
use serde_json::Value;

use browser_server::mcp::{ToolInvoker, SERVER_INSTRUCTIONS, SERVER_NAME};

use crate::error::McpServerError;
use crate::tools::{call_tool_error, call_tool_result, rmcp_tools, to_rmcp_tool};

/// The version reported to clients.
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// An MCP server over the `browser-rs` engine.
///
/// Cloning is cheap and shares the invoker; `rmcp` needs the handler by value.
#[derive(Clone)]
pub struct BrowserMcpServer {
    invoker: Arc<dyn ToolInvoker>,
}

impl std::fmt::Debug for BrowserMcpServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `dyn ToolInvoker` is not `Debug`, and the useful fact about a server
        // is its tool set, not the address of its invoker.
        f.debug_struct("BrowserMcpServer")
            .field("tools", &browser_server::mcp::TOOL_NAMES)
            .finish()
    }
}

impl BrowserMcpServer {
    /// Wrap a tool invoker.
    #[must_use]
    pub fn new(invoker: Arc<dyn ToolInvoker>) -> Self {
        Self { invoker }
    }

    /// Serve this handler over the process's stdin and stdout.
    ///
    /// This is the transport a desktop agent runtime uses: it spawns `brs-mcp`
    /// as a child process and speaks newline-delimited JSON-RPC over the pipe.
    /// Nothing else may write to stdout for the lifetime of the server — a
    /// stray `println!` corrupts the frame stream — which is why `brs-mcp`
    /// sends its logs to stderr.
    ///
    /// The returned handle resolves when the client disconnects.
    ///
    /// # Errors
    ///
    /// [`McpServerError::Transport`] if the initialize handshake fails.
    pub async fn serve_stdio(self) -> Result<RunningService<RoleServer, Self>, McpServerError> {
        self.serve(rmcp::transport::stdio())
            .await
            .map_err(McpServerError::transport)
    }
}

impl ServerHandler for BrowserMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::default())
            .with_server_info(Implementation::new(SERVER_NAME, SERVER_VERSION))
            // The one place to explain the *system* rather than a single tool:
            // the cost order between `fetch` and `browse`, and what a session
            // buys. Clients show it to the model before any tool call.
            .with_instructions(SERVER_INSTRUCTIONS)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        // Eight tools; paginating them would only give clients a cursor to get
        // wrong. `next_cursor: None` says the list is complete.
        Ok(ListToolsResult {
            tools: rmcp_tools(),
            next_cursor: None,
            meta: None,
        })
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        browser_server::mcp::tool_by_name(name).map(|tool| to_rmcp_tool(&tool))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, ErrorData> {
        let name = request.name.to_string();
        let arguments = request.arguments.map_or(Value::Null, Value::Object);

        tracing::debug!(tool = %name, "tools/call");
        match self.invoker.invoke(&name, arguments).await {
            Ok(output) => Ok(call_tool_result(&output)),
            Err(error) => {
                tracing::info!(tool = %name, error = %error.message, "tool failed");
                call_tool_error(&error)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use browser_server::mcp::{async_trait, ToolError, ToolOutput};

    struct Stub;

    #[async_trait]
    impl ToolInvoker for Stub {
        async fn invoke(&self, name: &str, _arguments: Value) -> Result<ToolOutput, ToolError> {
            Ok(ToolOutput::text(format!("ran {name}")))
        }
    }

    #[test]
    fn get_info_advertises_tools_and_the_instructions() {
        let server = BrowserMcpServer::new(Arc::new(Stub));
        let info = server.get_info();
        assert!(info.capabilities.tools.is_some());
        assert_eq!(info.server_info.name, SERVER_NAME);
        assert_eq!(info.server_info.version, SERVER_VERSION);
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.contains("session_open"));
        assert!(instructions.contains("`fetch`"));
    }

    #[test]
    fn get_tool_resolves_the_catalogue_and_nothing_else() {
        let server = BrowserMcpServer::new(Arc::new(Stub));
        for name in browser_server::mcp::TOOL_NAMES {
            assert_eq!(server.get_tool(name).expect("known tool").name, name);
        }
        assert!(server.get_tool("navigate").is_none());
    }

    #[test]
    fn debug_names_the_tools_rather_than_the_invoker() {
        let rendered = format!("{:?}", BrowserMcpServer::new(Arc::new(Stub)));
        assert!(rendered.contains("fetch"));
        assert!(rendered.contains("session_close"));
    }
}
