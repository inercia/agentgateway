//! MCP presentation rewrite: rename, describe, meta, federation `initialize` overrides.

use std::borrow::Cow;
use std::collections::HashMap;

use regex::Regex;
use rmcp::model::{Meta, Prompt, Resource, ResourceTemplate, Task, Tool};
use tracing::warn;

use crate::types::proto::agent::backend_policy_spec::mcp_rewrite;
use crate::types::proto::agent::backend_policy_spec::mcp_rewrite::server_rewrite::{
	ResourceNaming as ProtoResourceNaming, UpstreamInstructionsMode as ProtoUpstreamInstructionsMode,
};
use crate::types::proto::agent::backend_policy_spec::McpRewrite as ProtoMcpRewrite;
use crate::types::agent_xds::Diagnostics;
use crate::types::proto::agent::backend_policy_spec::mcp_rewrite::Rule as ProtoMcpRewriteRule;
use crate::{apply, schema_ser};

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

#[apply(schema_ser!)]
#[derive(Default)]
pub struct CompiledServerRewrite {
	pub name: Option<String>,
	pub version: Option<String>,
	pub instructions: Option<String>,
	pub upstream_instructions: UpstreamInstructionsMode,
	pub resource_naming: ResourceNaming,
}

#[apply(schema_ser!)]
#[derive(Default)]
pub struct McpRewritePolicy {
	pub server: Option<CompiledServerRewrite>,
	pub tools: Vec<CompiledItemRule>,
	pub prompts: Vec<CompiledItemRule>,
	pub resources: Vec<CompiledItemRule>,
}

#[derive(Debug, Clone)]
pub struct CompiledItemRule {
	pub rule_name: String,
	matcher: ItemMatcher,
	pub rewrite: RewriteValue,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemMatcherView<'a> {
	#[serde(skip_serializing_if = "Option::is_none")]
	exact: Option<&'a str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pattern: Option<&'a str>,
}

impl serde::Serialize for CompiledItemRule {
	fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
	where
		S: serde::Serializer,
	{
		use serde::ser::SerializeStruct;

		let matcher = match &self.matcher {
			ItemMatcher::Exact(name) => ItemMatcherView {
				exact: Some(name.as_str()),
				pattern: None,
			},
			ItemMatcher::Pattern(re) => ItemMatcherView {
				exact: None,
				pattern: Some(re.as_str()),
			},
		};
		let mut st = serializer.serialize_struct("CompiledItemRule", 3)?;
		st.serialize_field("ruleName", &self.rule_name)?;
		st.serialize_field("matcher", &matcher)?;
		st.serialize_field("rewrite", &self.rewrite)?;
		st.end()
	}
}

#[derive(Debug, Clone)]
enum ItemMatcher {
	Exact(String),
	Pattern(Regex),
}

#[apply(schema_ser!)]
#[derive(Default)]
pub struct RewriteValue {
	pub name: Option<String>,
	pub description: Option<String>,
	pub meta: Option<HashMap<String, serde_json::Value>>,
	pub remove_meta: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct CompiledTargetRewrite {
	pub tools: Vec<CompiledItemRule>,
	pub prompts: Vec<CompiledItemRule>,
	pub resources: Vec<CompiledItemRule>,
	/// Client-visible (exposed) name → upstream name for this target.
	exposed_to_upstream: HashMap<String, String>,
}

impl CompiledTargetRewrite {
	pub fn resolve_upstream_tool(&self, exposed: &str) -> String {
		self.exposed_to_upstream
			.get(exposed)
			.cloned()
			.unwrap_or_else(|| exposed.to_string())
	}

	pub fn resolve_upstream_prompt(&self, exposed: &str) -> String {
		self.resolve_upstream_tool(exposed)
	}

	pub fn resolve_upstream_resource_name(&self, exposed: &str) -> String {
		self.resolve_upstream_tool(exposed)
	}

