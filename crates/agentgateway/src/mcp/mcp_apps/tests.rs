//! Unit tests for MCP Apps routing and capability cache.

use rmcp::model::{
	Content, JsonRpcNotification, JsonRpcVersion2_0, LoggingLevel, LoggingMessageNotification,
	LoggingMessageNotificationParam, Meta, RawContent, ResourceContents,
	ResourceUpdatedNotification, ResourceUpdatedNotificationParam, ServerCapabilities,
	ServerJsonRpcMessage, ServerNotification,
};
use serde_json::json;

use crate::mcp::upstream::UpstreamError;

use super::capabilities::TargetCapabilities;
use super::routing;

#[test]
fn wrap_unwrap_ui_resource_uri_roundtrip() {
	let wrapped =
		routing::wrap_resource_uri_mixed(None, "mcp-server-map", "ui://cesium-map/mcp-app.html");
	assert!(
		wrapped.starts_with("ui://"),
		"MCP Apps wrapped URIs must use ui scheme: {wrapped}"
	);
	let (target, uri) = routing::parse_resource_uri_mixed(None, &wrapped).unwrap();
	assert_eq!(target, "mcp-server-map");
	assert_eq!(uri, "ui://cesium-map/mcp-app.html");
}

#[test]
fn wrap_unwrap_plus_scheme_roundtrip() {
	let wrapped = routing::wrap_resource_uri_mixed(None, "my-target", "https://example.com/file.txt");
	assert!(
		wrapped.contains('+'),
		"expected target+scheme form: {wrapped}"
	);
	let (target, uri) = routing::parse_resource_uri_mixed(None, &wrapped).unwrap();
	assert_eq!(target, "my-target");
	assert_eq!(uri, "https://example.com/file.txt");
}

#[test]
fn wrap_unwrap_resource_uri_roundtrip_template() {
	let wrapped = routing::wrap_resource_uri_mixed(None, "t", "https://example.com/files/{filename}");
	let (_, uri) = routing::parse_resource_uri_mixed(None, &wrapped).unwrap();
	assert_eq!(uri, "https://example.com/files/{filename}");
}

#[test]
fn wrap_resource_template_uri_preserves_expressions() {
	let wrapped = routing::wrap_resource_template_uri_mixed(None, "svc1", "file:///{path}");
	assert!(
		wrapped.contains("{path}"),
		"template expression must remain literal: {wrapped}"
	);
	assert!(
		wrapped.starts_with("svc1+"),
		"expected plus-scheme multiplex: {wrapped}"
	);
}

#[test]
fn wrap_resource_template_uri_expanded_then_parse_plus() {
	let template = routing::wrap_resource_template_uri_mixed(None, "svc1", "file:///{path}");
	let expanded = template.replace("{path}", "README.md");
	let (target, uri) = routing::parse_resource_uri_mixed(None, &expanded).unwrap();
	assert_eq!(target, "svc1");
	assert_eq!(uri, "file:///README.md");
}

#[test]
fn wrap_resource_template_uri_ui_scheme() {
	let template = routing::wrap_resource_template_uri_mixed(None, "svc1", "ui://x/{path}");
	assert!(
		template.starts_with("ui://"),
		"MCP Apps template wrap must preserve ui scheme: {template}"
	);
	assert!(
		template.contains("{path}"),
		"expression must remain literal: {template}"
	);
	let expanded = template.replace("{path}", "mcp-app.html");
	let (target, uri) = routing::parse_resource_uri_mixed(None, &expanded).unwrap();
	assert_eq!(target, "svc1");
	assert_eq!(uri, "ui://x/mcp-app.html");
}

#[test]
fn wrap_resource_template_uri_single_backend_passthrough() {
	let default = Some("only".to_string());
	let result =
		routing::wrap_resource_template_uri_mixed(default.as_ref(), "only", "file:///{path}");
	assert_eq!(result, "file:///{path}");
}

#[test]
fn resource_name_single_backend_passthrough() {
	let default = Some("only".to_string());
	let out = routing::resource_name(default.as_ref(), "only", "mytool");
	assert_eq!(out, "mytool");
}

