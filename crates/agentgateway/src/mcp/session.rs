use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use ::http::StatusCode;
use ::http::header::CONTENT_TYPE;
use ::http::request::Parts;
use agent_core::version::BuildInfo;
use anyhow::anyhow;
use futures_util::StreamExt;
use headers::HeaderMapExt;
use rmcp::model::{
	ClientInfo, ClientJsonRpcMessage, ClientNotification, ClientRequest, ConstString, Implementation,
	InitializeRequest, JsonRpcRequest, ProtocolVersion, Reference, RequestId,
	ServerJsonRpcMessage,
};
#[cfg(feature = "adobe")]
use rmcp::model::ServerResult;
use rmcp::transport::common::http_header::{EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE};
use sse_stream::{KeepAlive, Sse, SseBody, SseStream};
use tokio::sync::mpsc::{Receiver, Sender};

use crate::http::Response;
#[cfg(feature = "adobe")]
use crate::mcp::federation_outbound::map_mux_outbound_message;
#[cfg(feature = "adobe")]
use crate::mcp::multiplex_naming::unwrap_server_request_id;
use crate::mcp::handler::{Relay, RelayInputs};
use crate::mcp::mergestream::Messages;
use crate::mcp::streamablehttp::{ServerSseMessage, StreamableHttpPostResponse};
use crate::mcp::upstream::{IncomingRequestContext, UpstreamError};
use crate::mcp::{ClientError, rbac};
use crate::proxy::ProxyError;
use crate::telemetry::log::{AsyncLog, SpanWriteOnDrop};
use crate::{mcp, *};

#[cfg(feature = "adobe")]
mod tasks;

#[cfg(feature = "adobe")]
pub(crate) type InFlightRegistry = Arc<
	std::sync::Mutex<HashMap<RequestId, (futures::stream::AbortHandle, String)>>,
>;

#[derive(Debug, Clone)]
pub struct Session {
	encoder: http::sessionpersistence::Encoder,
	relay: Arc<Relay>,
	pub id: Arc<str>,
	tx: Option<Sender<ServerJsonRpcMessage>>,
	#[cfg(feature = "adobe")]
	in_flight: InFlightRegistry,
}

#[cfg(feature = "adobe")]
fn new_in_flight_registry() -> InFlightRegistry {
	Arc::new(std::sync::Mutex::new(HashMap::new()))
}

#[derive(Debug, Clone)]
struct SessionEntry {
	session: Session,
	last_access: Instant,
	idle_ttl: Duration,
}

const SESSION_REAP_INTERVAL: Duration = Duration::from_secs(30);

impl Session {
	#[cfg(feature = "adobe")]
	pub(super) fn relay(&self) -> &Relay {
		self.relay.as_ref()
	}

	#[cfg(feature = "adobe")]
	fn cancel_in_flight(&self, request_id: &RequestId) -> Option<String> {
		let mut map = self.in_flight.lock().ok()?;
		let (handle, target) = map.remove(request_id)?;
		handle.abort();
		Some(target)
	}

	/// send a message to upstream server(s)
	pub async fn send(
		&mut self,
		parts: Parts,
		message: ClientJsonRpcMessage,
	) -> Result<Response, ProxyError> {
		let req_id = match &message {
			ClientJsonRpcMessage::Request(r) => Some(r.id.clone()),
			_ => None,
		};
		Self::handle_error(req_id, self.send_internal(parts, message).await).await
	}

	/// send a message to upstream server(s), when using stateless mode. In stateless mode, every message
	/// is wrapped in an InitializeRequest (except the actual InitializeRequest from the downstream).
	/// This ensures servers that require an InitializeRequest behave correctly.
	/// In the future, we may have a mode where we know the downstream is stateless as well, and can just forward as-is.
	pub async fn stateless_send_and_initialize(
		&mut self,
		parts: Parts,
		message: ClientJsonRpcMessage,
	) -> Result<Response, ProxyError> {
		let (req_id, request_type) = match &message {
			ClientJsonRpcMessage::Request(r) => (Some(r.id.clone()), Some(&r.request)),
			_ => (None, None),
		};
		let is_init = request_type.is_some_and(|r| matches!(r, ClientRequest::InitializeRequest(_)));
		if !is_init {
			let init_request = rmcp::model::InitializeRequest::new(get_client_info());
			// first, determine how widely to send the initialize
			match request_type {
				Some(ClientRequest::CallToolRequest(_)) | Some(ClientRequest::GetPromptRequest(_)) => {
					// Single-target methods only hit one backend, so initialize/initialized should be scoped
					// to that backend rather than fanning out.
					let name = match request_type {
						Some(ClientRequest::CallToolRequest(ctr)) => ctr.params.name.to_string(),
						Some(ClientRequest::GetPromptRequest(gpr)) => gpr.params.name.clone(),
						_ => unreachable!("match arm guarantees single-target request type"),
					};
					let ctx = IncomingRequestContext::new(&parts);
					let resolved = match request_type {
						Some(ClientRequest::CallToolRequest(_)) => {
							self.relay.resolve_tool_call(name.as_str(), &ctx).await
						},
						Some(ClientRequest::GetPromptRequest(_)) => {
							self.relay.resolve_prompt_call(name.as_str(), &ctx).await
						},
						_ => unreachable!("match arm guarantees single-target request type"),
					};
					let (service_name, _) = match resolved {
						Ok(v) => v,
						Err(err) => return Self::handle_error(req_id.clone(), Err(err)).await,
					};
					let res = self
						.send_init_single(parts.clone(), init_request, service_name.as_str())
						.await;
					if let Some(sessions) = self.relay.get_sessions() {
						let s = http::sessionpersistence::SessionState::MCP(
							http::sessionpersistence::MCPSessionState::new(sessions),
						);
						if let Ok(id) = s.encode(&self.encoder) {
							self.id = id.into();
						}
					}
					Self::handle_error(Some(RequestId::Number(0)), res).await?;
					// Now send the initialized notification
					let _ = Self::handle_error(
						None,
						self
							.send_initialized_notification_single(parts.clone(), service_name.as_str())
							.await,
					)
					.await?;
				},
				_ => {
					// We should fan out the initialize request to all MCP servers
					let _ = self
						.send(
							parts.clone(),
							ClientJsonRpcMessage::request(init_request.into(), RequestId::Number(0)),
						)
						.await?;
					let notification = ClientJsonRpcMessage::notification(
						rmcp::model::InitializedNotification {
							method: Default::default(),
							extensions: Default::default(),
						}
						.into(),
					);
					let _ = self.send(parts.clone(), notification).await?;
				},
			}
		}
		// Now we can send the message like normal (if it's tools/call, it'll go to the initialized target)
		self.send(parts, message).await
	}

