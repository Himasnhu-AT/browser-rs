//! The tool surface exposed to agents, and its JSON schemas.
//!
//! The schemas themselves are **not** written here. There are two transports —
//! this crate's stdio binary and `browser-server`'s `/mcp` HTTP mount — and
//! `API.md` §3 requires them to carry one tool set, so the catalogue lives in
//! [`browser_server::mcp`], which both can reach. This module is the thin
//! adapter that turns that SDK-agnostic description into `rmcp`'s types.
//!
//! Keeping the adapter this thin is the point: a tool described twice
//! eventually gets described differently, and an agent that cached the stdio
//! spelling would then call the HTTP one wrongly.

use std::borrow::Cow;
use std::sync::Arc;

use rmcp::model::{CallToolResult, ContentBlock, JsonObject, Tool, ToolAnnotations};
use rmcp::ErrorData;

use browser_server::mcp::{ToolContent, ToolDescriptor, ToolError, ToolErrorKind, ToolOutput};

/// Convert one catalogue entry into an `rmcp` [`Tool`].
///
/// # Panics
///
/// Never in practice: every schema in the catalogue is a JSON object, and
/// `catalogue_schemas_are_objects` in `browser-server` fails first if one ever
/// is not.
#[must_use]
pub fn to_rmcp_tool(descriptor: &ToolDescriptor) -> Tool {
    let schema: JsonObject = match descriptor.input_schema.clone() {
        serde_json::Value::Object(map) => map,
        // A non-object schema would be a bug in the catalogue, not in a
        // request; degrade to the empty object rather than take the process
        // down mid-handshake.
        other => {
            tracing::error!(
                tool = descriptor.name,
                schema = %other,
                "input schema is not a JSON object; serving an empty one"
            );
            JsonObject::new()
        }
    };

    Tool::new(
        Cow::Borrowed(descriptor.name),
        Cow::Borrowed(descriptor.description),
        Arc::new(schema),
    )
    .with_annotations(
        ToolAnnotations::new()
            .read_only(descriptor.read_only)
            .idempotent(descriptor.idempotent)
            // Every tool here reaches out to somebody else's web server.
            .open_world(true),
    )
}

/// The whole catalogue as `rmcp` tools, in the catalogue's own order.
///
/// Agents cache the tool list, so the order is part of the contract; it is
/// asserted in `tests/mcp_protocol.rs`.
#[must_use]
pub fn rmcp_tools() -> Vec<Tool> {
    browser_server::mcp::tool_catalog()
        .iter()
        .map(to_rmcp_tool)
        .collect()
}

/// Convert one output block into `rmcp`'s content union.
#[must_use]
pub fn to_rmcp_content(content: &ToolContent) -> ContentBlock {
    match content {
        ToolContent::Text(text) => ContentBlock::text(text.clone()),
        ToolContent::Image { base64, mime_type } => ContentBlock::image(base64.clone(), *mime_type),
    }
}

/// Convert a successful tool run into a [`CallToolResult`].
#[must_use]
pub fn call_tool_result(output: &ToolOutput) -> CallToolResult {
    let content = output.content.iter().map(to_rmcp_content).collect();
    let mut result = CallToolResult::success(content);
    result.structured_content.clone_from(&output.structured);
    result
}

