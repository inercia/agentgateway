pub(crate) mod auth;
pub(crate) mod guardrails;
#[cfg(feature = "adobe")]
mod federation_outbound;
mod handler;
mod mcp_apps;
mod mergestream;
pub(crate) mod multiplex_naming;
mod rbac;
#[cfg(feature = "adobe")]
pub(crate) mod rewrite;
#[cfg(not(feature = "adobe"))]
pub(crate) mod rewrite_stub;
#[cfg(not(feature = "adobe"))]
pub(crate) use rewrite_stub as rewrite;
mod router;
mod session;
mod sse;
mod streamablehttp;
mod upstream;
use std::fmt::{Display, Write};
use std::io;
use std::sync::Arc;
use std::time::Duration;

use axum_core::BoxError;
use prometheus_client::encoding::{EncodeLabelValue, LabelValueEncoder};
pub use rbac::{McpAuthorization, McpAuthorizationSet, ResourceId, ResourceType};
pub use rewrite::{
	CompiledServerRewrite, McpRewritePolicy, McpRewriteSet, ResourceNaming,
};
use rmcp::model::RequestId;
pub use router::App;
use serde::{Deserialize, Serialize};
use thiserror::Error;
#[cfg(feature = "adobe")]
pub(crate) use upstream::UpstreamError;

#[cfg(feature = "schema")]
use crate::JsonSchema;
use crate::http::SendDirectResponse;
use crate::proxy::ProxyError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
#[cfg_attr(feature = "schema", schemars(rename = "McpBackendFailureMode"))]
#[serde(rename_all = "camelCase")]
pub enum FailureMode {
	/// Fail the entire session if any target fails to initialize or any
	/// upstream fails during a fanout. This is the default and matches
	/// current behavior.
	#[default]
	FailClosed,
	/// Skip failed targets/upstreams and continue serving from healthy ones.
	/// If ALL targets fail, still return an error.
	FailOpen,
}

pub(crate) const DEFAULT_SESSION_IDLE_TTL: Duration = Duration::from_mins(30);

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;

#[derive(Error, Debug)]
pub enum Error {
	#[error("method not allowed; must be GET, POST, or DELETE")]
	MethodNotAllowed,
	#[error("client must accept both application/json and text/event-stream")]
	InvalidAccept,
	#[error("client must accept text/event-stream")]
	InvalidAcceptGet,
	#[error("client must send application/json")]
	InvalidContentType,
	#[error("fail to deserialize request body: {0}")]
	Deserialize(crate::http::Error),
	#[error("fail to create session: {0}")]
	StartSession(crate::http::Error),
	#[error("session not found")]
	UnknownSession,
	#[error("session header is required for non-initialize requests")]
	MissingSessionHeader,
	#[error("session ID is required")]
	SessionIdRequired,
	#[error("invalid session ID header")]
	InvalidSessionIdHeader,
	#[error("invalid MCP protocol version header")]
	InvalidProtocolVersion,
	#[error("failed to start stdio server: {0}")]
	Stdio(io::Error),
	#[error("upstream error: {}", .0.status())]
	UpstreamError(Box<SendDirectResponse>),
	#[error("send error: {}", .1)]
	SendError(Option<RequestId>, String),
	// Intentionally do NOT say its not authorized; we hide the existence of the tool
	#[error("Unknown {1}: {2}")]
	Authorization(RequestId, String, String),
	#[error("mcpGuardrails rejected")]
	McpGuardrails(RequestId, crate::mcp::guardrails::Denial),
	#[error("failed to process session_id query parameter")]
	InvalidSessionIdQuery,
	#[error("failed to establish get stream: {0}")]
	EstablishGetStream(String),
	#[error("failed to forward message to legacy SSE: {0}")]
	ForwardLegacySse(String),
	#[error("failed to create SSE url: {0}")]
	CreateSseUrl(String),
	#[error("failed to parse openapi: {0}")]
	OpenAPI(upstream::OpenAPIParseError),
	#[error("no backends configured")]
	NoBackends,
}

impl From<Error> for ProxyError {
	fn from(value: Error) -> Self {
		ProxyError::MCP(value)
	}
}
impl<T> From<Error> for Result<T, ProxyError> {
	fn from(val: Error) -> Self {
		Err(ProxyError::MCP(val))
	}
}

