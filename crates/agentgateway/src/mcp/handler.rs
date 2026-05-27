use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

use agent_core::version::BuildInfo;
use futures_core::Stream;
use http::StatusCode;
use http::request::Parts;
use itertools::Itertools;
use rmcp::ErrorData;
use rmcp::model::{
	ClientNotification, ClientRequest, Implementation, JsonRpcNotification, JsonRpcRequest,
	ListPromptsResult, ListResourceTemplatesResult, ListResourcesResult, ListToolsResult,
	ProtocolVersion, RequestId, ServerCapabilities, ServerInfo, ServerJsonRpcMessage,
	ServerNotification, ServerResult,
};
use tracing::{debug, warn};

use crate::http::Response;
use crate::http::sessionpersistence::MCPSession;
use crate::mcp;
use crate::mcp::mergestream::{MergeFn, Messages};
use crate::mcp::multiplex_naming;
use crate::mcp::rbac::{CelExecWrapper, McpAuthorizationSet};
use crate::mcp::rewrite::{
	apply_prompt_rewrite, apply_resource_rewrite, apply_tool_rewrite, build_flat_tool_route_index,
	filter_flat_prompt_collisions, filter_flat_resource_collisions,
	filter_flat_resource_template_collisions, filter_flat_tool_collisions,
	CompiledServerRewrite, McpRewriteSet, UpstreamInstructionsMode,
};
#[cfg(feature = "adobe")]
use crate::mcp::rewrite::filter_flat_task_collisions;
use crate::mcp::router::McpBackendGroup;
use crate::mcp::streamablehttp::ServerSseMessage;
use crate::mcp::upstream::{IncomingRequestContext, UpstreamError};
use crate::mcp::{ClientError, FailureMode, MCPInfo, mergestream, rbac, upstream};
use crate::proxy::httpproxy::PolicyClient;
use crate::telemetry::log::{AsyncLog, SpanWriteOnDrop, SpanWriter};

#[cfg_attr(feature = "adobe", allow(dead_code))]
fn resource_uri(default_target_name: Option<&String>, target: &str, uri: &str) -> String {
	if default_target_name.is_none() {
		// Transform URI to service+scheme:// format for multiplexing
		// e.g., "http://example.com" becomes "service+http://example.com"
		if let Some(scheme_end) = uri.find("://") {
			let (scheme, rest) = uri.split_at(scheme_end);
			format!("{target}+{scheme}{rest}")
		} else {
			// URI must have a scheme - if not, return as-is and let validation handle it
			uri.to_string()
		}
	} else {
		uri.to_string()
	}
}

fn rewrite_resource_update_message(
	default_target_name: Option<&String>,
	target: &str,
	mut message: ServerJsonRpcMessage,
) -> ServerJsonRpcMessage {
	if let ServerJsonRpcMessage::Notification(notification) = &mut message
		&& let ServerNotification::ResourceUpdatedNotification(resource_updated) =
			&mut notification.notification
	{
		resource_updated.params.uri = resource_uri(
			default_target_name,
			target,
			resource_updated.params.uri.as_str(),
		);
	}
	message
}

fn apply_multiplex_to_listed_resource(
	default_target_name: Option<&String>,
	server_name: &str,
	mut r: rmcp::model::Resource,
	flat: bool,
) -> rmcp::model::Resource {
	#[cfg(feature = "adobe")]
	{
		r.uri = crate::mcp::mcp_apps::routing::wrap_resource_uri_mixed(
			default_target_name,
			server_name,
			&r.uri,
		);
		if !flat {
			r.name = multiplex_naming::resource_name(default_target_name, server_name, &r.name);
		}
		r
	}
	#[cfg(not(feature = "adobe"))]
	{
		let _ = flat;
		r.uri = resource_uri(default_target_name, server_name, &r.uri);
		r
	}
}

