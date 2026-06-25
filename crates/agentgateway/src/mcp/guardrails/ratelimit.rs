use std::sync::Arc;

use bytes::Bytes;
use prost_wkt_types::Struct;
use rmcp::model::{ErrorData, ServerResult};
use serde::Deserialize;
use tracing::{trace, warn};

use crate::cel::{self, Executor};
use crate::http::ext_proc::GrpcReferenceChannel;
use crate::http::localratelimit::RateLimitType;
use crate::http::remoteratelimit::proto;
use crate::mcp::guardrails::wire::ext_mcp_client::ExtMcpClient;
use crate::mcp::guardrails::wire::authorization_error::Code as AuthCode;
use crate::mcp::guardrails::wire::{AuthorizationError, McpRequest, McpResponse, mcp_request_result};
use crate::mcp::guardrails::{FailureMode, Outcome, RateLimit, RateLimitDescriptor as ConfigDescriptor, client};
use crate::mcp::upstream::IncomingRequestContext;
use crate::proxy::httpproxy::PolicyClient;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescriptorLimitOverride {
	unit: String,
	#[serde(alias = "requests_per_unit")]
	requests_per_unit: u32,
}

pub(crate) async fn check_request<P>(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	body: Option<&Bytes>,
	req_ctx: &IncomingRequestContext,
	client: &PolicyClient,
) -> Outcome<P> {
	let mcp = build_mcp_info(method, body);
	let req = req_ctx.as_request();
	let Some(metadata_context) = build_metadata(rate_limit, &req, Some(&mcp), Phase::Request) else {
		return Outcome::Pass;
	};
	let request = McpRequest {
		service_names: backends.to_vec(),
		method: method.to_string(),
		metadata_context: Some(metadata_context),
		mcp_request: body.cloned(),
		headers: Vec::new(),
	};
	let mut grpc = build_client(rate_limit, client.clone());
	let resp = match grpc.check_request(tonic::Request::new(request)).await {
		Ok(resp) => resp.into_inner(),
		Err(status) => return on_grpc_error(rate_limit, method, backends, "checkRequest", status),
	};
	match resp.result {
		Some(mcp_request_result::Result::Pass(_)) => Outcome::Pass,
		Some(mcp_request_result::Result::Mutated(_)) => {
			warn!(
				method,
				?backends,
				"mcpGuardrails rateLimit: ignoring unexpected request mutation"
			);
			Outcome::Pass
		},
		Some(mcp_request_result::Result::Error(e)) => {
			on_inband_service_error(rate_limit, method, backends, e)
		},
		None => on_protocol_violation(rate_limit, method, backends, "missing result oneof"),
	}
}

pub(crate) async fn check_response(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	body: &Bytes,
	req_ctx: &IncomingRequestContext,
	request_mcp: Option<&crate::mcp::MCPInfo>,
	client: &PolicyClient,
) -> Outcome<ServerResult> {
	let Ok(result) = serde_json::from_slice::<ServerResult>(body) else {
		return Outcome::Pass;
	};
	let mut mcp = match request_mcp {
		Some(req_mcp) => req_mcp.clone(),
		None => build_mcp_info_from_result(method, &result),
	};
	apply_response_outcome(&mut mcp, method, &result);
	let req = req_ctx.as_request();
	let Some(metadata_context) = build_metadata(rate_limit, &req, Some(&mcp), Phase::Response) else {
		return Outcome::Pass;
	};
	let spawn_target = rate_limit.target.clone();
	let spawn_policies = Arc::new(rate_limit.policies.clone());
	let spawn_client = client.clone();
	let method = method.to_string();
	let backends = backends.to_vec();
	let mcp_response = body.clone();
	tokio::spawn(async move {
		let request = McpResponse {
			service_names: backends.clone(),
			method: method.clone(),
			metadata_context: Some(metadata_context),
			mcp_response,
		};
		let mut grpc = ExtMcpClient::new(GrpcReferenceChannel {
			target: spawn_target,
			policies: spawn_policies,
			client: spawn_client,
		});
		if let Err(e) = grpc.check_response(tonic::Request::new(request)).await {
			trace!(method, ?backends, error = %e, "mcpGuardrails rateLimit increment failed");
		}
	});
	Outcome::Pass
}

pub(crate) fn build_mcp_info(method: &str, body: Option<&Bytes>) -> crate::mcp::MCPInfo {
	let mut info = crate::mcp::MCPInfo {
		method_name: Some(method.to_string()),
		..Default::default()
	};
	match method {
		crate::mcp::guardrails::methods::TOOLS_CALL => {
			if let Some(value) = body.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok()) {
				let name = value
					.get("name")
					.and_then(|v| v.as_str())
					.unwrap_or_default()
					.to_string();
				let arguments = value.get("arguments").and_then(|v| v.as_object()).cloned();
				info.set_tool(String::new(), name);
				info.capture_call_arguments(arguments);
			}
		},
		crate::mcp::guardrails::methods::PROMPTS_GET => {
			if let Some(name) = body
				.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
				.and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
			{
				info.set_prompt(String::new(), name);
			}
		},
		crate::mcp::guardrails::methods::RESOURCES_READ => {
			if let Some(uri) = body
				.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
				.and_then(|v| v.get("uri").and_then(|n| n.as_str()).map(str::to_string))
			{
				info.set_resource(String::new(), uri);
			}
		},
		_ => {},
	}
	info
}

