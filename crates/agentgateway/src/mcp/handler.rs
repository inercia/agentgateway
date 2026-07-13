use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use futures_core::Stream;
use http::StatusCode;
use http::request::Parts;
use itertools::Itertools;
use rmcp::ErrorData;
use rmcp::model::{
	ClientNotification, ClientRequest, JsonRpcNotification, JsonRpcRequest,
	ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListPromptsRequest,
	ListToolsRequest, ListToolsResult, ProtocolVersion, RequestId, ServerJsonRpcMessage,
	ServerResult,
};
#[cfg(feature = "adobe")]
use rmcp::model::ClientJsonRpcMessage;
use tracing::{debug, warn};

use crate::http::Response;
use crate::http::sessionpersistence::MCPSession;
use crate::mcp;
use crate::mcp::mergestream::{MergeFn, Messages};
use crate::mcp::multiplex_naming;
use crate::mcp::rbac::{CelExecWrapper, McpAuthorizationSet};
use crate::mcp::rewrite::{
	apply_prompt_rewrite, apply_resource_rewrite, apply_tool_rewrite, build_flat_prompt_route_index,
	build_flat_tool_route_index, filter_flat_prompt_collisions, filter_flat_resource_collisions,
	filter_flat_resource_template_collisions, filter_flat_tool_collisions,
	CompiledServerRewrite, McpRewriteSet,
};
#[cfg(feature = "adobe")]
use crate::mcp::rewrite::{build_flat_task_route_index, filter_flat_task_collisions};
use crate::mcp::router::McpBackendGroup;
use crate::mcp::streamablehttp::ServerSseMessage;
use crate::mcp::upstream::{IncomingRequestContext, UpstreamError};
use crate::mcp::{ClientError, FailureMode, MCPInfo, mergestream, rbac, upstream};
use crate::proxy::httpproxy::PolicyClient;
use crate::telemetry::log::{AsyncLog, SpanWriteOnDrop, SpanWriter};

use crate::mcp::mcp_apps::routing::{
	apply_multiplex_to_listed_resource, apply_multiplex_to_resource_template,
};

#[derive(Debug, Clone)]
pub struct Relay {
	pub(crate) upstreams: Arc<upstream::UpstreamGroup>,
	pub policies: McpAuthorizationSet,
	pub(crate) mcp_guardrails: Option<Arc<crate::mcp::guardrails::McpGuardrails>>,
	pub(crate) policy_client: PolicyClient,
	pub mcp_rewrite: McpRewriteSet,
	#[cfg(feature = "adobe")]
	pub(crate) capabilities: Arc<crate::mcp::mcp_apps::capabilities::TargetCapabilities>,
	/// Populated by federated `tools/list` when `resourceNaming: Flat`; used by `tools/call`.
	///
	/// Always present on [`Relay`] (non-Adobe builds use stub `flat()` and never populate).
	flat_tool_routes: Arc<RwLock<HashMap<String, (String, String)>>>,
	/// Populated by federated `prompts/list` when `resourceNaming: Flat`; used by `prompts/get`.
	flat_prompt_routes: Arc<RwLock<HashMap<String, (String, String)>>>,
	/// Populated by federated `tasks/list` and `tools/call` create when `resourceNaming: Flat`.
	///
	/// Adobe-only: task RPCs are behind `feature = "adobe"`; tools index stays on `Relay` for
	/// shared `resolve_tool_call` API surface in all builds.
	#[cfg(feature = "adobe")]
	flat_task_routes: Arc<RwLock<HashMap<String, (String, String)>>>,
}

pub struct RelayInputs {
	pub backend: McpBackendGroup,
	pub policies: McpAuthorizationSet,
	pub mcp_guardrails: Option<Arc<crate::mcp::guardrails::McpGuardrails>>,
	pub client: PolicyClient,
}

impl RelayInputs {
	pub fn build_new_connections(self) -> Result<Relay, mcp::Error> {
		let r = Relay::new(self.backend, self.policies, self.client)?;
		Ok(Relay {
			mcp_guardrails: self.mcp_guardrails,
			..r
		})
	}
}

impl Relay {
	pub fn new(
		backend: McpBackendGroup,
		policies: McpAuthorizationSet,
		client: PolicyClient,
	) -> Result<Self, mcp::Error> {
		let mcp_rewrite = McpRewriteSet::from_backend_targets(backend.targets.iter().map(|t| {
			(
				t.name.to_string(),
				t.backend_policies.mcp_rewrite.clone(),
			)
		}));
		Ok(Self {
			upstreams: Arc::new(upstream::UpstreamGroup::new(client.clone(), backend)?),
			policies,
			mcp_guardrails: None,
			policy_client: client,
			mcp_rewrite,
			#[cfg(feature = "adobe")]
			capabilities: Arc::new(crate::mcp::mcp_apps::capabilities::TargetCapabilities::new()),
			flat_tool_routes: Arc::new(RwLock::new(HashMap::new())),
			flat_prompt_routes: Arc::new(RwLock::new(HashMap::new())),
			#[cfg(feature = "adobe")]
			flat_task_routes: Arc::new(RwLock::new(HashMap::new())),
		})
	}
	pub fn with_policies(&self, policies: McpAuthorizationSet) -> Self {
		Self {
			upstreams: self.upstreams.clone(),
			policies,
			mcp_guardrails: self.mcp_guardrails.clone(),
			policy_client: self.policy_client.clone(),
			mcp_rewrite: self.mcp_rewrite.clone(),
			#[cfg(feature = "adobe")]
			capabilities: self.capabilities.clone(),
			flat_tool_routes: self.flat_tool_routes.clone(),
			flat_prompt_routes: self.flat_prompt_routes.clone(),
			#[cfg(feature = "adobe")]
			flat_task_routes: self.flat_task_routes.clone(),
		}
	}

	fn rewrite_outbound_server_messages(&self, target: &str, stream: Messages) -> Messages {
		let target = target.to_string();
		let default_target_name = self.upstreams.default_target_name.clone();
		stream.map_server_messages(move |mut message| {
			crate::mcp::mcp_apps::routing::rewrap_outbound_multiplex_server_message(
				default_target_name.as_ref(),
				&target,
				&mut message,
			);
			message
		})
	}

	/// Record a Flat-mode task route when a task is created or listed.
	#[cfg(feature = "adobe")]
	pub(crate) fn record_flat_task_route(&self, target: &str, upstream_id: &str) {
		if !self.mcp_rewrite.flat() || self.upstreams.default_target_name.is_some() {
			return;
		}
		let exposed = upstream_id.to_string();
		let mut routes = self.flat_task_routes.write();
		if routes.contains_key(&exposed) {
			warn!(
				exposed = %exposed,
				"mcp_flat_name_collision: keeping first exposed task id in route index"
			);
			return;
		}
		routes.insert(exposed, (target.to_string(), upstream_id.to_string()));
	}

