//! External MCP policy hooks (mcpGuardrails).
//!
//! Single-target methods (`tools/call`, ...) fire server-facing in the upstream's
//! native namespace — processors see unmuxed names (`echo`, not `serverA_echo`) and the
//! lone backend name in `service_names`. Fanout methods (`*/list`, ...) run the hook
//! once for the whole client call (request hook before fanout, response hook on the
//! merged result). Names there match the client-facing view, which tracks the
//! multiplexing config rather than the method: muxed names when multiplexing, a single
//! backend's unmuxed names when there is just one (the usual single-backend case).
//! `service_names` lists every fanned-out backend either way.

use std::collections::HashMap;
use std::sync::Arc;

use bytes::Bytes;
use rmcp::model::ErrorData;
use serde::Deserialize;
use serde_json::Value;

use crate::mcp::upstream::IncomingRequestContext;
use crate::proxy::httpproxy::PolicyClient;
use crate::types::agent::{BackendTrafficPolicy, SimpleBackendReference};
use crate::*;

/// Per-request bag of values that `mcpGuardrails` request-phase processors attach via
/// `McpRequestResult.metadata`. Merged into the request extensions and exposed
/// to CEL as `mcpGuardrails.<key>` for backend request filters (e.g. `transformation`).
/// Multiple processors merge into the same map; later writes win on key collisions.
#[apply(schema!)]
#[derive(Default, ::cel::DynamicType)]
pub struct McpGuardrailsDynamicMetadata(serde_json::Map<String, serde_json::Value>);

impl McpGuardrailsDynamicMetadata {
	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

mod client;
pub mod methods;
pub mod phase;
mod ratelimit;

pub use phase::Phase;
pub(crate) use ratelimit::build_mcp_info;

/// MCP context captured during request-phase guardrails. Names are upstream names
/// (post-rewrite), consistent with the RBAC namespace.
#[derive(Debug, Clone)]
pub(crate) struct GuardrailsRequestMcpInfo(pub(crate) crate::mcp::MCPInfo);

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
	pub fn from_proto(
		value: i32,
	) -> Self {
		use protos::agent::backend_policy_spec::mcp_guardrails::rate_limit::rejection_override::ResponseAs;
		match ResponseAs::try_from(value).unwrap_or(ResponseAs::Unspecified) {
			ResponseAs::ToolResult => Self::ToolResult,
			ResponseAs::JsonRpcError | ResponseAs::Unspecified => Self::JsonRpcError,
		}
	}
}

/// MCP denial payload: protocol error or tool execution error.
#[derive(Debug, Clone)]
pub enum McpDenialEnvelope {
	JsonRpc(rmcp::model::ErrorData),
	ToolResult { message: String },
}

/// A guardrail rejection plus optional HTTP-layer overrides.
#[derive(Debug, Clone)]
pub struct Rejection {
	pub envelope: McpDenialEnvelope,
	pub http_status: Option<u16>,
	pub http_headers: Vec<(String, String)>,
}

impl Rejection {
	pub fn json_rpc(error: rmcp::model::ErrorData) -> Self {
		Self {
			envelope: McpDenialEnvelope::JsonRpc(error),
			http_status: None,
			http_headers: Vec::new(),
		}
	}

	pub fn to_server_json_rpc_message(
		&self,
		id: rmcp::model::RequestId,
	) -> rmcp::model::ServerJsonRpcMessage {
		use rmcp::model::{CallToolResult, Content, ServerResult};
		match &self.envelope {
			McpDenialEnvelope::JsonRpc(error) => {
				rmcp::model::ServerJsonRpcMessage::error(error.clone(), id)
			},
			McpDenialEnvelope::ToolResult { message } => {
				let mut result = CallToolResult::success(vec![Content::text(message.clone())]);
				result.is_error = Some(true);
				rmcp::model::ServerJsonRpcMessage::response(
					ServerResult::CallToolResult(result),
					id,
				)
			},
		}
	}
}

