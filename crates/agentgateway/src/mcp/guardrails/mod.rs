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
#[cfg(feature = "adobe")]
use rmcp::model::ErrorData;

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

/// Merge ExtMCP `McpRequestResult.metadata` into request extensions for CEL `mcpGuardrails.*`.
pub(crate) fn merge_metadata_into_extensions(
	method: &str,
	backends: &[String],
	s: &prost_wkt_types::Struct,
	ext: &mut ::http::Extensions,
) {
	let mut acc = ext
		.remove::<McpGuardrailsDynamicMetadata>()
		.unwrap_or_default();
	for (k, v) in &s.fields {
		match serde_json::to_value(v) {
			Ok(j) => {
				acc.0.insert(k.clone(), j);
			},
			Err(e) => {
				tracing::warn!(method, ?backends, key = %k, error = %e, "mcpGuardrails: metadata: failed to convert value");
			},
		}
	}
	if !acc.0.is_empty() {
		ext.insert(acc);
	}
}

mod client;
pub mod methods;
pub mod phase;
#[cfg(feature = "adobe")]
mod rate_limit_types;
#[cfg(feature = "adobe")]
mod ratelimit;
#[cfg(feature = "adobe")]
mod ratelimit_pin;

#[cfg(feature = "adobe")]
pub use rate_limit_types::{
	RateLimit, RateLimitDescriptor, RateLimitDescriptorEntry, RateLimitDescriptorSet,
	RateLimitRejectionOverride, RateLimitRejectionOverrideBody, RateLimitRejectionOverrideHeader,
	RejectionResponseAs,
};

pub use phase::Phase;

/// MCP context for CEL/rejection overrides from request params (shared across guardrail kinds).
pub(crate) fn build_mcp_info(method: &str, body: Option<&Bytes>) -> crate::mcp::MCPInfo {
	let mut info = crate::mcp::MCPInfo {
		method_name: Some(method.to_string()),
		..Default::default()
	};
	match method {
		methods::TOOLS_CALL => {
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
		methods::PROMPTS_GET => {
			if let Some(name) = body
				.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
				.and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_string))
			{
				info.set_prompt(String::new(), name);
			}
		},
		methods::RESOURCES_READ => {
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

/// MCP context captured during request-phase guardrails. Names are upstream names
/// (post-rewrite), consistent with the RBAC namespace.
#[derive(Debug, Clone)]
pub(crate) struct GuardrailsRequestMcpInfo(pub(crate) crate::mcp::MCPInfo);

/// Guardrail rejection payload. Adobe builds use enriched [`Rejection`]; upstream-style
/// builds carry plain JSON-RPC [`ErrorData`].
#[cfg(feature = "adobe")]
pub type Denial = Rejection;
#[cfg(not(feature = "adobe"))]
pub type Denial = rmcp::model::ErrorData;

pub(crate) fn denial_from_error(error: rmcp::model::ErrorData) -> Denial {
	#[cfg(feature = "adobe")]
	{
		error.into()
	}
	#[cfg(not(feature = "adobe"))]
	{
		error
	}
}

pub(crate) fn denial_to_server_message(
	denial: Denial,
	id: rmcp::model::RequestId,
) -> rmcp::model::ServerJsonRpcMessage {
	#[cfg(feature = "adobe")]
	{
		denial.to_server_json_rpc_message(id)
	}
	#[cfg(not(feature = "adobe"))]
	{
		rmcp::model::ServerJsonRpcMessage::error(denial, id)
	}
}

#[cfg(feature = "adobe")]
/// MCP denial payload: protocol error or tool execution error.
#[derive(Debug, Clone)]
pub enum McpDenialEnvelope {
	JsonRpc(rmcp::model::ErrorData),
	ToolResult { message: String },
}

#[cfg(feature = "adobe")]
/// A guardrail rejection plus optional HTTP-layer overrides.
#[derive(Debug, Clone)]
pub struct Rejection {
	pub envelope: McpDenialEnvelope,
	pub http_status: Option<u16>,
	pub http_headers: Vec<(String, String)>,
}

#[cfg(feature = "adobe")]
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

#[cfg(feature = "adobe")]
impl From<rmcp::model::ErrorData> for Rejection {
	fn from(error: rmcp::model::ErrorData) -> Self {
		Self::json_rpc(error)
	}
}

#[derive(Debug)]
pub enum Outcome<T> {
	Pass,
	Mutated(T),
	Reject(Denial),
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
	#[cfg(feature = "adobe")]
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
			#[cfg(feature = "adobe")]
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
		#[allow(unused_variables)] mcp: Option<&crate::mcp::MCPInfo>,
		client: &PolicyClient,
	) -> Outcome<rmcp::model::ServerResult> {
		match &self.kind {
			ProcessorKind::Remote(remote) => {
				client::check_response(remote, method, backends, body, req_ctx, client).await
			},
			#[cfg(feature = "adobe")]
			ProcessorKind::RateLimit(rate_limit) => {
				ratelimit::check_response(rate_limit, method, backends, body, req_ctx, mcp, client).await
			},
		}
	}
}

/// ExtMCP `AuthorizationError.mcp_error` is merged into request extensions for CEL; it must not be forwarded to clients.
#[cfg(feature = "adobe")]
pub(crate) fn strip_client_error_data(error: ErrorData) -> ErrorData {
	ErrorData::new(error.code, error.message.clone(), None)
}

#[cfg(feature = "adobe")]
fn maybe_override_rejection(
	processor: &Processor,
	method: &str,
	req_ctx: &IncomingRequestContext,
	mcp: Option<&crate::mcp::MCPInfo>,
	rej: ErrorData,
) -> Denial {
	use rate_limit_types::eval_override_headers;

	let ProcessorKind::RateLimit(rate_limit) = &processor.kind else {
		return rej.into();
	};
	if rate_limit.rejection_overrides.is_empty() {
		return rej.into();
	}
	let req = req_ctx.as_request();
	let exec = if let Some(mcp) = mcp {
		cel::Executor::new_mcp_request(&req, mcp)
	} else {
		cel::Executor::new_request(&req)
	};
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
						None,
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
				None,
			)),
			http_status,
			http_headers,
		};
	}
	strip_client_error_data(rej).into()
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
				#[cfg(feature = "adobe")]
				{
					let mcp = build_mcp_info(ctx.method, ctx.params.as_ref());
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
				}
				#[cfg(not(feature = "adobe"))]
				{
					return Outcome::Reject(r);
				}
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
				#[cfg(feature = "adobe")]
				{
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
				}
				#[cfg(not(feature = "adobe"))]
				{
					return Outcome::Reject(r);
				}
			},
		}
	}
	composed
}

