//! Per-target cache of upstream [`ServerCapabilities`] from initialize responses.

use std::collections::HashMap;

use parking_lot::RwLock;
use rmcp::model::ServerCapabilities;
#[cfg(feature = "adobe")]
use rmcp::model::TasksCapability;

/// Per-target cache of [`ServerCapabilities`], populated when merging initialize results.
#[derive(Debug, Default)]
pub struct TargetCapabilities {
	capabilities: RwLock<HashMap<String, ServerCapabilities>>,
}

impl TargetCapabilities {
	pub fn new() -> Self {
		Self::default()
	}

	pub fn store(&self, target_name: &str, capabilities: ServerCapabilities) {
		self
			.capabilities
			.write()
			.insert(target_name.to_string(), capabilities);
	}

	pub fn upstreams_with_tools(&self, all_targets: &[String]) -> Vec<String> {
		self.targets_matching(all_targets, |c| c.tools.is_some())
	}

	pub fn upstreams_with_prompts(&self, all_targets: &[String]) -> Vec<String> {
		self.targets_matching(all_targets, |c| c.prompts.is_some())
	}

	pub fn upstreams_with_resources(&self, all_targets: &[String]) -> Vec<String> {
		self.targets_matching(all_targets, |c| c.resources.is_some())
	}

	#[cfg_attr(not(feature = "adobe"), allow(dead_code))]
	pub fn upstreams_with_tasks(&self, all_targets: &[String]) -> Vec<String> {
		self.targets_matching(all_targets, |c| c.tasks.is_some())
	}

	/// Whether each upstream in `all_targets` has capability predicate `check` satisfied.
	///
	/// **Fail-open for uncached targets:** targets not yet present in the post-initialize cache
	/// are included so clients do not miss results while initialize fanout is still completing.
	/// Targets with a cached capability entry that fails `check` are excluded.
	fn targets_matching<F>(&self, all_targets: &[String], check: F) -> Vec<String>
	where
		F: Fn(&ServerCapabilities) -> bool,
	{
		let caps = self.capabilities.read();
		all_targets
			.iter()
			.filter(|t| {
				#[allow(clippy::redundant_closure)]
				// `check` cannot be `map(check)` (E0507) without `Clone`/`Copy` on `F`
				caps.get(t.as_str()).map(|c| check(c)).unwrap_or(true)
			})
			.cloned()
			.collect()
	}
}

/// Capabilities advertised by the gateway when merging upstream MCP servers.
///
/// Prompts and tasks follow federation `resourceNaming` (Flat vs Prefix) like tools.
/// Prefix mode uses `target_`-prefixed names; Flat mode uses bare upstream ids with a
/// gateway route index. Resources use federated URI wrapping in
/// [`crate::mcp::mcp_apps::routing`].
pub(crate) fn gateway_merged_capabilities(
	multiplexing: bool,
	resource_subscribe: bool,
) -> ServerCapabilities {
	let b = ServerCapabilities::builder()
		.enable_tools()
		.enable_prompts()
		.enable_resources();
	let b = if multiplexing {
		b.enable_tool_list_changed()
	} else {
		b
	};
	let b = if resource_subscribe {
		b.enable_resources_subscribe()
	} else {
		b
	};
	#[cfg(feature = "adobe")]
	let b = b.enable_tasks_with(TasksCapability::server_default()); // `{}` breaks MCP Inspector Tasks UI
	b.build()
}