	fn rebuild_exposed_map(&mut self) {
		let mut map = HashMap::new();
		for rules in [&self.tools, &self.prompts, &self.resources] {
			for rule in rules.iter() {
				if let Some(exposed) = rule.rewrite.name.as_ref() {
					if let ItemMatcher::Exact(upstream) = &rule.matcher {
						map.insert(exposed.clone(), upstream.clone());
					}
				}
			}
		}
		self.exposed_to_upstream = map;
	}
}

#[derive(Debug, Clone, Default)]
pub struct McpRewriteSet {
	pub server: Option<CompiledServerRewrite>,
	pub resource_naming: ResourceNaming,
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
		self.resource_naming() == ResourceNaming::Flat
	}

	pub fn target(&self, name: &str) -> Option<&CompiledTargetRewrite> {
		self.per_target.get(name)
	}

	/// Build a federation rewrite set from per-target backend policies (already merged federation+target).
	pub fn from_backend_targets(
		targets: impl Iterator<Item = (String, Option<McpRewritePolicy>)>,
	) -> Self {
		let mut per_target = HashMap::new();
		let mut server = None;
		let mut resource_naming = ResourceNaming::default();

		for (name, pol) in targets {
			let Some(pol) = pol else { continue };
			if let Some(s) = pol.server.clone() {
				resource_naming = s.resource_naming;
				server = Some(s);
			}
			if !pol.tools.is_empty() || !pol.prompts.is_empty() || !pol.resources.is_empty() {
				per_target.insert(name, pol.compile_target());
			}
		}

		Self {
			server,
			resource_naming,
			per_target,
		}
	}

	/// Concatenate federation and per-target policies: `[per_target…, federation…]`, first match wins.
	pub fn merge_for_target(
		federation: Option<&McpRewritePolicy>,
		per_target: Option<&McpRewritePolicy>,
	) -> McpRewritePolicy {
		let mut out = McpRewritePolicy::default();
		if let Some(t) = per_target {
			out.tools.extend(t.tools.iter().cloned());
			out.prompts.extend(t.prompts.iter().cloned());
			out.resources.extend(t.resources.iter().cloned());
		}
		if let Some(f) = federation {
			out.tools.extend(f.tools.iter().cloned());
			out.prompts.extend(f.prompts.iter().cloned());
			out.resources.extend(f.resources.iter().cloned());
			if out.server.is_none() {
				out.server = f.server.clone();
			}
		}
		if let Some(t) = per_target {
			out.server = t.server.clone().or(out.server);
		}
		out
	}

	pub fn concat_policies(policies: impl IntoIterator<Item = McpRewritePolicy>) -> Option<McpRewritePolicy> {
		let mut out: Option<McpRewritePolicy> = None;
		for p in policies {
			out = Some(match out {
				None => p,
				Some(mut acc) => {
					acc.concat(p);
					acc
				},
			});
		}
		out
	}

	/// Resolve a Flat-mode client prompt name to `(target, upstream)`.
	///
	/// When `routes` is populated (after a federated `prompts/list`), use the same
	/// exposed-name → `(target, upstream)` mapping as the merged catalog. Otherwise
	/// fall back to rewrite reverse-map and pass-through on targets without per-target
	/// rewrite rules.
	pub fn resolve_flat_prompt(
		&self,
		exposed: &str,
		routes: Option<&HashMap<String, (String, String)>>,
		all_targets: &[String],
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		use crate::mcp::upstream::UpstreamError;

		if let Some(routes) = routes {
			if let Some(route) = routes.get(exposed) {
				return Ok(route.clone());
			}
		}

		let mut hits = Vec::new();
		for target in all_targets {
			let Some(rw) = self.per_target.get(target) else {
				hits.push((target.clone(), exposed.to_string()));
				continue;
			};
			if let Some(upstream) = rw.exposed_to_upstream.get(exposed) {
				hits.push((target.clone(), upstream.clone()));
				continue;
			}
			let renamed_to_exposed = rw
				.prompts
				.iter()
				.any(|r| r.rewrite.name.as_deref() == Some(exposed));
			if !renamed_to_exposed {
				hits.push((target.clone(), exposed.to_string()));
			}
		}
		match hits.len() {
			0 => Err(UpstreamError::InvalidRequest(format!(
				"unknown flat prompt name: {exposed}"
			))),
			1 => Ok(hits.pop().unwrap()),
			_ => Err(UpstreamError::InvalidRequest(format!(
				"ambiguous flat prompt name: {exposed}"
			))),
		}
	}