fn build_mcp_info_from_result(method: &str, result: &ServerResult) -> crate::mcp::MCPInfo {
	let mut info = crate::mcp::MCPInfo {
		method_name: Some(method.to_string()),
		..Default::default()
	};
	apply_response_outcome(&mut info, method, result);
	info
}

fn apply_response_outcome(mcp: &mut crate::mcp::MCPInfo, method: &str, result: &ServerResult) {
	if let ServerResult::CallToolResult(call) = result {
		if mcp.tool.is_none() {
			mcp.set_tool(String::new(), String::new());
		}
		mcp.capture_call_result(call);
		mcp.is_error = Some(call.is_error == Some(true));
	}
	if mcp.method_name.is_none() {
		mcp.method_name = Some(method.to_string());
	}
}

#[derive(Clone, Copy)]
enum Phase {
	Request,
	Response,
}

fn build_metadata(
	rate_limit: &RateLimit,
	req: &crate::http::Request,
	mcp: Option<&crate::mcp::MCPInfo>,
	phase: Phase,
) -> Option<Struct> {
	let exec = if let Some(mcp) = mcp {
		Executor::new_mcp_request(req, mcp)
	} else {
		Executor::new_request(req)
	};
	let mut descriptors = Vec::with_capacity(rate_limit.descriptors.0.len());
	for desc in rate_limit
		.descriptors
		.0
		.iter()
		.filter(|d| d.limit_type == RateLimitType::Requests)
		.filter(|d| match phase {
			Phase::Request => true,
			Phase::Response => d.peek,
		}) {
		let Some(entries) = eval_descriptor(&exec, &desc.entries) else {
			continue;
		};
		if entries.is_empty() {
			continue;
		}
		let limit_override = match eval_limit_override(&exec, desc.limit_override.as_deref()) {
			Ok(limit) => limit,
			Err(e) => {
				trace!(error = %e, "mcpGuardrails rateLimit limitOverride failed; skipping descriptor");
				continue;
			},
		};
		let hits_addend = match phase {
			Phase::Request if desc.peek => 0,
			Phase::Request => 1,
			Phase::Response => 1,
		};
		if hits_addend > 0
			&& matches!(phase, Phase::Response)
			&& mcp.and_then(|m| m.is_error) == Some(true)
		{
			continue;
		}
		let mut descriptor = serde_json::json!({
			"entries": entries,
			"hitsAddend": hits_addend,
		});
		if let Some(limit_override) = limit_override
			&& let Some(object) = descriptor.as_object_mut()
		{
			object.insert("limit".to_string(), limit_override);
		}
		descriptors.push(descriptor);
	}
	if descriptors.is_empty() {
		return None;
	}
	let metadata = serde_json::json!({
		"domain": rate_limit.domain,
		"descriptors": descriptors,
	});
	match serde_json::from_value(metadata) {
		Ok(metadata) => Some(metadata),
		Err(e) => {
			warn!(error = %e, "mcpGuardrails rateLimit metadata conversion failed");
			None
		},
	}
}

fn eval_descriptor(
	exec: &Executor<'_>,
	entries: &[ConfigDescriptor],
) -> Option<Vec<serde_json::Value>> {
	let mut out = Vec::with_capacity(entries.len());
	for entry in entries {
		let value = match exec.eval(entry.value.as_ref()) {
			Ok(value) => match value.as_string() {
				Ok(value) => value,
				Err(e) => {
					trace!(key = %entry.key, error = %e, "mcpGuardrails rateLimit descriptor value was not a string");
					return None;
				},
			},
			Err(e) => {
				trace!(key = %entry.key, error = %e, "mcpGuardrails rateLimit descriptor expression failed");
				return None;
			},
		};
		out.push(serde_json::json!({
			"key": entry.key,
			"value": value,
		}));
	}
	Some(out)
}