/// Convert a failed tool run into what MCP should carry back.
///
/// The split is the one `rmcp` documents on [`CallToolResult::error`] and
/// `API.md` §3 asks for: an unknown tool is unroutable and becomes a JSON-RPC
/// `-32601`, while a bad argument or a failed fetch becomes an `isError`
/// **result**. That matters because clients render a protocol error opaquely —
/// the model would be told "tool result missing due to internal error" and
/// never see "the URL you gave has no scheme", which is exactly the sentence it
/// needs in order to fix its next call.
pub fn call_tool_error(error: &ToolError) -> Result<CallToolResult, ErrorData> {
    match error.kind {
        ToolErrorKind::UnknownTool => Err(ErrorData::new(
            rmcp::model::ErrorCode::METHOD_NOT_FOUND,
            error.message.clone(),
            None,
        )),
        ToolErrorKind::InvalidArguments | ToolErrorKind::Execution => Ok(CallToolResult::error(
            vec![ContentBlock::text(error.message.clone())],
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use browser_server::mcp::TOOL_NAMES;

    #[test]
    fn the_catalogue_survives_the_crossing_intact() {
        let descriptors = browser_server::mcp::tool_catalog();
        let tools = rmcp_tools();
        assert_eq!(tools.len(), descriptors.len());

        for (tool, descriptor) in tools.iter().zip(&descriptors) {
            assert_eq!(tool.name, descriptor.name);
            assert_eq!(
                tool.description.as_deref(),
                Some(descriptor.description),
                "{} lost its description",
                descriptor.name
            );
            assert_eq!(
                tool.input_schema.get("type"),
                Some(&serde_json::json!("object"))
            );
            let annotations = tool.annotations.as_ref().expect("annotations");
            assert_eq!(annotations.read_only_hint, Some(descriptor.read_only));
            assert_eq!(annotations.idempotent_hint, Some(descriptor.idempotent));
            assert_eq!(annotations.open_world_hint, Some(true));
        }
    }

    #[test]
    fn tool_order_is_the_catalogue_order() {
        let names: Vec<String> = rmcp_tools()
            .iter()
            .map(|tool| tool.name.to_string())
            .collect();
        assert_eq!(names, TOOL_NAMES.map(str::to_owned).to_vec());
    }

    /// Descriptions cross the boundary whole. `rmcp` will happily serve a
    /// truncated one, and an agent choosing between `fetch` and `browse` reads
    /// the end of the description, not the start.
    #[test]
    fn descriptions_are_not_shortened_in_transit() {
        for tool in rmcp_tools() {
            let description = tool.description.as_deref().expect("description");
            assert!(description.len() > 80, "{} is a stub", tool.name);
            let schema = &tool.input_schema;
            let properties = schema
                .get("properties")
                .and_then(|value| value.as_object())
                .expect("properties");
            for (field, property) in properties {
                assert!(
                    property["description"].as_str().unwrap_or_default().len() > 20,
                    "{}.{field} lost its description",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn a_successful_output_becomes_a_non_error_result() {
        let output = ToolOutput::text("hello").with_structured(serde_json::json!({ "n": 1 }));
        let result = call_tool_result(&output);
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.content.len(), 1);
        assert_eq!(
            result.content[0].as_text().expect("text").text.as_str(),
            "hello"
        );
        assert_eq!(
            result.structured_content,
            Some(serde_json::json!({ "n": 1 }))
        );
    }

    #[test]
    fn an_image_block_keeps_its_mime_type() {
        let output = ToolOutput {
            content: vec![ToolContent::Image {
                base64: "AAAA".to_owned(),
                mime_type: "image/png",
            }],
            structured: None,
        };
        let result = call_tool_result(&output);
        let image = result.content[0].as_image().expect("image");
        assert_eq!(image.data, "AAAA");
        assert_eq!(image.mime_type, "image/png");
    }

    #[test]
    fn a_bad_argument_reaches_the_model_as_a_visible_error() {
        let result = call_tool_error(&ToolError::invalid("`url` must be absolute"))
            .expect("a result, not a protocol error");
        assert_eq!(result.is_error, Some(true));
        assert_eq!(
            result.content[0].as_text().expect("text").text.as_str(),
            "`url` must be absolute"
        );
    }

    #[test]
    fn a_failed_fetch_also_reaches_the_model() {
        let result = call_tool_error(&ToolError::execution("502 from upstream"))
            .expect("a result, not a protocol error");
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn an_unknown_tool_is_the_one_case_that_becomes_a_protocol_error() {
        let error =
            call_tool_error(&ToolError::unknown_tool("navigate")).expect_err("a protocol error");
        assert_eq!(error.code, rmcp::model::ErrorCode::METHOD_NOT_FOUND);
        assert!(error.message.contains("navigate"));
        assert!(error.message.contains("fetch"));
    }
}
