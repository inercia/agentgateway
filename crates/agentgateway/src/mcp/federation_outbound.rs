//! Federated outbound message transforms shared by handler and session (Adobe MCP).

#![cfg(feature = "adobe")]

use rmcp::model::{ServerJsonRpcMessage, ServerResult};

use crate::mcp::handler::Relay;
use crate::mcp::mcp_apps::routing;

/// Map closure for multiplex GET/SSE streams and single-upstream RPC responses.
///
/// Combines upstream's resource-URI rewrap with Adobe-only task-id rewrap. Kept here
/// (and not in `mcp_apps::routing`) so the upstream-shared
/// [`routing::rewrap_outbound_multiplex_server_message`] signature stays sync-stable.
pub fn map_mux_outbound_message(
	default_target_name: Option<String>,
	flat: bool,
	upstream_target: String,
) -> impl FnMut(&mut ServerJsonRpcMessage) + Send {
	move |msg| {
		routing::rewrap_outbound_multiplex_server_message(
			default_target_name.as_ref(),
			upstream_target.as_str(),
			msg,
		);
		crate::mcp::multiplex_naming::task_outbound::rewrap_outbound_multiplex_task_message(
			default_target_name.as_ref(),
			flat,
			upstream_target.as_str(),
			msg,
		);
	}
}

/// Record Flat-mode task route on `tools/call` → `CreateTaskResult` before outbound rewrap.
pub fn record_create_task_route_if_flat(
	relay: &Relay,
	default_mux: &Option<String>,
	flat: bool,
	target: &str,
	msg: &mut ServerJsonRpcMessage,
) {
	if !flat || default_mux.is_some() {
		return;
	}
	if let ServerJsonRpcMessage::Response(jr) = msg
		&& let ServerResult::CreateTaskResult(ctr) = &jr.result
	{
		relay.record_flat_task_route(target, &ctr.task.task_id);
	}
}
