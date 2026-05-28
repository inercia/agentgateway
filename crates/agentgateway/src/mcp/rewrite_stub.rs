//! No-op MCP rewrite types when `feature = "adobe"` is disabled.
//! Proto may still carry `McpRewrite`; dataplane ignores it.

use std::collections::HashMap;

use rmcp::model::{Prompt, Resource, ResourceTemplate, Tool};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResourceNaming {
	#[default]
	Prefix,
	Flat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum UpstreamInstructionsMode {
	#[default]
	Append,
	Replace,
	Prepend,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CompiledServerRewrite {
	pub name: Option<String>,
	pub version: Option<String>,
	pub instructions: Option<String>,
	pub upstream_instructions: UpstreamInstructionsMode,
	pub resource_naming: ResourceNaming,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpRewritePolicy {
	pub server: Option<CompiledServerRewrite>,
}

/// Placeholder rule type (unused without `adobe`).
#[derive(Debug, Clone, Default)]
pub struct CompiledItemRule;

#[derive(Debug, Clone, Default)]
pub struct CompiledTargetRewrite {
	pub tools: Vec<CompiledItemRule>,
	pub prompts: Vec<CompiledItemRule>,
	pub resources: Vec<CompiledItemRule>,
}

impl CompiledTargetRewrite {
	pub fn resolve_upstream_tool(&self, exposed: &str) -> String {
		exposed.to_string()
	}

	pub fn resolve_upstream_prompt(&self, exposed: &str) -> String {
		exposed.to_string()
	}

	pub fn resolve_upstream_resource_name(&self, exposed: &str) -> String {
		exposed.to_string()
	}
}

#[derive(Debug, Clone, Default)]
pub struct McpRewriteSet {
	pub server: Option<CompiledServerRewrite>,
	resource_naming: ResourceNaming,
	per_target: HashMap<String, CompiledTargetRewrite>,
}

impl McpRewriteSet {
	pub fn is_empty(&self) -> bool {
		self.server.is_none() && self.per_target.is_empty()
	}

	pub fn resource_naming(&self) -> ResourceNaming {
		self.server
			.as_ref()
			.map(|s| s.resource_naming)
			.unwrap_or(self.resource_naming)
	}

	pub fn flat(&self) -> bool {
		false
	}

	pub fn target(&self, _name: &str) -> Option<&CompiledTargetRewrite> {
		None
	}

	pub fn from_backend_targets(
		_targets: impl Iterator<Item = (String, Option<McpRewritePolicy>)>,
	) -> Self {
		Self::default()
	}

	pub fn merge_for_target(
		federation: Option<&McpRewritePolicy>,
		per_target: Option<&McpRewritePolicy>,
	) -> McpRewritePolicy {
		match (federation, per_target) {
			(None, None) => McpRewritePolicy::default(),
			(Some(f), None) => f.clone(),
			(None, Some(t)) => t.clone(),
			(Some(f), Some(t)) => {
				let mut out = f.clone();
				out.server = t.server.clone().or(out.server);
				out
			},
		}
	}

	pub fn concat_policies(
		policies: impl IntoIterator<Item = McpRewritePolicy>,
	) -> Option<McpRewritePolicy> {
		let mut out: Option<McpRewritePolicy> = None;
		for p in policies {
			if p.server.is_some() {
				out = Some(match out {
					None => p,
					Some(mut acc) => {
						if acc.server.is_none() {
							acc.server = p.server;
						}
						acc
					},
				});
			}
		}
		out.filter(|p| p.server.is_some())
	}

	pub fn resolve_flat_prompt(
		&self,
		exposed: &str,
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		Err(crate::mcp::upstream::UpstreamError::InvalidRequest(format!(
			"unknown flat prompt name: {exposed}"
		)))
	}

	pub fn resolve_flat_tool(
		&self,
		exposed: &str,
		_routes: Option<&HashMap<String, (String, String)>>,
		_all_targets: &[String],
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		Err(crate::mcp::upstream::UpstreamError::InvalidRequest(format!(
			"unknown flat tool name: {exposed}"
		)))
	}

	pub fn resolve_flat_resource(
		&self,
		exposed: &str,
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		Err(crate::mcp::upstream::UpstreamError::InvalidRequest(format!(
			"unknown flat resource name: {exposed}"
		)))
	}

}

impl McpRewritePolicy {
	pub fn concat(&mut self, other: McpRewritePolicy) {
		if self.server.is_none() {
			self.server = other.server;
		}
	}
}

pub fn apply_tool_rewrite(_tool: &mut Tool, _rules: &[CompiledItemRule]) {}

pub fn apply_prompt_rewrite(_prompt: &mut Prompt, _rules: &[CompiledItemRule]) {}

pub fn apply_resource_rewrite(_resource: &mut Resource, _rules: &[CompiledItemRule]) {}

pub fn build_flat_tool_route_index(
	_entries: impl IntoIterator<Item = (String, String, String)>,
) -> HashMap<String, (String, String)> {
	HashMap::new()
}

pub fn filter_flat_tool_collisions(tools: Vec<Tool>) -> Vec<Tool> {
	tools
}

pub fn filter_flat_prompt_collisions(prompts: Vec<Prompt>) -> Vec<Prompt> {
	prompts
}

pub fn filter_flat_resource_collisions(resources: Vec<Resource>) -> Vec<Resource> {
	resources
}

pub fn filter_flat_resource_template_collisions(
	templates: Vec<ResourceTemplate>,
) -> Vec<ResourceTemplate> {
	templates
}

#[allow(dead_code)]
pub fn filter_flat_task_collisions(tasks: Vec<rmcp::model::Task>) -> Vec<rmcp::model::Task> {
	tasks
}