	pub fn with_inputs(mut self, inputs: RelayInputs) -> Self {
		self.relay = Arc::new(self.relay.with_policies(inputs.policies));
		self
	}

	async fn authorize_prompt_request(
		&self,
		name: &str,
		method: &str,
		ctx: &IncomingRequestContext,
		span: &mut SpanWriteOnDrop,
		log: &AsyncLog<mcp::MCPInfo>,
		cel: &rbac::CelExecWrapper,
	) -> Result<(String, String), UpstreamError> {
		let (service_name, upstream_prompt) = self.relay.resolve_prompt_call(name, ctx).await?;
		span.rename_span(format!("{method} {service_name}"));
		log.non_atomic_mutate(|l| {
			l.set_prompt(service_name.clone(), upstream_prompt.clone());
		});
		if !self.relay.policies.validate(
			&rbac::ResourceType::Prompt(rbac::ResourceId::new(
				service_name.clone(),
				upstream_prompt.clone(),
			)),
			cel,
		) {
			return Err(UpstreamError::Authorization {
				resource_type: "prompt".to_string(),
				resource_name: name.to_string(),
			});
		}
		Ok((service_name, upstream_prompt))
	}

	#[cfg(not(feature = "adobe"))]
	fn authorize_resource_request(
		&self,
		service_name: &str,
		uri: &str,
		method: &str,
		span: &mut SpanWriteOnDrop,
		log: &AsyncLog<mcp::MCPInfo>,
		cel: &rbac::CelExecWrapper,
	) -> Result<(), UpstreamError> {
		span.rename_span(format!("{method} {service_name}"));
		log.non_atomic_mutate(|l| {
			l.set_resource(service_name.to_string(), uri.to_string());
		});
		if !self.relay.policies.validate(
			&rbac::ResourceType::Resource(rbac::ResourceId::new(
				service_name.to_string(),
				uri.to_string(),
			)),
			cel,
		) {
			return Err(UpstreamError::Authorization {
				resource_type: "resource".to_string(),
				resource_name: uri.to_string(),
			});
		}
		Ok(())
	}

	#[allow(clippy::too_many_arguments)]
	async fn authorize_with_ctx<P>(
		&self,
		backend: &str,
		method: &str,
		params: &mut P,
		ctx: &mut IncomingRequestContext,
		res: rbac::ResourceType,
		resource_type: &str,
		resource_name: &str,
	) -> Result<(), UpstreamError>
	where
		P: serde::Serialize + serde::de::DeserializeOwned,
	{
		// run guardrails before other policies, as it may add context to CEL
		self
			.relay
			.maybe_run_guardrails_call_request(backend, method, params, ctx)
			.await?;
		let cel = rbac::CelExecWrapper::new(ctx.as_request().map(|_| ()));
		if self.relay.policies.validate(&res, &cel) {
			Ok(())
		} else {
			Err(UpstreamError::Authorization {
				resource_type: resource_type.to_string(),
				resource_name: resource_name.to_string(),
			})
		}
	}

	fn authorize_federated_resource_uri(
		&self,
		uri: &str,
		method: &str,
		span: &mut SpanWriteOnDrop,
		log: &AsyncLog<mcp::MCPInfo>,
		cel: &rbac::CelExecWrapper,
	) -> Result<(String, String), UpstreamError> {
		let (target_name, original_uri) = self.relay.parse_resource_uri(uri)?;
		span.rename_span(format!("{method} {target_name}"));
		log.non_atomic_mutate(|l| {
			l.set_resource(target_name.clone(), original_uri.clone());
		});
		if !self.relay.policies.validate(
			&rbac::ResourceType::Resource(rbac::ResourceId::new(
				target_name.clone(),
				original_uri.clone(),
			)),
			cel,
		) {
			return Err(UpstreamError::Authorization {
				resource_type: "resource".to_string(),
				resource_name: uri.to_string(),
			});
		}
		Ok((target_name, original_uri))
	}