	pub fn parse_resource_name<'a, 'b: 'a>(
		&'a self,
		res: &'b str,
	) -> Result<(&'a str, &'b str), UpstreamError> {
		multiplex_naming::parse_resource_name(self.upstreams.default_target_name.as_ref(), res)
	}

	/// Resolve a client tool name to `(target, upstream_name)` for auth and upstream `tools/call`.
	///
	/// In Flat mode the resolution depends on the route index populated by `tools/list`.
	/// The index is in-memory only; with multi-replica agentgateway the session can
	/// resume on a pod whose index is empty. This async path therefore lazily refreshes
	/// the index via an internal `tools/list` (no-op if already populated or not Flat).
	pub async fn resolve_tool_call(
		&self,
		client_name: &str,
		ctx: &IncomingRequestContext,
	) -> Result<(String, String), UpstreamError> {
		if self.mcp_rewrite.flat() {
			self.ensure_flat_tool_routes_loaded(ctx).await?;
			let routes = self.flat_tool_routes.read();
			return self.mcp_rewrite.resolve_flat_tool(
				client_name,
				Some(&routes),
				&self.all_target_names(),
			);
		}
		let (target, exposed) = self.parse_resource_name(client_name)?;
		let upstream = self
			.mcp_rewrite
			.target(target)
			.map(|t| t.resolve_upstream_tool(exposed))
			.unwrap_or_else(|| exposed.to_string());
		Ok((target.to_string(), upstream))
	}

	/// Populate `flat_tool_routes` via an internal `tools/list` if it is empty.
	///
	/// The route index lives in-memory per Relay. With agentgateway running
	/// >1 replicas a session can resume on a pod where this index is empty, and
	/// `resolve_flat_tool` would then fall through to its broken pass-through fallback
	/// and produce `ambiguous flat tool name`. This helper makes the route index
	/// self-healing: any pod that holds a session can repopulate from upstreams.
	///
	/// No-op when not Flat or when the route index is already populated. Best-effort
	/// on internal errors — a later `tools/call` will still surface the underlying
	/// `unknown flat tool name` if population genuinely failed.
	async fn ensure_flat_tool_routes_loaded(
		&self,
		ctx: &IncomingRequestContext,
	) -> Result<(), UpstreamError> {
		if !self.mcp_rewrite.flat() {
			return Ok(());
		}
		if !self.flat_tool_routes.read().is_empty() {
			return Ok(());
		}
		// send_fanout_to derives the per-request CEL context from `ctx` and supplies it to
		// the merge closure at invocation (upstream #1842 model), so no local cel is needed.
		#[cfg(feature = "adobe")]
		let targets = self.capabilities.upstreams_with_tools(&self.all_target_names());
		#[cfg(not(feature = "adobe"))]
		let targets = self.all_target_names();
		if targets.is_empty() {
			return Ok(());
		}
		let merge = self.merge_tools();
		let list_req: rmcp::model::ListToolsRequest = ListToolsRequest::default();
		let req = JsonRpcRequest::new(RequestId::Number(-1), ClientRequest::ListToolsRequest(list_req));
		// Best-effort: if the internal tools/list fails entirely the route index
		// stays empty and the subsequent `resolve_flat_tool` will return
		// `unknown flat tool name`, which is more accurate than the historical
		// `ambiguous flat tool name` from the broken pass-through fallback.
		let resp = match self
			.send_fanout_to(&targets, req, ctx.clone(), merge)
			.await
		{
			Ok(resp) => resp,
			Err(e) => {
				tracing::warn!(error = %e, "ensure_flat_tool_routes_loaded: internal tools/list failed");
				return Ok(());
			},
		};
		// Drain the SSE body so MergeStream polls to completion and the merge
		// closure runs — that's the side effect that populates flat_tool_routes.
		let _ = crate::http::read_resp_body(resp).await;
		Ok(())
	}

	/// Populate `flat_task_routes` via an internal `tasks/list` if it is empty.
	///
	/// Mirrors [`Self::ensure_flat_tool_routes_loaded`]: multi-replica sessions can resume on a
	/// pod with an empty task route index after create-but-before-list.
	#[cfg(feature = "adobe")]
	pub(crate) async fn ensure_flat_task_routes_loaded(
		&self,
		ctx: &IncomingRequestContext,
	) -> Result<(), UpstreamError> {
		if !self.mcp_rewrite.flat() || self.upstreams.default_target_name.is_some() {
			return Ok(());
		}
		if !self.flat_task_routes.read().is_empty() {
			return Ok(());
		}
		let targets = self
			.capabilities
			.upstreams_with_tasks(&self.all_target_names());
		if targets.is_empty() {
			return Ok(());
		}
		let merge = self.merge_tasks();
		let list_req = rmcp::model::ListTasksRequest::default();
		let req = JsonRpcRequest::new(RequestId::Number(-1), ClientRequest::ListTasksRequest(list_req));
		let resp = match self
			.send_fanout_to(&targets, req, ctx.clone(), merge)
			.await
		{
			Ok(resp) => resp,
			Err(e) => {
				tracing::warn!(error = %e, "ensure_flat_task_routes_loaded: internal tasks/list failed");
				return Ok(());
			},
		};
		let _ = crate::http::read_resp_body(resp).await;
		Ok(())
	}

	/// Populate `flat_prompt_routes` via an internal `prompts/list` if it is empty.
	///
	/// Mirrors [`Self::ensure_flat_tool_routes_loaded`]: multi-replica sessions can resume on a
	/// pod with an empty prompt route index after list-but-before-get.
	async fn ensure_flat_prompt_routes_loaded(
		&self,
		ctx: &IncomingRequestContext,
	) -> Result<(), UpstreamError> {
		if !self.mcp_rewrite.flat() {
			return Ok(());
		}
		if !self.flat_prompt_routes.read().is_empty() {
			return Ok(());
		}
		#[cfg(feature = "adobe")]
		let targets = self
			.capabilities
			.upstreams_with_prompts(&self.all_target_names());
		#[cfg(not(feature = "adobe"))]
		let targets = self.all_target_names();
		if targets.is_empty() {
			return Ok(());
		}
		let merge = self.merge_prompts();
		let list_req: ListPromptsRequest = ListPromptsRequest::default();
		let req = JsonRpcRequest::new(RequestId::Number(-1), ClientRequest::ListPromptsRequest(list_req));
		let resp = match self
			.send_fanout_to(&targets, req, ctx.clone(), merge)
			.await
		{
			Ok(resp) => resp,
			Err(e) => {
				tracing::warn!(error = %e, "ensure_flat_prompt_routes_loaded: internal prompts/list failed");
				return Ok(());
			},
		};
		let _ = crate::http::read_resp_body(resp).await;
		Ok(())
	}

	/// Resolve a client prompt name to `(target, upstream_name)`.
	pub async fn resolve_prompt_call(
		&self,
		client_name: &str,
		ctx: &IncomingRequestContext,
	) -> Result<(String, String), UpstreamError> {
		if self.mcp_rewrite.flat() {
			self.ensure_flat_prompt_routes_loaded(ctx).await?;
			let routes = self.flat_prompt_routes.read();
			return self.mcp_rewrite.resolve_flat_prompt(
				client_name,
				Some(&routes),
				&self.all_target_names(),
			);
		}
		let (target, exposed) = self.parse_resource_name(client_name)?;
		let upstream = self
			.mcp_rewrite
			.target(target)
			.map(|t| t.resolve_upstream_prompt(exposed))
			.unwrap_or_else(|| exposed.to_string());
		Ok((target.to_string(), upstream))
	}

	/// Resolve a client task id to `(target, upstream_task_id)` for `tasks/get`, `tasks/result`,
	/// and `tasks/cancel`.
	///
	/// Prefix federation: `target_upstreamId` parses directly. Flat federation: bare ids use
	/// [`flat_task_routes`] (populated by `tasks/list` and `tools/call` create). Single-backend
	/// passthrough applies in both modes.
	#[cfg(feature = "adobe")]
	pub fn resolve_task_call(
		&self,
		client_id: &str,
	) -> Result<(String, String), UpstreamError> {
		if self.mcp_rewrite.flat() && self.upstreams.default_target_name.is_none() {
			let routes = self.flat_task_routes.read();
			return self.mcp_rewrite.resolve_flat_task(
				client_id,
				Some(&routes),
				&self.all_target_names(),
			);
		}
		let mut iter = self.upstreams.iter_named();
		let first = iter.next().map(|(name, _)| name);
		let count = self.upstreams.size();
		let single = if iter.next().is_none() { first } else { None };
		multiplex_naming::resolve_client_task_id(
			self.upstreams.default_target_name.as_ref(),
			client_id,
			count,
			single.as_deref(),
		)
	}

	pub fn get_sessions(&self) -> Option<Vec<MCPSession>> {
		let mut sessions = Vec::with_capacity(self.upstreams.size());
		for (_, us) in self.upstreams.iter_named() {
			sessions.push(us.get_session_state()?);
		}
		Some(sessions)
	}

	pub fn set_sessions(&self, sessions: Vec<MCPSession>) -> anyhow::Result<()> {
		if sessions.iter().all(|session| session.target_name.is_none()) {
			if sessions.len() != self.upstreams.size() {
				anyhow::bail!(
					"session count {} did not match initialized upstreams {}",
					sessions.len(),
					self.upstreams.size()
				);
			}
			for ((_, us), session) in self.upstreams.iter_named().zip(sessions) {
				us.set_session_id(session.session.as_deref(), session.backend);
			}
			return Ok(());
		}

		if sessions.iter().any(|session| session.target_name.is_none()) {
			anyhow::bail!("mixed keyed and unkeyed MCP session state is unsupported");
		}

		// Target-keyed resume is intentionally strict: if the initialized target set changed,
		// failing the resume is safer than binding persisted session state to the wrong target.
		let mut by_target = HashMap::with_capacity(sessions.len());
		for session in sessions {
			let target_name = session
				.target_name
				.clone()
				.expect("checked all sessions are target-keyed above");
			if by_target.insert(target_name.clone(), session).is_some() {
				anyhow::bail!("duplicate persisted session for target {target_name}");
			}
		}

		if by_target.len() != self.upstreams.size() {
			anyhow::bail!(
				"persisted target count {} did not match initialized upstreams {}",
				by_target.len(),
				self.upstreams.size()
			);
		}

		for (target_name, us) in self.upstreams.iter_named() {
			let session = by_target
				.remove(target_name.as_str())
				.ok_or_else(|| anyhow::anyhow!("missing persisted session for target {target_name}"))?;
			us.set_session_id(session.session.as_deref(), session.backend);
		}
		Ok(())
	}
	pub fn is_multiplexing(&self) -> bool {
		self.upstreams.is_multiplexing
	}

	fn build_guardrails_ctx(
		&self,
		r: &JsonRpcRequest<ClientRequest>,
		ctx: &IncomingRequestContext,
		backends: Vec<String>,
	) -> Option<GuardrailsCtx> {
		let ext = self.mcp_guardrails.as_ref()?;
		let method = r.request.method().to_string();
		if !ext.runs_response(&method) {
			// we only need an GuardrailsCtx for response-phase guardrails hooks
			return None;
		}
		Some(GuardrailsCtx {
			ext: ext.clone(),
			method,
			mcp: ctx
				.extensions()
				.get::<crate::mcp::guardrails::GuardrailsRequestMcpInfo>()
				.map(|info| info.0.clone())
				.unwrap_or_else(|| guardrails_mcp_info(&r.request)),
			backends,
			client: self.policy_client.clone(),
			req_ctx: Arc::new(ctx.clone()),
		})
	}

	pub(crate) async fn run_guardrails_call_request<P: serde::de::DeserializeOwned>(
		&self,
		ext_ctx: &mut crate::mcp::guardrails::CallRequestCtx<'_>,
		ctx: &mut IncomingRequestContext,
	) -> Result<Option<P>, UpstreamError> {
		use crate::mcp::guardrails::Outcome;
		let Some(ext) = self.mcp_guardrails.as_ref() else {
			return Ok(None);
		};
		let method = ext_ctx.method;
		match crate::mcp::guardrails::run_call_request::<P>(ext, ext_ctx, ctx, &self.policy_client)
			.await
		{
			Outcome::Pass => Ok(None),
			Outcome::Mutated(p) => {
				tracing::debug!(method, "mcpGuardrails: request mutated");
				Ok(Some(p))
			},
			Outcome::Reject(rej) => {
				#[cfg(feature = "adobe")]
				{
					if let crate::mcp::guardrails::McpDenialEnvelope::JsonRpc(ref error) = rej.envelope {
						tracing::debug!(
							method,
							code = error.code.0,
							message = %error.message,
							"mcpGuardrails: request rejected",
						);
					} else {
						tracing::debug!(method, "mcpGuardrails: request rejected with ToolResult");
					}
				}
				#[cfg(not(feature = "adobe"))]
				{
					tracing::debug!(
						method,
						code = rej.code.0,
						message = %rej.message,
						"mcpGuardrails: request rejected",
					);
				}
				Err(UpstreamError::McpGuardrails(rej))
			},
		}
	}

	pub(crate) async fn maybe_run_guardrails_call_request<P>(
		&self,
		backend: &str,
		method: &str,
		params: &mut P,
		ctx: &mut IncomingRequestContext,
	) -> Result<(), UpstreamError>
	where
		P: serde::Serialize + serde::de::DeserializeOwned,
	{
		let Some(ext) = self.mcp_guardrails.as_ref() else {
			return Ok(());
		};
		// Skip the (potentially expensive) params serialization when this method
		// has no request-phase hook configured.
		if !ext.runs_request(method) {
			return Ok(());
		}
		let params_b = serde_json::to_vec(&*params)
			.map_err(|e| UpstreamError::InvalidRequest(format!("serialize {method} params: {e}")))?;
		let params_bytes = bytes::Bytes::from(params_b);
		ctx.extensions_mut().insert(
			crate::mcp::guardrails::GuardrailsRequestMcpInfo(crate::mcp::guardrails::build_mcp_info(
				method,
				Some(&params_bytes),
			)),
		);
		let backends = [backend.to_string()];
		if let Some(p) = self
			.run_guardrails_call_request::<P>(
				&mut crate::mcp::guardrails::CallRequestCtx {
					backends: &backends,
					method,
					params: Some(params_bytes),
				},
				ctx,
			)
			.await?
		{
			*params = p;
		}
		Ok(())
	}

	#[cfg(feature = "adobe")]
	pub fn default_target_name(&self) -> Option<String> {
		self.upstreams.default_target_name.clone()
	}

	pub fn merge_tools(&self) -> Box<MergeFn> {
		let policies = self.policies.clone();
		let rewrite = self.mcp_rewrite.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = rewrite.flat();
		let flat_tool_routes = self.flat_tool_routes.clone();
		Box::new(move |streams, cel| {
			let mut route_entries = Vec::new();
			let mut tools = streams
				.into_iter()
				.flat_map(|(server_name, s)| {
					let tools = match s {
						ServerResult::ListToolsResult(ltr) => ltr.tools,
						_ => vec![],
					};
					let target_rules = rewrite.target(server_name.as_str());
					tools
						.into_iter()
						.filter(|t| {
							policies.validate(
								&rbac::ResourceType::Tool(rbac::ResourceId::new(
									server_name.to_string(),
									t.name.to_string(),
								)),
								cel,
							)
						})
						.map(|mut t| {
							let upstream_name = t.name.to_string();
							if let Some(tr) = target_rules {
								apply_tool_rewrite(&mut t, &tr.tools);
							}
							let exposed = if flat {
								t.name.to_string()
							} else {
								multiplex_naming::resource_name(
									default_target_name.as_ref(),
									server_name.as_str(),
									&t.name,
								)
							};
							if flat {
								route_entries.push((
									server_name.to_string(),
									upstream_name,
									exposed.clone(),
								));
							}
							t.name = Cow::Owned(exposed);
							#[cfg(feature = "adobe")]
							crate::mcp::mcp_apps::routing::rewrite_tool_ui_meta(
								default_target_name.as_ref(),
								server_name.as_str(),
								&mut t.meta,
							);
							t
						})
						.collect_vec()
				})
				.collect_vec();
			if flat {
				*flat_tool_routes.write() = build_flat_tool_route_index(route_entries);
				tools = filter_flat_tool_collisions(tools);
			}
			Ok(
				ListToolsResult {
					tools,
					next_cursor: None,
					meta: None,
				}
				.into(),
			)
		})
	}

	pub fn merge_initialize(&self, pv: ProtocolVersion, multiplexing: bool) -> Box<MergeFn> {
		let resource_subscribe = self.upstreams.stateful();
		#[cfg(feature = "adobe")]
		let capabilities = self.capabilities.clone();
		let server = self.mcp_rewrite.server.clone();
		Box::new(move |s, _cel| {
			if !multiplexing {
				// Happy case: we can forward everything
				let res = s.into_iter().next().and_then(|(_, r)| match r {
					ServerResult::InitializeResult(ir) => Some(ir),
					_ => None,
				});
				if let Some(ir) = res {
					return Ok(ir.into());
				}
				// If we got here in FailOpen mode, it means the only target failed.
				// Return a default info response to keep the client session alive.
				return Ok(Self::get_info(pv, multiplexing, resource_subscribe, Vec::new(), server.clone()).into());
			}

			// Multiplexing is more complex. We need to find the lowest protocol version
			// that all servers support and merge instructions from all upstreams.
			let mut lowest_version = pv;
			let mut upstream_instructions: Vec<(String, String)> = Vec::new();

			for (server_name, v) in s {
				if let ServerResult::InitializeResult(r) = v {
					#[cfg(feature = "adobe")]
					if multiplexing {
						capabilities.store(server_name.as_str(), r.capabilities.clone());
					}
					if r.protocol_version.to_string() < lowest_version.to_string() {
						lowest_version = r.protocol_version;
					}
					if let Some(instructions) = r.instructions
						&& !instructions.is_empty()
					{
						upstream_instructions.push((server_name.to_string(), instructions));
					}
				}
			}

			Ok(Self::get_info(lowest_version, multiplexing, resource_subscribe, upstream_instructions, server.clone()).into())
		})
	}

	pub fn merge_prompts(&self) -> Box<MergeFn> {
		let policies = self.policies.clone();
		let rewrite = self.mcp_rewrite.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = rewrite.flat();
		let flat_prompt_routes = self.flat_prompt_routes.clone();
		Box::new(move |streams, cel| {
			let mut route_entries = Vec::new();
			let mut prompts = streams
				.into_iter()
				.flat_map(|(server_name, s)| {
					let prompts = match s {
						ServerResult::ListPromptsResult(lpr) => lpr.prompts,
						_ => vec![],
					};
					let target_rules = rewrite.target(server_name.as_str());
					prompts
						.into_iter()
						.filter(|p| {
							policies.validate(
								&rbac::ResourceType::Prompt(rbac::ResourceId::new(
									server_name.to_string(),
									p.name.to_string(),
								)),
								cel,
							)
						})
						.map(|mut p| {
							let upstream_name = p.name.clone();
							if let Some(tr) = target_rules {
								apply_prompt_rewrite(&mut p, &tr.prompts);
							}
							let exposed = if flat {
								p.name.clone()
							} else {
								multiplex_naming::resource_name(
									default_target_name.as_ref(),
									server_name.as_str(),
									&p.name,
								)
							};
							if flat {
								route_entries.push((
									server_name.to_string(),
									upstream_name,
									exposed.clone(),
								));
							}
							p.name = exposed;
							p
						})
						.collect_vec()
				})
				.collect_vec();
			if flat {
				*flat_prompt_routes.write() = build_flat_prompt_route_index(route_entries);
				prompts = filter_flat_prompt_collisions(prompts);
			}
			Ok(
				ListPromptsResult {
					prompts,
					next_cursor: None,
					meta: None,
				}
				.into(),
			)
		})
	}
	pub fn merge_resources(&self) -> Box<MergeFn> {
		let policies = self.policies.clone();
		let rewrite = self.mcp_rewrite.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = rewrite.flat();
		Box::new(move |streams, cel| {
			let mut resources = streams
				.into_iter()
				.flat_map(|(server_name, s)| {
					let resources = match s {
						ServerResult::ListResourcesResult(lrr) => lrr.resources,
						_ => vec![],
					};
					let target_rules = rewrite.target(server_name.as_str());
					resources
						.into_iter()
						.filter(|r| {
							policies.validate(
								&rbac::ResourceType::Resource(rbac::ResourceId::new(
									server_name.to_string(),
									r.uri.to_string(),
								)),
								cel,
							)
						})
						.map(|mut r| {
							if let Some(tr) = target_rules {
								apply_resource_rewrite(&mut r, &tr.resources);
							}
							apply_multiplex_to_listed_resource(
								default_target_name.as_ref(),
								server_name.as_str(),
								r,
								flat,
							)
						})
						.collect_vec()
				})
				.collect_vec();
			if flat {
				resources = filter_flat_resource_collisions(resources);
			}
			Ok(
				ListResourcesResult {
					resources,
					next_cursor: None,
					meta: None,
				}
				.into(),
			)
		})
	}
	pub fn merge_resource_templates(&self) -> Box<MergeFn> {
		let policies = self.policies.clone();
		let rewrite = self.mcp_rewrite.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = rewrite.flat();
		Box::new(move |streams, cel| {
			let mut resource_templates = streams
				.into_iter()
				.flat_map(|(server_name, s)| {
					let resource_templates = match s {
						ServerResult::ListResourceTemplatesResult(lrr) => lrr.resource_templates,
						_ => vec![],
					};
					resource_templates
						.into_iter()
						.filter(|rt| {
							policies.validate(
								&rbac::ResourceType::Resource(rbac::ResourceId::new(
									server_name.to_string(),
									rt.uri_template.to_string(),
								)),
								cel,
							)
						})
						.map(|rt| {
							apply_multiplex_to_resource_template(
								default_target_name.as_ref(),
								server_name.as_str(),
								rt,
								flat,
							)
						})
						.collect_vec()
				})
				.collect_vec();
			if flat {
				resource_templates = filter_flat_resource_template_collisions(resource_templates);
			}
			Ok(
				ListResourceTemplatesResult {
					resource_templates,
					next_cursor: None,
					meta: None,
				}
				.into(),
			)
		})
	}
	pub fn merge_empty(&self) -> Box<MergeFn> {
		Box::new(move |_, _cel| Ok(rmcp::model::ServerResult::empty(())))
	}
	pub async fn send_single(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		service_name: &str,
		mcp_log: Option<AsyncLog<MCPInfo>>,
	) -> Result<Response, UpstreamError> {
		let id = r.id.clone();
		let Ok(us) = self.upstreams.get(service_name) else {
			return Err(UpstreamError::InvalidRequest(format!(
				"unknown service {service_name}"
			)));
		};
		let guardrails = self.build_guardrails_ctx(&r, &ctx, vec![service_name.to_string()]);
		let raw_stream = match us.generic_stream(r, &ctx).await {
			Ok(s) => s,
			Err(e) => {
				// Adobe-only: classify + stash the upstream error_type for
				// mcp_upstream_errors_total (emitted at the log.rs finalize site).
				#[cfg(feature = "adobe")]
				if let Some(log) = &mcp_log {
					if let Some(t) = crate::metrics::adobe_metrics::classify_upstream_error(&e) {
						log.non_atomic_mutate(|i| i.set_upstream_error(t));
					}
					let et = super::classify_upstream_error_for_cel(&e);
					let msg = super::sanitize_error_message(&e.to_string());
					log.non_atomic_mutate(|i| i.stamp_error(et, msg, None));
				}
				return Err(e);
			},
		};
		let stream = self.rewrite_outbound_server_messages(service_name, raw_stream);

		#[cfg(feature = "adobe")]
		{
			// Guardrails (decision 2) wrap the stream first; the adobe build then applies
			// federated multiplex outbound rewriting via messages_to_response_mapped.
			let default_mux = self.upstreams.default_target_name.clone();
			let flat = self.mcp_rewrite.flat();
			let tn = service_name.to_string();
			let map = crate::mcp::federation_outbound::map_mux_outbound_message(default_mux, flat, tn);
			return match guardrails {
				Some(guardrails) => {
					messages_to_response_mapped(id, wrap_with_guardrails(stream, guardrails), mcp_log, map)
				},
				None => messages_to_response_mapped(id, stream, mcp_log, map),
			};
		}
		#[cfg(not(feature = "adobe"))]
		{
			match guardrails {
				Some(guardrails) => {
					messages_to_response(id, wrap_with_guardrails(stream, guardrails), mcp_log)
				},
				None => messages_to_response(id, stream, mcp_log),
			}
		}
	}
	pub async fn send_fanout_deletion(
		&self,
		ctx: IncomingRequestContext,
	) -> Result<Response, UpstreamError> {
		let futs: Vec<_> = self
			.upstreams
			.iter_named()
			.map(|(name, con)| {
				let ctx = &ctx;
				async move { (name, con.delete(ctx).await) }
			})
			.collect();

		let fut_results = futures::future::join_all(futs).await;

		for (name, result) in fut_results {
			match result {
				Ok(_) => {},
				Err(e) => {
					if self.upstreams.failure_mode == FailureMode::FailOpen {
						warn!(
							"upstream '{}' failed during deletion, skipping: {}",
							name, e
						);
					} else {
						return Err(e);
					}
				},
			}
		}
		Ok(accepted_response())
	}
	pub async fn send_fanout_get(
		&self,
		ctx: IncomingRequestContext,
	) -> Result<Response, UpstreamError> {
		let mut streams = Vec::new();

		let futs: Vec<_> = self
			.upstreams
			.iter_named()
			.map(|(name, con)| {
				let ctx = &ctx;
				async move { (name, con.get_event_stream(ctx).await) }
			})
			.collect();

		let fut_results = futures::future::join_all(futs).await;

		for (name, result) in fut_results {
			match result {
				Ok(s) => {
				#[cfg(feature = "adobe")]
					let s = {
						let default_mux = self.upstreams.default_target_name.clone();
						let flat = self.mcp_rewrite.flat();
						let upstream = name.clone();
						s.map_each(crate::mcp::federation_outbound::map_mux_outbound_message(
							default_mux,
							flat,
							upstream.to_string(),
						))
					};
				// In non-Adobe builds, rewrite resource subscription notification URIs for multiplexing.
				// Adobe builds use map_mux_outbound_message above which already handles this.
				#[cfg(not(feature = "adobe"))]
				let s = self.rewrite_outbound_server_messages(name.as_str(), s);
					streams.push((name, s));
				},
				Err(e) => {
					if self.upstreams.failure_mode == FailureMode::FailOpen {
						let is_405 = if let UpstreamError::Http(ClientError::Status(ref r)) = e
							&& r.status() == StatusCode::METHOD_NOT_ALLOWED
						{
							true
						} else {
							false
						};
						if !is_405 {
							// per spec, a 405 is a valid response to say a GET stream is not supported so avoid log spam.
							warn!("upstream '{}' failed for GET stream, skipping: {}", name, e);
						} else {
							debug!("upstream '{}' failed for GET stream, skipping: {}", name, e);
						}
					} else {
						return Err(e);
					}
				},
			}
		}

		if streams.is_empty() {
			// FailClosed: unreachable — InitializeRequest would have failed with NoBackends.
			// FailOpen: keep the SSE connection open so legacy SSE clients do not immediately
			// reconnect in a tight loop after all upstream GET streams disappear.
			return messages_to_response(RequestId::Number(0), Messages::pending(), None);
		}

		let ms = mergestream::MergeStream::new_without_merge(streams, self.upstreams.failure_mode);
		messages_to_response(RequestId::Number(0), ms, None)
	}

	pub async fn send_fanout(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		mut ctx: IncomingRequestContext,
		merge: Box<MergeFn>,
	) -> Result<Response, UpstreamError> {
		let id = r.id.clone();
		let mut streams = Vec::new();
		let method = r.request.method().to_string();
		let method = method.as_str();

		// service_names for the single fanout-wide mcpGuardrails hook: every backend this call
		// fans out to (just the one name when there is a single backend).
		let service_names = self.mcp_guardrails.as_ref().map(|_| {
			self
				.upstreams
				.iter_named()
				.map(|(n, _)| n.to_string())
				.collect::<Vec<_>>()
		});

		// Request-phase hook runs once for the whole client call. params is None for
		// fanout (no body to rewrite); header/metadata side effects apply to the single
		// shared ctx forwarded to every upstream. A reject fails the whole call.
		if let Some(ext) = self.mcp_guardrails.as_ref() {
			// params is None, so mutations are discarded unparsed and the params
			// type is never used.
			let outcome = crate::mcp::guardrails::run_call_request::<serde_json::Value>(
				ext,
				&mut crate::mcp::guardrails::CallRequestCtx {
					backends: service_names.as_deref().unwrap_or_default(),
					method,
					params: None,
				},
				&mut ctx,
				&self.policy_client,
			)
			.await;
			if let crate::mcp::guardrails::Outcome::Reject(rej) = outcome {
				return Err(UpstreamError::McpGuardrails(rej));
			}
		}

		let futs: Vec<_> = self
			.upstreams
			.iter_named()
			.map(|(name, con)| {
				let r = r.clone();
				let ctx = &ctx;
				async move { (name, con.generic_stream(r, ctx).await) }
			})
			.collect();

		let fut_results = futures::future::join_all(futs).await;

		let cel = CelExecWrapper::new(ctx.as_request().map(|_| ()));
		for (name, result) in fut_results {
			match result {
				Ok(s) => {
					let s = self.rewrite_outbound_server_messages(name.as_str(), s);
					streams.push((name, s));
				},
				Err(e) => {
					if self.upstreams.failure_mode == FailureMode::FailOpen {
						warn!("upstream '{}' failed during fanout, skipping: {}", name, e);
					} else {
						#[cfg(feature = "adobe")]
						{
							warn!("upstream '{}' failed during fanout (fail-closed): {}", name, e);
							return Err(UpstreamError::FanoutError {
								name: name.to_string(),
								source: Box::new(e),
							});
						}
						#[cfg(not(feature = "adobe"))]
						return Err(e);
					}
				},
			}
		}

		if streams.is_empty() {
			// Unlike GET fanout, ordinary request fanout does not have a transport-level
			// "stay connected" fallback, and most MCP methods do not have a safe generic
			// synthetic success response. By the time we get here, every initialized
			// upstream has failed this request, so we surface that as an error even in
			// FailOpen rather than inventing a method-specific response.
			return Err(UpstreamError::InvalidRequest(
				"no upstreams available".to_string(),
			));
		}

		let ms =
			mergestream::MergeStream::new(streams, id.clone(), merge, cel, self.upstreams.failure_mode);

		// Response-phase hook runs once on the merged (muxed) result.
		match service_names.and_then(|sn| self.build_guardrails_ctx(&r, &ctx, sn)) {
			Some(guardrails) => messages_to_response(id, wrap_with_guardrails(ms, guardrails), None),
			None => messages_to_response(id, ms, None),
		}
	}

	pub fn parse_resource_uri(&self, uri: &str) -> Result<(String, String), UpstreamError> {
		#[cfg(feature = "adobe")]
		{
			crate::mcp::mcp_apps::routing::parse_resource_uri_mixed(
				self.upstreams.default_target_name.as_ref(),
				uri,
			)
		}
		#[cfg(not(feature = "adobe"))]
		{
			multiplex_naming::parse_multiplex_resource_uri(
				self.upstreams.default_target_name.as_ref(),
				uri,
			)
		}
	}

	#[cfg(feature = "adobe")]
	pub async fn send_single_map_response<F>(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		service_name: &str,
		map_msg: F,
		mcp_log: Option<AsyncLog<MCPInfo>>,
	) -> Result<Response, UpstreamError>
	where
		F: FnMut(&mut ServerJsonRpcMessage) + Send + 'static,
	{
		let Ok(us) = self.upstreams.get(service_name) else {
			return Err(UpstreamError::InvalidRequest(format!(
				"unknown service {service_name}"
			)));
		};
		let id = r.id.clone();
		let guardrails = self.build_guardrails_ctx(&r, &ctx, vec![service_name.to_string()]);
		let stream = match us.generic_stream(r, &ctx).await {
			Ok(s) => s,
			Err(e) => {
				// Adobe-only: classify + stash the upstream error_type for
				// mcp_upstream_errors_total (emitted at the log.rs finalize site).
				if let Some(log) = &mcp_log {
					if let Some(t) = crate::metrics::adobe_metrics::classify_upstream_error(&e) {
						log.non_atomic_mutate(|i| i.set_upstream_error(t));
					}
					let et = super::classify_upstream_error_for_cel(&e);
					let msg = super::sanitize_error_message(&e.to_string());
					log.non_atomic_mutate(|i| i.stamp_error(et, msg, None));
				}
				return Err(e);
			},
		};
		let stream = map_server_messages(stream, map_msg);

		match guardrails {
			Some(guardrails) => {
				messages_to_response(id, wrap_with_guardrails(stream, guardrails), mcp_log)
			},
			None => messages_to_response(id, stream, mcp_log),
		}
	}

	#[cfg(feature = "adobe")]
	pub async fn send_single_map_response_cancellable<F>(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		service_name: &str,
		map_msg: F,
		mcp_log: Option<AsyncLog<MCPInfo>>,
		in_flight: crate::mcp::session::InFlightRegistry,
	) -> Result<Response, UpstreamError>
	where
		F: FnMut(&mut ServerJsonRpcMessage) + Send + 'static,
	{
		let Ok(us) = self.upstreams.get(service_name) else {
			return Err(UpstreamError::InvalidRequest(format!(
				"unknown service {service_name}"
			)));
		};
		let id = r.id.clone();
		let guardrails = self.build_guardrails_ctx(&r, &ctx, vec![service_name.to_string()]);
		let stream = match us.generic_stream(r, &ctx).await {
			Ok(s) => s,
			Err(e) => {
				if let Some(log) = &mcp_log {
					if let Some(t) = crate::metrics::adobe_metrics::classify_upstream_error(&e) {
						log.non_atomic_mutate(|i| i.set_upstream_error(t));
					}
					let et = super::classify_upstream_error_for_cel(&e);
					let msg = super::sanitize_error_message(&e.to_string());
					log.non_atomic_mutate(|i| i.stamp_error(et, msg, None));
				}
				return Err(e);
			},
		}
		.register_cancellable(in_flight, id.clone(), service_name.to_string());
		let stream = map_server_messages(stream, map_msg);

		match guardrails {
			Some(guardrails) => {
				messages_to_response(id, wrap_with_guardrails(stream, guardrails), mcp_log)
			},
			None => messages_to_response(id, stream, mcp_log),
		}
	}

	pub async fn send_notification(
		&self,
		r: JsonRpcNotification<ClientNotification>,
		ctx: IncomingRequestContext,
	) -> Result<Response, UpstreamError> {
		let futs: Vec<_> = self
			.upstreams
			.iter_named()
			.map(|(name, con)| {
				let notification = r.notification.clone();
				let ctx = &ctx;
				async move { (name, con.generic_notification(notification, ctx).await) }
			})
			.collect();

		let fut_results = futures::future::join_all(futs).await;

		for (name, result) in fut_results {
			match result {
				Ok(_) => {},
				Err(e) => {
					if self.upstreams.failure_mode == FailureMode::FailOpen {
						warn!(
							"upstream '{}' failed during notification, skipping: {}",
							name, e
						);
					} else {
						return Err(e);
					}
				},
			}
		}

		Ok(accepted_response())
	}

	pub async fn send_notification_single(
		&self,
		r: ClientNotification,
		ctx: IncomingRequestContext,
		service_name: &str,
	) -> Result<Response, UpstreamError> {
		let Ok(us) = self.upstreams.get(service_name) else {
			return Err(UpstreamError::InvalidRequest(format!(
				"unknown service {service_name}"
			)));
		};
		us.generic_notification(r, &ctx).await?;
		Ok(accepted_response())
	}

	/// Forward a client-originated JSON-RPC message (response/error) to one upstream target.
	#[cfg(feature = "adobe")]
	pub async fn send_message_single(
		&self,
		message: ClientJsonRpcMessage,
		ctx: IncomingRequestContext,
		service_name: &str,
	) -> Result<Response, UpstreamError> {
		let Ok(us) = self.upstreams.get(service_name) else {
			return Err(UpstreamError::InvalidRequest(format!(
				"unknown service {service_name}"
			)));
		};
		us.generic_client_message(message, &ctx).await?;
		Ok(accepted_response())
	}

	fn get_info(
		pv: ProtocolVersion,
		multiplexing: bool,
		resource_subscribe: bool,
		upstream_instructions: Vec<(String, String)>,
		server: Option<CompiledServerRewrite>,
	) -> rmcp::model::ServerInfo {
		crate::mcp::mcp_apps::server_info::build_server_info(
			pv,
			multiplexing,
			resource_subscribe,
			upstream_instructions,
			server,
		)
	}

	pub fn all_target_names(&self) -> Vec<String> {
		self
			.upstreams
			.iter_named()
			.map(|(n, _)| n.to_string())
			.collect()
	}

	/// Fan out a request only to named upstreams (capability-filtered lists/tasks).
	///
	/// Unlike [`Self::send_fanout`], when **every** filtered upstream fails or the filter
	/// yields no targets, this still runs the merge on an empty result stream so list-style
	/// methods return an empty merged payload (e.g. no tasks) instead of
	/// `InvalidRequest("no upstreams available")`.
	pub async fn send_fanout_to(
		&self,
		targets: &[String],
		r: JsonRpcRequest<ClientRequest>,
		mut ctx: IncomingRequestContext,
		merge: Box<MergeFn>,
	) -> Result<Response, UpstreamError> {
		let id = r.id.clone();
		let method = r.request.method().to_string();
		let method = method.as_str();
		// Request-phase mcpGuardrails hook (decision 2): runs once for the federated call so
		// any metadata it injects into `ctx` is visible to the merge/list authz below. Backends
		// are the fanned-to targets; a reject fails the whole call. Mirrors send_fanout.
		if let Some(ext) = self.mcp_guardrails.as_ref() {
			let outcome = crate::mcp::guardrails::run_call_request::<serde_json::Value>(
				ext,
				&mut crate::mcp::guardrails::CallRequestCtx {
					backends: targets,
					method,
					params: None,
				},
				&mut ctx,
				&self.policy_client,
			)
			.await;
			if let crate::mcp::guardrails::Outcome::Reject(rej) = outcome {
				return Err(UpstreamError::McpGuardrails(rej));
			}
		}
		// Per-request CEL context, derived AFTER the request hook so injected metadata is
		// captured; the MergeFn closure receives it at invocation (upstream #1842 model).
		let cel = CelExecWrapper::new(ctx.as_request().map(|_| ()));
		let target_set: std::collections::HashSet<&str> = targets.iter().map(String::as_str).collect();
		let mut streams = Vec::new();

		let futs: Vec<_> = self
			.upstreams
			.iter_named()
			.filter(|(name, _)| target_set.contains(name.as_str()))
			.map(|(name, con)| {
				let r = r.clone();
				let ctx = &ctx;
				async move { (name, con.generic_stream(r, ctx).await) }
			})
			.collect();

		let fut_results = futures::future::join_all(futs).await;

		for (name, result) in fut_results {
			match result {
				Ok(s) => streams.push((name, s)),
				Err(e) => {
					if self.upstreams.failure_mode == FailureMode::FailOpen {
						warn!("upstream '{}' failed during fanout, skipping: {}", name, e);
					} else {
						#[cfg(feature = "adobe")]
						{
							warn!("upstream '{}' failed during fanout (fail-closed): {}", name, e);
							return Err(UpstreamError::FanoutError {
								name: name.to_string(),
								source: Box::new(e),
							});
						}
						#[cfg(not(feature = "adobe"))]
						return Err(e);
					}
				},
			}
		}

		if streams.is_empty() {
			let ms =
				mergestream::MergeStream::new(vec![], id.clone(), merge, cel, self.upstreams.failure_mode);
			return messages_to_response(id, ms, None);
		}

		let ms =
			mergestream::MergeStream::new(streams, id.clone(), merge, cel, self.upstreams.failure_mode);
		// Response-phase mcpGuardrails hook on the merged federated result (decision 2:
		// guardrails apply to federated/multiplexed list & read results, mirroring send_fanout).
		match self.build_guardrails_ctx(&r, &ctx, targets.to_vec()) {
			Some(guardrails) => messages_to_response(id, wrap_with_guardrails(ms, guardrails), None),
			None => messages_to_response(id, ms, None),
		}
	}
}

