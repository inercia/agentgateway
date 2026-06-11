//! Federated outbound message transforms shared by handler and session (Adobe MCP).

#![cfg(feature = "adobe")]

use rmcp::model::{ServerJsonRpcMessage, ServerResult};

use crate::mcp::handler::Relay;
use crate::mcp::mcp_apps::routing;
use crate::mcp::multiplex_naming::wrap_server_request_id;

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
		if default_target_name.is_none()
			&& let ServerJsonRpcMessage::Request(jr) = msg
		{
			jr.id = wrap_server_request_id(upstream_target.as_str(), &jr.id);
		}
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

#[cfg(test)]
mod tests {
	use rmcp::model::{RequestId, ServerJsonRpcMessage, ServerRequest};

	use super::map_mux_outbound_message;
	use crate::mcp::multiplex_naming::unwrap_server_request_id;

	#[test]
	fn map_mux_outbound_message_wraps_server_request_id_when_multiplexing() {
		let mut msg = ServerJsonRpcMessage::request(
			ServerRequest::PingRequest(Default::default()),
			RequestId::Number(7),
		);
		let mut map = map_mux_outbound_message(None, false, "mcp-server-everything".to_string());
		map(&mut msg);
		let ServerJsonRpcMessage::Request(jr) = msg else {
			panic!("expected request");
		};
		let (target, orig) = unwrap_server_request_id(&jr.id).unwrap();
		assert_eq!(target, "mcp-server-everything");
		assert_eq!(orig, RequestId::Number(7));
	}

	#[test]
	fn map_mux_outbound_message_skips_request_id_wrap_for_single_target() {
		let default = "only".to_string();
		let mut msg = ServerJsonRpcMessage::request(
			ServerRequest::PingRequest(Default::default()),
			RequestId::Number(7),
		);
		let mut map = map_mux_outbound_message(Some(default), false, "ignored".to_string());
		map(&mut msg);
		let ServerJsonRpcMessage::Request(jr) = msg else {
			panic!("expected request");
		};
		assert_eq!(jr.id, RequestId::Number(7));
	}
}
