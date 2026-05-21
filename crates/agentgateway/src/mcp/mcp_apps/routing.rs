//! Resource name and URI routing for MCP multiplexing with MCP Apps (`ui://`) wrapping.

#![cfg_attr(not(feature = "adobe"), allow(dead_code))]

use percent_encoding::NON_ALPHANUMERIC;
use percent_encoding::percent_decode_str;
use percent_encoding::utf8_percent_encode;
use rmcp::model::{
	Content, Meta, RawContent, ResourceContents, ServerJsonRpcMessage, ServerNotification,
};
use serde_json::Value;

use crate::mcp::multiplex_naming;
use crate::mcp::upstream::UpstreamError;

#[allow(unused_imports)] // re-exports for `routing::` consumers (tests and external callers)
pub use crate::mcp::multiplex_naming::{DELIMITER, parse_resource_name, resource_name};

const UI_SCHEME: &str = "ui";
const MULTIPLEX_URI_QUERY_PARAM: &str = "u";

fn percent_encode(s: &str) -> String {
	utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn percent_decode(s: &str) -> Result<String, UpstreamError> {
	percent_decode_str(s)
		.decode_utf8()
		.map(|cow| cow.into_owned())
		.map_err(|_| UpstreamError::InvalidRequest("invalid multiplex URI encoding".to_string()))
}

fn wrap_plus_scheme_multiplex(target: &str, uri: &str) -> String {
	if let Some(scheme_end) = uri.find("://") {
		let (scheme, rest) = uri.split_at(scheme_end);
		format!("{target}+{scheme}{rest}")
	} else {
		uri.to_string()
	}
}

/// Wrap a concrete resource URI for multiplexing:
/// - single-backend: passthrough
/// - `ui://...`: `ui://{encoded_target}/?u={encoded_uri}`
/// - otherwise: `target+scheme://...` (upstream-style multiplex URI).
pub fn wrap_resource_uri_mixed(
	default_target_name: Option<&String>,
	target: &str,
	uri: &str,
) -> String {
	if default_target_name.is_some() {
		return uri.to_string();
	}
	if uri.starts_with("ui://") {
		let encoded_target = percent_encode(target);
		let encoded_uri = percent_encode(uri);
		return format!("{UI_SCHEME}://{encoded_target}/?{MULTIPLEX_URI_QUERY_PARAM}={encoded_uri}");
	}
	wrap_plus_scheme_multiplex(target, uri)
}

pub fn wrap_resource_template_uri_mixed(
	default_target_name: Option<&String>,
	target: &str,
	uri_template: &str,
) -> String {
	if default_target_name.is_some() {
		return uri_template.to_string();
	}

	let literal_prefix_for_scheme = uri_template.split('{').next().unwrap_or("");

	if literal_prefix_for_scheme.starts_with("ui://") {
		let encoded_target = percent_encode(target);
		let mut result = format!("{UI_SCHEME}://{encoded_target}/?{MULTIPLEX_URI_QUERY_PARAM}=");
		let mut rest = uri_template;
		while let Some(open) = rest.find('{') {
			result.push_str(&percent_encode(&rest[..open]));
			if let Some(close_offset) = rest[open..].find('}') {
				result.push_str(&rest[open..open + close_offset + 1]);
				rest = &rest[open + close_offset + 1..];
			} else {
				result.push_str(&percent_encode(&rest[open..]));
				rest = "";
				break;
			}
		}
		result.push_str(&percent_encode(rest));
		return result;
	}

	let open = uri_template.find('{');
	let Some(sep) = uri_template.find("://") else {
		tracing::debug!(
			target: "mcp_apps",
			"multiplex resource template missing scheme separator; passthrough"
		);
		return uri_template.to_string();
	};
	if open.is_some_and(|o| o < sep) {
		tracing::debug!(
			target: "mcp_apps",
			"multiplex resource template has templated scheme; passthrough"
		);
		return uri_template.to_string();
	}
	let scheme_part = &uri_template[..sep];
	if scheme_part.contains('{') {
		tracing::debug!(
			target: "mcp_apps",
			"multiplex resource template has templated scheme segment; passthrough"
		);
		return uri_template.to_string();
	}
	wrap_plus_scheme_multiplex(target, uri_template)
}

fn unwrap_ui_multiplex_uri(uri: &str) -> Result<(String, String), UpstreamError> {
	let parsed = url::Url::parse(uri)
		.map_err(|e| UpstreamError::InvalidRequest(format!("invalid multiplex URI: {e}")))?;
	if parsed.scheme() != UI_SCHEME {
		return Err(UpstreamError::InvalidRequest(
			"URI is not a ui multiplex resource URI".to_string(),
		));
	}
	let host = parsed
		.host_str()
		.ok_or_else(|| UpstreamError::InvalidRequest("multiplex URI has no host".to_string()))?;
	let target = percent_decode(host)?;
	let original_uri = parsed
		.query_pairs()
		.find(|(k, _)| k == MULTIPLEX_URI_QUERY_PARAM)
		.map(|(_, v)| percent_decode(v.as_ref()))
		.ok_or_else(|| {
			UpstreamError::InvalidRequest(format!(
				"multiplex URI missing '{MULTIPLEX_URI_QUERY_PARAM}' query param"
			))
		})??;
	Ok((target, original_uri))
}

fn unwrap_plus_scheme_multiplex(uri: &str) -> Result<(String, String), UpstreamError> {
	let (target, remainder) = uri.split_once('+').ok_or_else(|| {
		UpstreamError::InvalidRequest("invalid multiplex resource URI (missing target)".to_string())
	})?;
	if !remainder.contains("://") {
		return Err(UpstreamError::InvalidRequest(
			"invalid multiplex resource URI (missing scheme)".to_string(),
		));
	}
	Ok((target.to_string(), remainder.to_string()))
}

/// Resolve a federated resource URI to `(target, original_uri)` for the upstream.
pub fn parse_resource_uri_mixed(
	default_target_name: Option<&String>,
	uri: &str,
) -> Result<(String, String), UpstreamError> {
	if let Some(default) = default_target_name {
		return Ok((default.clone(), uri.to_string()));
	}
	if uri.starts_with("ui://") {
		return unwrap_ui_multiplex_uri(uri);
	}
	unwrap_plus_scheme_multiplex(uri)
}

pub fn parse_task_id(
	default_target_name: Option<&String>,
	id: &str,
) -> Result<(String, String), UpstreamError> {
	let (t, rest) = multiplex_naming::parse_resource_name(default_target_name, id)?;
	Ok((t.to_string(), rest.to_string()))
}

/// Rewrap outbound multiplex [`ServerJsonRpcMessage`] for the federated client (subscribe streams).
pub fn rewrap_outbound_multiplex_server_message(
	default_target_name: Option<&String>,
	upstream_target: &str,
	msg: &mut ServerJsonRpcMessage,
) {
	if default_target_name.is_some() {
		return;
	}
	let ServerJsonRpcMessage::Notification(jn) = msg else {
		return;
	};
	if let ServerNotification::ResourceUpdatedNotification(n) = &mut jn.notification {
		n.params.uri = wrap_resource_uri_mixed(default_target_name, upstream_target, &n.params.uri);
	}
}

const META_RESOURCE_URI_CAMEL: &str = "resourceUri";
/// Legacy flat key from MCP Apps spec (deprecated vs `_meta.ui.resourceUri`; still emitted by `registerAppTool`).
const META_LEGACY_RESOURCE_URI_KEY: &str = "ui/resourceUri";

fn wrap_tool_ui_uri(
	default_target_name: Option<&String>,
	target: &str,
	uri: &str,
) -> Option<String> {
	if uri.starts_with("ui://") {
		Some(wrap_resource_uri_mixed(default_target_name, target, uri))
	} else {
		None
	}
}

/// Rewrite MCP Apps tool [`Meta`] for multiplexing:
/// - `_meta.ui.resourceUri` (modern form)
/// - `_meta["ui/resourceUri"]` (legacy flat form; hosts and `registerAppTool` still set this)
///
/// Without wrapping both keys, MCP Inspector may read an upstream-native `ui://...`
/// and subsequent `resources/read` fails multiplex parsing (`missing 'u' query param`).
pub fn rewrite_tool_ui_meta(
	default_target_name: Option<&String>,
	target: &str,
	meta: &mut Option<Meta>,
) {
	let Some(meta) = meta else {
		return;
	};
	if let Some(Value::Object(ui)) = meta.get_mut("ui")
		&& let Some(Value::String(uri)) = ui.get_mut(META_RESOURCE_URI_CAMEL)
		&& let Some(w) = wrap_tool_ui_uri(default_target_name, target, uri.as_str())
	{
		*uri = w;
	}
	if let Some(Value::String(uri)) = meta.get_mut(META_LEGACY_RESOURCE_URI_KEY)
		&& let Some(w) = wrap_tool_ui_uri(default_target_name, target, uri.as_str())
	{
		*uri = w;
	}
}

/// Rewrap embedded resource URIs inside [`CallToolResult::content`] for multiplex federation.
///
/// A2UI and other MCP Apps servers often return `ui://...` (or `a2ui://...`) via
/// `EmbeddedResource` content blocks rather than `_meta.ui.resourceUri`. Without wrapping
/// these URIs, federated hosts fail follow-up `resources/read` with multiplex parse errors.
pub fn rewrap_call_tool_result_content(
	default_target_name: Option<&String>,
	target: &str,
	content: &mut [Content],
) {
	if default_target_name.is_some() {
		return;
	}
	for item in content.iter_mut() {
		let RawContent::Resource(embedded) = &mut item.raw else {
			continue;
		};
		match &mut embedded.resource {
			ResourceContents::TextResourceContents { uri, .. } => {
				*uri = wrap_resource_uri_mixed(default_target_name, target, uri);
			},
			ResourceContents::BlobResourceContents { uri, .. } => {
				*uri = wrap_resource_uri_mixed(default_target_name, target, uri);
			},
		}
	}
}

/// Rewrite returned [`ResourceContents`] URIs to the federated wire form.
pub fn rewrap_read_resource_contents(
	default_target_name: Option<&String>,
	target: &str,
	contents: &mut [ResourceContents],
) {
	if default_target_name.is_some() {
		return;
	}
	for c in contents.iter_mut() {
		match c {
			ResourceContents::TextResourceContents { uri, .. } => {
				*uri = wrap_resource_uri_mixed(default_target_name, target, uri);
			},
			ResourceContents::BlobResourceContents { uri, .. } => {
				*uri = wrap_resource_uri_mixed(default_target_name, target, uri);
			},
		}
	}
}