#[derive(Error, Debug)]
pub enum ClientError {
	#[error("http request failed with code: {}", .0.status())]
	Status(Box<crate::http::Response>),
	#[error("http request failed: {0}")]
	General(Arc<crate::http::Error>),
	#[error("http request failed: {0}")]
	Proxy(#[from] ProxyError),
}

impl ClientError {
	pub fn new(error: impl Into<BoxError>) -> Self {
		Self::General(Arc::new(crate::http::Error::new(error.into())))
	}
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum MCPOperation {
	Tool,
	Prompt,
	Resource,
	ResourceTemplates,
	Task,
}

impl EncodeLabelValue for MCPOperation {
	fn encode(&self, encoder: &mut LabelValueEncoder) -> Result<(), std::fmt::Error> {
		encoder.write_str(&self.to_string())
	}
}

impl Display for MCPOperation {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			MCPOperation::Tool => write!(f, "tool"),
			MCPOperation::Prompt => write!(f, "prompt"),
			MCPOperation::Resource => write!(f, "resource"),
			MCPOperation::ResourceTemplates => write!(f, "templates"),
			MCPOperation::Task => write!(f, "task"),
		}
	}
}

#[derive(Default, Serialize, Deserialize, Clone, Debug, PartialEq, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct MCPTool {
	/// The target handling the tool call after multiplexing resolution.
	pub target: String,
	/// The resolved tool name sent to the upstream target.
	pub name: String,
	/// The JSON arguments passed to the tool call.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub arguments: Option<serde_json::Map<String, serde_json::Value>>,
	/// The terminal tool result payload, if available.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub result: Option<serde_json::Value>,
	/// The terminal JSON-RPC error payload, if available.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<serde_json::Value>,
}

/// Adobe-only: categorical error info stamped on every MCP failure path and
/// exposed to access-log CEL as `mcp.error.*`.
#[cfg(feature = "adobe")]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, ::cel::DynamicType)]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct McpErrorInfo {
	/// Categorical error type: permission_denied | timeout | connection_error |
	/// upstream_error | upstream_tool_error | rate_limited.
	#[serde(rename = "type")]
	#[dynamic(rename = "type")]
	pub error_type: String,
	/// Sanitized error message (max 500 bytes, UTF-8-safe).
	#[serde(rename = "message")]
	#[dynamic(rename = "message")]
	pub error_message: String,
	/// JSON-RPC error code when the error originated from a JSON-RPC error response.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub code: Option<i32>,
}

#[derive(Default, Serialize, Deserialize, Clone, Debug, PartialEq, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(JsonSchema))]
pub struct MCPInfo {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub method_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub session_id: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tool: Option<MCPTool>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub prompt: Option<ResourceId>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub resource: Option<ResourceId>,
	/// Present for MCP task operations on the federated wire form.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub task: Option<ResourceId>,
	/// Outcome signal for increment gating (`mcp.isError` in CEL). Set on the
	/// response/increment path when the terminal MCP result is known.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub is_error: Option<bool>,
	/// Adobe-only: authoritative operation outcome, stamped on every MCP request
	/// after the operation has been observed. Mirrors the CSCS pattern where the
	/// proxy layer is the source of truth for success (not derived downstream).
	/// None until the MCP layer stamps an outcome.
	#[cfg(feature = "adobe")]
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub success: Option<bool>,
	/// Adobe-only: categorical error info for access-log CEL (`mcp.error.*`).
	/// Absent on success.
	#[cfg(feature = "adobe")]
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub error: Option<McpErrorInfo>,
	/// Adobe-only: in-memory carrier for the `mcp_upstream_errors_total` metric.
	/// Set by single-target dispatch on a classified upstream failure and read at
	/// the `log.rs` finalize site. `#[serde(skip)]` keeps it out of serde, the CEL
	/// `DynamicType` surface, and the JSON schema (the cel-derive macro treats
	/// `#[serde(skip)]` as `#[dynamic(skip)]`), so there is no observable output
	/// change — it is purely an internal metrics carrier.
	#[cfg(feature = "adobe")]
	#[serde(skip)]
	pub upstream_error: Option<String>,
	/// Adobe-only: in-memory carrier for the backend name label on MCP metrics.
	/// Set at App::serve entry and read at the `log.rs` finalize site.
	/// `#[serde(skip)]` keeps it out of serde, CEL `DynamicType`, and JSON schema.
	#[cfg(feature = "adobe")]
	#[serde(skip)]
	pub backend_name: Option<String>,
}

impl MCPInfo {
	pub fn is_empty(&self) -> bool {
		self.method_name.is_none()
			&& self.session_id.is_none()
			&& self.tool.is_none()
			&& self.prompt.is_none()
			&& self.resource.is_none()
			&& self.task.is_none()
			&& self.is_error.is_none()
	}

