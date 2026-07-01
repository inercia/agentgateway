//! Native MCP guardrails rate-limit processor config (Adobe-only).
#![cfg(feature = "adobe")]

use std::sync::Arc;

use serde::Deserialize;

use crate::types::agent::{BackendTrafficPolicy, SimpleBackendReference};
use crate::*;

/// JSON-RPC envelope for a rate-limit denial.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum RejectionResponseAs {
	#[default]
	JsonRpcError,
	ToolResult,
}

impl RejectionResponseAs {
	pub fn from_proto(value: i32) -> Self {
		use protos::agent::backend_policy_spec::mcp_guardrails::rate_limit::rejection_override::ResponseAs;
		match ResponseAs::try_from(value).unwrap_or(ResponseAs::Unspecified) {
			ResponseAs::ToolResult => Self::ToolResult,
			ResponseAs::JsonRpcError | ResponseAs::Unspecified => Self::JsonRpcError,
		}
	}
}

#[apply(schema!)]
pub struct RateLimit {
	/// Rate-limit domain sent to the platform RLS service.
	pub domain: String,
	/// Platform-managed rate-limit backend target.
	#[serde(flatten)]
	pub target: Arc<SimpleBackendReference>,
	/// Policies used when connecting to the platform RLS backend.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	#[serde(deserialize_with = "crate::types::local::de_from_local_backend_policy")]
	#[cfg_attr(
		feature = "schema",
		schemars(with = "Option<crate::types::local::SimpleLocalBackendPolicies>")
	)]
	pub policies: Vec<BackendTrafficPolicy>,
	/// Rate-limit descriptors evaluated from request and MCP context.
	pub descriptors: Arc<RateLimitDescriptorSet>,
	/// Behavior when the peek call to the rate-limit service fails.
	#[serde(default)]
	pub failure_mode: super::FailureMode,
	/// Reshape JSON-RPC errors when this rate-limit processor rejects. CEL context:
	/// `mcpGuardrails.rateLimit.*`, plus request/mcp/jwt context.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub rejection_overrides: Vec<RateLimitRejectionOverride>,
}

#[apply(schema!)]
pub struct RateLimitDescriptorSet(pub Vec<RateLimitDescriptorEntry>);

#[apply(schema!)]
pub struct RateLimitDescriptorEntry {
	#[serde(deserialize_with = "de_rate_limit_descriptors")]
	#[cfg_attr(feature = "schema", schemars(with = "Vec<RateLimitDescriptorSerde>"))]
	pub entries: Arc<Vec<RateLimitDescriptor>>,
	#[serde(default)]
	#[serde(rename = "type", alias = "unit")]
	pub limit_type: crate::http::localratelimit::RateLimitType,
	#[serde(default)]
	pub limit_override: Option<Arc<cel::Expression>>,
	#[serde(default)]
	pub peek: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RateLimitDescriptor {
	pub key: String,
	#[serde(skip)]
	#[cfg_attr(feature = "schema", schemars(skip))]
	pub value: Arc<cel::Expression>,
}

#[derive(serde::Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct RateLimitDescriptorSerde {
	#[serde(alias = "name")]
	pub key: String,
	#[serde(alias = "expression")]
	pub value: String,
}

fn de_rate_limit_descriptors<'de: 'a, 'a, D>(
	deserializer: D,
) -> Result<Arc<Vec<RateLimitDescriptor>>, D::Error>
where
	D: serde::Deserializer<'de>,
{
	let raw = Vec::<RateLimitDescriptorSerde>::deserialize(deserializer)?;
	let parsed = raw
		.into_iter()
		.map(|i| {
			cel::Expression::new_strict(&i.value).map(|value| RateLimitDescriptor {
				key: i.key,
				value: Arc::new(value),
			})
		})
		.collect::<Result<Vec<_>, _>>()
		.map_err(|e| serde::de::Error::custom(e.to_string()))?;
	Ok(Arc::new(parsed))
}

#[apply(schema!)]
/// Reshapes a rate-limit denial before it reaches the MCP client. Ignored for
/// remote guardrail rejections.
pub struct RateLimitRejectionOverride {
	pub when: Arc<cel::Expression>,
	#[serde(default)]
	pub response_as: RejectionResponseAs,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub status: Option<u16>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub body: Option<RateLimitRejectionOverrideBody>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub headers: Vec<RateLimitRejectionOverrideHeader>,
}

#[apply(schema!)]
pub struct RateLimitRejectionOverrideBody {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub code: Option<i32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub message: Option<Arc<cel::Expression>>,
}

#[apply(schema!)]
pub struct RateLimitRejectionOverrideHeader {
	pub name: String,
	pub value: Arc<cel::Expression>,
}

pub(crate) fn eval_override_headers(
	exec: &cel::Executor<'_>,
	override_cfg: &RateLimitRejectionOverride,
) -> Vec<(String, String)> {
	override_cfg
		.headers
		.iter()
		.filter_map(|header| match exec.eval(header.value.as_ref()) {
			Ok(value) => value.as_string().ok().map(|v| (header.name.clone(), v)),
			Err(e) => {
				tracing::debug!(
					name = %header.name,
					error = %e,
					"mcpGuardrails rejectionOverride header failed"
				);
				None
			},
		})
		.collect()
}