#[test]
fn resource_name_multiplex_prefixes() {
	let out = routing::resource_name(None, "svc1", "mytool");
	assert_eq!(out, "svc1_mytool");
}

#[test]
fn parse_resource_name_single_backend() {
	let default = Some("only".to_string());
	let (t, n) = routing::parse_resource_name(default.as_ref(), "mytool").unwrap();
	assert_eq!(t, "only");
	assert_eq!(n, "mytool");
}

#[test]
fn parse_resource_name_multiplex() {
	let (t, n) = routing::parse_resource_name(None, "svc1_mytool").unwrap();
	assert_eq!(t, "svc1");
	assert_eq!(n, "mytool");
}

#[test]
fn parse_task_id_multiplex() {
	let (t, id) = routing::parse_task_id(None, "svc1_task-123").unwrap();
	assert_eq!(t, "svc1");
	assert_eq!(id, "task-123");
}

#[test]
fn capability_cache_store_and_query() {
	let caps = TargetCapabilities::new();
	let sc = ServerCapabilities::builder()
		.enable_tools()
		.enable_prompts()
		.build();
	caps.store("a", sc);

	let all = vec!["a".to_string(), "b".to_string(), "c".to_string()];
	let with_tools = caps.upstreams_with_tools(&all);
	assert!(with_tools.contains(&"a".to_string()));
	assert!(with_tools.contains(&"b".to_string()));
	assert!(with_tools.contains(&"c".to_string()));
}

#[test]
fn capability_cache_filters_uncached_fail_open() {
	let caps = TargetCapabilities::new();
	let sc = ServerCapabilities::builder().enable_tools().build();
	caps.store("a", sc);
	let sc_b = ServerCapabilities::builder().build();
	caps.store("b", sc_b);

	let all = vec!["a".to_string(), "b".to_string()];
	let with_tools = caps.upstreams_with_tools(&all);
	assert!(with_tools.contains(&"a".to_string()));
	assert!(!with_tools.contains(&"b".to_string()));
}

#[test]
fn rewrite_tool_ui_meta_modern_form() {
	let mut meta = Meta::new();
	meta.insert("ui".into(), json!({ "resourceUri": "ui://x/app.html" }));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(None, "my-target", &mut opt);
	let ui = opt
		.as_ref()
		.unwrap()
		.get("ui")
		.and_then(|v| v.as_object())
		.unwrap();
	let uri = ui.get("resourceUri").and_then(|v| v.as_str()).unwrap();
	let expected = routing::wrap_resource_uri_mixed(None, "my-target", "ui://x/app.html");
	assert!(expected.starts_with("ui://"));
	assert_eq!(uri, expected);
}

#[test]
fn rewrite_tool_ui_meta_single_backend_noop() {
	let default = Some("only".to_string());
	let mut meta = Meta::new();
	meta.insert("ui".into(), json!({ "resourceUri": "ui://z/a" }));
	meta.insert("ui/resourceUri".into(), json!("ui://z/a"));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(default.as_ref(), "only", &mut opt);
	let out = opt.unwrap();
	let uri = out["ui"]["resourceUri"].as_str().unwrap();
	assert_eq!(uri, "ui://z/a");
	let leg = out["ui/resourceUri"].as_str().unwrap();
	assert_eq!(leg, "ui://z/a");
}

#[test]
fn rewrite_tool_ui_meta_wraps_legacy_flat_key() {
	let mut meta = Meta::new();
	meta.insert("ui/resourceUri".into(), json!("ui://app/page.html"));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(None, "a", &mut opt);
	let out = opt.unwrap();
	let leg = out["ui/resourceUri"]
		.as_str()
		.expect("legacy key must remain string");
	assert!(
		leg.starts_with("ui://"),
		"federated URI keeps ui scheme; got {leg}"
	);
	let (target, original) =
		routing::parse_resource_uri_mixed(None, leg).expect("legacy key must multiplex-parse");
	assert_eq!(target, "a");
	assert_eq!(original, "ui://app/page.html");
}