	pub fn resource_type(&self) -> Option<MCPOperation> {
		if self.tool.is_some() {
			Some(MCPOperation::Tool)
		} else if self.prompt.is_some() {
			Some(MCPOperation::Prompt)
		} else if self.resource.is_some() {
			Some(MCPOperation::Resource)
		} else {
			self.task.as_ref().map(|_| MCPOperation::Task)
		}
	}

	pub fn target_name(&self) -> Option<&str> {
		self
			.tool
			.as_ref()
			.map(|tool| tool.target.as_str())
			.or_else(|| self.prompt.as_ref().map(ResourceId::target))
			.or_else(|| self.resource.as_ref().map(ResourceId::target))
			.or_else(|| self.task.as_ref().map(ResourceId::target))
	}

	pub fn resource_name(&self) -> Option<&str> {
		self
			.tool
			.as_ref()
			.map(|tool| tool.name.as_str())
			.or_else(|| self.prompt.as_ref().map(ResourceId::name))
			.or_else(|| self.resource.as_ref().map(ResourceId::name))
			.or_else(|| self.task.as_ref().map(ResourceId::name))
	}

	pub fn set_tool(&mut self, target: String, name: String) {
		self.prompt = None;
		self.resource = None;
		self.task = None;
		match self.tool.as_mut() {
			Some(tool) => {
				tool.target = target;
				tool.name = name;
			},
			None => {
				self.tool = Some(MCPTool {
					target,
					name,
					..Default::default()
				});
			},
		}
	}

	pub fn set_prompt(&mut self, target: String, name: String) {
		self.tool = None;
		self.resource = None;
		self.task = None;
		self.prompt = Some(ResourceId::new(target, name));
	}

	pub fn set_resource(&mut self, target: String, name: String) {
		self.tool = None;
		self.prompt = None;
		self.task = None;
		self.resource = Some(ResourceId::new(target, name));
	}

	pub fn set_task(&mut self, target: String, task_id: String) {
		self.tool = None;
		self.prompt = None;
		self.resource = None;
		self.task = Some(ResourceId::new(target, task_id));
	}

	pub fn capture_call_arguments(
		&mut self,
		arguments: Option<serde_json::Map<String, serde_json::Value>>,
	) {
		let Some(tool) = self.tool.as_mut() else {
			return;
		};

		tool.arguments = arguments;
	}

	pub fn capture_call_result<T: serde::Serialize>(&mut self, result: &T) {
		if let Some(tool) = self.tool.as_mut() {
			tool.result = serde_json::to_value(result).ok();
		}
	}

	pub fn capture_call_error<T: serde::Serialize>(&mut self, error: &T) {
		if let Some(tool) = self.tool.as_mut() {
			tool.error = serde_json::to_value(error).ok();
		}
	}

	#[cfg(feature = "adobe")]
	pub fn set_upstream_error(&mut self, error_type: &str) {
		self.upstream_error = Some(error_type.to_string());
	}

	#[cfg(feature = "adobe")]
	pub fn set_backend_name(&mut self, name: &str) {
		self.backend_name = Some(name.to_string());
	}

	/// Stamp `success = true` and clear any prior error info.
	#[cfg(feature = "adobe")]
	pub fn stamp_success(&mut self) {
		self.success = Some(true);
		self.error = None;
	}

	/// Stamp `success = false` and populate categorical error info.
	#[cfg(feature = "adobe")]
	pub fn stamp_error(&mut self, error_type: &str, error_message: String, code: Option<i32>) {
		self.success = Some(false);
		self.error = Some(McpErrorInfo {
			error_type: error_type.to_string(),
			error_message,
			code,
		});
	}
}

/// Truncate `s` to at most 500 bytes, walking back to a valid UTF-8 boundary.
#[cfg(feature = "adobe")]
pub fn sanitize_error_message(s: &str) -> String {
	const MAX_BYTES: usize = 500;
	if s.len() <= MAX_BYTES {
		return s.to_string();
	}
	let mut end = MAX_BYTES;
	while end > 0 && !s.is_char_boundary(end) {
		end -= 1;
	}
	s[..end].to_string()
}

/// Classify an `UpstreamError` into the CEL-visible error_type categories.
/// Non-transport errors (RBAC, invalid request, etc.) are classified at a higher layer.
#[cfg(feature = "adobe")]
pub fn classify_upstream_error_for_cel(e: &UpstreamError) -> &'static str {
	use crate::proxy::ProxyError;
	match e {
		UpstreamError::Proxy(ProxyError::UpstreamCallTimeout)
		| UpstreamError::Proxy(ProxyError::RequestTimeout) => "timeout",
		UpstreamError::Http(ClientError::General(_))
		| UpstreamError::Http(ClientError::Proxy(_)) => "connection_error",
		UpstreamError::Http(ClientError::Status(r)) if r.status().as_u16() >= 500 => "upstream_error",
		UpstreamError::FanoutError { source, .. } => classify_upstream_error_for_cel(source),
		_ => "upstream_error",
	}
}

