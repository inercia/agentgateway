#![cfg(feature = "adobe")]

use std::sync::{Arc, Mutex};

use bytes::Bytes;
use prost_wkt_types::Struct;
use rmcp::model::{ErrorData, ServerResult};
use serde::Deserialize;
use tracing::{trace, warn};

use crate::cel::{self, Executor};
use crate::http::localratelimit::RateLimitType;
use crate::http::remoteratelimit::proto;
use crate::mcp::guardrails::wire::ext_mcp_client::ExtMcpClient;
use crate::mcp::guardrails::wire::authorization_error::Code as AuthCode;
use crate::mcp::guardrails::wire::{AuthorizationError, McpRequest, McpRequestResult, McpResponse, mcp_request_result};
use crate::mcp::guardrails::ratelimit_pin::{self, RateLimitExtMcpPin};
use crate::mcp::guardrails::{FailureMode, McpGuardrailsDynamicMetadata, Outcome, RateLimit, RateLimitDescriptor as ConfigDescriptor, build_mcp_info, client, denial_from_error, merge_metadata_into_extensions};
use crate::mcp::upstream::IncomingRequestContext;
use crate::proxy::httpproxy::PolicyClient;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DescriptorLimitOverride {
	unit: String,
	#[serde(alias = "requests_per_unit")]
	requests_per_unit: u32,
}

/// Merge a parsed ExtMCP rate-limit status object as `mcpGuardrails.rateLimit`.
pub(crate) fn merge_rate_limit_status_into_extensions(
	method: &str,
	backends: &[String],
	status: serde_json::Value,
	ext: &mut ::http::Extensions,
) {
	if !status.is_object() {
		tracing::warn!(method, ?backends, "mcpGuardrails: rateLimit status must be a JSON object");
		return;
	}
	let mut acc = ext
		.remove::<McpGuardrailsDynamicMetadata>()
		.unwrap_or_default();
	acc.0.insert("rateLimit".to_string(), status);
	ext.insert(acc);
}

/// Parse ExtMCP `AuthorizationError.mcp_error` bytes and expose as `mcpGuardrails.rateLimit` before rejection.
pub(crate) fn merge_rate_limit_mcp_error_into_extensions(
	method: &str,
	backends: &[String],
	mcp_error: &[u8],
	ext: &mut ::http::Extensions,
) {
	match serde_json::from_slice::<serde_json::Value>(mcp_error) {
		Ok(status) => merge_rate_limit_status_into_extensions(method, backends, status, ext),
		Err(e) => {
			tracing::debug!(method, ?backends, error = %e, "mcpGuardrails: ignoring unparseable rateLimit mcp_error");
		},
	}
}