#[test]
fn rewrite_tool_ui_meta_wraps_both_keys_when_present() {
	let mut meta = Meta::new();
	meta.insert("ui".into(), json!({ "resourceUri": "ui://x/app.html" }));
	meta.insert("ui/resourceUri".into(), json!("ui://x/app.html"));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(None, "svc1", &mut opt);
	let out = opt.unwrap();
	let nested = out["ui"]["resourceUri"].as_str().unwrap();
	let flat = out["ui/resourceUri"].as_str().unwrap();
	let expected = routing::wrap_resource_uri_mixed(None, "svc1", "ui://x/app.html");
	assert_eq!(nested, expected);
	assert_eq!(flat, expected);
	assert_eq!(nested, flat);
}

#[test]
fn rewrite_tool_ui_meta_legacy_non_ui_scheme_noop() {
	let mut meta = Meta::new();
	meta.insert("ui/resourceUri".into(), json!("https://example.com/widget"));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(None, "svc1", &mut opt);
	assert_eq!(
		opt.unwrap()["ui/resourceUri"].as_str().unwrap(),
		"https://example.com/widget"
	);
}

#[cfg(feature = "adobe")]
#[test]
fn gateway_capabilities_advertise_tasks_with_list_and_cancel_when_adobe() {
	let caps = super::capabilities::gateway_merged_capabilities(/* multiplexing */ true, /* resource_subscribe */ false);
	let tasks = caps
		.tasks
		.as_ref()
		.expect("Adobe gateway must advertise tasks");
	assert!(
		tasks.list.is_some(),
		"MCP Inspector expects tasks.list; got {:?}",
		caps.tasks
	);
	assert!(tasks.cancel.is_some());
	let supports_call = tasks
		.requests
		.as_ref()
		.and_then(|r| r.tools.as_ref())
		.and_then(|t| t.call.as_ref());
	assert!(
		supports_call.is_some(),
		"MCP Inspector expects tasks.requests.tools.call"
	);
}

#[test]
fn rewrap_read_resource_contents_text_and_blob() {
	let ui_a = "ui://a/app.html";
	let ui_b = "ui://c/other";
	let tgt = "mcp-server-map";
	let mut contents = vec![
		ResourceContents::text("hello", ui_a),
		ResourceContents::blob("Ym9keQ==", ui_b),
	];
	let want_a = routing::wrap_resource_uri_mixed(None, tgt, ui_a);
	let want_b = routing::wrap_resource_uri_mixed(None, tgt, ui_b);
	assert!(
		want_a.starts_with("ui://"),
		"rewrapped UI resource A: {want_a}"
	);
	assert!(
		want_b.starts_with("ui://"),
		"rewrapped UI resource B: {want_b}"
	);
	routing::rewrap_read_resource_contents(None, tgt, &mut contents);
	match &contents[0] {
		ResourceContents::TextResourceContents { uri, text, .. } => {
			assert_eq!(uri, &want_a);
			assert_eq!(text, "hello");
		},
		_ => panic!("expected text contents"),
	}
	match &contents[1] {
		ResourceContents::BlobResourceContents { uri, blob, .. } => {
			assert_eq!(uri, &want_b);
			assert_eq!(blob, "Ym9keQ==");
		},
		_ => panic!("expected blob contents"),
	}
}

#[test]
fn rewrap_read_resource_contents_single_backend_noop() {
	let default = Some("only".to_string());
	let mut contents = vec![ResourceContents::text("x", "file:///a")];
	routing::rewrap_read_resource_contents(default.as_ref(), "only", &mut contents);
	match &contents[0] {
		ResourceContents::TextResourceContents { uri, .. } => assert_eq!(uri, "file:///a"),
		_ => panic!("expected text contents"),
	}
}

#[test]
fn rewrap_resource_updated_notification_multiplexes_uri() {
	let notif = ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(
		"https://example.com/r",
	));
	let mut msg = ServerJsonRpcMessage::Notification(JsonRpcNotification {
		jsonrpc: JsonRpcVersion2_0,
		notification: ServerNotification::ResourceUpdatedNotification(notif),
	});
	routing::rewrap_outbound_multiplex_server_message(None, "svc1", &mut msg);
	let ServerJsonRpcMessage::Notification(jn) = &msg else {
		panic!("expected notification");
	};
	let ServerNotification::ResourceUpdatedNotification(n) = &jn.notification else {
		panic!("expected resource updated notification");
	};
	assert!(
		n.params.uri.contains("svc1+https://") || n.params.uri.starts_with("ui://"),
		"unexpected federated uri: {}",
		n.params.uri
	);
}