impl From<rmcp::model::ErrorData> for Rejection {
	fn from(error: rmcp::model::ErrorData) -> Self {
		Self::json_rpc(error)
	}
}

#[derive(Debug)]
pub enum Outcome<T> {
	Pass,
	Mutated(T),
	Reject(Rejection),
}

pub mod wire {
	pub use protos::ext_mcp::*;
}

#[apply(schema!)]
#[derive(Default)]
pub struct McpGuardrails {
	/// Ordered list of policy processors applied to matched methods; the first
	/// to reject a request short-circuits the chain. Processors may run on the
	/// request or response side, or both; see `Processor.methods`.
	pub processors: Vec<Processor>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct Processor {
	/// Allowlist: only methods listed here run through this processor, at the
	/// configured phase. Keys may be exact (`tools/call`), prefix (`tools/*`),
	/// or suffix (`*/list`) wildcards, or `*` for all methods. Methods matching
	/// no key bypass this processor; see [`phase::resolve`] for match precedence.
	#[serde(default, skip_serializing_if = "HashMap::is_empty")]
	pub methods: HashMap<String, Phase>,
	#[serde(flatten)]
	pub kind: ProcessorKind,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum ProcessorKind {
	Remote(Remote),
	RateLimit(RateLimit),
}

impl McpGuardrails {
	/// Whether any processor runs the request side for `method`.
	pub fn runs_request(&self, method: &str) -> bool {
		self.processors.iter().any(|d| d.runs_request(method))
	}

	/// Whether any processor runs the response side for `method`.
	pub fn runs_response(&self, method: &str) -> bool {
		self.processors.iter().any(|d| d.runs_response(method))
	}

	/// Config warnings to surface at load time (xds diagnostics or logs).
	pub fn load_warnings(&self) -> Vec<String> {
		let mut out = Vec::new();
		for m in methods::REQUEST_PHASE_UNSUPPORTED {
			if self.runs_request(m) {
				out.push(format!(
					"mcpGuardrails: methods match {m:?} with a request phase, but only the response phase runs for this method"
				));
			}
		}
		let mut bad_patterns: Vec<_> = self
			.processors
			.iter()
			.flat_map(|d| d.methods.keys())
			.filter(|p| !phase::pattern_is_matchable(p))
			.map(|p| {
				format!(
					"mcpGuardrails: methods key {p:?} can never match; use an exact method, 'prefix/*', '*/suffix', or '*'"
				)
			})
			.collect();
		bad_patterns.sort();
		out.append(&mut bad_patterns);
		out
	}
}

// Retries and load balancing come from the backend referenced by `target`;
// TLS/auth may also be set inline via `policies`.
#[apply(schema!)]
pub struct Remote {
	/// Reference to the external MCP policy service backend.
	#[serde(flatten)]
	pub target: Arc<SimpleBackendReference>,
	/// Policies to connect to the backend.
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	#[serde(deserialize_with = "crate::types::local::de_from_local_backend_policy")]
	#[cfg_attr(
		feature = "schema",
		schemars(with = "Option<crate::types::local::SimpleLocalBackendPolicies>")
	)]
	pub policies: Vec<BackendTrafficPolicy>,
	/// Behavior when the processor is unavailable or returns an error.
	#[serde(default)]
	pub failure_mode: FailureMode,
	/// CEL expressions evaluated per request and sent to the processor as metadata.
	#[serde(default, skip_serializing_if = "HashMap::is_empty")]
	pub metadata: HashMap<String, Arc<cel::Expression>>,
	/// Which incoming request headers are forwarded to the policy server.
	#[serde(default, skip_serializing_if = "HeaderFilter::is_default")]
	pub request_headers: HeaderFilter,
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
	pub failure_mode: FailureMode,
	/// Reshape JSON-RPC errors when this rate-limit processor rejects. CEL context:
	/// `guardrail.rateLimit.*`, plus request/mcp/jwt context.
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

#[derive(Debug, Clone, serde::Serialize, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
/// CEL context while evaluating rate-limit `rejectionOverrides`.
pub struct GuardrailContext {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[dynamic(rename = "rateLimit")]
	pub rate_limit: Option<GuardrailRateLimitContext>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
pub struct GuardrailRateLimitContext {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub domain: Option<String>,
	pub overall_code: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limit: Option<GuardrailRateLimitStatusContext>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
pub struct GuardrailRateLimitStatusContext {
	pub descriptor: GuardrailRateLimitDescriptorContext,
	pub code: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub requests_per_unit: Option<u32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub unit: Option<String>,
	pub remaining: u32,
	pub reset_seconds: i64,
	pub retry_after_seconds: i64,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
pub struct GuardrailRateLimitDescriptorContext {
	pub entries: Vec<GuardrailRateLimitDescriptorEntryContext>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, ::cel::DynamicType)]
#[serde(rename_all = "camelCase")]
#[dynamic(rename_all = "camelCase")]
pub struct GuardrailRateLimitDescriptorEntryContext {
	pub key: String,
	pub value: String,
}


/// Allow/deny filter over request headers, mirroring ext_authz: empty `allowed`
/// forwards every header plus all pseudo-headers (`:authority`, `:method`, ...);
/// a non-empty `allowed` forwards only the listed names. `disallowed` always
/// wins. Header names match case-insensitively; pseudo-headers match exactly.
#[apply(schema!)]
#[derive(Default)]
pub struct HeaderFilter {
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub allowed: Vec<crate::http::HeaderOrPseudo>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub disallowed: Vec<crate::http::HeaderOrPseudo>,
}

impl HeaderFilter {
	fn is_default(&self) -> bool {
		self.allowed.is_empty() && self.disallowed.is_empty()
	}
	/// Whether a header (or pseudo-header) should be sent to the policy server.
	pub fn allows(&self, name: &crate::http::HeaderOrPseudo) -> bool {
		if self.disallowed.iter().any(|n| n == name) {
			return false;
		}
		self.allowed.is_empty() || self.allowed.iter().any(|n| n == name)
	}
}

// Behavior when a processor errors or returns an unhandleable response.
#[apply(schema_enum!)]
#[derive(Default)]
pub enum FailureMode {
	#[default]
	FailClosed,
	FailOpen,
}

/// `params` is `None` for methods with no per-request body (e.g. `*/list`);
/// any `Mutated` outcome there is logged and discarded.
pub struct CallRequestCtx<'a> {
	pub backends: &'a [String],
	pub method: &'a str,
	pub params: Option<Bytes>,
}

impl Processor {
	fn runs_request(&self, method: &str) -> bool {
		phase::resolve(method, &self.methods).runs_request()
	}

