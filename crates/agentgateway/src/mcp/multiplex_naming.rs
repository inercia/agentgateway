//! Shared tool/prompt/resource name prefixing for MCP multiplexing (`target_localName`).
//!
//! Kept out of `mcp_apps` so upstream `handler.rs` can share logic without the `adobe` feature.

use crate::mcp::upstream::UpstreamError;

pub const DELIMITER: &str = "_";

pub fn resource_name(default_target_name: Option<&String>, target: &str, name: &str) -> String {
	if default_target_name.is_none() {
		format!("{target}{DELIMITER}{name}")
	} else {
		name.to_string()
	}
}

pub fn parse_resource_name<'a, 'b: 'a>(
	default_target_name: Option<&'a String>,
	res: &'b str,
) -> Result<(&'a str, &'b str), UpstreamError> {
	if let Some(default) = default_target_name {
		Ok((default.as_str(), res))
	} else {
		res
			.split_once(DELIMITER)
			.ok_or_else(|| UpstreamError::InvalidRequest("invalid resource name".to_string()))
	}
}

/// Client-visible task id when multiplexing (`target_upstreamId`).
#[cfg_attr(not(feature = "adobe"), allow(dead_code))]
pub fn wrap_client_task_id(
	default_target_name: Option<&String>,
	target: &str,
	upstream_id: &str,
) -> String {
	resource_name(default_target_name, target, upstream_id)
}

/// Parse a multiplexed client task id to `(target, upstream_task_id)`.
///
/// Uses [`DELIMITER`] (`_`): the first `_` separates federated target from upstream id.
/// Upstream task ids may contain `_`; target names must not (same rule as tools/prompts).
#[cfg_attr(not(feature = "adobe"), allow(dead_code))]
pub fn unwrap_client_task_id(
	default_target_name: Option<&String>,
	client_id: &str,
) -> Result<(String, String), UpstreamError> {
	let (target, upstream) = parse_resource_name(default_target_name, client_id)?;
	Ok((target.to_string(), upstream.to_string()))
}

/// Resolve a client task id to `(target, upstream_task_id)` for `tasks/get`, `tasks/result`, and
/// `tasks/cancel`.
#[cfg_attr(not(feature = "adobe"), allow(dead_code))]
pub fn resolve_client_task_id(
	default_target_name: Option<&String>,
	client_id: &str,
	upstream_count: usize,
	single_upstream_name: Option<&str>,
) -> Result<(String, String), UpstreamError> {
	if default_target_name.is_some() {
		return unwrap_client_task_id(default_target_name, client_id);
	}
	if let Ok(route) = unwrap_client_task_id(default_target_name, client_id) {
		return Ok(route);
	}
	match upstream_count {
		0 => Err(UpstreamError::InvalidRequest(format!(
			"unknown task id: {client_id}"
		))),
		1 => {
			let name = single_upstream_name.ok_or_else(|| {
				UpstreamError::InvalidRequest(format!("unknown task id: {client_id}"))
			})?;
			Ok((name.to_string(), client_id.to_string()))
		},
		_ => Err(UpstreamError::InvalidRequest(format!(
			"flat task id requires task creation via tools/call: {client_id}"
		))),
	}
}