#[cfg(feature = "adobe")]
impl Relay {
	pub fn merge_tasks(&self) -> Box<MergeFn> {
		use rmcp::model::ListTasksResult;
		let policies = self.policies.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = self.mcp_rewrite.flat();
		let flat_task_routes = self.flat_task_routes.clone();
		Box::new(move |streams, cel| {
			let mut route_entries = Vec::new();
			let mut tasks = streams
				.into_iter()
				.flat_map(|(server_name, s)| {
					let tasks = match s {
						ServerResult::ListTasksResult(ltr) => ltr.tasks,
						_ => vec![],
					};
					tasks
						.into_iter()
						.filter(|t| {
							policies.validate(
								&rbac::ResourceType::Task(rbac::ResourceId::new(
									server_name.to_string(),
									t.task_id.to_string(),
								)),
								cel,
							)
						})
						.map(|mut t| {
							let upstream_id = t.task_id.to_string();
							let exposed = if flat {
								upstream_id.clone()
							} else if default_target_name.is_none() {
								multiplex_naming::wrap_client_task_id(
									default_target_name.as_ref(),
									server_name.as_str(),
									&upstream_id,
								)
							} else {
								upstream_id.clone()
							};
							if flat && default_target_name.is_none() {
								route_entries.push((
									server_name.to_string(),
									upstream_id,
									exposed.clone(),
								));
							}
							t.task_id = exposed;
							t
						})
						.collect_vec()
				})
				.collect_vec();
			if flat && default_target_name.is_none() {
				// First-wins merge into the existing index — preserves routes that
				// `record_flat_task_route` added on `tools/call` create when the
				// upstream's `tasks/list` hasn't surfaced the new task yet.
				let new_index = build_flat_task_route_index(route_entries);
				let mut routes = flat_task_routes.write();
				for (k, v) in new_index {
					routes.entry(k).or_insert(v);
				}
				tasks = filter_flat_task_collisions(tasks);
			}
			Ok(ListTasksResult::new(tasks).into())
		})
	}
}