	/// Resolve a Flat-mode client task id to `(target, upstream_task_id)`.
	///
	/// When `routes` is populated (after federated `tasks/list` or `tools/call` create),
	/// use the same exposed-id → `(target, upstream)` mapping as the merged catalog.
	pub fn resolve_flat_task(
		&self,
		exposed: &str,
		routes: Option<&HashMap<String, (String, String)>>,
		all_targets: &[String],
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		use crate::mcp::upstream::UpstreamError;

		if let Some(routes) = routes {
			if let Some(route) = routes.get(exposed) {
				return Ok(route.clone());
			}
		}

		let mut hits = Vec::new();
		for target in all_targets {
			hits.push((target.clone(), exposed.to_string()));
		}
		match hits.len() {
			0 => Err(UpstreamError::InvalidRequest(format!(
				"unknown flat task id: {exposed}"
			))),
			1 => Ok(hits.pop().unwrap()),
			_ => Err(UpstreamError::InvalidRequest(format!(
				"ambiguous flat task id: {exposed}"
			))),
		}
	}

	/// Resolve a Flat-mode client tool name to `(target, upstream)`.
	///
	/// When `routes` is populated (after a federated `tools/list`), use the same
	/// exposed-name → `(target, upstream)` mapping as the merged catalog. Otherwise
	/// fall back to rewrite reverse-map and pass-through on targets without per-target
	/// rewrite rules.
	pub fn resolve_flat_tool(
		&self,
		exposed: &str,
		routes: Option<&HashMap<String, (String, String)>>,
		all_targets: &[String],
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		use crate::mcp::upstream::UpstreamError;

		if let Some(routes) = routes {
			if let Some(route) = routes.get(exposed) {
				return Ok(route.clone());
			}
		}

		let mut hits = Vec::new();
		for target in all_targets {
			let Some(rw) = self.per_target.get(target) else {
				hits.push((target.clone(), exposed.to_string()));
				continue;
			};
			if let Some(upstream) = rw.exposed_to_upstream.get(exposed) {
				hits.push((target.clone(), upstream.clone()));
			}
		}
		match hits.len() {
			0 => Err(UpstreamError::InvalidRequest(format!(
				"unknown flat tool name: {exposed}"
			))),
			1 => Ok(hits.pop().unwrap()),
			_ => Err(UpstreamError::InvalidRequest(format!(
				"ambiguous flat tool name: {exposed}"
			))),
		}
	}

	/// Resolve a Flat-mode client resource name to `(target, upstream_name)`.
	pub fn resolve_flat_resource(
		&self,
		exposed: &str,
	) -> Result<(String, String), crate::mcp::upstream::UpstreamError> {
		use crate::mcp::upstream::UpstreamError;
		let mut hits = Vec::new();
		for (target, rw) in &self.per_target {
			if let Some(upstream) = rw.exposed_to_upstream.get(exposed) {
				hits.push((target.clone(), upstream.clone()));
				continue;
			}
			let renamed_to_exposed = rw
				.resources
				.iter()
				.any(|r| r.rewrite.name.as_deref() == Some(exposed));
			if !renamed_to_exposed {
				hits.push((target.clone(), exposed.to_string()));
			}
		}
		match hits.len() {
			0 => Err(UpstreamError::InvalidRequest(format!(
				"unknown flat resource name: {exposed}"
			))),
			1 => Ok(hits.pop().unwrap()),
			_ => Err(UpstreamError::InvalidRequest(format!(
				"ambiguous flat resource name: {exposed}"
			))),
		}
	}

}

impl McpRewritePolicy {
	pub fn concat(&mut self, other: McpRewritePolicy) {
		self.tools.extend(other.tools);
		self.prompts.extend(other.prompts);
		self.resources.extend(other.resources);
		self.server = other.server.or(self.server.clone());
	}

	pub fn compile_target(&self) -> CompiledTargetRewrite {
		let mut t = CompiledTargetRewrite {
			tools: self.tools.clone(),
			prompts: self.prompts.clone(),
			resources: self.resources.clone(),
			..Default::default()
		};
		t.rebuild_exposed_map();
		t
	}
}