	fn runs_response(&self, method: &str) -> bool {
		phase::resolve(method, &self.methods).runs_response()
	}

	async fn call_request<P: serde::de::DeserializeOwned>(
		&self,
		ctx: &mut CallRequestCtx<'_>,
		req_ctx: &mut IncomingRequestContext,
		client: &PolicyClient,
	) -> Outcome<P> {
		match &self.kind {
			ProcessorKind::Remote(remote) => {
				client::check_request::<P>(
					remote,
					ctx.method,
					ctx.backends,
					ctx.params.as_mut(),
					req_ctx,
					client,
				)
				.await
			},
			ProcessorKind::RateLimit(rate_limit) => {
				ratelimit::check_request::<P>(
					rate_limit,
					ctx.method,
					ctx.backends,
					ctx.params.as_ref(),
					req_ctx,
					client,
				)
				.await
			},
		}
	}

	async fn response(
		&self,
		method: &str,
		backends: &[String],
		body: &mut Bytes,
		req_ctx: &IncomingRequestContext,
		mcp: Option<&crate::mcp::MCPInfo>,
		client: &PolicyClient,
	) -> Outcome<rmcp::model::ServerResult> {
		match &self.kind {
			ProcessorKind::Remote(remote) => {
				client::check_response(remote, method, backends, body, req_ctx, client).await
			},
			ProcessorKind::RateLimit(rate_limit) => {
				ratelimit::check_response(rate_limit, method, backends, body, req_ctx, mcp, client).await
			},
		}
	}
}

fn maybe_override_rejection(
	processor: &Processor,
	method: &str,
	req_ctx: &IncomingRequestContext,
	mcp: Option<&crate::mcp::MCPInfo>,
	rej: ErrorData,
) -> Rejection {
	let ProcessorKind::RateLimit(rate_limit) = &processor.kind else {
		return rej.into();
	};
	if rate_limit.rejection_overrides.is_empty() {
		return rej.into();
	}
	let rate_limit_ctx = rej
		.data
		.as_ref()
		.and_then(parse_rate_limit_error_payload);
	let guardrail = GuardrailContext {
		rate_limit: rate_limit_ctx,
	};
	let req = req_ctx.as_request();
	let exec = if let Some(mcp) = mcp {
		cel::Executor::new_mcp_request(&req, mcp)
	} else {
		cel::Executor::new_request(&req)
	}
	.with_guardrail(&guardrail);
	for override_cfg in &rate_limit.rejection_overrides {
		if !exec.eval_bool(override_cfg.when.as_ref()) {
			continue;
		}
		let http_status = override_cfg.status;
		let http_headers = eval_override_headers(&exec, override_cfg);
		let Some(body) = override_cfg.body.as_ref() else {
			if http_status.is_some() || !http_headers.is_empty() {
				return Rejection {
					envelope: McpDenialEnvelope::JsonRpc(ErrorData::new(
						rej.code,
						rej.message.clone(),
						rej.data.clone(),
					)),
					http_status,
					http_headers,
				};
			}
			continue;
		};
		let message = body
			.message
			.as_ref()
			.and_then(|expr| match exec.eval(expr.as_ref()) {
				Ok(value) => value.as_string().ok(),
				Err(e) => {
					tracing::debug!(error = %e, "mcpGuardrails rejectionOverride message failed");
					None
				},
			})
			.unwrap_or_else(|| rej.message.to_string());
		if override_cfg.response_as == RejectionResponseAs::ToolResult && method == "tools/call" {
			return Rejection {
				envelope: McpDenialEnvelope::ToolResult { message },
				http_status: http_status.or(Some(200)),
				http_headers,
			};
		}
		if override_cfg.response_as == RejectionResponseAs::ToolResult {
			tracing::debug!(
				method,
				"mcpGuardrails: ToolResult override on non-tools/call, using JsonRpcError"
			);
		}
		let code = body.code.unwrap_or(rej.code.0);
		return Rejection {
			envelope: McpDenialEnvelope::JsonRpc(ErrorData::new(
				rmcp::model::ErrorCode(code),
				message,
				rej.data.clone(),
			)),
			http_status,
			http_headers,
		};
	}
	rej.into()
}

fn parse_rate_limit_error_payload(data: &Value) -> Option<GuardrailRateLimitContext> {
	serde_json::from_value::<GuardrailRateLimitContext>(data.clone())
		.map_err(|e| {
			tracing::debug!(error = %e, "mcpGuardrails: ignoring unparseable rateLimit mcp_error payload");
		})
		.ok()
}

fn eval_override_headers(
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

/// Processors fire in order; first `Reject` short-circuits leaving `ctx` in whatever
/// partially-mutated state earlier processors produced. When `ctx.params` is `None`
/// (e.g. `*/list`) mutations are discarded — list filtering belongs in the response phase.
pub async fn run_call_request<P: serde::de::DeserializeOwned>(
	ext: &McpGuardrails,
	ctx: &mut CallRequestCtx<'_>,
	req_ctx: &mut IncomingRequestContext,
	client: &PolicyClient,
) -> Outcome<P> {
	let mut composed = Outcome::Pass;
	for processor in &ext.processors {
		if !processor.runs_request(ctx.method) {
			continue;
		}
		match processor.call_request::<P>(ctx, req_ctx, client).await {
			Outcome::Pass => {},
			Outcome::Mutated(p) => composed = Outcome::Mutated(p),
			Outcome::Reject(r) => {
				let mcp = ratelimit::build_mcp_info(ctx.method, ctx.params.as_ref());
				let McpDenialEnvelope::JsonRpc(error) = r.envelope else {
					return Outcome::Reject(r);
				};
				return Outcome::Reject(maybe_override_rejection(
					processor,
					ctx.method,
					req_ctx,
					Some(&mcp),
					error,
				));
			},
		}
	}
	composed
}

/// Processors fire in order; first `Reject` short-circuits.
pub async fn run_response(
	ext: &McpGuardrails,
	method: &str,
	backends: &[String],
	mut body: Bytes,
	req_ctx: &IncomingRequestContext,
	mcp: Option<&crate::mcp::MCPInfo>,
	client: &PolicyClient,
) -> Outcome<rmcp::model::ServerResult> {
	let mut composed = Outcome::Pass;
	for processor in &ext.processors {
		if !processor.runs_response(method) {
			continue;
		}
		match processor
			.response(method, backends, &mut body, req_ctx, mcp, client)
			.await
		{
			Outcome::Pass => {},
			Outcome::Mutated(r) => composed = Outcome::Mutated(r),
			Outcome::Reject(r) => {
				let McpDenialEnvelope::JsonRpc(error) = r.envelope else {
					return Outcome::Reject(r);
				};
				return Outcome::Reject(maybe_override_rejection(
					processor,
					method,
					req_ctx,
					mcp,
					error,
				));
			},
		}
	}
	composed
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn deser_local_config() {
		let cfg = r#"
processors:
  - kind: rateLimit
    methods: { "tools/call": full }
    host: 127.0.0.1:9998
    domain: mcp
    descriptors:
      - entries:
          - key: tool
            value: mcp.tool.name
        type: requests
        peek: true
    failureMode: failOpen
  - kind: remote
    methods: { "tools/call": request, "*/list": response }
    host: 127.0.0.1:9999
    policies:
      backendTLS: {}
    failureMode: failOpen
    requestHeaders:
      allowed: [x-tenant]
      disallowed: [":authority"]
  - kind: remote
    methods: { "tools/call": full }
    backend: my-backend
"#;
		let ext: McpGuardrails = serde_yaml::from_str(cfg).expect("deser McpGuardrails");
		assert_eq!(ext.processors.len(), 3);

		let d0 = &ext.processors[0];
		assert_eq!(d0.methods.get("tools/call"), Some(&Phase::Full));
		let ProcessorKind::RateLimit(rl0) = &d0.kind else {
			panic!("expected rateLimit processor");
		};
		assert_eq!(rl0.domain, "mcp");
		assert_eq!(rl0.descriptors.0.len(), 1);
		assert!(rl0.descriptors.0[0].peek);
		assert_eq!(rl0.failure_mode, FailureMode::FailOpen);

		let d0 = &ext.processors[1];
		assert_eq!(d0.methods.get("tools/call"), Some(&Phase::Request));
		assert_eq!(d0.methods.get("*/list"), Some(&Phase::Response));
		let ProcessorKind::Remote(r0) = &d0.kind else {
			panic!("expected remote processor");
		};
		assert!(matches!(
			r0.target.as_ref(),
			SimpleBackendReference::InlineBackend(_)
		));
		assert_eq!(r0.failure_mode, FailureMode::FailOpen);
		assert_eq!(r0.policies.len(), 1, "backendTLS should translate");
		assert_eq!(r0.request_headers.allowed.len(), 1);
		assert!(
			r0.request_headers
				.disallowed
				.contains(&crate::http::HeaderOrPseudo::Authority)
		);

		let ProcessorKind::Remote(r1) = &ext.processors[2].kind else {
			panic!("expected remote processor");
		};
		assert!(matches!(
			r1.target.as_ref(),
			SimpleBackendReference::Backend(_)
		));
		assert_eq!(r1.failure_mode, FailureMode::FailClosed);
	}

	#[test]
	fn deser_rejection_overrides() {
		let cfg = r#"
processors:
  - kind: rateLimit
    methods: { "tools/call": request }
    host: 127.0.0.1:9998
    domain: mcp
    descriptors:
      - entries:
          - key: tool
            value: mcp.tool.name
        peek: true
    rejectionOverrides:
      - when: has(guardrail.rateLimit.limit)
        status: 200
        body:
          code: -32001
          message: '"retry after " + string(guardrail.rateLimit.limit.retryAfterSeconds)'
"#;
		let ext: McpGuardrails = serde_yaml::from_str(cfg).expect("deser McpGuardrails");
		let ProcessorKind::RateLimit(rl) = &ext.processors[0].kind else {
			panic!("expected rateLimit processor");
		};
		assert_eq!(rl.rejection_overrides.len(), 1);
		let ro = &rl.rejection_overrides[0];
		assert_eq!(ro.status, Some(200));
		assert_eq!(ro.body.as_ref().and_then(|body| body.code), Some(-32001));
		assert!(
			ro.body
				.as_ref()
				.and_then(|body| body.message.as_ref())
				.is_some_and(|expr| expr.original_expression.contains("retryAfterSeconds"))
		);
	}

	fn ext_with_methods(pairs: &[(&str, Phase)]) -> McpGuardrails {
		McpGuardrails {
			processors: vec![Processor {
				methods: pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
				kind: ProcessorKind::Remote(Remote {
					target: Arc::new(SimpleBackendReference::Backend("b".into())),
					policies: Vec::new(),
					failure_mode: FailureMode::default(),
					metadata: HashMap::new(),
					request_headers: HeaderFilter::default(),
				}),
			}],
		}
	}

	#[test]
	fn warns_on_request_phase_for_unsupported_methods() {
		// A catchall request phase matches subscribe/unsubscribe/complete, none of
		// which run the request hook.
		let warnings = ext_with_methods(&[("*", Phase::Full)]).load_warnings();
		assert_eq!(warnings.len(), 3, "{warnings:?}");
		assert!(warnings[0].contains("resources/subscribe"));

		// Response-only and supported-method configs are clean.
		assert!(
			ext_with_methods(&[("*", Phase::Response), ("tools/call", Phase::Full)])
				.load_warnings()
				.is_empty()
		);
	}

	#[test]
	fn warns_on_unmatchable_method_patterns() {
		let warnings = ext_with_methods(&[
			("a*b", Phase::Response),
			("**", Phase::Response),
			("", Phase::Response),
			("tools/*", Phase::Response),
			("*/list", Phase::Response),
		])
		.load_warnings();
		assert_eq!(warnings.len(), 3, "{warnings:?}");
		assert!(warnings.iter().all(|w| w.contains("can never match")));
	}

	#[test]
	fn rejection_override_status_only_applies_http_overrides() {
		use rmcp::model::ErrorCode;

		let ext = McpGuardrails {
			processors: vec![Processor {
				methods: HashMap::new(),
				kind: ProcessorKind::RateLimit(RateLimit {
					domain: "mcp".to_string(),
					target: Arc::new(SimpleBackendReference::Backend("unused".into())),
					policies: Vec::new(),
					failure_mode: FailureMode::FailClosed,
					descriptors: Arc::new(RateLimitDescriptorSet(Vec::new())),
					rejection_overrides: vec![RateLimitRejectionOverride {
						when: Arc::new(cel::Expression::new_strict("true").unwrap()),
						response_as: RejectionResponseAs::default(),
						status: Some(429),
						body: None,
						headers: Vec::new(),
					}],
				}),
			}],
		};
		let processor = &ext.processors[0];
		let req_ctx = IncomingRequestContext::empty();
		let original = ErrorData::new(
			ErrorCode(-32003),
			"rate limit exceeded",
			None,
		);

		let overridden = maybe_override_rejection(processor, "tools/call", &req_ctx, None, original);

		let McpDenialEnvelope::JsonRpc(error) = overridden.envelope else {
			panic!("expected JsonRpc envelope");
		};
		assert_eq!(error.code, ErrorCode(-32003));
		assert_eq!(error.message.as_ref(), "rate limit exceeded");
		assert_eq!(overridden.http_status, Some(429));
	}

	#[test]
	fn rejection_override_uses_mcp_context() {
		use rmcp::model::ErrorCode;

		let ext = McpGuardrails {
			processors: vec![Processor {
				methods: HashMap::new(),
				kind: ProcessorKind::RateLimit(RateLimit {
					domain: "mcp".to_string(),
					target: Arc::new(SimpleBackendReference::Backend("unused".into())),
					policies: Vec::new(),
					failure_mode: FailureMode::FailClosed,
					descriptors: Arc::new(RateLimitDescriptorSet(Vec::new())),
					rejection_overrides: vec![RateLimitRejectionOverride {
						when: Arc::new(
							cel::Expression::new_strict(r#"mcp.tool.name == "echo_demo""#).unwrap(),
						),
						response_as: RejectionResponseAs::default(),
						status: None,
						body: Some(RateLimitRejectionOverrideBody {
							code: Some(-32001),
							message: Some(Arc::new(
								cel::Expression::new_strict(r#""limited: " + mcp.tool.name"#).unwrap(),
							)),
						}),
						headers: Vec::new(),
					}],
				}),
			}],
		};
		let processor = &ext.processors[0];
		let req_ctx = IncomingRequestContext::empty();
		let original = ErrorData::new(ErrorCode(-32003), "rate limit exceeded", None);
		let mcp = crate::mcp::MCPInfo {
			tool: Some(crate::mcp::MCPTool {
				target: "server".into(),
				name: "echo_demo".into(),
				arguments: None,
				result: None,
				error: None,
			}),
			..Default::default()
		};

		let without_mcp =
			maybe_override_rejection(processor, "tools/call", &req_ctx, None, original.clone());
		let McpDenialEnvelope::JsonRpc(error) = without_mcp.envelope else {
			panic!("expected JsonRpc envelope");
		};
		assert_eq!(error.code, ErrorCode(-32003));
		assert_eq!(error.message.as_ref(), "rate limit exceeded");

		let with_mcp = maybe_override_rejection(processor, "tools/call", &req_ctx, Some(&mcp), original);
		let McpDenialEnvelope::JsonRpc(error) = with_mcp.envelope else {
			panic!("expected JsonRpc envelope");
		};
		assert_eq!(error.code, ErrorCode(-32001));
		assert_eq!(error.message.as_ref(), "limited: echo_demo");
	}

	#[test]
	fn rejection_override_tool_result_envelope() {
		use rmcp::model::ErrorCode;

		let ext = McpGuardrails {
			processors: vec![Processor {
				methods: HashMap::new(),
				kind: ProcessorKind::RateLimit(RateLimit {
					domain: "mcp".to_string(),
					target: Arc::new(SimpleBackendReference::Backend("unused".into())),
					policies: Vec::new(),
					failure_mode: FailureMode::FailClosed,
					descriptors: Arc::new(RateLimitDescriptorSet(Vec::new())),
					rejection_overrides: vec![RateLimitRejectionOverride {
						when: Arc::new(cel::Expression::new_strict("true").unwrap()),
						response_as: RejectionResponseAs::ToolResult,
						status: None,
						body: Some(RateLimitRejectionOverrideBody {
							code: None,
							message: Some(Arc::new(
								cel::Expression::new_strict(r#""quota exceeded""#).unwrap(),
							)),
						}),
						headers: Vec::new(),
					}],
				}),
			}],
		};
		let processor = &ext.processors[0];
		let req_ctx = IncomingRequestContext::empty();
		let original = ErrorData::new(ErrorCode(-32003), "rate limit exceeded", None);

		let overridden =
			maybe_override_rejection(processor, "tools/call", &req_ctx, None, original);
		match overridden.envelope {
			McpDenialEnvelope::ToolResult { message } => assert_eq!(message, "quota exceeded"),
			_ => panic!("expected ToolResult envelope"),
		}
		assert_eq!(overridden.http_status, Some(200));
	}
}