fn messages_to_response(
	id: RequestId,
	stream: impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static,
	mcp_log: Option<AsyncLog<MCPInfo>>,
) -> Result<Response, UpstreamError> {
	messages_to_response_mapped(id, stream, mcp_log, |_: &mut ServerJsonRpcMessage| {})
}

#[cfg(feature = "adobe")]
fn map_server_messages<F>(
	stream: impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static,
	mut map_msg: F,
) -> impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static
where
	F: FnMut(&mut ServerJsonRpcMessage) + Send + 'static,
{
	use futures_util::StreamExt;
	stream.map(move |rpc| {
		rpc.map(|mut msg| {
			map_msg(&mut msg);
			msg
		})
	})
}

fn messages_to_response_mapped<F>(
	id: RequestId,
	stream: impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static,
	mcp_log: Option<AsyncLog<MCPInfo>>,
	mut map_msg: F,
) -> Result<Response, UpstreamError>
where
	F: FnMut(&mut ServerJsonRpcMessage) + Send + 'static,
{
	use futures_util::StreamExt;
	let request_id = id.clone();
	let mut captured_terminal = false;
	let stream = stream.map(move |rpc| {
		let (mut msg, capture_terminal_for_msg) = match rpc {
			Ok(rpc) => (rpc, true),
			Err(e) => (
				ServerJsonRpcMessage::error(ErrorData::internal_error(e.to_string(), None), id.clone()),
				false,
			),
		};
		map_msg(&mut msg);
		if capture_terminal_for_msg
			&& !captured_terminal
			&& let Some(log) = mcp_log.as_ref()
		{
			captured_terminal = capture_terminal_mcp_payload(log, &request_id, &msg);
		}
		// TODO: is it ok to have no event_id here?
		ServerSseMessage {
			event_id: None,
			message: Arc::new(msg),
		}
	});
	Ok(mcp::session::sse_stream_response(stream, None))
}