fn apply_multiplex_to_resource_template(
	default_target_name: Option<&String>,
	server_name: &str,
	mut rt: rmcp::model::ResourceTemplate,
	flat: bool,
) -> rmcp::model::ResourceTemplate {
	#[cfg(feature = "adobe")]
	{
		rt.uri_template = crate::mcp::mcp_apps::routing::wrap_resource_template_uri_mixed(
			default_target_name,
			server_name,
			&rt.uri_template,
		);
		if !flat {
			rt.name = multiplex_naming::resource_name(default_target_name, server_name, &rt.name);
		}
		rt
	}
	#[cfg(not(feature = "adobe"))]
	{
		let _ = (default_target_name, server_name, flat);
		rt
	}
}

#[derive(Debug, Clone)]
pub struct Relay {
	pub(crate) upstreams: Arc<upstream::UpstreamGroup>,
	pub policies: McpAuthorizationSet,
	pub(crate) mcp_guardrails: Option<Arc<crate::mcp::guardrails::McpGuardrails>>,
	pub(crate) policy_client: PolicyClient,
	pub mcp_rewrite: McpRewriteSet,
	pub(crate) capabilities: Arc<crate::mcp::mcp_apps::capabilities::TargetCapabilities>,
	/// Populated by federated `tools/list` when `resourceNaming: Flat`; used by `tools/call`.
	flat_tool_routes: Arc<RwLock<HashMap<String, (String, String)>>>,
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
			capabilities: Arc::new(crate::mcp::mcp_apps::capabilities::TargetCapabilities::new()),
			flat_tool_routes: Arc::new(RwLock::new(HashMap::new())),
		})
	}
	pub fn with_policies(&self, policies: McpAuthorizationSet) -> Self {
		Self {
			upstreams: self.upstreams.clone(),
			policies,
			mcp_guardrails: self.mcp_guardrails.clone(),
			policy_client: self.policy_client.clone(),
			mcp_rewrite: self.mcp_rewrite.clone(),
			capabilities: self.capabilities.clone(),
			flat_tool_routes: self.flat_tool_routes.clone(),
		}
	}

	fn rewrite_outbound_server_messages(&self, target: &str, stream: Messages) -> Messages {
		let target = target.to_string();
		let default_target_name = self.upstreams.default_target_name.clone();
		stream.map_server_messages(move |message| {
			rewrite_resource_update_message(default_target_name.as_ref(), &target, message)
		})
	}

	pub fn parse_resource_name<'a, 'b: 'a>(
		&'a self,
		res: &'b str,
	) -> Result<(&'a str, &'b str), UpstreamError> {
		multiplex_naming::parse_resource_name(self.upstreams.default_target_name.as_ref(), res)
	}

	/// Resolve a client tool name to `(target, upstream_name)` for auth and upstream `tools/call`.
	pub fn resolve_tool_call(
		&self,
		client_name: &str,
	) -> Result<(String, String), UpstreamError> {
		if self.mcp_rewrite.flat() {
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

	/// Resolve a client prompt name to `(target, upstream_name)`.
	pub fn resolve_prompt_call(
		&self,
		client_name: &str,
	) -> Result<(String, String), UpstreamError> {
		if self.mcp_rewrite.flat() {
			return self.mcp_rewrite.resolve_flat_prompt(client_name);
		}
		let (target, exposed) = self.parse_resource_name(client_name)?;
		let upstream = self
			.mcp_rewrite
			.target(target)
			.map(|t| t.resolve_upstream_prompt(exposed))
			.unwrap_or_else(|| exposed.to_string());
		Ok((target.to_string(), upstream))
	}

	/// Resolve a Flat-mode client task id to `(target, upstream_task_id)`.
	#[cfg(feature = "adobe")]
	pub fn resolve_task_call(
		&self,
		client_id: &str,
	) -> Result<(String, String), UpstreamError> {
		if self.mcp_rewrite.flat() {
			let mut hits = Vec::new();
			for (name, _) in self.upstreams.iter_named() {
				hits.push((name.to_string(), client_id.to_string()));
			}
			return match hits.len() {
				0 => Err(UpstreamError::InvalidRequest(format!(
					"unknown flat task id: {client_id}"
				))),
				1 => Ok(hits.pop().unwrap()),
				_ => Err(UpstreamError::InvalidRequest(format!(
					"ambiguous flat task id: {client_id}"
				))),
			};
		}
		let (target, exposed) = self.parse_task_id(client_id)?;
		Ok((target, exposed))
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
				tracing::debug!(
					method,
					code = rej.code.0,
					message = %rej.message,
					"mcpGuardrails: request rejected",
				);
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
		let backends = [backend.to_string()];
		if let Some(p) = self
			.run_guardrails_call_request::<P>(
				&mut crate::mcp::guardrails::CallRequestCtx {
					backends: &backends,
					method,
					params: Some(params_b.into()),
				},
				ctx,
			)
			.await?
		{
			*params = p;
		}
		Ok(())
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
				return Ok(Self::get_info(pv, resource_subscribe, Vec::new(), server.clone()).into());
			}

			// Multiplexing is more complex. We need to find the lowest protocol version
			// that all servers support and merge instructions from all upstreams.
			let mut lowest_version = pv;
			let mut upstream_instructions: Vec<(String, String)> = Vec::new();

			for (server_name, v) in s {
				if let ServerResult::InitializeResult(r) = v {
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

			Ok(Self::get_info(lowest_version, resource_subscribe, upstream_instructions, server.clone()).into())
		})
	}

	pub fn merge_prompts(&self) -> Box<MergeFn> {
		let policies = self.policies.clone();
		let rewrite = self.mcp_rewrite.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = rewrite.flat();
		Box::new(move |streams, cel| {
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
							if let Some(tr) = target_rules {
								apply_prompt_rewrite(&mut p, &tr.prompts);
							}
							p.name = if flat {
								p.name.clone()
							} else {
								multiplex_naming::resource_name(
									default_target_name.as_ref(),
									server_name.as_str(),
									&p.name,
								)
							};
							p
						})
						.collect_vec()
				})
				.collect_vec();
			if flat {
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
		let stream =
			self.rewrite_outbound_server_messages(service_name, us.generic_stream(r, &ctx).await?);

		match guardrails {
			Some(guardrails) => {
				messages_to_response(id, wrap_with_guardrails(stream, guardrails), mcp_log)
			},
			None => messages_to_response(id, stream, mcp_log),
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
			return crate::mcp::mcp_apps::routing::parse_resource_uri_mixed(
				self.upstreams.default_target_name.as_ref(),
				uri,
			);
		}
		#[cfg(not(feature = "adobe"))]
		{
			multiplex_naming::parse_multiplex_resource_uri(
				self.upstreams.default_target_name.as_ref(),
				uri,
			)
		}
	}

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
		let id = r.id.clone();
		let Ok(us) = self.upstreams.get(service_name) else {
			return Err(UpstreamError::InvalidRequest(format!(
				"unknown service {service_name}"
			)));
		};
		let stream = us.generic_stream(r, &ctx).await?;

		messages_to_response_mapped(id, stream, mcp_log, map_msg)
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

	fn get_info(
		pv: ProtocolVersion,
		resource_subscribe: bool,
		upstream_instructions: Vec<(String, String)>,
		server: Option<CompiledServerRewrite>,
	) -> ServerInfo {
		let capabilities = {
			let mut builder = ServerCapabilities::builder()
				.enable_tools()
				.enable_tool_list_changed()
				.enable_prompts()
				.enable_prompts_list_changed()
				.enable_resources()
				.enable_resources_list_changed();
			if resource_subscribe {
				builder = builder.enable_resources_subscribe();
			}
			builder.build()
		};
		let gateway_preamble = server
			.as_ref()
			.and_then(|s| s.instructions.clone())
			.unwrap_or_else(|| {
				"This server is a gateway to a set of mcp servers. It is responsible for routing requests to the correct server and aggregating the results.".to_string()
			});
		let instructions = match server.as_ref().map(|s| s.upstream_instructions) {
			Some(UpstreamInstructionsMode::Replace) => Some(gateway_preamble),
			Some(UpstreamInstructionsMode::Prepend) => {
				if upstream_instructions.is_empty() {
					Some(gateway_preamble)
				} else {
					let mut merged = gateway_preamble;
					for (server_name, instruction) in &upstream_instructions {
						merged.push_str(&format!("\n\n[{server_name}]\n{instruction}"));
					}
					Some(merged)
				}
			},
			_ => {
				if upstream_instructions.is_empty() {
					Some(gateway_preamble)
				} else {
					let mut merged = gateway_preamble;
					for (server_name, instruction) in &upstream_instructions {
						merged.push_str(&format!("\n\n[{server_name}]\n{instruction}"));
					}
					Some(merged)
				}
			},
		};
		let server_name = server
			.as_ref()
			.and_then(|s| s.name.clone())
			.unwrap_or_else(|| "agentgateway".to_string());
		let server_version = server
			.as_ref()
			.and_then(|s| s.version.clone())
			.unwrap_or_else(|| BuildInfo::new().version.to_string());
		ServerInfo::new(capabilities)
			.with_protocol_version(pv)
			.with_server_info(Implementation::new(server_name, server_version))
			.with_instructions(instructions.unwrap_or_default())
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
		ctx: IncomingRequestContext,
		merge: Box<MergeFn>,
	) -> Result<Response, UpstreamError> {
		let id = r.id.clone();
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
						return Err(e);
					}
				},
			}
		}

		if streams.is_empty() {
			let ms =
				mergestream::MergeStream::new(vec![], id.clone(), merge, self.upstreams.failure_mode);
			return messages_to_response(id, ms, None);
		}

		let ms = mergestream::MergeStream::new(streams, id.clone(), merge, self.upstreams.failure_mode);
		messages_to_response(id, ms, None)
	}
}