impl CompiledItemRule {
	fn matches(&self, upstream_name: &str) -> bool {
		match &self.matcher {
			ItemMatcher::Exact(n) => n == upstream_name,
			ItemMatcher::Pattern(re) => re.is_match(upstream_name),
		}
	}

	fn apply_to_tool(&self, tool: &mut Tool) {
		if let Some(name) = &self.rewrite.name {
			tool.name = Cow::Owned(name.clone());
		}
		if let Some(desc) = &self.rewrite.description {
			tool.description = Some(Cow::Owned(desc.clone()));
		}
		apply_meta_to_tool(tool, &self.rewrite);
	}

	fn apply_to_prompt(&self, prompt: &mut Prompt) {
		if let Some(name) = &self.rewrite.name {
			prompt.name = name.clone();
		}
		if let Some(desc) = &self.rewrite.description {
			prompt.description = Some(desc.clone());
		}
	}

	fn apply_to_resource(&self, resource: &mut Resource) {
		if let Some(name) = &self.rewrite.name {
			resource.name = name.clone();
		}
		if let Some(desc) = &self.rewrite.description {
			resource.description = Some(desc.clone());
		}
	}
}

fn apply_meta_to_tool(tool: &mut Tool, rewrite: &RewriteValue) {
	let Some(mut meta) = tool.meta.take().map(|m| m.0) else {
		if rewrite.meta.is_none() && rewrite.remove_meta.is_empty() {
			return;
		}
		let mut obj = serde_json::Map::new();
		for key in &rewrite.remove_meta {
			obj.remove(key);
		}
		if let Some(add) = &rewrite.meta {
			for (k, v) in add {
				obj.insert(k.clone(), v.clone());
			}
		}
		if !obj.is_empty() {
			tool.meta = Some(Meta(obj));
		}
		return;
	};

	for key in &rewrite.remove_meta {
		meta.remove(key);
	}
	if let Some(add) = &rewrite.meta {
		for (k, v) in add {
			meta.insert(k.clone(), v.clone());
		}
	}
	if meta.is_empty() {
		tool.meta = None;
	} else {
		tool.meta = Some(Meta(meta));
	}
}

pub fn apply_tool_rewrite(tool: &mut Tool, rules: &[CompiledItemRule]) {
	let upstream = tool.name.to_string();
	for rule in rules {
		if rule.matches(&upstream) {
			rule.apply_to_tool(tool);
			return;
		}
	}
}

pub fn apply_prompt_rewrite(prompt: &mut Prompt, rules: &[CompiledItemRule]) {
	let upstream = prompt.name.clone();
	for rule in rules {
		if rule.matches(&upstream) {
			rule.apply_to_prompt(prompt);
			return;
		}
	}
}

pub fn apply_resource_rewrite(resource: &mut Resource, rules: &[CompiledItemRule]) {
	let upstream = resource.name.clone();
	for rule in rules {
		if rule.matches(&upstream) {
			rule.apply_to_resource(resource);
			return;
		}
	}
}

/// Omit tools whose exposed name collides under Flat naming.
pub fn filter_flat_prompt_collisions(prompts: Vec<Prompt>) -> Vec<Prompt> {
	let mut seen: HashMap<String, ()> = HashMap::new();
	let mut out = Vec::with_capacity(prompts.len());
	for p in prompts {
		if seen.insert(p.name.clone(), ()).is_some() {
			warn!(
				exposed = %p.name,
				"mcp_flat_name_collision: omitting duplicate exposed prompt name"
			);
			continue;
		}
		out.push(p);
	}
	out
}

fn build_flat_route_index(
	entries: impl IntoIterator<Item = (String, String, String)>,
	collision_log_label: &'static str,
) -> HashMap<String, (String, String)> {
	let mut out = HashMap::new();
	for (target, upstream, exposed) in entries {
		if out.contains_key(&exposed) {
			warn!(
				exposed = %exposed,
				kind = collision_log_label,
				"mcp_flat_name_collision: keeping first exposed name in route index"
			);
			continue;
		}
		out.insert(exposed, (target, upstream));
	}
	out
}