pub fn setup_request_log(
	http: Parts,
	span_name: &str,
) -> (SpanWriteOnDrop, AsyncLog<MCPInfo>, CelExecWrapper) {
	let log = http
		.extensions
		.get::<AsyncLog<MCPInfo>>()
		.cloned()
		.unwrap_or_default();

	let tracer = http
		.extensions
		.get::<SpanWriter>()
		.cloned()
		.unwrap_or_default();
	let cel = CelExecWrapper::new(::http::Request::from_parts(http, ()));
	let _span = tracer.start(span_name.to_string());
	(_span, log, cel)
}

pub(crate) struct GuardrailsCtx {
	pub ext: Arc<crate::mcp::guardrails::McpGuardrails>,
	pub method: String,
	pub mcp: crate::mcp::MCPInfo,
	pub backends: Vec<String>,
	pub client: PolicyClient,
	pub req_ctx: Arc<IncomingRequestContext>,
}

fn guardrails_mcp_info(request: &ClientRequest) -> crate::mcp::MCPInfo {
	let mut info = crate::mcp::MCPInfo {
		method_name: Some(request.method().to_string()),
		..Default::default()
	};
	match request {
		ClientRequest::CallToolRequest(r) => {
			info.set_tool(String::new(), r.params.name.to_string());
			info.capture_call_arguments(r.params.arguments.clone());
		},
		ClientRequest::GetPromptRequest(r) => {
			info.set_prompt(String::new(), r.params.name.clone());
		},
		ClientRequest::ReadResourceRequest(r) => {
			info.set_resource(String::new(), r.params.uri.clone());
		},
		_ => {},
	}
	info
}