	async fn handle_read_resource_request(
		&self,
		mut r: JsonRpcRequest<ClientRequest>,
		mut ctx: IncomingRequestContext,
		method: &str,
		span: &mut SpanWriteOnDrop,
		log: &AsyncLog<mcp::MCPInfo>,
		cel: &rbac::CelExecWrapper,
	) -> Result<Response, UpstreamError> {
		let ClientRequest::ReadResourceRequest(rrr) = &mut r.request else {
			return Err(UpstreamError::InvalidRequest(
				"internal: expected ReadResourceRequest".to_string(),
			));
		};
		let uri = rrr.params.uri.clone();
		let (target_name, original_uri) =
			self.authorize_federated_resource_uri(&uri, method, span, log, cel)?;
		// Set upstream URI before guardrails so mcp.resource.uri in CEL sees the canonical
		// upstream URI, consistent with how tool/prompt names are handled.
		rrr.params.uri = original_uri;
		self
			.relay
			.maybe_run_guardrails_call_request(
				target_name.as_str(),
				mcp::guardrails::methods::RESOURCES_READ,
				&mut rrr.params,
				&mut ctx,
			)
			.await?;
		#[cfg(feature = "adobe")]
		{
			let default_mux = self.relay.default_target_name();
			let tn_for_map = target_name.clone();
			return self
				.relay
				.send_single_map_response(
					r,
					ctx,
					target_name.as_str(),
					move |msg| {
						if let ServerJsonRpcMessage::Response(jr) = msg
							&& let ServerResult::ReadResourceResult(rr) = &mut jr.result
						{
							crate::mcp::mcp_apps::routing::rewrap_read_resource_contents(
								default_mux.as_ref(),
								tn_for_map.as_str(),
								&mut rr.contents,
							);
						}
					},
					None,
				)
				.await;
		}
		#[cfg(not(feature = "adobe"))]
		{
			self
				.relay
				.send_single(r, ctx, target_name.as_str(), None)
				.await
		}
	}

	async fn handle_list_tools(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		_cel: rbac::CelExecWrapper,
	) -> Result<Response, UpstreamError> {
		#[cfg(feature = "adobe")]
		let targets = self.relay.capabilities.upstreams_with_tools(&self.relay.all_target_names());
		#[cfg(not(feature = "adobe"))]
		let targets = self.relay.all_target_names();
		self
			.relay
			.send_fanout_to(&targets, r, ctx, self.relay.merge_tools())
			.await
	}

	async fn handle_list_prompts(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		_cel: rbac::CelExecWrapper,
	) -> Result<Response, UpstreamError> {
		#[cfg(feature = "adobe")]
		let targets = self.relay.capabilities.upstreams_with_prompts(&self.relay.all_target_names());
		#[cfg(not(feature = "adobe"))]
		let targets = self.relay.all_target_names();
		self
			.relay
			.send_fanout_to(&targets, r, ctx, self.relay.merge_prompts())
			.await
	}

	async fn handle_list_resources(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		_cel: rbac::CelExecWrapper,
	) -> Result<Response, UpstreamError> {
		#[cfg(feature = "adobe")]
		let targets = self.relay.capabilities.upstreams_with_resources(&self.relay.all_target_names());
		#[cfg(not(feature = "adobe"))]
		let targets = self.relay.all_target_names();
		self
			.relay
			.send_fanout_to(&targets, r, ctx, self.relay.merge_resources())
			.await
	}

	async fn handle_list_resource_templates(
		&self,
		r: JsonRpcRequest<ClientRequest>,
		ctx: IncomingRequestContext,
		_cel: rbac::CelExecWrapper,
	) -> Result<Response, UpstreamError> {
		#[cfg(feature = "adobe")]
		let targets = self.relay.capabilities.upstreams_with_resources(&self.relay.all_target_names());
		#[cfg(not(feature = "adobe"))]
		let targets = self.relay.all_target_names();
		self
			.relay
			.send_fanout_to(&targets, r, ctx, self.relay.merge_resource_templates())
			.await
	}

	/// delete any active sessions
	pub async fn delete_session(&self, parts: Parts) -> Result<Response, ProxyError> {
		let ctx = IncomingRequestContext::new(&parts);
		let (_span, log, _cel) = mcp::handler::setup_request_log(parts, "delete_session");
		let session_id = self.id.to_string();
		log.non_atomic_mutate(|l| {
			// NOTE: l.method_name keep None to respect the metrics logic: not handle GET, DELETE.
			l.session_id = Some(session_id);
		});
		Self::handle_error(None, self.relay.send_fanout_deletion(ctx).await).await
	}