/// Build exposed tool name → `(target, upstream)` using first-wins on collision.
///
/// Matches [`filter_flat_tool_collisions`] which keeps the first occurrence of an
/// exposed name in the user-visible `tools/list`: when a name collides across
/// federation targets the FIRST target's mapping stays so `tools/call` can still
/// route what the client saw in the list. Previously this dropped both copies,
/// making the tool visible in the list but unroutable.
pub fn build_flat_tool_route_index(
	entries: impl IntoIterator<Item = (String, String, String)>,
) -> HashMap<String, (String, String)> {
	build_flat_route_index(entries, "tool name")
}

/// Build exposed prompt name → `(target, upstream)` using first-wins on collision.
///
/// Matches [`filter_flat_prompt_collisions`]: when the same bare prompt name appears on
/// multiple federation targets, the first target's mapping stays so `prompts/get` can
/// route what the client saw in `prompts/list`.
pub fn build_flat_prompt_route_index(
	entries: impl IntoIterator<Item = (String, String, String)>,
) -> HashMap<String, (String, String)> {
	build_flat_route_index(entries, "prompt name")
}

/// Build exposed task id → `(target, upstream_task_id)` using first-wins on collision.
///
/// Matches [`filter_flat_task_collisions`]: when the same bare task id appears on
/// multiple federation targets, the first target's mapping stays so `tasks/get` can
/// route what the client saw in `tasks/list`.
pub fn build_flat_task_route_index(
	entries: impl IntoIterator<Item = (String, String, String)>,
) -> HashMap<String, (String, String)> {
	build_flat_route_index(entries, "task id")
}

pub fn filter_flat_tool_collisions(tools: Vec<Tool>) -> Vec<Tool> {
	let mut seen: HashMap<String, ()> = HashMap::new();
	let mut out = Vec::with_capacity(tools.len());
	for t in tools {
		let exposed = t.name.to_string();
		if seen.insert(exposed.clone(), ()).is_some() {
			warn!(
				exposed = %exposed,
				"mcp_flat_name_collision: omitting duplicate exposed tool name"
			);
			continue;
		}
		out.push(t);
	}
	out
}

/// Omit resources whose exposed name collides under Flat naming.
pub fn filter_flat_resource_collisions(resources: Vec<Resource>) -> Vec<Resource> {
	let mut seen: HashMap<String, ()> = HashMap::new();
	let mut out = Vec::with_capacity(resources.len());
	for r in resources {
		if seen.insert(r.name.clone(), ()).is_some() {
			warn!(
				exposed = %r.name,
				"mcp_flat_name_collision: omitting duplicate exposed resource name"
			);
			continue;
		}
		out.push(r);
	}
	out
}

/// Omit resource templates whose exposed name collides under Flat naming.
pub fn filter_flat_resource_template_collisions(
	templates: Vec<ResourceTemplate>,
) -> Vec<ResourceTemplate> {
	let mut seen: HashMap<String, ()> = HashMap::new();
	let mut out = Vec::with_capacity(templates.len());
	for rt in templates {
		if seen.insert(rt.name.clone(), ()).is_some() {
			warn!(
				exposed = %rt.name,
				"mcp_flat_name_collision: omitting duplicate exposed resource template name"
			);
			continue;
		}
		out.push(rt);
	}
	out
}

/// Omit tasks whose bare exposed task id collides under Flat naming.
///
/// When two upstreams return the same bare id, keep the first occurrence in the
/// user-visible `tasks/list` (same first-wins rule as [`build_flat_task_route_index`]).
pub fn filter_flat_task_collisions(tasks: Vec<Task>) -> Vec<Task> {
	let mut seen: HashMap<String, ()> = HashMap::new();
	let mut out = Vec::with_capacity(tasks.len());
	for t in tasks {
		if seen.insert(t.task_id.clone(), ()).is_some() {
			warn!(
				exposed = %t.task_id,
				"mcp_flat_name_collision: omitting duplicate exposed task id"
			);
			continue;
		}
		out.push(t);
	}
	out
}