#[test]
fn rewrap_task_status_notification_multiplexes_task_id() {
	use rmcp::model::{CustomNotification, JsonRpcNotification, JsonRpcVersion2_0, ServerNotification};
	use serde_json::json;

	let mut msg = ServerJsonRpcMessage::Notification(JsonRpcNotification {
		jsonrpc: JsonRpcVersion2_0,
		notification: ServerNotification::CustomNotification(CustomNotification::new(
			"notifications/tasks/status",
			Some(json!({ "taskId": "job-42", "status": "working" })),
		)),
	});
	crate::mcp::multiplex_naming::task_outbound::rewrap_outbound_multiplex_task_message(
		None, false, "svc1", &mut msg,
	);
	let ServerJsonRpcMessage::Notification(jn) = &msg else {
		panic!("expected notification");
	};
	let ServerNotification::CustomNotification(n) = &jn.notification else {
		panic!("expected custom notification");
	};
	assert_eq!(n.method, "notifications/tasks/status");
	let params = n.params.as_ref().expect("params");
	assert_eq!(params["taskId"], "svc1_job-42");
}

#[test]
fn rewrap_server_result_get_task_multiplexes_task_id() {
	use rmcp::model::{GetTaskResult, ServerResult, Task, TaskStatus};

	let ts = "2020-01-01T00:00:00Z";
	let mut result = ServerResult::GetTaskResult(GetTaskResult {
		meta: None,
		task: Task::new("job-7".into(), TaskStatus::Working, ts.into(), ts.into()),
	});
	routing::rewrap_server_result_task_ids(None, false, "airbnb", &mut result);
	let ServerResult::GetTaskResult(gtr) = result else {
		panic!("expected GetTaskResult");
	};
	assert_eq!(gtr.task.task_id, "airbnb_job-7");
}

#[test]
fn rewrap_task_status_notification_flat_mode_keeps_bare_id() {
	use rmcp::model::{CustomNotification, JsonRpcNotification, JsonRpcVersion2_0, ServerNotification};
	use serde_json::json;

	let mut msg = ServerJsonRpcMessage::Notification(JsonRpcNotification {
		jsonrpc: JsonRpcVersion2_0,
		notification: ServerNotification::CustomNotification(CustomNotification::new(
			"notifications/tasks/status",
			Some(json!({ "taskId": "job-42", "status": "working" })),
		)),
	});
	crate::mcp::multiplex_naming::task_outbound::rewrap_outbound_multiplex_task_message(
		None, true, "svc1", &mut msg,
	);
	let ServerJsonRpcMessage::Notification(jn) = &msg else {
		panic!("expected notification");
	};
	let ServerNotification::CustomNotification(n) = &jn.notification else {
		panic!("expected custom notification");
	};
	let params = n.params.as_ref().expect("params");
	assert_eq!(params["taskId"], "job-42");
}

#[test]
fn parse_resource_uri_mixed_single_backend_passthrough() {
	let default = Some("only".to_string());
	let (t, u) = routing::parse_resource_uri_mixed(default.as_ref(), "anything://x/y").unwrap();
	assert_eq!(t, "only");
	assert_eq!(u, "anything://x/y");
}

#[test]
fn parse_task_id_single_backend_passthrough() {
	let default = Some("only".to_string());
	let (t, id) = routing::parse_task_id(default.as_ref(), "task-123").unwrap();
	assert_eq!(t, "only");
	assert_eq!(id, "task-123");
}

#[test]
fn parse_resource_uri_mixed_ui_non_url_returns_error() {
	let err = routing::parse_resource_uri_mixed(None, "ui://[/").unwrap_err();
	let s = err.to_string();
	assert!(
		s.contains("invalid multiplex URI"),
		"expected URL parse failure: {err:?}"
	);
}

#[test]
fn parse_resource_uri_mixed_ui_missing_query_returns_error() {
	let err = routing::parse_resource_uri_mixed(None, "ui://enc/").unwrap_err();
	let s = err.to_string();
	assert!(
		s.contains("missing") && s.contains("u"),
		"expected missing u= query: {err:?}"
	);
}

