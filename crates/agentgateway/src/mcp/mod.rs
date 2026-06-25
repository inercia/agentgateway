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
	McpGuardrails(RequestId, crate::mcp::guardrails::Rejection),
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
}