fn eval_limit_override(
	exec: &Executor<'_>,
	limit_override: Option<&cel::Expression>,
) -> anyhow::Result<Option<serde_json::Value>> {
	let Some(expr) = limit_override else {
		return Ok(None);
	};
	let raw = exec
		.eval(expr)?
		.json()
		.map_err(|_| cel::Error::JsonConvert)?;
	let override_config: DescriptorLimitOverride = serde_json::from_value(raw)?;
	let unit = match override_config.unit.to_ascii_lowercase().as_str() {
		"second" => proto::RateLimitUnit::Second,
		"minute" => proto::RateLimitUnit::Minute,
		"hour" => proto::RateLimitUnit::Hour,
		"day" => proto::RateLimitUnit::Day,
		"month" => proto::RateLimitUnit::Month,
		"year" => proto::RateLimitUnit::Year,
		unit => anyhow::bail!("invalid limit override unit: {unit}"),
	};
	Ok(Some(serde_json::json!({
		"requestsPerUnit": override_config.requests_per_unit,
		"unit": unit.as_str_name(),
	})))
}


fn build_client(
	rate_limit: &RateLimit,
	client: PolicyClient,
) -> ExtMcpClient<GrpcReferenceChannel> {
	ExtMcpClient::new(GrpcReferenceChannel {
		target: rate_limit.target.clone(),
		policies: Arc::new(rate_limit.policies.clone()),
		client,
	})
}

fn on_inband_service_error<T>(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	e: AuthorizationError,
) -> Outcome<T> {
	let code = AuthCode::try_from(e.code).unwrap_or(AuthCode::Unknown);
	match code {
		AuthCode::ResourceExhausted | AuthCode::PermissionDenied => {
			Outcome::Reject(client::translate_error(method, backends, e).into())
		},
		AuthCode::Unknown | AuthCode::Invalid => match rate_limit.failure_mode {
			FailureMode::FailOpen => Outcome::Pass,
			FailureMode::FailClosed => {
				Outcome::Reject(client::translate_error(method, backends, e).into())
			},
		},
	}
}

fn on_grpc_error<T>(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	rpc: &str,
	status: tonic::Status,
) -> Outcome<T> {
	trace!(method, ?backends, rpc, code = ?status.code(), message = %status.message(), "mcpGuardrails rateLimit gRPC error");
	match rate_limit.failure_mode {
		FailureMode::FailOpen => Outcome::Pass,
		FailureMode::FailClosed => {
			Outcome::Reject(ErrorData::internal_error(
				format!("mcpGuardrails rateLimit {rpc} failed: {}", status.message()),
				None,
			).into())
		},
	}
}