#[cfg(test)]
mod tests {
	use super::*;

	#[cfg(feature = "adobe")]
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

	#[cfg(feature = "adobe")]
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
      - when: has(mcpGuardrails.rateLimit.limit)
        status: 200
        body:
          code: -32001
          message: '"retry after " + string(mcpGuardrails.rateLimit.limit.retryAfterSeconds)'
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

	#[cfg(not(feature = "adobe"))]
	#[test]
	fn deser_rate_limit_processor_requires_adobe_feature() {
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
        peek: true
"#;
		let err = serde_yaml::from_str::<McpGuardrails>(cfg).expect_err("rateLimit should not deser");
		assert!(
			err.to_string().contains("rateLimit"),
			"unexpected error: {err}"
		);
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

	#[cfg(feature = "adobe")]
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

	#[cfg(feature = "adobe")]
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

	#[cfg(feature = "adobe")]
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

	#[cfg(feature = "adobe")]
	#[test]
	fn rejection_override_uses_mcp_guardrails_rate_limit_metadata() {
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
							cel::Expression::new_strict("has(mcpGuardrails.rateLimit.limit)").unwrap(),
						),
						response_as: RejectionResponseAs::default(),
						status: None,
						body: Some(RateLimitRejectionOverrideBody {
							code: Some(-32001),
							message: Some(Arc::new(
								cel::Expression::new_strict(
									r#""retry after " + string(mcpGuardrails.rateLimit.limit.retryAfterSeconds)"#,
								)
								.unwrap(),
							)),
						}),
						headers: Vec::new(),
					}],
				}),
			}],
		};
		let processor = &ext.processors[0];
		let mut req_ctx = IncomingRequestContext::empty();
		ratelimit::merge_rate_limit_mcp_error_into_extensions(
			"tools/call",
			&["mcp".to_string()],
			serde_json::to_vec(&serde_json::json!({
				"domain": "mcp_api",
				"overallCode": "OVER_LIMIT",
				"limit": {
					"descriptor": { "entries": [{ "key": "tool", "value": "echo" }] },
					"code": "OVER_LIMIT",
					"retryAfterSeconds": 42
				}
			}))
			.unwrap()
			.as_slice(),
			req_ctx.extensions_mut(),
		);
		let original = ErrorData::new(ErrorCode(-32003), "rate limit exceeded", None);

		let overridden = maybe_override_rejection(processor, "tools/call", &req_ctx, None, original);
		let McpDenialEnvelope::JsonRpc(error) = overridden.envelope else {
			panic!("expected JsonRpc envelope");
		};
		assert_eq!(error.code, ErrorCode(-32001));
		assert_eq!(error.message.as_ref(), "retry after 42");
		assert!(error.data.is_none(), "AuthorizationError.mcp_error must not be exposed in error.data");
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn rejection_override_strips_internal_mcp_error_data() {
		use rmcp::model::ErrorCode;
		use serde_json::json;

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
						status: Some(401),
						body: Some(RateLimitRejectionOverrideBody {
							code: Some(-32013),
							message: Some(Arc::new(
								cel::Expression::new_strict(r#""auth required""#).unwrap(),
							)),
						}),
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
			Some(json!({
				"domain": "mcp_api",
				"overallCode": "OVER_LIMIT",
			})),
		);

		let overridden = maybe_override_rejection(processor, "tools/call", &req_ctx, None, original);

		let McpDenialEnvelope::JsonRpc(error) = overridden.envelope else {
			panic!("expected JsonRpc envelope");
		};
		assert_eq!(error.code, ErrorCode(-32013));
		assert_eq!(error.message.as_ref(), "auth required");
		assert!(error.data.is_none());
	}
}