#[cfg(feature = "adobe")]
impl Relay {
	pub fn parse_task_id(&self, id: &str) -> Result<(String, String), UpstreamError> {
		crate::mcp::mcp_apps::routing::parse_task_id(self.upstreams.default_target_name.as_ref(), id)
	}

	pub fn merge_tasks(&self, cel: CelExecWrapper) -> Box<MergeFn> {
		use rmcp::model::ListTasksResult;
		let policies = self.policies.clone();
		let rewrite = self.mcp_rewrite.clone();
		let default_target_name = self.upstreams.default_target_name.clone();
		let flat = rewrite.flat();
		Box::new(move |streams| {
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
								&cel,
							)
						})
						.map(|mut t| {
							if !flat {
								t.task_id = multiplex_naming::resource_name(
									default_target_name.as_ref(),
									server_name.as_str(),
									&t.task_id,
								);
							}
							t
						})
						.collect_vec()
				})
				.collect_vec();
			if flat {
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
	pub backends: Vec<String>,
	pub client: PolicyClient,
	pub req_ctx: Arc<IncomingRequestContext>,
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
		&ctx.client,
	)
	.await
	{
		Outcome::Pass => None,
		Outcome::Mutated(new_result) => {
			Some(ServerJsonRpcMessage::response(new_result, resp.id.clone()))
		},
		Outcome::Reject(rej) => Some(ServerJsonRpcMessage::error(rej, resp.id.clone())),
	}
}

fn capture_terminal_mcp_payload(
	log: &AsyncLog<MCPInfo>,
	request_id: &RequestId,
	message: &ServerJsonRpcMessage,
) -> bool {
	match message {
		ServerJsonRpcMessage::Response(response) if response.id == *request_id => {
			if let ServerResult::CallToolResult(result) = &response.result {
				log.non_atomic_mutate(|mcp| mcp.capture_call_result(result));
			}
			true
		},
		ServerJsonRpcMessage::Error(error) if error.id == *request_id => {
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