	/// forward_legacy_sse takes an upstream Response and forwards all messages to the SSE data stream.
	/// In SSE, POST requests always just get a 202 response and the messages go on a separate stream.
	/// Note: its plausible we could rewrite the rest of the proxy to return a more structured type than
	/// `Response` here, so we don't have to re-process it. However, since SSE is deprecated its best to
	/// optimize for the non-deprecated code paths; this works fine.
	pub async fn forward_legacy_sse(&self, resp: Response) -> Result<(), ClientError> {
		let Some(tx) = self.tx.clone() else {
			return Err(ClientError::new(anyhow!(
				"may only be called for SSE streams",
			)));
		};
		let content_type = resp.headers().get(CONTENT_TYPE);
		let sse = match content_type {
			Some(ct) if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) => {
				trace!("forward SSE got SSE stream response");
				let content_encoding = resp.headers().typed_get::<headers::ContentEncoding>();
				let (body, _encoding) =
					crate::http::compression::decompress_body(resp.into_body(), content_encoding.as_ref())
						.map_err(ClientError::new)?;
				let event_stream = SseStream::from_byte_stream(body.into_data_stream()).boxed();
				StreamableHttpPostResponse::Sse(event_stream, None)
			},
			Some(ct) if ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
				trace!("forward SSE got single JSON response");
				let message = json::from_response_body::<ServerJsonRpcMessage>(resp)
					.await
					.map_err(ClientError::new)?;
				StreamableHttpPostResponse::Json(message, None)
			},
			_ => {
				trace!("forward SSE got accepted, no action needed");
				return Ok(());
			},
		};
		let mut ms: Messages = sse.try_into()?;
		tokio::spawn(async move {
			while let Some(Ok(msg)) = ms.next().await {
				let Ok(()) = tx.send(msg).await else {
					return;
				};
			}
		});
		Ok(())
	}

	/// get_stream establishes a stream for server-sent messages
	pub async fn get_stream(&self, parts: Parts) -> Result<Response, ProxyError> {
		let ctx = IncomingRequestContext::new(&parts);
		let (_span, log, _cel) = mcp::handler::setup_request_log(parts, "get_stream");
		let session_id = self.id.to_string();
		log.non_atomic_mutate(|l| {
			// NOTE: l.method_name keep None to respect the metrics logic: which do not want to handle GET, DELETE.
			l.session_id = Some(session_id);
		});
		Self::handle_error(None, self.relay.send_fanout_get(ctx).await).await
	}

	async fn handle_error(
		req_id: Option<RequestId>,
		d: Result<Response, UpstreamError>,
	) -> Result<Response, ProxyError> {
		match d {
			Ok(r) => Ok(r),
			Err(UpstreamError::Http(ClientError::Status(resp))) => {
				let resp = http::SendDirectResponse::new(*resp)
					.await
					.map_err(ProxyError::Body)?;
				Err(mcp::Error::UpstreamError(Box::new(resp)).into())
			},
			Err(UpstreamError::Proxy(p)) => Err(p),
			Err(UpstreamError::Authorization {
				resource_type,
				resource_name,
			}) if req_id.is_some() => {
				Err(mcp::Error::Authorization(req_id.unwrap(), resource_type, resource_name).into())
			},
			Err(UpstreamError::McpGuardrails(rej)) if req_id.is_some() => {
				Err(mcp::Error::McpGuardrails(req_id.unwrap(), rej).into())
			},
			// TODO: this is too broad. We have a big tangle of errors to untangle though
			Err(e) => Err(mcp::Error::SendError(req_id, e.to_string()).into()),
		}
	}

	async fn send_init_single(
		&self,
		parts: Parts,
		mut init_request: InitializeRequest,
		service_name: &str,
	) -> Result<Response, UpstreamError> {
		let method = init_request.method.as_str().to_string();
		let ctx = IncomingRequestContext::new(&parts);
		let (_, log, _) = mcp::handler::setup_request_log(parts, &method);
		let session_id = self.id.to_string();
		log.non_atomic_mutate(|l| {
			l.method_name = Some(method.clone());
			l.session_id = Some(session_id);
		});

		self.strip_unsupported_client_capabilities(&mut init_request.params.capabilities);
		self
			.relay
			.send_single(
				JsonRpcRequest::new(RequestId::Number(0), init_request.into()),
				ctx,
				service_name,
				Some(log),
			)
			.await
	}

	/// Resolve the upstream target for a client-originated response/error/cancel.
	///
	/// Wrapped ids are client-echoed correlation tokens; routing is bounded to configured
	/// upstreams via [`Relay::send_message_single`]. Pending-request validation is omitted
	/// to keep federation stateless across gateway replicas.
	#[cfg(feature = "adobe")]
	fn resolve_client_response_target(
		relay: &Relay,
		id: &RequestId,
	) -> Result<(String, RequestId), UpstreamError> {
		if let Some(default) = relay.default_target_name() {
			return Ok((default, id.clone()));
		}
		if let Some((target, orig)) = unwrap_server_request_id(id) {
			return Ok((target, orig));
		}
		Err(UpstreamError::InvalidRequest(format!(
			"unknown request id for client response: {id}"
		)))
	}

	#[cfg(feature = "adobe")]
	async fn forward_client_message_to_upstream(
		&self,
		parts: Parts,
		method: &str,
		message: ClientJsonRpcMessage,
	) -> Result<Response, UpstreamError> {
		let (target, message) = match message {
			ClientJsonRpcMessage::Response(mut jr) => {
				let (target, orig_id) = Self::resolve_client_response_target(&self.relay, &jr.id)?;
				jr.id = orig_id;
				(target, ClientJsonRpcMessage::Response(jr))
			},
			ClientJsonRpcMessage::Error(mut je) => {
				let (target, orig_id) = Self::resolve_client_response_target(&self.relay, &je.id)?;
				je.id = orig_id;
				(target, ClientJsonRpcMessage::Error(je))
			},
			_ => {
				return Err(UpstreamError::InvalidRequest(
					"internal: expected client response or error".to_string(),
				));
			},
		};
		let ctx = IncomingRequestContext::new(&parts);
		let (_span, log, _cel) = mcp::handler::setup_request_log(parts, method);
		let session_id = self.id.to_string();
		log.non_atomic_mutate(|l| {
			l.method_name = Some(method.to_string());
			l.session_id = Some(session_id);
		});
		self.relay.send_message_single(message, ctx, target.as_str()).await
	}

	async fn send_initialized_notification_single(
		&self,
		parts: Parts,
		service_name: &str,
	) -> Result<Response, UpstreamError> {
		let initialized = rmcp::model::InitializedNotification {
			method: Default::default(),
			extensions: Default::default(),
		};
		let method = initialized.method.as_str().to_string();
		let ctx = IncomingRequestContext::new(&parts);
		let (_, log, _) = mcp::handler::setup_request_log(parts, &method);
		let session_id = self.id.to_string();
		log.non_atomic_mutate(|l| {
			l.method_name = Some(method.clone());
			l.session_id = Some(session_id);
		});

		self
			.relay
			.send_notification_single(initialized.into(), ctx, service_name)
			.await
	}

	async fn send_internal(
		&mut self,
		parts: Parts,
		message: ClientJsonRpcMessage,
	) -> Result<Response, UpstreamError> {
		// Sending a message entails fanning out the message to each upstream, and then aggregating the responses.
		// The responses may include any number of notifications on the same HTTP response, and then finish with the
		// response to the request.
		// To merge these, we use a MergeStream which will join all of the notifications together, and then apply
		// some per-request merge logic across all the responses.
		// For example, this may return [server1-notification, server2-notification, server2-notification, merge(server1-response, server2-response)].
		// It's very common to not have any notifications, though.
		match message {
			ClientJsonRpcMessage::Request(mut r) => {
				let method = r.request.method().to_string();
				let mut ctx = IncomingRequestContext::new(&parts);
				let (mut span, log, cel) = mcp::handler::setup_request_log(parts, &method);
				let session_id = self.id.to_string();
				log.non_atomic_mutate(|l| {
					l.method_name = Some(method.clone());
					l.session_id = Some(session_id);
				});
				match &mut r.request {
					ClientRequest::InitializeRequest(ir) => {
						self.strip_unsupported_client_capabilities(&mut ir.params.capabilities);

						let pv = ir.params.protocol_version.clone();
						let res = self
							.relay
							.send_fanout(
								r,
								ctx,
								self
									.relay
									.merge_initialize(pv, self.relay.is_multiplexing()),
							)
							.await;
						if let Some(sessions) = self.relay.get_sessions() {
							let s = http::sessionpersistence::SessionState::MCP(
								http::sessionpersistence::MCPSessionState::new(sessions),
							);
							if let Ok(id) = s.encode(&self.encoder) {
								self.id = id.into();
							}
						}
						res
					},
					ClientRequest::ListToolsRequest(_) => self.handle_list_tools(r, ctx, cel).await,
					// TODO(keithmattix): should we forward pings or should we do our own independent pings
					// as heuristic for the connection pool (and handle client pings as a local reply from agentgateway)?
					ClientRequest::PingRequest(_) | ClientRequest::SetLevelRequest(_) => {
						self
							.relay
							.send_fanout(r, ctx, self.relay.merge_empty())
							.await
					},
					ClientRequest::ListPromptsRequest(_) => self.handle_list_prompts(r, ctx, cel).await,
					ClientRequest::ListResourcesRequest(_) => self.handle_list_resources(r, ctx, cel).await,
					ClientRequest::ListResourceTemplatesRequest(_) => {
						self.handle_list_resource_templates(r, ctx, cel).await
					},
					ClientRequest::CallToolRequest(ctr) => {
						let name = ctr.params.name.clone();
						let (service_name, upstream_tool) =
							self.relay.resolve_tool_call(name.as_ref(), &ctx).await?;
						span.rename_span(format!("{method} {service_name}"));
						let call_arguments = ctr.params.arguments.clone();
						log.non_atomic_mutate(|l| {
							l.set_tool(service_name.clone(), upstream_tool.clone());
							l.capture_call_arguments(call_arguments);
						});
						// Set upstream name before guardrails so CEL (e.g. mcp.tool.name in rate-limit
						// descriptors) sees the canonical upstream name, consistent with RBAC rules.
						ctr.params.name = upstream_tool.clone().into();
						self
							.authorize_with_ctx(
								service_name.as_str(),
								mcp::guardrails::methods::TOOLS_CALL,
								&mut ctr.params,
								&mut ctx,
								rbac::ResourceType::Tool(rbac::ResourceId::new(
									service_name.to_string(),
									upstream_tool.to_string(),
								)),
								"tool",
								&name,
							)
							.await?;
						// Re-apply after authorize in case a remote guardrail returned Mutated params.
						ctr.params.name = upstream_tool.into();

						#[cfg(feature = "adobe")]
						{
							let default_mux = self.relay.default_target_name();
							let flat = self.relay.mcp_rewrite.flat();
							let target_for_map = service_name.to_string();
							self
								.relay
								.send_single_map_response_cancellable(
									r,
									ctx,
									service_name.as_str(),
									tasks::map_tools_call_outbound(
										self.relay().clone(),
										default_mux,
										flat,
										target_for_map,
									),
									Some(log.clone()),
									self.in_flight.clone(),
								)
								.await
						}
						#[cfg(not(feature = "adobe"))]
						{
							self
								.relay
								.send_single(r, ctx, service_name.as_str(), Some(log.clone()))
								.await
						}
					},
					ClientRequest::GetPromptRequest(gpr) => {
						let name = gpr.params.name.clone();
						let (service_name, upstream_prompt) =
							self.relay.resolve_prompt_call(name.as_str(), &ctx).await?;
						span.rename_span(format!("{method} {service_name}"));
						log.non_atomic_mutate(|l| {
							l.set_prompt(service_name.clone(), upstream_prompt.clone());
						});
						// Set upstream name before guardrails so CEL (e.g. mcp.prompt.name in rate-limit
						// descriptors) sees the canonical upstream name, consistent with RBAC rules.
						gpr.params.name = upstream_prompt.clone();
						self
							.authorize_with_ctx(
								service_name.as_str(),
								mcp::guardrails::methods::PROMPTS_GET,
								&mut gpr.params,
								&mut ctx,
								rbac::ResourceType::Prompt(rbac::ResourceId::new(
									service_name.to_string(),
									upstream_prompt.clone(),
								)),
								"prompt",
								&name,
							)
							.await?;
						// Re-apply after authorize in case a remote guardrail returned Mutated params.
						gpr.params.name = upstream_prompt;
						self.relay.send_single(r, ctx, service_name.as_str(), None).await
					},
					ClientRequest::ReadResourceRequest(_) => {
						self
							.handle_read_resource_request(r, ctx, &method, &mut span, &log, &cel)
							.await
					},
					#[cfg(not(feature = "adobe"))]
					ClientRequest::SubscribeRequest(sr) => {
						let uri = sr.params.uri.clone();
						let (service_name, original_uri) = self.relay.parse_resource_uri(&uri)?;
						self.authorize_resource_request(
							&service_name,
							&original_uri,
							&method,
							&mut span,
							&log,
							&cel,
						)?;
						sr.params.uri = original_uri;
						self.relay.send_single(r, ctx, service_name.as_str(), None).await
					},
					#[cfg(not(feature = "adobe"))]
					ClientRequest::UnsubscribeRequest(ur) => {
						let uri = ur.params.uri.clone();
						let (service_name, original_uri) = self.relay.parse_resource_uri(&uri)?;
						self.authorize_resource_request(
							&service_name,
							&original_uri,
							&method,
							&mut span,
							&log,
							&cel,
						)?;
						ur.params.uri = original_uri;
						self.relay.send_single(r, ctx, service_name.as_str(), None).await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::SubscribeRequest(srr) => {
						let uri = srr.params.uri.clone();
						let (target_name, original_uri) =
							self.authorize_federated_resource_uri(&uri, &method, &mut span, &log, &cel)?;
						srr.params.uri = original_uri;
						let default_mux = self.relay.default_target_name();
						let flat = self.relay.mcp_rewrite.flat();
						let tn = target_name.clone();
						self
							.relay
							.send_single_map_response(
								r,
								ctx,
								target_name.as_str(),
								map_mux_outbound_message(default_mux, flat, tn),
								None,
							)
							.await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::UnsubscribeRequest(urr) => {
						let uri = urr.params.uri.clone();
						let (target_name, original_uri) =
							self.authorize_federated_resource_uri(&uri, &method, &mut span, &log, &cel)?;
						urr.params.uri = original_uri;
						let default_mux = self.relay.default_target_name();
						let flat = self.relay.mcp_rewrite.flat();
						let tn = target_name.clone();
						self
							.relay
							.send_single_map_response(
								r,
								ctx,
								target_name.as_str(),
								map_mux_outbound_message(default_mux, flat, tn),
								None,
							)
							.await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::ListTasksRequest(_) => {
						tasks::handle_list_tasks(self, r, ctx, cel).await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::GetTaskInfoRequest(gtr) => {
						let task_id = gtr.params.task_id.clone();
						tasks::forward_task_rpc(self, r, task_id, ctx, &method, &mut span, &log, &cel)
							.await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::GetTaskResultRequest(gtr) => {
						let task_id = gtr.params.task_id.clone();
						tasks::forward_task_rpc(self, r, task_id, ctx, &method, &mut span, &log, &cel)
							.await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::CancelTaskRequest(ctr) => {
						let task_id = ctr.params.task_id.clone();
						tasks::forward_task_rpc(self, r, task_id, ctx, &method, &mut span, &log, &cel)
							.await
					},

					#[cfg(feature = "adobe")]
					ClientRequest::CustomRequest(_) => {
						Err(UpstreamError::InvalidMethod(r.request.method().to_string()))
					},

					#[cfg(not(feature = "adobe"))]
					ClientRequest::ListTasksRequest(_)
					| ClientRequest::GetTaskInfoRequest(_)
					| ClientRequest::GetTaskResultRequest(_)
					| ClientRequest::CancelTaskRequest(_)
					| ClientRequest::CustomRequest(_) => {
						Err(UpstreamError::InvalidMethod(r.request.method().to_string()))
					},
					ClientRequest::CompleteRequest(cr) => match &cr.params.r#ref {
						Reference::Prompt(prompt) => {
							let name = prompt.name.clone();
							let (service_name, prompt_name) = self
								.authorize_prompt_request(&name, &method, &ctx, &mut span, &log, &cel)
								.await?;
							cr.params.r#ref = Reference::for_prompt(prompt_name.to_string());
							self.relay.send_single(r, ctx, service_name.as_str(), None).await
						},
						Reference::Resource(resource) => {
							let uri = resource.uri.clone();
							let (target_name, original_uri) =
								self.authorize_federated_resource_uri(&uri, &method, &mut span, &log, &cel)?;
							cr.params.r#ref =
								Reference::Resource(rmcp::model::ResourceReference { uri: original_uri });
							self
								.relay
								.send_single(r, ctx, target_name.as_str(), None)
								.await
						},
					},
				}
			},
			ClientJsonRpcMessage::Notification(r) => {
				let method = match &r.notification {
					ClientNotification::CancelledNotification(r) => r.method.as_str(),
					ClientNotification::ProgressNotification(r) => r.method.as_str(),
					ClientNotification::InitializedNotification(r) => r.method.as_str(),
					ClientNotification::RootsListChangedNotification(r) => r.method.as_str(),
					ClientNotification::CustomNotification(r) => r.method.as_str(),
				};
				let ctx = IncomingRequestContext::new(&parts);
				let (_span, log, _cel) = mcp::handler::setup_request_log(parts, method);
				let session_id = self.id.to_string();
				log.non_atomic_mutate(|l| {
					l.method_name = Some(method.to_string());
					l.session_id = Some(session_id);
				});
				#[cfg(feature = "adobe")]
				{
					let mut notification = r.notification.clone();
					if let ClientNotification::CancelledNotification(cn) = &mut notification {
						if let Some(target) = self.cancel_in_flight(&cn.params.request_id) {
							return self
								.relay
								.send_notification_single(notification, ctx, target.as_str())
								.await;
						}
						if let Ok((target, orig_id)) =
							Self::resolve_client_response_target(&self.relay, &cn.params.request_id)
						{
							cn.params.request_id = orig_id;
							return self
								.relay
								.send_notification_single(notification, ctx, target.as_str())
								.await;
						}
					}
				}
				self.relay.send_notification(r, ctx).await
			},

			#[cfg(feature = "adobe")]
			ClientJsonRpcMessage::Response(jr) => {
				self
					.forward_client_message_to_upstream(parts, "client_response", ClientJsonRpcMessage::Response(jr))
					.await
			},
			#[cfg(feature = "adobe")]
			ClientJsonRpcMessage::Error(je) => {
				self
					.forward_client_message_to_upstream(parts, "client_error", ClientJsonRpcMessage::Error(je))
					.await
			},

			#[cfg(not(feature = "adobe"))]
			ClientJsonRpcMessage::Response(_) | ClientJsonRpcMessage::Error(_) => {
				Err(UpstreamError::InvalidRequest(
					"unsupported message type".to_string(),
				))
			},
		}
	}

	fn strip_unsupported_client_capabilities(
		&self,
		capabilities: &mut rmcp::model::ClientCapabilities,
	) {
		// Until server-to-client request routing is implemented, do not advertise
		// capabilities that require the proxy to route upstream requests back to
		// the downstream client and route the client's JSON-RPC response upstream.
		capabilities.roots = None;
		capabilities.sampling = None;
		// Adobe has federated elicitation routing (forward_client_message_to_upstream +
		// resolve_client_response_target), so preserve the capability in adobe builds.
		#[cfg(not(feature = "adobe"))]
		{
			capabilities.elicitation = None;
		}
	}
}

#[derive(Debug)]
pub struct SessionManager {
	encoder: http::sessionpersistence::Encoder,
	sessions: Arc<RwLock<HashMap<String, SessionEntry>>>,
	idle_reaper: OnceLock<tokio::task::AbortHandle>,
}

fn session_id() -> Arc<str> {
	uuid::Uuid::new_v4().to_string().into()
}

impl SessionManager {
	pub fn new(encoder: http::sessionpersistence::Encoder) -> Arc<Self> {
		Arc::new(Self {
			encoder,
			sessions: Arc::new(RwLock::new(HashMap::new())),
			idle_reaper: OnceLock::new(),
		})
	}

	pub fn ensure_idle_running(&self) {
		self
			.idle_reaper
			.get_or_init(|| tokio::spawn(run_idle_reaper(self.sessions.clone())).abort_handle());
	}

	pub fn get_session(&self, id: &str, builder: RelayInputs) -> Option<Session> {
		let mut sessions = self.sessions.write().ok()?;
		let entry = sessions.get_mut(id)?;
		entry.last_access = Instant::now();
		Some(entry.session.clone().with_inputs(builder))
	}

	pub fn get_or_resume_session(
		&self,
		id: &str,
		builder: RelayInputs,
	) -> Result<Option<Session>, mcp::Error> {
		if let Some(s) = self.sessions.write().expect("poisoned").get_mut(id) {
			s.last_access = Instant::now();
			return Ok(Some(s.session.clone().with_inputs(builder)));
		}
		let idle_ttl = builder.backend.session_idle_ttl;
		let d = http::sessionpersistence::SessionState::decode(id, &self.encoder)
			.map_err(|_| mcp::Error::InvalidSessionIdHeader)?;
		let http::sessionpersistence::SessionState::MCP(state) = d else {
			return Ok(None);
		};
		let relay = builder.build_new_connections()?;
		if let Err(err) = relay.set_sessions(state.sessions) {
			warn!("failed to resume session: {err}");
			return Ok(None);
		}

		let sess = Session {
			id: id.into(),
			relay: Arc::new(relay),
			tx: None,
			encoder: self.encoder.clone(),
			#[cfg(feature = "adobe")]
			in_flight: new_in_flight_registry(),
		};
		let mut sm = self.sessions.write().expect("write lock");
		sm.insert(
			id.to_string(),
			SessionEntry {
				session: sess.clone(),
				last_access: Instant::now(),
				idle_ttl,
			},
		);
		Ok(Some(sess))
	}

	/// create_session establishes an MCP session.
	pub fn create_session(&self, relay: Relay) -> Session {
		let id = session_id();

		// Do NOT insert yet
		Session {
			id: id.clone(),
			relay: Arc::new(relay),
			tx: None,
			encoder: self.encoder.clone(),
			#[cfg(feature = "adobe")]
			in_flight: new_in_flight_registry(),
		}
	}

	pub fn insert_session(&self, sess: Session, idle_ttl: Duration) {
		let mut sm = self.sessions.write().expect("write lock");
		sm.insert(
			sess.id.to_string(),
			SessionEntry {
				session: sess,
				last_access: Instant::now(),
				idle_ttl,
			},
		);
	}

	/// create_stateless_session creates a session for stateless mode.
	/// Unlike create_session, this does NOT register the session in the session manager.
	/// The caller is responsible for calling session.delete_session() when done
	/// to clean up upstream resources (e.g., stdio processes).
	pub fn create_stateless_session(&self, relay: Relay) -> Session {
		let id = session_id();
		Session {
			id,
			relay: Arc::new(relay),
			tx: None,
			encoder: self.encoder.clone(),
			#[cfg(feature = "adobe")]
			in_flight: new_in_flight_registry(),
		}
	}

	/// create_legacy_session establishes a legacy SSE session.
	/// These will have the ability to send messages to them via a channel.
	pub fn create_legacy_session(
		&self,
		relay: Relay,
		idle_ttl: Duration,
	) -> (Session, Receiver<ServerJsonRpcMessage>) {
		let (tx, rx) = tokio::sync::mpsc::channel(64);
		let id = session_id();
		let sess = Session {
			id: id.clone(),
			relay: Arc::new(relay),
			tx: Some(tx),
			encoder: self.encoder.clone(),
			#[cfg(feature = "adobe")]
			in_flight: new_in_flight_registry(),
		};
		let mut sm = self.sessions.write().expect("write lock");
		sm.insert(
			id.to_string(),
			SessionEntry {
				session: sess.clone(),
				last_access: Instant::now(),
				idle_ttl,
			},
		);
		(sess, rx)
	}

	pub async fn delete_session(&self, id: &str, parts: Parts) -> Option<Response> {
		let sess = {
			let mut sm = self.sessions.write().expect("write lock");
			sm.remove(id)?.session
		};
		// Swallow the error
		sess.delete_session(parts).await.ok()
	}
}

impl Drop for SessionManager {
	fn drop(&mut self) {
		if let Some(abort) = self.idle_reaper.take() {
			abort.abort();
		}
	}
}

async fn run_idle_reaper(sessions: Arc<RwLock<HashMap<String, SessionEntry>>>) {
	let mut ticker = tokio::time::interval(SESSION_REAP_INTERVAL);
	ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		ticker.tick().await;
		reap_expired_entries(&sessions);
	}
}

fn reap_expired_entries(sessions: &Arc<RwLock<HashMap<String, SessionEntry>>>) {
	let now = Instant::now();
	let mut guard = sessions.write().expect("write lock");
	let pre = guard.len();
	guard.retain(|_, entry| now.duration_since(entry.last_access) < entry.idle_ttl);
	let post = guard.len();
	if post < pre {
		tracing::debug!("reaped {} sessions", pre - post);
	}
}

#[derive(Debug, Clone)]
pub struct SessionDropper {
	sm: Arc<SessionManager>,
	s: Option<(Session, Parts)>,
}

/// Dropper returns a handle that, when dropped, removes the session
pub fn dropper(sm: Arc<SessionManager>, s: Session, parts: Parts) -> SessionDropper {
	SessionDropper {
		sm,
		s: Some((s, parts)),
	}
}

impl Drop for SessionDropper {
	fn drop(&mut self) {
		let Some((s, parts)) = self.s.take() else {
			return;
		};
		let mut sm = self.sm.sessions.write().expect("write lock");
		debug!("delete session {}", s.id);
		sm.remove(s.id.as_ref());
		tokio::task::spawn(async move { s.delete_session(parts).await });
	}
}

pub(crate) fn sse_stream_response(
	stream: impl futures::Stream<Item = ServerSseMessage> + Send + 'static,
	keep_alive: Option<Duration>,
) -> Response {
	use futures::StreamExt;
	let stream = SseBody::new(stream.map(|message| {
		let data = serde_json::to_string(&message.message).expect("valid message");
		let mut sse = Sse::default().data(data);
		sse.id = message.event_id;
		Result::<Sse, Infallible>::Ok(sse)
	}));
	let stream = match keep_alive {
		Some(duration) => {
			http::Body::new(stream.with_keep_alive::<TokioSseTimer>(KeepAlive::new().interval(duration)))
		},
		None => http::Body::new(stream),
	};
	::http::Response::builder()
		.status(StatusCode::OK)
		.header(http::header::CONTENT_TYPE, EVENT_STREAM_MIME_TYPE)
		.header(http::header::CACHE_CONTROL, "no-cache")
		.body(stream)
		.expect("valid response")
}

pin_project_lite::pin_project! {
		struct TokioSseTimer {
				#[pin]
				sleep: tokio::time::Sleep,
		}
}
impl Future for TokioSseTimer {
	type Output = ();

	fn poll(
		self: std::pin::Pin<&mut Self>,
		cx: &mut std::task::Context<'_>,
	) -> std::task::Poll<Self::Output> {
		let this = self.project();
		this.sleep.poll(cx)
	}
}
impl sse_stream::Timer for TokioSseTimer {
	fn from_duration(duration: Duration) -> Self {
		Self {
			sleep: tokio::time::sleep(duration),
		}
	}

	fn reset(self: std::pin::Pin<&mut Self>, when: std::time::Instant) {
		let this = self.project();
		this.sleep.reset(tokio::time::Instant::from_std(when));
	}
}

fn get_client_info() -> ClientInfo {
	let mut client_info = ClientInfo::default();
	client_info.protocol_version = ProtocolVersion::V_2025_11_25;
	client_info.capabilities = rmcp::model::ClientCapabilities::default();
	client_info.client_info =
		Implementation::new("agentgateway", BuildInfo::new().version.to_string());
	client_info
}