pub fn mcp_rewrite_from_proto(
	m: &ProtoMcpRewrite,
	diagnostics: &mut Diagnostics,
) -> McpRewritePolicy {
	let mut pol = McpRewritePolicy::default();
	if let Some(s) = m.server.as_ref() {
		pol.server = Some(server_rewrite_from_proto(s));
	}
	if let Some(t) = m.tools.as_ref() {
		pol.tools = item_rules_from_proto(t, "tools", diagnostics);
	}
	if let Some(p) = m.prompts.as_ref() {
		pol.prompts = item_rules_from_proto(p, "prompts", diagnostics);
	}
	if let Some(r) = m.resources.as_ref() {
		pol.resources = item_rules_from_proto(r, "resources", diagnostics);
	}
	pol
}

fn server_rewrite_from_proto(s: &mcp_rewrite::ServerRewrite) -> CompiledServerRewrite {
	let upstream_instructions = match ProtoUpstreamInstructionsMode::try_from(s.upstream_instructions)
	{
		Ok(ProtoUpstreamInstructionsMode::Replace) => UpstreamInstructionsMode::Replace,
		Ok(ProtoUpstreamInstructionsMode::Prepend) => UpstreamInstructionsMode::Prepend,
		_ => UpstreamInstructionsMode::Append,
	};
	let resource_naming = match ProtoResourceNaming::try_from(s.resource_naming) {
		Ok(ProtoResourceNaming::Flat) => ResourceNaming::Flat,
		_ => ResourceNaming::Prefix,
	};
	CompiledServerRewrite {
		name: s.name.clone(),
		version: s.version.clone(),
		instructions: s.instructions.clone(),
		upstream_instructions,
		resource_naming,
	}
}

fn item_rules_from_proto(
	item: &mcp_rewrite::ItemRewrite,
	context: &str,
	diagnostics: &mut Diagnostics,
) -> Vec<CompiledItemRule> {
	item
		.rules
		.iter()
		.filter_map(|r| compile_item_rule(r, context, diagnostics))
		.collect()
}

fn compile_item_rule(
	r: &ProtoMcpRewriteRule,
	context: &str,
	diagnostics: &mut Diagnostics,
) -> Option<CompiledItemRule> {
	let matcher = match &r.r#match {
		Some(m) => match &m.kind {
			Some(mcp_rewrite::r#match::Kind::ExactName(n)) => ItemMatcher::Exact(n.clone()),
			Some(mcp_rewrite::r#match::Kind::NamePattern(p)) => {
				match Regex::new(p.as_str()) {
					Ok(re) => ItemMatcher::Pattern(re),
					Err(e) => {
						diagnostics.add_warning(format!(
							"invalid regex for backend.mcpRewrite.{context} rule {}: {e}",
							r.name
						));
						return None;
					},
				}
			},
			None => {
				diagnostics.add_warning(format!(
					"backend.mcpRewrite.{context} rule {}: match must set exact_name or name_pattern",
					r.name
				));
				return None;
			},
		},
		None => {
			diagnostics.add_warning(format!(
				"backend.mcpRewrite.{context} rule {}: missing match",
				r.name
			));
			return None;
		},
	};

	let rewrite = r.rewrite.as_ref()?;
	let meta = if rewrite.meta.is_empty() {
		None
	} else {
		Some(
			rewrite
				.meta
				.iter()
				.filter_map(|(k, v)| prost_value_to_json(v).map(|jv| (k.clone(), jv)))
				.collect(),
		)
	};

	Some(CompiledItemRule {
		rule_name: r.name.clone(),
		matcher,
		rewrite: RewriteValue {
			name: rewrite.name.clone(),
			description: rewrite.description.clone(),
			meta,
			remove_meta: rewrite.remove_meta.clone(),
		},
	})
}

fn prost_value_to_json(v: &prost_wkt_types::Value) -> Option<serde_json::Value> {
	serde_json::to_value(v).ok()
}

impl McpRewritePolicy {
	#[cfg(test)]
	pub(crate) fn single_tool_rename(upstream: &str, exposed: &str) -> Self {
		Self {
			tools: vec![CompiledItemRule {
				rule_name: "rename".into(),
				matcher: ItemMatcher::Exact(upstream.to_string()),
				rewrite: RewriteValue {
					name: Some(exposed.to_string()),
					..Default::default()
				},
			}],
			..Default::default()
		}
	}