#[test]
fn parse_resource_uri_mixed_plus_missing_separator_returns_error() {
	match routing::parse_resource_uri_mixed(None, "no-plus-here").unwrap_err() {
		UpstreamError::InvalidRequest(m) => {
			assert!(m.contains("missing target"), "{m}");
		},
		e => panic!("unexpected {e:?}"),
	}
}

#[test]
fn parse_resource_uri_mixed_plus_missing_scheme_returns_error() {
	match routing::parse_resource_uri_mixed(None, "svc1+notascheme").unwrap_err() {
		UpstreamError::InvalidRequest(m) => assert!(m.contains("missing scheme"), "{m}"),
		e => panic!("unexpected {e:?}"),
	}
}

#[test]
fn parse_resource_uri_mixed_ui_missing_host_returns_error() {
	match routing::parse_resource_uri_mixed(None, "ui:///?u=test").unwrap_err() {
		UpstreamError::InvalidRequest(m) => {
			assert!(
				m.contains("no host"),
				"expected missing-host error, got {m:?}"
			);
		},
		e => panic!("unexpected {e:?}"),
	}
}

#[test]
fn wrap_resource_template_uri_mixed_no_scheme_passthrough() {
	let tmpl = "just/a/path/{x}";
	let out = routing::wrap_resource_template_uri_mixed(None, "svc", tmpl);
	assert_eq!(out, tmpl);
}

#[test]
fn wrap_resource_template_uri_mixed_templated_scheme_passthrough() {
	let tmpl = "{scheme}://x/y";
	let out = routing::wrap_resource_template_uri_mixed(None, "svc", tmpl);
	assert_eq!(out, tmpl);
}

#[test]
fn wrap_resource_template_uri_mixed_templated_scheme_segment_passthrough() {
	let tmpl = "htt{x}p://x/y";
	let out = routing::wrap_resource_template_uri_mixed(None, "svc", tmpl);
	assert_eq!(out, tmpl);
}

#[test]
fn rewrap_outbound_multiplex_server_message_single_backend_noop() {
	let uri = "https://example.com/r";
	let default = Some("only".to_string());
	let notif = ResourceUpdatedNotification::new(ResourceUpdatedNotificationParam::new(uri));
	let mut msg = ServerJsonRpcMessage::Notification(JsonRpcNotification {
		jsonrpc: JsonRpcVersion2_0,
		notification: ServerNotification::ResourceUpdatedNotification(notif),
	});
	routing::rewrap_outbound_multiplex_server_message(default.as_ref(), "upstream", &mut msg);
	let ServerJsonRpcMessage::Notification(jn) = &msg else {
		panic!("expected notification");
	};
	let ServerNotification::ResourceUpdatedNotification(n) = &jn.notification else {
		panic!("expected resource updated notification");
	};
	assert_eq!(n.params.uri.as_str(), uri);
}

#[test]
fn rewrap_outbound_multiplex_server_message_non_resource_updated_noop() {
	let logged = LoggingMessageNotification::new(LoggingMessageNotificationParam::new(
		LoggingLevel::Info,
		json!("x"),
	));
	let mut msg = ServerJsonRpcMessage::Notification(JsonRpcNotification {
		jsonrpc: JsonRpcVersion2_0,
		notification: ServerNotification::LoggingMessageNotification(logged.clone()),
	});
	routing::rewrap_outbound_multiplex_server_message(None, "svc1", &mut msg);
	let ServerJsonRpcMessage::Notification(jn) = &msg else {
		panic!("expected notification");
	};
	match &jn.notification {
		ServerNotification::LoggingMessageNotification(l) => {
			assert_eq!(l.params.data, logged.params.data);
			assert_eq!(l.params.level, logged.params.level);
		},
		other => panic!("expected LoggingMessage unchanged, got {other:?}"),
	}
}

#[test]
fn rewrite_tool_ui_meta_none_meta_noop() {
	let mut opt: Option<Meta> = None;
	routing::rewrite_tool_ui_meta(None, "t", &mut opt);
	assert!(opt.is_none());
}

