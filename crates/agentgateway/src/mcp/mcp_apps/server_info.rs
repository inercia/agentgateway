//! Gateway [`ServerInfo`] builder — centralises capability assembly and instructions merging.
//!
//! Keeping this logic in `mcp_apps/` rather than `handler.rs` minimises rebase conflicts when
//! upstream modifies the `get_info` function signature or body.

use agent_core::version::BuildInfo;
use rmcp::model::{Implementation, ProtocolVersion, ServerInfo};

use crate::mcp::mcp_apps::capabilities::gateway_merged_capabilities;
use crate::mcp::rewrite::{CompiledServerRewrite, UpstreamInstructionsMode};

pub(crate) fn build_server_info(
	pv: ProtocolVersion,
	multiplexing: bool,
	resource_subscribe: bool,
	upstream_instructions: Vec<(String, String)>,
	server: Option<CompiledServerRewrite>,
) -> ServerInfo {
	let capabilities = gateway_merged_capabilities(multiplexing, resource_subscribe);
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