fn wrap_with_guardrails(
	stream: impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static,
	guardrails: GuardrailsCtx,
) -> impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static {
	use futures_util::StreamExt;
	let guardrails = Arc::new(guardrails);
	stream.then(move |rpc| {
		let ctx = guardrails.clone();
		async move {
			match rpc {
				Ok(mut rpc) => {
					if let Some(scrubbed) = apply_guardrails_response_intercept(&ctx, &rpc).await {
						rpc = scrubbed;
					}
					Ok(rpc)
				},
				Err(e) => Err(e),
			}
		}
	})
}

// Upstream's plain SSE wrapper (#1842). Adobe routes all responses through
// `messages_to_response_mapped`, which builds the SSE stream inline, so this helper is
// currently unreferenced but kept to minimize divergence from upstream.
#[allow(dead_code)]
fn into_sse_stream(
	request_id: RequestId,
	stream: impl Stream<Item = Result<ServerJsonRpcMessage, ClientError>> + Send + 'static,
	mcp_log: Option<AsyncLog<MCPInfo>>,
) -> impl Stream<Item = ServerSseMessage> + Send + 'static {
	use futures_util::StreamExt;
	let mut captured_terminal = false;
	stream.map(move |rpc| {
		let r = match rpc {
			Ok(rpc) => {
				if !captured_terminal && let Some(log) = mcp_log.as_ref() {
					captured_terminal = capture_terminal_mcp_payload(log, &request_id, &rpc);
				}
				rpc
			},
			Err(e) => ServerJsonRpcMessage::error(
				ErrorData::internal_error(e.to_string(), None),
				request_id.clone(),
			),
		};
		// TODO: is it ok to have no event_id here?
		ServerSseMessage {
			event_id: None,
			message: Arc::new(r),
		}
	})
}