#[test]
fn rewrite_tool_ui_meta_ui_non_object_noop() {
	let mut meta = Meta::new();
	meta.insert("ui".into(), json!("not-an-object"));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(None, "svc1", &mut opt);
	let out = opt.unwrap();
	let ui = out.get("ui").expect("ui key");
	assert_eq!(ui.as_str(), Some("not-an-object"));
	assert!(out.get("resourceUri").is_none());
	assert!(out.get("ui/resourceUri").is_none());
}

#[test]
fn rewrite_tool_ui_meta_resource_uri_non_string_noop() {
	let mut meta = Meta::new();
	meta.insert("ui".into(), json!({ "resourceUri": 42 }));
	let mut opt = Some(meta);
	routing::rewrite_tool_ui_meta(None, "svc1", &mut opt);
	let out = opt.unwrap();
	let ui_obj = out["ui"].as_object().expect("ui.object");
	let r = ui_obj.get("resourceUri").expect("resourceUri");
	assert!(r.as_i64().is_some());
}

#[test]
fn rewrap_call_tool_result_content_ui_embedded_resource() {
	let mut content = vec![Content::resource(ResourceContents::TextResourceContents {
		uri: "ui://basic/app".to_string(),
		mime_type: Some("text/html;profile=mcp-app".to_string()),
		text: String::new(),
		meta: None,
	})];
	routing::rewrap_call_tool_result_content(None, "mcp-server-a2ui", &mut content);
	let RawContent::Resource(embedded) = &content[0].raw else {
		panic!("expected embedded resource");
	};
	let ResourceContents::TextResourceContents { uri, .. } = &embedded.resource else {
		panic!("expected text resource contents");
	};
	assert!(uri.starts_with("ui://"), "expected ui scheme: {uri}");
	assert!(uri.contains("u="), "expected multiplex query: {uri}");
	let (target, original) = routing::parse_resource_uri_mixed(None, uri).unwrap();
	assert_eq!(target, "mcp-server-a2ui");
	assert_eq!(original, "ui://basic/app");
}

#[test]
fn rewrap_call_tool_result_content_a2ui_plus_scheme() {
	let mut content = vec![Content::resource(ResourceContents::TextResourceContents {
		uri: "a2ui://ping-result".to_string(),
		mime_type: Some("application/json+a2ui".to_string()),
		text: "[]".to_string(),
		meta: None,
	})];
	routing::rewrap_call_tool_result_content(None, "mcp-server-a2ui", &mut content);
	let RawContent::Resource(embedded) = &content[0].raw else {
		panic!("expected embedded resource");
	};
	let ResourceContents::TextResourceContents { uri, .. } = &embedded.resource else {
		panic!("expected text resource contents");
	};
	assert!(
		uri.starts_with("mcp-server-a2ui+a2ui://"),
		"expected plus-scheme multiplex: {uri}"
	);
}

#[test]
fn rewrap_call_tool_result_content_single_backend_noop() {
	let default = Some("only".to_string());
	let mut content = vec![Content::resource(ResourceContents::TextResourceContents {
		uri: "ui://basic/app".to_string(),
		mime_type: None,
		text: String::new(),
		meta: None,
	})];
	routing::rewrap_call_tool_result_content(default.as_ref(), "only", &mut content);
	let RawContent::Resource(embedded) = &content[0].raw else {
		panic!("expected embedded resource");
	};
	let ResourceContents::TextResourceContents { uri, .. } = &embedded.resource else {
		panic!("expected text resource contents");
	};
	assert_eq!(uri, "ui://basic/app");
}

#[test]
fn rewrap_call_tool_result_content_skips_non_resource_blocks() {
	let mut content = vec![
		Content::text("hello"),
		Content::resource(ResourceContents::text("body", "ui://x")),
	];
	routing::rewrap_call_tool_result_content(None, "svc1", &mut content);
	let RawContent::Text(_) = &content[0].raw else {
		panic!("text block unchanged");
	};
	let RawContent::Resource(embedded) = &content[1].raw else {
		panic!("expected embedded resource");
	};
	let ResourceContents::TextResourceContents { uri, .. } = &embedded.resource else {
		panic!("expected text resource contents");
	};
	assert!(uri.starts_with("ui://") && uri.contains("u="));
}