fn on_protocol_violation<T>(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	reason: &str,
) -> Outcome<T> {
	warn!(
		method,
		?backends,
		reason,
		"mcpGuardrails rateLimit protocol violation"
	);
	match rate_limit.failure_mode {
		FailureMode::FailOpen => Outcome::Pass,
		FailureMode::FailClosed => {
			Outcome::Reject(ErrorData::internal_error(
				format!("mcpGuardrails rateLimit protocol violation: {reason}"),
				None,
			).into())
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::mcp::guardrails::{RateLimitDescriptorEntry, RateLimitDescriptorSet};
	use crate::types::agent::SimpleBackendReference;

	fn test_rate_limit() -> RateLimit {
		RateLimit {
			domain: "mcp".to_string(),
			target: Arc::new(SimpleBackendReference::Backend("unused".into())),
			policies: Vec::new(),
			failure_mode: FailureMode::FailClosed,
			descriptors: Arc::new(RateLimitDescriptorSet(vec![RateLimitDescriptorEntry {
				entries: Arc::new(vec![ConfigDescriptor {
					key: "method".to_string(),
					value: Arc::new(cel::Expression::new_strict("mcp.methodName").unwrap()),
				}]),
				limit_type: RateLimitType::Requests,
				limit_override: None,
				peek: true,
			}])),
			rejection_overrides: Vec::new(),
		}
	}

	#[test]
	fn response_skips_increment_on_is_error() {
		let req_ctx = IncomingRequestContext::empty();
		let req = req_ctx.as_request();

		// Successful call (is_error = Some(false)) → increment.
		let mcp_success = build_mcp_info_from_result(
			crate::mcp::guardrails::methods::TOOLS_CALL,
			&ServerResult::CallToolResult(rmcp::model::CallToolResult::success(vec![])),
		);
		assert_eq!(mcp_success.is_error, Some(false));
		let meta = build_metadata(&test_rate_limit(), &req, Some(&mcp_success), Phase::Response)
			.unwrap();
		let val = serde_json::to_value(meta).unwrap();
		assert_eq!(val["descriptors"].as_array().map(|a| a.len()), Some(1));

		// Errored call (is_error = Some(true)) → skip increment.
		let mut mcp_err = mcp_success.clone();
		mcp_err.is_error = Some(true);
		let meta = build_metadata(&test_rate_limit(), &req, Some(&mcp_err), Phase::Response);
		assert!(meta.is_none(), "increment should be skipped when is_error=true");

		// Non-tool method (is_error = None) → increment.
		let mcp_list = build_mcp_info("tools/list", None);
		assert_eq!(mcp_list.is_error, None);
		let meta =
			build_metadata(&test_rate_limit(), &req, Some(&mcp_list), Phase::Response).unwrap();
		let val = serde_json::to_value(meta).unwrap();
		assert_eq!(val["descriptors"].as_array().map(|a| a.len()), Some(1));
	}

	fn rate_limit_with_failure_mode(
		failure_mode: FailureMode,
	) -> RateLimit {
		RateLimit {
			failure_mode,
			..test_rate_limit()
		}
	}

	#[tokio::test]
	async fn fail_open_honors_inband_unknown_service_error() {
		use crate::test_helpers::extmcpmock::{closure_mock, pass_response, reject_request};
		use protos::ext_mcp::authorization_error::Code;

		let gtx = closure_mock(
			|_| reject_request(Code::Unknown, "gtx unavailable"),
			|_| pass_response(),
		)
		.spawn()
		.await;
		let client = PolicyClient::new(
			crate::test_helpers::proxymock::setup_proxy_test("{}")
				.unwrap()
				.pi,
		);
		let rate_limit = RateLimit {
			target: Arc::new(SimpleBackendReference::InlineBackend(
				crate::types::agent::Target::Address(gtx.address),
			)),
			..rate_limit_with_failure_mode(FailureMode::FailOpen)
		};
		let body = bytes::Bytes::from_static(br#"{"name":"echo"}"#);

		let outcome: Outcome<()> = check_request(
			&rate_limit,
			crate::mcp::guardrails::methods::TOOLS_CALL,
			&["mcp".to_string()],
			Some(&body),
			&IncomingRequestContext::empty(),
			&client,
		)
		.await;

		assert!(matches!(outcome, Outcome::Pass));
	}

	#[tokio::test]
	async fn fail_open_still_rejects_resource_exhausted() {
		use crate::test_helpers::extmcpmock::{closure_mock, pass_response, reject_request};
		use protos::ext_mcp::authorization_error::Code;

		let gtx = closure_mock(
			|_| reject_request(Code::ResourceExhausted, "quota exhausted"),
			|_| pass_response(),
		)
		.spawn()
		.await;
		let client = PolicyClient::new(
			crate::test_helpers::proxymock::setup_proxy_test("{}")
				.unwrap()
				.pi,
		);
		let rate_limit = RateLimit {
			target: Arc::new(SimpleBackendReference::InlineBackend(
				crate::types::agent::Target::Address(gtx.address),
			)),
			..rate_limit_with_failure_mode(FailureMode::FailOpen)
		};
		let body = bytes::Bytes::from_static(br#"{"name":"echo"}"#);

		let outcome: Outcome<()> = check_request(
			&rate_limit,
			crate::mcp::guardrails::methods::TOOLS_CALL,
			&["mcp".to_string()],
			Some(&body),
			&IncomingRequestContext::empty(),
			&client,
		)
		.await;

		assert!(matches!(outcome, Outcome::Reject(_)));
	}

	#[test]
	fn response_metadata_uses_request_mcp_tool_name() {
		let rate_limit = RateLimit {
			domain: "mcp".to_string(),
			target: Arc::new(SimpleBackendReference::Backend("unused".into())),
			policies: Vec::new(),
			failure_mode: FailureMode::FailClosed,
			descriptors: Arc::new(RateLimitDescriptorSet(vec![RateLimitDescriptorEntry {
				entries: Arc::new(vec![ConfigDescriptor {
					key: "tool".to_string(),
					value: Arc::new(cel::Expression::new_strict("mcp.tool.name").unwrap()),
				}]),
				limit_type: RateLimitType::Requests,
				limit_override: None,
				peek: true,
			}])),
			rejection_overrides: Vec::new(),
		};
		let req_ctx = IncomingRequestContext::empty();
		let req = req_ctx.as_request();
		let client_facing = build_mcp_info(
			crate::mcp::guardrails::methods::TOOLS_CALL,
			Some(&bytes::Bytes::from_static(br#"{"name":"echo_demo"}"#)),
		);
		let upstream_facing = build_mcp_info(
			crate::mcp::guardrails::methods::TOOLS_CALL,
			Some(&bytes::Bytes::from_static(br#"{"name":"echo"}"#)),
		);

		let client_meta =
			build_metadata(&rate_limit, &req, Some(&client_facing), Phase::Response).unwrap();
		let upstream_meta =
			build_metadata(&rate_limit, &req, Some(&upstream_facing), Phase::Response).unwrap();

		let client_val = serde_json::to_value(client_meta).unwrap();
		let upstream_val = serde_json::to_value(upstream_meta).unwrap();
		assert_eq!(
			client_val["descriptors"][0]["entries"][0]["value"],
			"echo_demo"
		);
		assert_eq!(
			upstream_val["descriptors"][0]["entries"][0]["value"],
			"echo"
		);
	}
}