	#[cfg(test)]
	pub(crate) fn single_prompt_rename(upstream: &str, exposed: &str) -> Self {
		Self {
			prompts: vec![CompiledItemRule {
				rule_name: "rename".into(),
				matcher: ItemMatcher::Exact(upstream.to_string()),
				rewrite: RewriteValue {
					name: Some(exposed.to_string()),
					..Default::default()
				},
			}],
			..Default::default()
		}
	}

	#[cfg(test)]
	pub(crate) fn flat_server() -> Self {
		Self {
			server: Some(CompiledServerRewrite {
				resource_naming: ResourceNaming::Flat,
				..Default::default()
			}),
			..Default::default()
		}
	}
}

#[cfg(all(test, feature = "adobe"))]
mod tests {
	use std::borrow::Cow;

	use super::*;
	use rmcp::model::Tool;

	fn exact_rule(upstream: &str, exposed: &str) -> CompiledItemRule {
		CompiledItemRule {
			rule_name: "r".into(),
			matcher: ItemMatcher::Exact(upstream.into()),
			rewrite: RewriteValue {
				name: Some(exposed.into()),
				..Default::default()
			},
		}
	}

	fn test_tool(name: &str) -> Tool {
		Tool::new(
			Cow::Owned(name.to_string()),
			Cow::Borrowed(""),
			std::sync::Arc::new(serde_json::Map::new()),
		)
	}

	#[test]
	fn apply_tool_rewrite_renames() {
		let mut tool = test_tool("get_font_recommendations");
		apply_tool_rewrite(
			&mut tool,
			&[exact_rule("get_font_recommendations", "font_recommend")],
		);
		assert_eq!(tool.name.as_ref(), "font_recommend");
	}

	#[test]
	fn apply_tool_rewrite_description() {
		let mut tool = test_tool("x");
		let rules = vec![CompiledItemRule {
			rule_name: "d".into(),
			matcher: ItemMatcher::Exact("x".into()),
			rewrite: RewriteValue {
				description: Some("new desc".into()),
				..Default::default()
			},
		}];
		apply_tool_rewrite(&mut tool, &rules);
		assert_eq!(tool.description.as_deref(), Some("new desc"));
	}

	#[test]
	fn apply_tool_rewrite_meta_merge() {
		let mut tool = test_tool("x");
		tool.meta = Some(Meta(serde_json::Map::from_iter([(
			"keep".into(),
			serde_json::json!(1),
		)])));
		let rules = vec![CompiledItemRule {
			rule_name: "m".into(),
			matcher: ItemMatcher::Exact("x".into()),
			rewrite: RewriteValue {
				meta: Some(HashMap::from([(
					"adobeFeatureFlag".into(),
					serde_json::json!("tool.x"),
				)])),
				remove_meta: vec!["drop".into()],
				..Default::default()
			},
		}];
		apply_tool_rewrite(&mut tool, &rules);
		let meta = tool.meta.unwrap().0;
		assert_eq!(meta.get("keep"), Some(&serde_json::json!(1)));
		assert_eq!(
			meta.get("adobeFeatureFlag"),
			Some(&serde_json::json!("tool.x"))
		);
		assert!(!meta.contains_key("drop"));
	}

	#[test]
	fn merge_for_target_per_target_rules_first() {
		let fed = McpRewritePolicy {
			tools: vec![exact_rule("a", "fed_a")],
			..Default::default()
		};
		let target = McpRewritePolicy {
			tools: vec![exact_rule("a", "tgt_a")],
			..Default::default()
		};
		let merged = McpRewriteSet::merge_for_target(Some(&fed), Some(&target));
		let compiled = merged.compile_target();
		let mut tool = test_tool("a");
		apply_tool_rewrite(&mut tool, &compiled.tools);
		assert_eq!(tool.name.as_ref(), "tgt_a");
	}