impl From<&ResourceType> for MCPInfo {
	fn from(value: &ResourceType) -> Self {
		match value {
			ResourceType::Tool(tool) => Self {
				tool: Some(MCPTool {
					target: tool.target().to_string(),
					name: tool.name().to_string(),
					..Default::default()
				}),
				..Default::default()
			},
			ResourceType::Prompt(prompt) => Self {
				prompt: Some(prompt.clone()),
				..Default::default()
			},
			ResourceType::Resource(resource) => Self {
				resource: Some(resource.clone()),
				..Default::default()
			},
			ResourceType::Task(task) => Self {
				task: Some(task.clone()),
				..Default::default()
			},
		}
	}
}

#[cfg(test)]
mod mcp_info_semantics_tests {
	use super::{MCPInfo, MCPOperation, ResourceId, ResourceType};

	#[test]
	fn mcpinfo_default_is_empty() {
		assert!(MCPInfo::default().is_empty());
	}

	#[test]
	fn mcpinfo_set_task_marks_non_empty_and_clears_others() {
		let mut m = MCPInfo::default();
		m.set_tool("x".into(), "tool1".into());
		m.set_task("a".into(), "t1".into());
		assert!(m.tool.is_none());
		assert!(m.prompt.is_none());
		assert!(m.resource.is_none());
		assert_eq!(m.task, Some(ResourceId::new("a".into(), "t1".into())));
		assert!(!m.is_empty());
	}

	#[test]
	fn mcpinfo_task_operation_target_and_name() {
		let mut m = MCPInfo::default();
		m.set_task("a".into(), "t1".into());
		assert_eq!(m.resource_type(), Some(MCPOperation::Task));
		assert_eq!(m.target_name(), Some("a"));
		assert_eq!(m.resource_name(), Some("t1"));
	}

	#[test]
	fn mcpinfo_from_resource_type_task() {
		let rid = ResourceId::new("a".into(), "t1".into());
		let rt = ResourceType::Task(rid.clone());
		let info = MCPInfo::from(&rt);
		assert_eq!(info.task, Some(rid));
		assert!(info.tool.is_none());
		assert!(info.prompt.is_none());
		assert!(info.resource.is_none());
		assert!(!info.is_empty());
	}

	#[test]
	fn mcpoperation_task_display_is_task() {
		assert_eq!(format!("{}", MCPOperation::Task), "task");
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn sanitize_error_message_empty() {
		assert_eq!(super::sanitize_error_message(""), "");
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn sanitize_error_message_short() {
		assert_eq!(super::sanitize_error_message("hello"), "hello");
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn sanitize_error_message_exactly_500_bytes() {
		let s: String = "a".repeat(500);
		let result = super::sanitize_error_message(&s);
		assert_eq!(result.len(), 500);
		assert_eq!(result, s);
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn sanitize_error_message_ascii_over_budget() {
		let s: String = "a".repeat(600);
		let result = super::sanitize_error_message(&s);
		assert_eq!(result.len(), 500);
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn sanitize_error_message_utf8_mid_rune_over_budget() {
		// Each '€' is 3 bytes (UTF-8 E2 82 AC).
		// 499 bytes of 'a' + '€' = 502 bytes. Truncating at 500 would split the 3-byte rune.
		let mut s = "a".repeat(499);
		s.push('€');
		assert_eq!(s.len(), 502);
		let result = super::sanitize_error_message(&s);
		// Must be a valid UTF-8 string truncated before the rune boundary.
		assert!(std::str::from_utf8(result.as_bytes()).is_ok());
		assert!(result.len() <= 500);
		// The euro sign must be dropped since it would split at byte 500.
		assert!(!result.contains('€'));
		assert_eq!(result, "a".repeat(499));
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn set_upstream_error_sets_field() {
		let mut m = MCPInfo::default();
		assert_eq!(m.upstream_error, None);
		m.set_upstream_error("http_5xx");
		assert_eq!(m.upstream_error.as_deref(), Some("http_5xx"));
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn set_backend_name_sets_field() {
		let mut m = MCPInfo::default();
		assert_eq!(m.backend_name, None);
		m.set_backend_name("mcp-federated");
		assert_eq!(m.backend_name.as_deref(), Some("mcp-federated"));
	}
}