async fn apply_guardrails_response_intercept(
	ctx: &GuardrailsCtx,
	msg: &ServerJsonRpcMessage,
) -> Option<ServerJsonRpcMessage> {
	use crate::mcp::guardrails::Outcome;
	// The stream is request-scoped, so the only Response on it is the terminal.
	let ServerJsonRpcMessage::Response(resp) = msg else {
		return None;
	};
	let json: bytes::Bytes = match serde_json::to_vec(&resp.result) {
		Ok(v) => v.into(),
		Err(e) => {
			// Fail the response rather than skip a hook the operator configured,
			// matching the request side's handling of serialize failures.
			tracing::warn!(error = %e, "mcpGuardrails: failed to serialize result for inspection");
			return Some(ServerJsonRpcMessage::error(
				ErrorData::internal_error(format!("mcpGuardrails: serialize result: {e}"), None),
				resp.id.clone(),
			));
		},
	};
	match crate::mcp::guardrails::run_response(
		&ctx.ext,
		&ctx.method,
		&ctx.backends,
		json,
		&ctx.req_ctx,
		Some(&ctx.mcp),
		&ctx.client,
	)
	.await
	{
		Outcome::Pass => None,
		Outcome::Mutated(new_result) => {
			Some(ServerJsonRpcMessage::response(new_result, resp.id.clone()))
		},
		Outcome::Reject(rej) => Some(crate::mcp::guardrails::denial_to_server_message(
			rej,
			resp.id.clone(),
		)),
	}
}