/// Parse federated `target+scheme://...` resource URIs (upstream multiplex form; no MCP Apps `ui://`).
#[cfg(not(feature = "adobe"))]
pub fn parse_multiplex_resource_uri(
	default_target_name: Option<&String>,
	uri: &str,
) -> Result<(String, String), UpstreamError> {
	if let Some(default) = default_target_name {
		return Ok((default.clone(), uri.to_string()));
	}
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

/// Outbound task id rewrap for federated MCP (Prefix mode); kept here so `handler`/`session`
/// do not depend on `mcp_apps` for task naming.
#[cfg(feature = "adobe")]
pub mod task_outbound {
	use rmcp::model::{
		CancelTaskResult, CreateTaskResult, GetTaskResult, ServerJsonRpcMessage, ServerNotification,
		ServerResult,
	};
	use serde_json::Value;

	use super::wrap_client_task_id;

	pub const TASK_STATUS_NOTIFICATION_METHOD: &str = "notifications/tasks/status";

	fn should_rewrap_task_ids(default_target_name: Option<&String>, flat: bool) -> bool {
		default_target_name.is_none() && !flat
	}

	/// Multiplex-wrap an upstream task id for the federated client (`target_upstreamId`).
	pub fn rewrap_outbound_task_id(
		default_target_name: Option<&String>,
		flat: bool,
		upstream_target: &str,
		task_id: &mut String,
	) {
		if !should_rewrap_task_ids(default_target_name, flat) {
			return;
		}
		let upstream_id = task_id.clone();
		*task_id = wrap_client_task_id(default_target_name, upstream_target, &upstream_id);
	}

	fn rewrap_task_id_in_json_value(
		default_target_name: Option<&String>,
		flat: bool,
		upstream_target: &str,
		value: &mut Value,
	) {
		let Value::Object(map) = value else {
			return;
		};
		for key in ["taskId", "task_id"] {
			if let Some(Value::String(id)) = map.get_mut(key) {
				rewrap_outbound_task_id(default_target_name, flat, upstream_target, id);
			}
		}
		if let Some(task) = map.get_mut("task") {
			rewrap_task_id_in_json_value(default_target_name, flat, upstream_target, task);
		}
	}

	pub fn rewrap_task_status_notification_params(
		default_target_name: Option<&String>,
		flat: bool,
		upstream_target: &str,
		params: &mut Option<Value>,
	) {
		let Some(params) = params else {
			return;
		};
		rewrap_task_id_in_json_value(default_target_name, flat, upstream_target, params);
	}

	/// Rewrap task ids embedded in terminal [`ServerResult`] variants for multiplex federation.
	pub fn rewrap_server_result_task_ids(
		default_target_name: Option<&String>,
		flat: bool,
		upstream_target: &str,
		result: &mut ServerResult,
	) {
		if !should_rewrap_task_ids(default_target_name, flat) {
			return;
		}
		match result {
			ServerResult::CreateTaskResult(CreateTaskResult { task, .. }) => {
				rewrap_outbound_task_id(default_target_name, flat, upstream_target, &mut task.task_id);
			},
			ServerResult::GetTaskResult(GetTaskResult { task, .. }) => {
				rewrap_outbound_task_id(default_target_name, flat, upstream_target, &mut task.task_id);
			},
			ServerResult::CancelTaskResult(CancelTaskResult { task, .. }) => {
				rewrap_outbound_task_id(default_target_name, flat, upstream_target, &mut task.task_id);
			},
			_ => {},
		}
	}

	/// Task branches of outbound multiplex rewrap (notifications + RPC responses).
	pub fn rewrap_outbound_multiplex_task_message(
		default_target_name: Option<&String>,
		flat: bool,
		upstream_target: &str,
		msg: &mut ServerJsonRpcMessage,
	) {
		if default_target_name.is_some() {
			return;
		}
		match msg {
			ServerJsonRpcMessage::Notification(jn) => {
				if let ServerNotification::CustomNotification(n) = &mut jn.notification
					&& n.method == TASK_STATUS_NOTIFICATION_METHOD
				{
					rewrap_task_status_notification_params(
						default_target_name,
						flat,
						upstream_target,
						&mut n.params,
					);
				}
			},
			ServerJsonRpcMessage::Response(jr) => {
				rewrap_server_result_task_ids(
					default_target_name,
					flat,
					upstream_target,
					&mut jr.result,
				);
			},
			_ => {},
		}
	}

	#[allow(dead_code)]
	pub fn rewrap_create_task_id(
		default_target_name: Option<&String>,
		flat: bool,
		target: &str,
		task_id: &mut String,
	) {
		rewrap_outbound_task_id(default_target_name, flat, target, task_id);
	}
}

#[cfg(feature = "adobe")]
pub use task_outbound::{
	rewrap_create_task_id, rewrap_outbound_task_id, rewrap_server_result_task_ids,
};

/// Encode upstream target into JSON-RPC request ids for server->client requests (multiplexing).
///
/// The client echoes the id verbatim in its response; decoding routes the reply to the correct
/// upstream without shared gateway state.
///
/// The embedded target name is client-echoed and only used to select a configured upstream via
/// [`UpstreamGroup::get`]. We intentionally do not validate that a request is still pending:
/// unknown ids are ignored by the upstream, and omitting shared pending state keeps routing
/// replica-safe across horizontally scaled gateways.
#[cfg(feature = "adobe")]
pub mod server_request_id {
	use rmcp::model::{NumberOrString, RequestId};

	const PREFIX: &str = "mcpgw";
	const SEP: char = '\u{1f}';

	/// Wrap an upstream JSON-RPC request id for the federated client.
	pub fn wrap_server_request_id(target: &str, id: &RequestId) -> RequestId {
		let (tag, payload) = match id {
			NumberOrString::Number(n) => ("N", n.to_string()),
			NumberOrString::String(s) => ("S", s.to_string()),
		};
		let encoded = format!("{PREFIX}{SEP}{target}{SEP}{tag}{SEP}{payload}");
		RequestId::String(encoded.into())
	}

	/// Parse a wrapped client-visible id back to `(target, upstream_id)`.
	pub fn unwrap_server_request_id(id: &RequestId) -> Option<(String, RequestId)> {
		let NumberOrString::String(s) = id else {
			return None;
		};
		let mut parts = s.splitn(4, SEP);
		if parts.next()? != PREFIX {
			return None;
		}
		let target = parts.next()?.to_string();
		let tag = parts.next()?;
		let payload = parts.next()?;
		let orig = match tag {
			"N" => RequestId::Number(payload.parse().ok()?),
			"S" => RequestId::String(payload.into()),
			_ => return None,
		};
		Some((target, orig))
	}
}

#[cfg(feature = "adobe")]
pub use server_request_id::{unwrap_server_request_id, wrap_server_request_id};

#[cfg(test)]
mod tests {
	use super::{
		parse_resource_name, resolve_client_task_id, unwrap_client_task_id, wrap_client_task_id,
	};
	#[cfg(feature = "adobe")]
	use super::server_request_id::{unwrap_server_request_id, wrap_server_request_id};
	#[cfg(feature = "adobe")]
	use rmcp::model::RequestId;
	use crate::mcp::upstream::UpstreamError;

	#[test]
	fn parse_resource_name_missing_delimiter_returns_error() {
		match parse_resource_name(None, "nodelim").unwrap_err() {
			UpstreamError::InvalidRequest(m) => {
				assert!(m.contains("invalid resource name"), "{m}");
			},
			e => panic!("unexpected {e:?}"),
		}
	}

	#[test]
	fn wrap_and_unwrap_client_task_id_multiplex() {
		let wrapped = wrap_client_task_id(None, "airbnb", "job-42");
		assert_eq!(wrapped, "airbnb_job-42");
		let (t, id) = unwrap_client_task_id(None, &wrapped).unwrap();
		assert_eq!(t, "airbnb");
		assert_eq!(id, "job-42");
	}

	#[test]
	fn wrap_client_task_id_single_backend_passthrough() {
		let default = "svc".to_string();
		assert_eq!(
			wrap_client_task_id(Some(&default), "ignored", "job-42"),
			"job-42"
		);
	}

	#[test]
	fn resolve_client_task_id_multiplex_prefixed() {
		let (t, id) = resolve_client_task_id(None, "a_job-42", 2, None).unwrap();
		assert_eq!(t, "a");
		assert_eq!(id, "job-42");
	}

	#[test]
	fn resolve_client_task_id_multiplex_bare_multi_errors() {
		let err = resolve_client_task_id(None, "job-42", 2, None).unwrap_err();
		assert!(
			err.to_string()
				.contains("flat task id requires task creation via tools/call")
		);
	}

	#[test]
	fn resolve_client_task_id_single_upstream_bare() {
		let (t, id) = resolve_client_task_id(None, "job-42", 1, Some("svc")).unwrap();
		assert_eq!(t, "svc");
		assert_eq!(id, "job-42");
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn wrap_and_unwrap_server_request_id_number() {
		let wrapped = wrap_server_request_id("mcp-server-everything", &RequestId::Number(42));
		let (target, orig) = unwrap_server_request_id(&wrapped).unwrap();
		assert_eq!(target, "mcp-server-everything");
		assert_eq!(orig, RequestId::Number(42));
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn wrap_and_unwrap_server_request_id_string() {
		let wrapped = wrap_server_request_id(
			"mcp-server-airbnb",
			&RequestId::String("upstream-id".into()),
		);
		let (target, orig) = unwrap_server_request_id(&wrapped).unwrap();
		assert_eq!(target, "mcp-server-airbnb");
		assert_eq!(orig, RequestId::String("upstream-id".into()));
	}

	#[cfg(feature = "adobe")]
	#[test]
	fn unwrap_server_request_id_rejects_plain_number() {
		assert!(unwrap_server_request_id(&RequestId::Number(1)).is_none());
	}
}