	#[test]
	fn resolve_upstream_tool_reverse_map() {
		let mut t = CompiledTargetRewrite::default();
		t.exposed_to_upstream
			.insert("font_recommend".into(), "get_font_recommendations".into());
		assert_eq!(
			t.resolve_upstream_tool("font_recommend"),
			"get_font_recommendations".to_string()
		);
		assert_eq!(t.resolve_upstream_tool("other"), "other".to_string());
	}

	#[test]
	fn build_flat_task_route_index_matches_collision_filter() {
		let index = build_flat_task_route_index([
			("a".into(), "job-1".into(), "job-1".into()),
			("b".into(), "job-1".into(), "job-1".into()),
			("c".into(), "job-9".into(), "job-9".into()),
		]);
		assert_eq!(index.get("job-1"), Some(&("a".into(), "job-1".into())));
		assert_eq!(index.get("job-9"), Some(&("c".into(), "job-9".into())));
	}

	#[test]
	fn resolve_flat_task_uses_route_index() {
		let mut routes = HashMap::new();
		routes.insert("job-42".into(), ("a".into(), "job-42".into()));
		let set = McpRewriteSet {
			server: Some(CompiledServerRewrite {
				resource_naming: ResourceNaming::Flat,
				..Default::default()
			}),
			..Default::default()
		};
		let (target, upstream) = set
			.resolve_flat_task("job-42", Some(&routes), &["b".into()])
			.unwrap();
		assert_eq!(target, "a");
		assert_eq!(upstream, "job-42");
	}

	#[test]
	fn build_flat_tool_route_index_matches_collision_filter() {
		// First-wins: matches what `filter_flat_tool_collisions` keeps in the
		// user-visible list. The first target's mapping for a colliding name
		// stays so the client can still call what it sees in `tools/list`.
		let index = build_flat_tool_route_index([
			("a".into(), "search".into(), "search".into()),
			("b".into(), "search".into(), "search".into()),
			("c".into(), "unique".into(), "unique".into()),
		]);
		assert_eq!(index.get("search"), Some(&("a".into(), "search".into())));
		assert_eq!(index.get("unique"), Some(&("c".into(), "unique".into())));
	}

	#[test]
	fn resolve_flat_tool_uses_route_index() {
		let mut routes = HashMap::new();
		routes.insert(
			"show_threejs_scene".into(),
			("mcp-server-threejs".into(), "show_threejs_scene".into()),
		);
		let set = McpRewriteSet {
			server: Some(CompiledServerRewrite {
				resource_naming: ResourceNaming::Flat,
				..Default::default()
			}),
			per_target: HashMap::from([(
				"mcp-server-everything".into(),
				{
					let mut t = CompiledTargetRewrite::default();
					t.exposed_to_upstream
						.insert("echo_demo".into(), "echo".into());
					t
				},
			)]),
			..Default::default()
		};
		let (target, upstream) = set
			.resolve_flat_tool("show_threejs_scene", Some(&routes), &[])
			.unwrap();
		assert_eq!(target, "mcp-server-threejs");
		assert_eq!(upstream, "show_threejs_scene");
		let (target, upstream) = set
			.resolve_flat_tool("echo_demo", Some(&routes), &["mcp-server-everything".into()])
			.unwrap();
		assert_eq!(target, "mcp-server-everything");
		assert_eq!(upstream, "echo");
	}

	#[test]
	fn filter_flat_tool_collisions_omits_duplicate() {
		let tools = vec![test_tool("search"), test_tool("search"), test_tool("unique")];
		let out = filter_flat_tool_collisions(tools);
		assert_eq!(out.len(), 2);
		assert_eq!(out[0].name.as_ref(), "search");
		assert_eq!(out[1].name.as_ref(), "unique");
	}

	#[test]
	fn mcp_rewrite_policy_serializes_for_config_dump() {
		use crate::types::agent::BackendTrafficPolicy;

		let policy = BackendTrafficPolicy::McpRewrite(McpRewritePolicy::single_tool_rename(
			"get_font_recommendations",
			"font_recommend",
		));
		let value = serde_json::to_value(&policy).expect("McpRewrite should serialize");
		assert!(value.get("mcpRewrite").is_some());
	}
}