pub(crate) async fn check_request<P>(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	body: Option<&Bytes>,
	req_ctx: &mut IncomingRequestContext,
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
	let capture = Arc::new(Mutex::new(None));
	let mut grpc = ExtMcpClient::new(ratelimit_pin::channel_for(
		rate_limit,
		client.clone(),
		None,
		Some(capture.clone()),
	));
	let resp = match grpc.check_request(tonic::Request::new(request)).await {
		Ok(resp) => resp.into_inner(),
		Err(status) => return on_grpc_error(rate_limit, method, backends, "checkRequest", status),
	};
	let McpRequestResult {
		result,
		metadata,
		..
	} = resp;
	let store_pin = |req_ctx: &mut IncomingRequestContext| {
		if let Some(pin) = capture.lock().unwrap().take() {
			req_ctx.extensions_mut().insert(RateLimitExtMcpPin(pin));
		}
	};
	match result {
		Some(mcp_request_result::Result::Pass(_)) => {
			if let Some(m) = metadata {
				merge_metadata_into_extensions(method, backends, &m, req_ctx.extensions_mut());
			}
			store_pin(req_ctx);
			Outcome::Pass
		},
		Some(mcp_request_result::Result::Mutated(_)) => {
			warn!(
				method,
				?backends,
				"mcpGuardrails rateLimit: ignoring unexpected request mutation"
			);
			if let Some(m) = metadata {
				merge_metadata_into_extensions(method, backends, &m, req_ctx.extensions_mut());
			}
			store_pin(req_ctx);
			Outcome::Pass
		},
		Some(mcp_request_result::Result::Error(e)) => {
			on_inband_service_error(rate_limit, method, backends, e, req_ctx)
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
	let override_dest = req_ctx.extensions().get::<RateLimitExtMcpPin>().map(|p| p.0);
	let grpc_channel = ratelimit_pin::channel_for(
		rate_limit,
		client.clone(),
		override_dest,
		None,
	);
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
		let mut grpc = ExtMcpClient::new(grpc_channel);
		if let Err(e) = grpc.check_response(tonic::Request::new(request)).await {
			trace!(method, ?backends, error = %e, "mcpGuardrails rateLimit increment failed");
		}
	});
	Outcome::Pass
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

fn client_denial_error(method: &str, backends: &[String], e: AuthorizationError) -> ErrorData {
	super::strip_client_error_data(client::translate_error(method, backends, e))
}

fn on_inband_service_error<T>(
	rate_limit: &RateLimit,
	method: &str,
	backends: &[String],
	e: AuthorizationError,
	req_ctx: &mut IncomingRequestContext,
) -> Outcome<T> {
	if let Some(mcp_error) = e.mcp_error.as_deref().filter(|b| !b.is_empty()) {
		merge_rate_limit_mcp_error_into_extensions(
			method,
			backends,
			mcp_error,
			req_ctx.extensions_mut(),
		);
	}
	let code = AuthCode::try_from(e.code).unwrap_or(AuthCode::Unknown);
	match code {
		AuthCode::ResourceExhausted | AuthCode::PermissionDenied => {
			Outcome::Reject(denial_from_error(client_denial_error(method, backends, e)))
		},
		AuthCode::Unknown | AuthCode::Invalid => match rate_limit.failure_mode {
			FailureMode::FailOpen => Outcome::Pass,
			FailureMode::FailClosed => {
				Outcome::Reject(denial_from_error(client_denial_error(method, backends, e)))
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
			Outcome::Reject(denial_from_error(ErrorData::internal_error(
				format!("mcpGuardrails rateLimit {rpc} failed: {}", status.message()),
				None,
			)))
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
			Outcome::Reject(denial_from_error(ErrorData::internal_error(
				format!("mcpGuardrails rateLimit protocol violation: {reason}"),
				None,
			)))
		},
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Arc;

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

		let extmcp_mock = closure_mock(
			|_| reject_request(Code::Unknown, "rate limit service unavailable"),
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
				crate::types::agent::Target::Address(extmcp_mock.address),
			)),
			..rate_limit_with_failure_mode(FailureMode::FailOpen)
		};
		let body = bytes::Bytes::from_static(br#"{"name":"echo"}"#);
		let mut req_ctx = IncomingRequestContext::empty();

		let outcome: Outcome<()> = check_request(
			&rate_limit,
			crate::mcp::guardrails::methods::TOOLS_CALL,
			&["mcp".to_string()],
			Some(&body),
			&mut req_ctx,
			&client,
		)
		.await;

		assert!(matches!(outcome, Outcome::Pass));
	}

	#[tokio::test]
	async fn fail_open_still_rejects_resource_exhausted() {
		use crate::test_helpers::extmcpmock::{closure_mock, pass_response, reject_request};
		use protos::ext_mcp::authorization_error::Code;

		let extmcp_mock = closure_mock(
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
				crate::types::agent::Target::Address(extmcp_mock.address),
			)),
			..rate_limit_with_failure_mode(FailureMode::FailOpen)
		};
		let body = bytes::Bytes::from_static(br#"{"name":"echo"}"#);
		let mut req_ctx = IncomingRequestContext::empty();

		let outcome: Outcome<()> = check_request(
			&rate_limit,
			crate::mcp::guardrails::methods::TOOLS_CALL,
			&["mcp".to_string()],
			Some(&body),
			&mut req_ctx,
			&client,
		)
		.await;

		assert!(matches!(outcome, Outcome::Reject(_)));
	}

	#[tokio::test]
	async fn default_denial_strips_mcp_error_from_client_payload() {
		use protos::ext_mcp::authorization_error::Code;
		use protos::ext_mcp::{AuthorizationError, McpRequestResult, mcp_request_result};
		use rmcp::model::ErrorCode;

		use crate::mcp::guardrails::McpDenialEnvelope;
		use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

		let extmcp_mock = closure_mock(
			|_| {
				Ok(McpRequestResult {
					result: Some(mcp_request_result::Result::Error(AuthorizationError {
						code: Code::ResourceExhausted as i32,
						reason: "rate limit exceeded".to_string(),
						mcp_error: Some(
							serde_json::to_vec(&serde_json::json!({
								"domain": "mcp_api",
								"overallCode": "OVER_LIMIT",
							}))
							.unwrap()
							.into(),
						),
					})),
					header_mutation: None,
					metadata: None,
				})
			},
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
				crate::types::agent::Target::Address(extmcp_mock.address),
			)),
			..rate_limit_with_failure_mode(FailureMode::FailClosed)
		};
		let body = bytes::Bytes::from_static(br#"{"name":"echo"}"#);
		let mut req_ctx = IncomingRequestContext::empty();

		let outcome: Outcome<()> = check_request(
			&rate_limit,
			crate::mcp::guardrails::methods::TOOLS_CALL,
			&["mcp".to_string()],
			Some(&body),
			&mut req_ctx,
			&client,
		)
		.await;

		let Outcome::Reject(rej) = outcome else {
			panic!("expected reject, got {outcome:?}");
		};
		let McpDenialEnvelope::JsonRpc(error) = rej.envelope else {
			panic!("expected JsonRpc envelope");
		};
		assert_eq!(error.code, ErrorCode(-32003));
		assert_eq!(error.message.as_ref(), "rate limit exceeded");
		assert!(error.data.is_none(), "AuthorizationError.mcp_error must not be exposed in error.data");
		// CEL extensions still receive the merged payload.
		let meta = req_ctx
			.extensions()
			.get::<McpGuardrailsDynamicMetadata>()
			.expect("rateLimit metadata");
		assert!(meta.0.contains_key("rateLimit"));
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

	mod pinning {
		use std::collections::HashMap;
		use std::net::SocketAddr;
		use std::sync::atomic::{AtomicUsize, Ordering};
		use std::sync::{Arc, Mutex};

		use agent_core::strng;
		use rmcp::model::ServerResult;

		use crate::mcp::guardrails::FailureMode;
		use crate::store::LocalWorkload;
		use crate::test_helpers::extmcpmock::{self, pass_request, pass_response};
		use crate::types::agent::SimpleBackendReference;
		use crate::types::discovery::{NamespacedHostname, NetworkAddress, Service, Workload};

		use super::*;

		fn two_replica_extmcp_rate_limit_service(
			pi: &Arc<crate::ProxyInputs>,
			addr_a: SocketAddr,
			addr_b: SocketAddr,
		) -> SimpleBackendReference {
			let svc = Service {
				name: strng::literal!("ratelimit-extmcp"),
				namespace: strng::literal!("default"),
				hostname: strng::literal!("ratelimit-extmcp.default.svc.cluster.local"),
				vips: vec![NetworkAddress {
					network: strng::EMPTY,
					address: addr_a.ip(),
				}],
				ports: HashMap::from([(80, addr_a.port())]),
				..Default::default()
			};
			let wl_a = LocalWorkload {
				workload: Workload {
					uid: strng::literal!("wl-a"),
					name: strng::literal!("ratelimit-a"),
					namespace: strng::literal!("default"),
					workload_ips: vec![addr_a.ip()],
					..Default::default()
				},
				services: HashMap::from([(
					"default/ratelimit-extmcp.default.svc.cluster.local".to_string(),
					HashMap::from([(80, addr_a.port())]),
				)]),
			};
			let wl_b = LocalWorkload {
				workload: Workload {
					uid: strng::literal!("wl-b"),
					name: strng::literal!("ratelimit-b"),
					namespace: strng::literal!("default"),
					workload_ips: vec![addr_b.ip()],
					..Default::default()
				},
				services: HashMap::from([(
					"default/ratelimit-extmcp.default.svc.cluster.local".to_string(),
					HashMap::from([(80, addr_b.port())]),
				)]),
			};
			pi.stores
				.discovery
				.sync_local(vec![svc], vec![wl_a, wl_b], Default::default())
				.unwrap();
			SimpleBackendReference::Service {
				name: NamespacedHostname {
					namespace: strng::literal!("default"),
					hostname: strng::literal!("ratelimit-extmcp.default.svc.cluster.local"),
				},
				port: 80,
			}
		}

		fn recording_mock(
			replica: SocketAddr,
			hits: Arc<Mutex<Vec<SocketAddr>>>,
			rpc_count: Arc<AtomicUsize>,
		) -> extmcpmock::ExtMcpMock<extmcpmock::ClosureHandler> {
			let hits_req = hits.clone();
			let rpc_count_req = rpc_count.clone();
			extmcpmock::closure_mock(
				move |_| {
					hits_req.lock().unwrap().push(replica);
					rpc_count_req.fetch_add(1, Ordering::SeqCst);
					pass_request()
				},
				move |_| {
					hits.lock().unwrap().push(replica);
					rpc_count.fetch_add(1, Ordering::SeqCst);
					pass_response()
				},
			)
		}

		#[tokio::test]
		async fn peek_and_increment_pin_to_same_extmcp_replica() {
			let addr_a: SocketAddr = "127.0.0.1:37101".parse().unwrap();
			let addr_b: SocketAddr = "127.0.0.1:37102".parse().unwrap();
			let hits: Arc<Mutex<Vec<SocketAddr>>> = Arc::new(Mutex::new(Vec::new()));
			let rpc_count = Arc::new(AtomicUsize::new(0));

			let mock_a = recording_mock(addr_a, hits.clone(), rpc_count.clone());
			let mock_b = recording_mock(addr_b, hits.clone(), rpc_count.clone());
			let _inst_a = mock_a.spawn_on(addr_a).await;
			let _inst_b = mock_b.spawn_on(addr_b).await;

			let t = crate::test_helpers::proxymock::setup_proxy_test("{}").unwrap();
			let backend_ref = two_replica_extmcp_rate_limit_service(&t.pi, addr_a, addr_b);
			let client = PolicyClient::new(t.pi);
			let rate_limit = RateLimit {
				target: Arc::new(backend_ref),
				failure_mode: FailureMode::FailClosed,
				..test_rate_limit()
			};
			let req_body = bytes::Bytes::from_static(br#"{"name":"echo"}"#);
			let resp_body = bytes::Bytes::from(
				serde_json::to_vec(&ServerResult::CallToolResult(
					rmcp::model::CallToolResult::success(vec![]),
				))
				.unwrap(),
			);
			let mut req_ctx = IncomingRequestContext::empty();
			let request_mcp = build_mcp_info(
				crate::mcp::guardrails::methods::TOOLS_CALL,
				Some(&req_body),
			);

			let _: Outcome<()> = check_request(
				&rate_limit,
				crate::mcp::guardrails::methods::TOOLS_CALL,
				&["mcp".to_string()],
				Some(&req_body),
				&mut req_ctx,
				&client,
			)
			.await;

			check_response(
				&rate_limit,
				crate::mcp::guardrails::methods::TOOLS_CALL,
				&["mcp".to_string()],
				&resp_body,
				&req_ctx,
				Some(&request_mcp),
				&client,
			)
			.await;

			let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
			while rpc_count.load(Ordering::SeqCst) < 2 && tokio::time::Instant::now() < deadline {
				tokio::time::sleep(std::time::Duration::from_millis(10)).await;
			}

			let hits = hits.lock().unwrap();
			assert_eq!(
				rpc_count.load(Ordering::SeqCst),
				2,
				"expected peek + increment RPCs"
			);
			assert_eq!(hits.len(), 2, "expected two recorded ExtMCP backend addresses");
			assert_eq!(
				hits[0], hits[1],
				"peek and increment must hit the same pinned replica"
			);
		}
	}
}