fn capture_terminal_mcp_payload(
	log: &AsyncLog<MCPInfo>,
	request_id: &RequestId,
	message: &ServerJsonRpcMessage,
) -> bool {
	match message {
		ServerJsonRpcMessage::Response(response) if response.id == *request_id => {
			match &response.result {
				ServerResult::CallToolResult(result) => {
					if result.is_error == Some(true) {
						#[cfg(feature = "adobe")]
						{
							let msg = result
								.content
								.first()
								.and_then(|c| c.raw.as_text())
								.map(|t| super::sanitize_error_message(&t.text))
								.unwrap_or_default();
							log.non_atomic_mutate(|mcp| {
								mcp.stamp_error("upstream_tool_error", msg, None);
							});
						}
					} else {
						#[cfg(feature = "adobe")]
						log.non_atomic_mutate(|mcp| mcp.stamp_success());
					}
					log.non_atomic_mutate(|mcp| mcp.capture_call_result(result));
				},
				_ => {
					#[cfg(feature = "adobe")]
					log.non_atomic_mutate(|mcp| mcp.stamp_success());
				},
			}
			true
		},
		ServerJsonRpcMessage::Error(error) if error.id == *request_id => {
			#[cfg(feature = "adobe")]
			{
				let err_msg = super::sanitize_error_message(&error.error.message);
				let code = error.error.code.0;
				log.non_atomic_mutate(|mcp| {
					mcp.stamp_error("upstream_tool_error", err_msg, Some(code));
					mcp.capture_call_error(&error.error);
				});
			}
			#[cfg(not(feature = "adobe"))]
			log.non_atomic_mutate(|mcp| mcp.capture_call_error(&error.error));
			true
		},
		_ => false,
	}
}

fn accepted_response() -> Response {
	::http::Response::builder()
		.status(StatusCode::ACCEPTED)
		.body(crate::http::Body::empty())
		.expect("valid response")
}

#[cfg(test)]
mod tests {
	use futures_util::stream;
	use rmcp::model::{CallToolResult, ListToolsResult};
	use serde_json::json;

	use super::*;

	#[tokio::test]
	async fn messages_to_response_captures_first_matching_tool_result() {
		let log = AsyncLog::default();
		let mut info = MCPInfo::default();
		info.set_tool("mcp".to_string(), "echo".to_string());
		log.store(Some(info));

		let stream = stream::iter(vec![
			Ok(ServerJsonRpcMessage::response(
				ServerResult::ListToolsResult(ListToolsResult {
					tools: vec![],
					next_cursor: None,
					meta: None,
				}),
				RequestId::Number(1),
			)),
			Ok(ServerJsonRpcMessage::response(
				ServerResult::CallToolResult(CallToolResult::structured(json!({
					"status": "ok",
				}))),
				RequestId::Number(42),
			)),
			Ok(ServerJsonRpcMessage::error(
				ErrorData::internal_error("later error", None),
				RequestId::Number(42),
			)),
		]);

		let response = messages_to_response(RequestId::Number(42), stream, Some(log.clone())).unwrap();
		let _ = crate::http::read_resp_body(response).await.unwrap();

		let info = log.take().unwrap();
		assert_eq!(
			info.tool.as_ref().unwrap().result.as_ref().unwrap()["structuredContent"]["status"],
			"ok"
		);
		assert!(info.tool.as_ref().unwrap().error.is_none());
	}

	#[tokio::test]
	async fn messages_to_response_ignores_transport_errors_before_result() {
		let log = AsyncLog::default();
		let mut info = MCPInfo::default();
		info.set_tool("mcp".to_string(), "echo".to_string());
		log.store(Some(info));

		let stream = stream::iter(vec![
			Err(ClientError::new(anyhow::anyhow!("boom"))),
			Ok(ServerJsonRpcMessage::response(
				ServerResult::CallToolResult(CallToolResult::structured(json!({
					"status": "ok",
				}))),
				RequestId::Number(7),
			)),
		]);
		let response = messages_to_response(RequestId::Number(7), stream, Some(log.clone())).unwrap();
		let _ = crate::http::read_resp_body(response).await.unwrap();

		let info = log.take().unwrap();
		assert_eq!(
			info.tool.as_ref().unwrap().result.as_ref().unwrap()["structuredContent"]["status"],
			"ok"
		);
		assert!(info.tool.as_ref().unwrap().error.is_none());
	}

	#[tokio::test]
	async fn messages_to_response_captures_json_rpc_error() {
		let log = AsyncLog::default();
		let mut info = MCPInfo::default();
		info.set_tool("mcp".to_string(), "echo".to_string());
		log.store(Some(info));

		let stream = stream::iter(vec![Ok(ServerJsonRpcMessage::error(
			ErrorData::internal_error("boom", None),
			RequestId::Number(7),
		))]);
		let response = messages_to_response(RequestId::Number(7), stream, Some(log.clone())).unwrap();
		let _ = crate::http::read_resp_body(response).await.unwrap();

		let info = log.take().unwrap();
		assert!(info.tool.as_ref().unwrap().result.is_none());
		assert_eq!(
			info.tool.as_ref().unwrap().error.as_ref().unwrap()["code"],
			-32603
		);
		assert!(
			info.tool.as_ref().unwrap().error.as_ref().unwrap()["message"]
				.as_str()
				.unwrap()
				.contains("boom")
		);
	}
}
