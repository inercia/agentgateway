//! Adobe MCP Tasks RPC dispatch (`tasks/list`, `tasks/get`, `tasks/result`, `tasks/cancel`).

use rmcp::model::{ClientRequest, JsonRpcRequest, ServerJsonRpcMessage};

use crate::http::Response;
use crate::mcp::federation_outbound::{map_mux_outbound_message, record_create_task_route_if_flat};
use crate::mcp::handler::Relay;
use crate::mcp::mcp_apps::routing;
use crate::mcp::upstream::IncomingRequestContext;
use crate::mcp::rbac;
use crate::mcp::upstream::UpstreamError;
use crate::telemetry::log::{AsyncLog, SpanWriteOnDrop};

use super::Session;

pub(super) async fn authorize_multiplex_task_id(
	session: &Session,
	task_id: &str,
	method: &str,
	ctx: &IncomingRequestContext,
	span: &mut SpanWriteOnDrop,
	log: &AsyncLog<crate::mcp::MCPInfo>,
	cel: &rbac::CelExecWrapper,
) -> Result<(String, String), UpstreamError> {
	let relay = session.relay();
	relay.ensure_flat_task_routes_loaded(ctx).await?;
	let (target_name, upstream_id) = relay
		.resolve_task_call(task_id)
		.map_err(|e| UpstreamError::InvalidRequest(format!("invalid task id: {task_id}: {e}")))?;
	span.rename_span(format!("{method} {target_name}"));
	log.non_atomic_mutate(|l| {
		l.set_task(target_name.clone(), upstream_id.clone());
	});
	if !relay.policies.validate(
		&rbac::ResourceType::Task(rbac::ResourceId::new(
			target_name.clone(),
			upstream_id.clone(),
		)),
		cel,
	) {
		return Err(UpstreamError::Authorization {
			resource_type: "task".to_string(),
			resource_name: task_id.to_string(),
		});
	}
	Ok((target_name, upstream_id))
}

pub(super) async fn forward_task_rpc(
	session: &Session,
	mut r: JsonRpcRequest<ClientRequest>,
	client_task_id: String,
	ctx: IncomingRequestContext,
	method: &str,
	span: &mut SpanWriteOnDrop,
	log: &AsyncLog<crate::mcp::MCPInfo>,
	cel: &rbac::CelExecWrapper,
) -> Result<Response, UpstreamError> {
	let relay = session.relay();
	let (target, upstream_id) =
		authorize_multiplex_task_id(session, &client_task_id, method, &ctx, span, log, cel).await?;
	match &mut r.request {
		ClientRequest::GetTaskInfoRequest(gtr) => {
			gtr.params.task_id = upstream_id;
		},
		ClientRequest::GetTaskResultRequest(gtr) => {
			gtr.params.task_id = upstream_id;
		},
		ClientRequest::CancelTaskRequest(ctr) => {
			ctr.params.task_id = upstream_id;
		},
		_ => {
			return Err(UpstreamError::InvalidRequest(
				"internal: expected task RPC".to_string(),
			));
		},
	}
	let default_mux = relay.default_target_name();
	let flat = relay.mcp_rewrite.flat();
	relay
		.send_single_map_response(
			r,
			ctx,
			target.as_str(),
			map_mux_outbound_message(default_mux, flat, target.clone()),
			None,
		)
		.await
}

pub(super) async fn handle_list_tasks(
	session: &Session,
	r: JsonRpcRequest<ClientRequest>,
	ctx: IncomingRequestContext,
	cel: rbac::CelExecWrapper,
) -> Result<Response, UpstreamError> {
	let relay = session.relay();
	let targets = relay
		.capabilities
		.upstreams_with_tasks(&relay.all_target_names());
	relay
		.send_fanout_to(&targets, r, ctx, relay.merge_tasks(cel))
		.await
}

/// Map `tools/call` outbound stream: Flat task route recording + multiplex rewrap +
/// MCP Apps `_meta.ui.resourceUri` rewrap for `CallToolResult`.
pub(super) fn map_tools_call_outbound(
	relay: Relay,
	default_mux: Option<String>,
	flat: bool,
	target_for_map: String,
) -> impl FnMut(&mut ServerJsonRpcMessage) + Send {
	let mut mux_rewrap = map_mux_outbound_message(default_mux.clone(), flat, target_for_map.clone());
	move |msg| {
		record_create_task_route_if_flat(&relay, &default_mux, flat, target_for_map.as_str(), msg);
		mux_rewrap(msg);
		if let ServerJsonRpcMessage::Response(jr) = msg
			&& let rmcp::model::ServerResult::CallToolResult(result) = &mut jr.result
		{
			routing::rewrite_tool_ui_meta(default_mux.as_ref(), target_for_map.as_str(), &mut result.meta);
			routing::rewrap_call_tool_result_content(
				default_mux.as_ref(),
				target_for_map.as_str(),
				&mut result.content,
			);
		}
	}
}
