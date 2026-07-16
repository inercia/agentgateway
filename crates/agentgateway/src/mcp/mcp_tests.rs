use std::net::SocketAddr;

use agent_core::strng;
use itertools::Itertools;
use openapiv3::OpenAPI;
use rmcp::RoleClient;
use rmcp::model::{ClientJsonRpcMessage, InitializeRequestParams, RequestId};
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpServerConfig;
use secrecy::SecretString;

use crate::http::auth::BackendAuth;
use crate::http::authorization::{PolicySet, RuleSet};
use crate::http::sessionpersistence::MCPSession;
use crate::mcp::guardrails;
use crate::mcp::handler::Relay;
use crate::mcp::router::{McpBackendGroup, McpTarget};
use crate::mcp::{FailureMode, McpAuthorization};
use crate::proxy::httpproxy::PolicyClient;
use crate::test_helpers::extauthmock::{ExtAuthMock, deny_response};
use crate::test_helpers::proxymock::{
	BIND_KEY, TestBind, basic_named_route, basic_route, setup_proxy_test, simple_bind,
};
use crate::test_helpers::ratelimitmock::{RateLimitMock, over_limit_response};
use crate::types::agent::{BackendTrafficPolicy, FrontendPolicy, PolicyTarget, TargetedPolicy};
use crate::*;

#[tokio::test]
async fn stream_to_stream_single() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy(&mock, true, false).await;
	let client = mcp_streamable_client(io).await;
	standard_assertions(client).await;
}

#[tokio::test]
async fn sse_to_stream_single() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy(&mock, true, false).await;
	let client = mcp_sse_client(io).await;
	standard_sse_assertions(client).await;
}

#[tokio::test]
async fn stream_to_sse_single() {
	let mock = mock_sse_server().await;
	let (_bind, io) = setup_proxy(&mock, true, true).await;
	let client = mcp_streamable_client(io).await;
	standard_assertions(client).await;
}

#[tokio::test]
async fn sse_to_sse_single() {
	let mock = mock_sse_server().await;
	let (_bind, io) = setup_proxy(&mock, true, true).await;
	let client = mcp_sse_client(io).await;
	standard_sse_assertions(client).await;
}

#[tokio::test]
async fn stream_to_multiplex() {
	let mock_stream = mock_streamable_http_server(true).await;
	let mock_sse = mock_sse_server().await;
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_multiplex_mcp_backend(
			"mcp",
			vec![
				("sse", mock_sse.addr, true),
				("mcp", mock_stream.addr, false),
			],
			true,
		)
		.with_bind(simple_bind())
		.with_route(basic_named_route(strng::new("/mcp")));
	let io = t.serve_real_listener(strng::new("bind")).await;
	let client = mcp_streamable_client(io).await;
	let tools = client.list_tools(None).await.unwrap();
	let t = tools
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.filter(|n| n.contains("decrement") || n.contains("echo"))
		.collect_vec();
	assert_eq!(
		t,
		vec![
			"mcp_decrement".to_string(),
			"mcp_echo".to_string(),
			"mcp_echo_http".to_string(),
			"sse_decrement".to_string(),
			"sse_echo".to_string(),
			"sse_echo_http".to_string()
		]
	);

	let ctr = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("mcp_echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();
	assert_eq!(
		&ctr.content[0].raw.as_text().unwrap().text,
		r#"{"hi":"world"}"#
	);

	let ctr = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("sse_echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();
	assert_eq!(
		&ctr.content[0].raw.as_text().unwrap().text,
		r#"{"hi":"world"}"#
	);

	// No target set...
	assert!(
		client
			.call_tool(
				rmcp::model::CallToolRequestParams::new("echo").with_arguments(
					serde_json::json!({"hi": "world"})
						.as_object()
						.cloned()
						.unwrap(),
				),
			)
			.await
			.is_err()
	);
}

#[tokio::test]
async fn stream_to_multiplex_resources() {
	let mock_a = mock_streamable_http_server(true).await;
	let mock_b = mock_streamable_http_server(true).await;
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_multiplex_mcp_backend(
			"mcp",
			vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
			true,
		)
		.with_bind(simple_bind())
		.with_route(basic_named_route(strng::new("/mcp")));
	let io = t.serve_real_listener(strng::new("bind")).await;
	let client = mcp_streamable_client(io).await;

	// 1. list_resources should return resources from both backends with prefixed URIs
	let resources = client.list_resources(None).await.unwrap();
	let uris: Vec<String> = resources
		.resources
		.iter()
		.map(|r| r.uri.clone())
		.sorted()
		.collect();
	// Each mock provides "str:////Users/to/some/path/" and "memo://insights"
	// With multiplexing these become "a+str:////Users/to/some/path/", "a+memo://insights", etc.
	assert!(
		uris.iter().any(|u| u == "a+memo://insights"),
		"Expected 'a+memo://insights' in resources, got: {:?}",
		uris
	);
	assert!(
		uris.iter().any(|u| u == "b+memo://insights"),
		"Expected 'b+memo://insights' in resources, got: {:?}",
		uris
	);

	// 2. read_resource with prefixed URI should route to the correct backend
	let result = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(
			"a+memo://insights",
		))
		.await
		.unwrap();
	assert!(
		!result.contents.is_empty(),
		"Expected non-empty resource contents"
	);
	let text = match &result.contents[0] {
		rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.clone(),
		other => panic!("Expected text resource content, got: {:?}", other),
	};
	assert!(
		text.contains("Business Intelligence Memo"),
		"Expected memo content, got: {}",
		text
	);

	// Also read from backend "b"
	let result_b = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(
			"b+memo://insights",
		))
		.await
		.unwrap();
	assert!(
		!result_b.contents.is_empty(),
		"Expected non-empty resource contents from backend b"
	);

	// 3. list_resource_templates should not error (mock returns empty)
	let templates = client.list_resource_templates(None).await.unwrap();
	// Templates may be empty since mock server returns empty vec, but should not error
	assert!(
		templates.resource_templates.is_empty(),
		"Expected empty resource templates from mock, got: {:?}",
		templates.resource_templates
	);

	// 4. read_resource with unprefixed URI should fail
	assert!(
		client
			.read_resource(rmcp::model::ReadResourceRequestParams::new(
				"memo://insights",
			))
			.await
			.is_err(),
		"Expected error when reading resource without service prefix"
	);
}

#[tokio::test]
async fn stateless_multiplex_tool_call_initializes_only_target() {
	let mock_a = mock_streamable_http_server(true).await;
	let mock_b = mock_streamable_http_server(true).await;
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_multiplex_mcp_backend(
			"mcp",
			vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
			false,
		)
		.with_bind(simple_bind())
		.with_route(basic_named_route(strng::new("/mcp")));
	let io = t.serve_real_listener(strng::new("bind")).await;
	let client = mcp_streamable_client(io).await;
	let a_init_before = mock_a.init_count().await;
	let b_init_before = mock_b.init_count().await;

	// A direct tool call to one target should initialize only that target.
	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("a_echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();
	let a_init_after = mock_a.init_count().await;
	let b_init_after = mock_b.init_count().await;
	assert_eq!(a_init_after, a_init_before + 1);
	assert_eq!(b_init_after, b_init_before);
}

#[tokio::test]
async fn stateless_multiplex_get_prompt_initializes_only_target() {
	let mock_a = mock_streamable_http_server(true).await;
	let mock_b = mock_streamable_http_server(true).await;
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_multiplex_mcp_backend(
			"mcp",
			vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
			false,
		)
		.with_bind(simple_bind())
		.with_route(basic_named_route(strng::new("/mcp")));
	let io = t.serve_real_listener(strng::new("bind")).await;
	let client = mcp_streamable_client(io).await;
	let a_init_before = mock_a.init_count().await;
	let b_init_before = mock_b.init_count().await;

	let _ = client
		.get_prompt(
			rmcp::model::GetPromptRequestParams::new("a_example_prompt").with_arguments(
				serde_json::json!({"message": "hello"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();

	let a_init_after = mock_a.init_count().await;
	let b_init_after = mock_b.init_count().await;
	assert_eq!(a_init_after, a_init_before + 1);
	assert_eq!(b_init_after, b_init_before);
}

#[tokio::test]
async fn stateless_multiplex_delete_session_skips_uninitialized_targets() {
	let mock_a = mock_streamable_http_server(true).await;
	let mock_b = mock_streamable_http_server(true).await;
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_streamable_target("a", mock_a.addr),
				fake_streamable_target("b", mock_b.addr),
			],
			stateful: false,
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();
	let session_manager =
		super::session::SessionManager::new(http::sessionpersistence::Encoder::base64());
	let mut session = session_manager.create_stateless_session(relay);
	let parts = ::http::Request::<()>::builder()
		.method(http::Method::POST)
		.uri("http://localhost/mcp")
		.body(())
		.unwrap()
		.into_parts()
		.0;

	session
		.stateless_send_and_initialize(
			parts.clone(),
			ClientJsonRpcMessage::request(
				rmcp::model::CallToolRequest::new(
					rmcp::model::CallToolRequestParams::new("a_echo").with_arguments(
						serde_json::json!({"hi": "world"})
							.as_object()
							.cloned()
							.unwrap(),
					),
				)
				.into(),
				RequestId::Number(1),
			),
		)
		.await
		.unwrap();

	let sessions = match http::sessionpersistence::SessionState::decode(
		session.id.as_ref(),
		&http::sessionpersistence::Encoder::base64(),
	)
	.unwrap()
	{
		http::sessionpersistence::SessionState::MCP(state) => state.sessions,
		_ => panic!("expected MCP session state"),
	};
	assert_eq!(sessions.len(), 2);
	assert_eq!(sessions[0].target_name.as_deref(), Some("a"));
	assert!(sessions[0].session.is_some());
	assert_eq!(sessions[1].target_name.as_deref(), Some("b"));
	assert!(sessions[1].session.is_none());

	let response = session.delete_session(parts).await.unwrap();
	assert_eq!(response.status(), http::StatusCode::ACCEPTED);
	assert_eq!(mock_b.init_count().await, 0);
}

#[tokio::test]
async fn stateful_streamable_http_rejects_no_session_non_initialize_messages() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy(&mock, true, false).await;
	let client = reqwest::Client::new();
	let url = format!("http://{io}/mcp");

	for body in [
		serde_json::json!({
			"jsonrpc": "2.0",
			"method": "notifications/initialized",
			"params": {}
		}),
		serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"result": {}
		}),
		serde_json::json!({
			"jsonrpc": "2.0",
			"id": 1,
			"error": {
				"code": -32603,
				"message": "client response error"
			}
		}),
	] {
		let response = mcp_json_post(&client, &url, &body).send().await.unwrap();
		assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
		assert!(
			response.headers().get("mcp-session-id").is_none(),
			"rejected no-session message must not create a session"
		);
	}
}

#[tokio::test]
async fn streamable_http_validates_protocol_version_header() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy(&mock, true, false).await;
	let client = reqwest::Client::new();
	let url = format!("http://{io}/mcp");
	let init_body = serde_json::json!({
		"jsonrpc": "2.0",
		"id": 1,
		"method": "initialize",
		"params": {
			"protocolVersion": "2025-06-18",
			"capabilities": {},
			"clientInfo": {
				"name": "test client",
				"version": "0.0.1"
			}
		}
	});

	let unsupported = mcp_json_post(&client, &url, &init_body)
		.header("mcp-protocol-version", "1900-01-01")
		.send()
		.await
		.unwrap();
	assert_eq!(unsupported.status(), reqwest::StatusCode::BAD_REQUEST);

	let mismatch = mcp_json_post(&client, &url, &init_body)
		.header("mcp-protocol-version", "2025-11-25")
		.send()
		.await
		.unwrap();
	assert_eq!(mismatch.status(), reqwest::StatusCode::BAD_REQUEST);

	let init = mcp_json_post(&client, &url, &init_body)
		.header("mcp-protocol-version", "2025-06-18")
		.send()
		.await
		.unwrap();
	assert_eq!(init.status(), reqwest::StatusCode::OK);
	let session_id = init
		.headers()
		.get("mcp-session-id")
		.expect("initialize response should include a session id")
		.to_str()
		.unwrap()
		.to_string();

	let list_body = serde_json::json!({
		"jsonrpc": "2.0",
		"id": 2,
		"method": "tools/list",
		"params": {}
	});
	let subsequent_unsupported = mcp_json_post(&client, &url, &list_body)
		.header("mcp-session-id", session_id)
		.header("mcp-protocol-version", "1900-01-01")
		.send()
		.await
		.unwrap();
	assert_eq!(
		subsequent_unsupported.status(),
		reqwest::StatusCode::BAD_REQUEST
	);
}

fn mcp_json_post<'a>(
	client: &'a reqwest::Client,
	url: &'a str,
	body: &'a serde_json::Value,
) -> reqwest::RequestBuilder {
	client
		.post(url)
		.header(
			http::header::ACCEPT.as_str(),
			"application/json, text/event-stream",
		)
		.header(http::header::CONTENT_TYPE.as_str(), "application/json")
		.json(body)
}

#[tokio::test]
async fn stateless_to_stateful() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy(&mock, false, false).await;
	let client = mcp_streamable_client(io).await;
	standard_assertions(client).await;
}

#[tokio::test]
async fn stateless_to_stateless() {
	let mock = mock_streamable_http_server(false).await;
	let (_bind, io) = setup_proxy(&mock, false, false).await;
	let client = mcp_streamable_client(io).await;
	standard_assertions(client).await;
}

#[tokio::test]
async fn stream_to_stream_single_tls() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::BackendAuth(BackendAuth::Key {
			value: SecretString::new("my-key".into()),
			location: None,
		})],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let ctr = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo_http").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();
	assert_eq!(
		&ctr.content[0].raw.as_text().unwrap().text,
		r#"Bearer my-key"#
	);
}

/// Test that calling a tool denied by MCP authorization policy returns proper JSON-RPC error
/// with INVALID_PARAMS error code (-32602) and message "Unknown tool: {tool_name}"
#[tokio::test]
async fn authorization_denied_returns_unknown_tool_error() {
	let mock = mock_streamable_http_server(true).await;

	// Create an MCP authorization policy that denies all tools
	// The deny rule matches all tools; no allow rules means everything is denied
	let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],                                                       // no allow rules
		vec![Arc::new(cel::Expression::new_strict("true").unwrap())], // deny all
		vec![],
	)));

	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
	)
	.await;

	let client = mcp_streamable_client(io).await;

	// Attempt to call a tool - should fail with "Unknown tool" error
	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await;

	// The call should fail
	assert!(
		result.is_err(),
		"Expected tool call to fail due to authorization denial"
	);

	let err = result.unwrap_err();

	// Verify error code is INVALID_PARAMS (-32602) and message format
	let mcp_error = match &err {
		rmcp::ServiceError::McpError(mcp_error) => mcp_error,
		// rmcp::ServiceError::TransportSend(d) => d.downcast::(),
		other => panic!("Expected ServiceError::McpError, got: {:?}", other),
	};

	assert_eq!(
		mcp_error.code.0, -32602,
		"Expected INVALID_PARAMS error code (-32602), got: {}",
		mcp_error.code.0
	);
	assert_eq!(
		mcp_error.message.as_ref(),
		"Unknown tool: echo",
		"Expected error message 'Unknown tool: echo', got: {}",
		mcp_error.message
	);
}

/// Test that getting a prompt denied by MCP authorization policy returns proper JSON-RPC error
/// with INVALID_PARAMS error code (-32602) and message "Unknown prompt: {prompt_name}"
#[tokio::test]
async fn authorization_denied_returns_unknown_prompt_error() {
	let mock = mock_streamable_http_server(true).await;

	// Create an MCP authorization policy that denies all prompts
	let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],                                                       // no allow rules
		vec![Arc::new(cel::Expression::new_strict("true").unwrap())], // deny all
		vec![],
	)));

	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
	)
	.await;

	let client = mcp_streamable_client(io).await;

	// Attempt to get a prompt - should fail with "Unknown prompt" error
	let result = client
		.get_prompt(rmcp::model::GetPromptRequestParams::new("example_prompt"))
		.await;

	// The call should fail
	assert!(
		result.is_err(),
		"Expected get_prompt call to fail due to authorization denial"
	);

	let err = result.unwrap_err();

	// Verify error code is INVALID_PARAMS (-32602) and message format
	match &err {
		rmcp::ServiceError::McpError(mcp_error) => {
			assert_eq!(
				mcp_error.code.0, -32602,
				"Expected INVALID_PARAMS error code (-32602), got: {}",
				mcp_error.code.0
			);
			assert_eq!(
				mcp_error.message.as_ref(),
				"Unknown prompt: example_prompt",
				"Expected error message 'Unknown prompt: example_prompt', got: {}",
				mcp_error.message
			);
		},
		other => panic!("Expected ServiceError::McpError, got: {:?}", other),
	}
}

/// Test that reading a resource denied by MCP authorization policy returns proper JSON-RPC error
/// with INVALID_PARAMS error code (-32602) and message "Unknown resource: {resource_uri}"
#[tokio::test]
async fn authorization_denied_returns_unknown_resource_error() {
	let mock = mock_streamable_http_server(true).await;

	// Create an MCP authorization policy that denies all resources
	let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],                                                       // no allow rules
		vec![Arc::new(cel::Expression::new_strict("true").unwrap())], // deny all
		vec![],
	)));

	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
	)
	.await;

	let client = mcp_streamable_client(io).await;

	// Attempt to read a resource - should fail with "Unknown resource" error
	let result = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(
			"memo://insights",
		))
		.await;

	// The call should fail
	assert!(
		result.is_err(),
		"Expected read_resource call to fail due to authorization denial"
	);

	let err = result.unwrap_err();

	// Verify error code is INVALID_PARAMS (-32602) and message format
	match &err {
		rmcp::ServiceError::McpError(mcp_error) => {
			assert_eq!(
				mcp_error.code.0, -32602,
				"Expected INVALID_PARAMS error code (-32602), got: {}",
				mcp_error.code.0
			);
			assert_eq!(
				mcp_error.message.as_ref(),
				"Unknown resource: memo://insights",
				"Expected error message 'Unknown resource: memo://insights', got: {}",
				mcp_error.message
			);
		},
		other => panic!("Expected ServiceError::McpError, got: {:?}", other),
	}
}

#[tokio::test]
async fn resource_subscribe_and_unsubscribe_forward_to_single_backend() {
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy(&mock, true, false).await;
	let client = mcp_streamable_client(io).await;

	client
		.subscribe(rmcp::model::SubscribeRequestParams::new("memo://insights"))
		.await
		.unwrap();
	client
		.unsubscribe(rmcp::model::UnsubscribeRequestParams::new(
			"memo://insights",
		))
		.await
		.unwrap();
}

/// Test that a deny policy targeting a specific tool filters only that tool from list_tools,
/// while leaving all other tools accessible.
#[tokio::test]
async fn authorization_deny_specific_tool_filters_only_that_tool() {
	let mock = mock_streamable_http_server(true).await;

	// Create a deny policy that only denies the "echo" tool
	let deny_echo_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(
			cel::Expression::new_strict(r#"mcp.tool.name == "echo""#).unwrap(),
		)],
		vec![],
	)));

	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_echo_policy)],
	)
	.await;

	let client = mcp_streamable_client(io).await;

	// List tools - "echo" should be filtered out, all others should remain
	let tools = client.list_tools(None).await.unwrap();
	let tool_names: Vec<String> = tools
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.collect();

	// The mock server has: increment, decrement, get_value, say_hello, echo, sum, echo_http
	// After denying "echo", we should have all except "echo"
	assert!(
		!tool_names.contains(&"echo".to_string()),
		"echo should be denied but was found in tools: {:?}",
		tool_names
	);
	assert!(
		tool_names.contains(&"increment".to_string()),
		"increment should be allowed but was not found in tools: {:?}",
		tool_names
	);
	assert!(
		tool_names.contains(&"decrement".to_string()),
		"decrement should be allowed but was not found in tools: {:?}",
		tool_names
	);
	assert!(
		tool_names.len() >= 5,
		"Expected at least 5 tools after denying 1, got {}: {:?}",
		tool_names.len(),
		tool_names
	);
}

/// Test that a deny policy using request.headers correctly filters tools per-agent.
/// This exercises the router.rs fix that registers authorization policies on the log's
/// CEL context so the request snapshot includes headers needed by CEL expressions.
#[tokio::test]
async fn authorization_deny_with_request_header_filters_per_agent() {
	use std::collections::HashMap;

	use ::http::{HeaderName, HeaderValue};
	use rmcp::ServiceExt;
	use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
	use rmcp::transport::StreamableHttpClientTransport;
	use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

	let mock = mock_streamable_http_server(true).await;

	// Deny "echo" only when request header x-agent-name == "agent-one"
	let deny_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(
			cel::Expression::new_strict(
				r#"mcp.tool.name == "echo" && request.headers["x-agent-name"] == "agent-one""#,
			)
			.unwrap(),
		)],
		vec![],
	)));

	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_policy)],
	)
	.await;

	// Helper to create a client with custom headers
	let make_client = |addr: SocketAddr, agent_name: &'static str| async move {
		let mut headers = HashMap::new();
		headers.insert(
			HeaderName::from_static("x-agent-name"),
			HeaderValue::from_static(agent_name),
		);
		let config = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/mcp"))
			.custom_headers(headers);
		let transport = StreamableHttpClientTransport::from_config(config);
		let client_info = ClientInfo::new(
			ClientCapabilities::default(),
			Implementation::new(format!("test-{agent_name}"), "0.0.1"),
		);
		client_info
			.serve(transport)
			.await
			.expect("client should connect")
	};

	// Agent-one: "echo" should be denied
	let client1 = make_client(io, "agent-one").await;
	let tools1: Vec<String> = client1
		.list_tools(None)
		.await
		.unwrap()
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.collect();

	assert!(
		!tools1.contains(&"echo".to_string()),
		"agent-one should NOT see 'echo' but tools were: {:?}",
		tools1
	);
	assert!(
		tools1.contains(&"increment".to_string()),
		"agent-one should still see 'increment' but tools were: {:?}",
		tools1
	);

	// Agent-two: "echo" should be allowed (header doesn't match deny rule)
	let client2 = make_client(io, "agent-two").await;
	let tools2: Vec<String> = client2
		.list_tools(None)
		.await
		.unwrap()
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.collect();

	assert!(
		tools2.contains(&"echo".to_string()),
		"agent-two SHOULD see 'echo' but tools were: {:?}",
		tools2
	);
	assert!(
		tools2.contains(&"increment".to_string()),
		"agent-two should still see 'increment' but tools were: {:?}",
		tools2
	);
}

#[tokio::test]
async fn mcp_authentication_early_response_transformation_has_request_context() {
	let mock = mock_streamable_http_server(true).await;
	let authn = crate::types::agent::McpAuthentication {
		issuer: "https://issuer.example.com".to_string(),
		audiences: vec!["mcp".to_string()],
		provider: None,
		resource_metadata: crate::types::agent::ResourceMetadata {
			extra: Default::default(),
		},
		jwt_validator: Arc::new(crate::http::jwt::Jwt::from_providers(
			vec![],
			crate::http::jwt::Mode::Strict,
			crate::http::auth::AuthorizationLocation::bearer_header(),
		)),
		mode: crate::types::agent::McpAuthenticationMode::Strict,
		client_id: None,
	};

	let mut t = setup_proxy_test("{}")
		.unwrap()
		.with_mcp_backend_policies(
			mock.addr,
			true,
			false,
			vec![BackendTrafficPolicy::McpAuthentication(authn)],
		)
		.with_bind(simple_bind())
		.with_route(basic_route(mock.addr));

	t.attach_route_policy(serde_json::json!({
		"transformations": {
			"response": {
				"set": {
					"x-request-id-from-cel": "request.headers[\"x-regression-id\"]",
					"x-request-path-from-cel": "request.path"
				}
			}
		}
	}))
	.await;

	let io = t.serve_real_listener(BIND_KEY).await;
	let resp = reqwest::Client::new()
		.get(format!(
			"http://{io}/.well-known/oauth-protected-resource/mcp"
		))
		.header("x-regression-id", "mcp-authn-snapshot")
		.send()
		.await
		.expect("metadata request should complete");

	assert_eq!(resp.status(), reqwest::StatusCode::OK);
	assert_eq!(
		resp
			.headers()
			.get("x-request-id-from-cel")
			.and_then(|v| v.to_str().ok()),
		Some("mcp-authn-snapshot")
	);
	assert_eq!(
		resp
			.headers()
			.get("x-request-path-from-cel")
			.and_then(|v| v.to_str().ok()),
		Some("/.well-known/oauth-protected-resource/mcp")
	);
}

async fn standard_assertions(client: RunningService<RoleClient, InitializeRequestParams>) {
	let tools = client.list_tools(None).await.unwrap();
	let t = tools
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.take(2)
		.collect_vec();
	assert_eq!(t, vec!["decrement".to_string(), "echo".to_string()]);
	let ctr = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();
	assert_eq!(
		&ctr.content[0].raw.as_text().unwrap().text,
		r#"{"hi":"world"}"#
	);
}

async fn standard_sse_assertions(client: LegacyService) {
	let tools = client.list_tools(None).await.unwrap();
	let t = tools
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.take(2)
		.collect_vec();
	assert_eq!(t, vec!["decrement".to_string(), "echo".to_string()]);
	let ctr = client
		.call_tool(legacy_rmcp::model::CallToolRequestParam {
			name: "echo".into(),
			arguments: serde_json::json!({"hi": "world"}).as_object().cloned(),
		})
		.await
		.unwrap();
	assert_eq!(
		&ctr.content[0].raw.as_text().unwrap().text,
		r#"{"hi":"world"}"#
	);
}

fn access_log_payload_policy() -> crate::types::frontend::LoggingPolicy {
	let mut policy: crate::types::frontend::LoggingPolicy =
		serde_json::from_value(serde_json::json!({
			"add": {
				"mcp_trace": "mcp.tool.arguments.traceId",
				"mcp_method_cel": "mcp.methodName",
				"mcp_session_cel": "mcp.sessionId",
				"mcp_tool_name_cel": "mcp.tool.name",
				"mcp_tool_target_cel": "mcp.tool.target",
				"mcp_prompt_name_cel": "mcp.prompt.name",
				"mcp_prompt_target_cel": "mcp.prompt.target",
				"mcp_resource_name_cel": "mcp.resource.name",
				"mcp_resource_target_cel": "mcp.resource.target",
				"mcp_args_cel": "mcp.tool.arguments",
				"mcp_result_cel": "mcp.tool.result",
				"mcp_error_cel": "mcp.tool.error",
				"mcp_success_cel": "mcp.success",
				"mcp_error_type_cel": "mcp.error.type",
				"mcp_error_msg_cel": "mcp.error.message",
				"mcp_error_code_cel": "mcp.error.code"
			}
		}))
		.unwrap();
	policy.init_access_log_policy();
	policy
}

async fn setup_access_log_mcp_proxy(mock: &MockServer) -> (TestBind, SocketAddr) {
	let (mut t, io) = setup_proxy(mock, true, false).await;
	let listener_name = t
		.pi
		.stores
		.read_binds()
		.bind(&BIND_KEY)
		.unwrap()
		.listeners
		.iter()
		.next()
		.unwrap()
		.name
		.clone();
	t.with_policy(TargetedPolicy {
		key: "frontend/accessLog".into(),
		name: None,
		target: PolicyTarget::Gateway(listener_name.clone().into()),
		inheritance: crate::types::agent::PolicyInheritance::Default,
		policy: FrontendPolicy::AccessLog(access_log_payload_policy()).into(),
	});
	assert!(
		t.pi
			.stores
			.read_binds()
			.listener_frontend_policies(&listener_name, None, None)
			.access_log
			.is_some()
	);
	(t, io)
}

#[tokio::test]
async fn tool_call_exposes_payload_fields_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;
	let trace_id = format!("mcp-e2e-{}", uuid::Uuid::new_v4());
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({
					"traceId": trace_id,
					"hi": "world",
				})
				.as_object()
				.cloned()
				.expect("tool arguments should serialize to an object"),
			),
		)
		.await
		.unwrap();
	let direct_result_text = &result.content[0].raw.as_text().unwrap().text;
	let direct_result_json: serde_json::Value =
		serde_json::from_str(direct_result_text).expect("tool result should be valid JSON text");
	assert_eq!(direct_result_json["traceId"], trace_id);
	assert_eq!(direct_result_json["hi"], "world");

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_trace", &trace_id),
	])
	.await
	.unwrap();

	assert_eq!(
		log.get("mcp_method_cel"),
		Some(&serde_json::json!("tools/call"))
	);
	assert_eq!(
		log.get("mcp_tool_name_cel"),
		Some(&serde_json::json!("echo"))
	);
	assert_eq!(
		log.get("mcp_tool_target_cel"),
		Some(&serde_json::json!("mcp"))
	);
	assert_eq!(log["mcp_args_cel"]["traceId"], trace_id);
	assert_eq!(log["mcp_args_cel"]["hi"], "world");
	assert!(
		log["mcp_session_cel"]
			.as_str()
			.is_some_and(|session_id| !session_id.is_empty())
	);
	assert_eq!(log["mcp_result_cel"]["isError"], false);

	let result_text = log["mcp_result_cel"]["content"][0]["text"]
		.as_str()
		.expect("tool result text should be logged");
	let result_json: serde_json::Value =
		serde_json::from_str(result_text).expect("tool result should be valid JSON text");
	assert_eq!(result_json["traceId"], trace_id);
	assert_eq!(result_json["hi"], "world");
	assert!(log.get("mcp_error_cel").is_none());

	assert_eq!(
		log.get("gen_ai.tool.name"),
		Some(&serde_json::json!("echo"))
	);
	assert!(log.get("gen_ai.tool.call.arguments").is_none());
	assert!(log.get("gen_ai.tool.call.result").is_none());

	#[cfg(feature = "adobe")]
	{
		assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(true)));
		assert!(log.get("mcp_error_type_cel").is_none());
		assert!(log.get("mcp_error_msg_cel").is_none());
		assert!(log.get("mcp_error_code_cel").is_none());
	}
}

#[tokio::test]
async fn tool_call_error_exposes_error_payload_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;
	let trace_id = format!("mcp-e2e-error-{}", uuid::Uuid::new_v4());
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("does_not_exist").with_arguments(
				serde_json::json!({
					"traceId": trace_id,
				})
				.as_object()
				.cloned()
				.expect("tool arguments should serialize to an object"),
			),
		)
		.await
		.unwrap_err();
	match &err {
		rmcp::ServiceError::McpError(mcp_error) => assert_eq!(mcp_error.code.0, -32602),
		other => panic!("Expected ServiceError::McpError, got: {other:?}"),
	}

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_trace", &trace_id),
	])
	.await
	.unwrap();

	assert_eq!(
		log.get("mcp_method_cel"),
		Some(&serde_json::json!("tools/call"))
	);
	assert_eq!(
		log.get("mcp_tool_name_cel"),
		Some(&serde_json::json!("does_not_exist"))
	);
	assert_eq!(log["mcp_args_cel"]["traceId"], trace_id);
	assert_eq!(log["mcp_error_cel"]["code"], -32602);
	assert!(
		log["mcp_error_cel"]["message"]
			.as_str()
			.is_some_and(|message| message.contains("tool"))
	);
	assert!(log.get("mcp_result_cel").is_none());
	assert_eq!(
		log.get("gen_ai.tool.name"),
		Some(&serde_json::json!("does_not_exist"))
	);
	assert!(log.get("gen_ai.tool.call.arguments").is_none());
	assert!(log.get("gen_ai.tool.call.result").is_none());

	#[cfg(feature = "adobe")]
	{
		assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(false)));
		assert_eq!(
			log.get("mcp_error_type_cel"),
			Some(&serde_json::json!("upstream_tool_error"))
		);
		assert!(
			log["mcp_error_msg_cel"]
				.as_str()
				.is_some_and(|m| m.contains("tool")),
			"mcp_error_msg_cel should mention 'tool', got: {:?}",
			log.get("mcp_error_msg_cel")
		);
		assert_eq!(log.get("mcp_error_code_cel"), Some(&serde_json::json!(-32602)));
	}
}

#[tokio::test]
async fn legacy_sse_tool_call_exposes_arguments_without_terminal_payloads() {
	let mock = mock_streamable_http_server(true).await;
	let trace_id = format!("mcp-e2e-sse-{}", uuid::Uuid::new_v4());
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_sse_client(io).await;

	let result = client
		.call_tool(legacy_rmcp::model::CallToolRequestParam {
			name: "echo".into(),
			arguments: serde_json::json!({
				"traceId": trace_id,
				"hi": "world",
			})
			.as_object()
			.cloned(),
		})
		.await
		.unwrap();
	let direct_result_text = &result.content[0].raw.as_text().unwrap().text;
	let direct_result_json: serde_json::Value =
		serde_json::from_str(direct_result_text).expect("tool result should be valid JSON text");
	assert_eq!(direct_result_json["traceId"], trace_id);
	assert_eq!(direct_result_json["hi"], "world");

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_trace", &trace_id),
	])
	.await
	.unwrap();

	assert_eq!(
		log.get("mcp_method_cel"),
		Some(&serde_json::json!("tools/call"))
	);
	assert_eq!(
		log.get("mcp_tool_name_cel"),
		Some(&serde_json::json!("echo"))
	);
	assert_eq!(log["mcp_args_cel"]["traceId"], trace_id);
	assert_eq!(log["mcp_args_cel"]["hi"], "world");
	assert!(log.get("mcp_result_cel").is_none());
	assert!(log.get("mcp_error_cel").is_none());

	assert_eq!(
		log.get("gen_ai.tool.name"),
		Some(&serde_json::json!("echo"))
	);
	assert!(log.get("gen_ai.tool.call.arguments").is_none());
	assert!(log.get("gen_ai.tool.call.result").is_none());
}

/// Verify that RBAC-denied tool calls stamp `mcp.success = false` and
/// `mcp.error.type = "permission_denied"` in the access-log CEL context.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn rbac_denial_exposes_permission_denied_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;
	let trace_id = format!("mcp-rbac-{}", uuid::Uuid::new_v4());

	let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(crate::cel::Expression::new_strict("true").unwrap())],
		vec![],
	)));

	let (mut t, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
	)
	.await;

	// Attach the access-log policy so CEL fields are emitted.
	let listener_name = t
		.pi
		.stores
		.read_binds()
		.bind(&BIND_KEY)
		.unwrap()
		.listeners
		.iter()
		.next()
		.unwrap()
		.name
		.clone();
	t.with_policy(TargetedPolicy {
		key: "frontend/accessLog".into(),
		name: None,
		target: PolicyTarget::Gateway(listener_name.into()),
		inheritance: crate::types::agent::PolicyInheritance::Default,
		policy: FrontendPolicy::AccessLog(access_log_payload_policy()).into(),
	});

	let client = mcp_streamable_client(io).await;

	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({ "traceId": trace_id })
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await;

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_trace", &trace_id),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(false)));
	assert_eq!(
		log.get("mcp_error_type_cel"),
		Some(&serde_json::json!("permission_denied"))
	);
	assert!(
		log["mcp_error_msg_cel"]
			.as_str()
			.is_some_and(|m| m.contains("echo")),
		"mcp_error_msg_cel should mention the tool name 'echo', got: {:?}",
		log.get("mcp_error_msg_cel")
	);
	assert!(log.get("mcp_error_code_cel").is_none());
}

/// Verify RBAC-denied resource reads stamp `mcp.success = false` and
/// `mcp.error.type = "permission_denied"` in the access-log CEL context.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn rbac_denial_resource_exposes_permission_denied_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;

	let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(crate::cel::Expression::new_strict("true").unwrap())],
		vec![],
	)));

	let (mut t, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
	)
	.await;

	let listener_name = t
		.pi
		.stores
		.read_binds()
		.bind(&BIND_KEY)
		.unwrap()
		.listeners
		.iter()
		.next()
		.unwrap()
		.name
		.clone();
	t.with_policy(TargetedPolicy {
		key: "frontend/accessLog".into(),
		name: None,
		target: PolicyTarget::Gateway(listener_name.into()),
		inheritance: crate::types::agent::PolicyInheritance::Default,
		policy: FrontendPolicy::AccessLog(access_log_payload_policy()).into(),
	});

	let client = mcp_streamable_client(io).await;
	let resource_uri = format!("str:////rbac-resource-{}", uuid::Uuid::new_v4());

	let _ = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(&resource_uri))
		.await;

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_resource_name_cel", &resource_uri),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(false)));
	assert_eq!(
		log.get("mcp_error_type_cel"),
		Some(&serde_json::json!("permission_denied"))
	);
	assert!(
		log["mcp_error_msg_cel"]
			.as_str()
			.is_some_and(|m| m.contains(&resource_uri)),
		"mcp_error_msg_cel should mention the resource URI, got: {:?}",
		log.get("mcp_error_msg_cel")
	);
	assert!(log.get("mcp_error_code_cel").is_none());
}

/// Verify RBAC-denied prompt gets stamp `mcp.success = false` and
/// `mcp.error.type = "permission_denied"` in the access-log CEL context.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn rbac_denial_prompt_exposes_permission_denied_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;

	let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(crate::cel::Expression::new_strict("true").unwrap())],
		vec![],
	)));

	let (mut t, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
	)
	.await;

	let listener_name = t
		.pi
		.stores
		.read_binds()
		.bind(&BIND_KEY)
		.unwrap()
		.listeners
		.iter()
		.next()
		.unwrap()
		.name
		.clone();
	t.with_policy(TargetedPolicy {
		key: "frontend/accessLog".into(),
		name: None,
		target: PolicyTarget::Gateway(listener_name.into()),
		inheritance: crate::types::agent::PolicyInheritance::Default,
		policy: FrontendPolicy::AccessLog(access_log_payload_policy()).into(),
	});

	let client = mcp_streamable_client(io).await;
	let prompt_name = format!("rbac-prompt-{}", uuid::Uuid::new_v4());

	let _ = client
		.get_prompt(rmcp::model::GetPromptRequestParams::new(&prompt_name))
		.await;

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_prompt_name_cel", &prompt_name),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(false)));
	assert_eq!(
		log.get("mcp_error_type_cel"),
		Some(&serde_json::json!("permission_denied"))
	);
	assert!(
		log["mcp_error_msg_cel"]
			.as_str()
			.is_some_and(|m| m.contains(&prompt_name)),
		"mcp_error_msg_cel should mention the prompt name, got: {:?}",
		log.get("mcp_error_msg_cel")
	);
	assert!(log.get("mcp_error_code_cel").is_none());
}

/// Verify that a resources/read JSON-RPC error stamps `mcp.success = false` and
/// `mcp.error.type = "upstream_tool_error"` with the JSON-RPC error code.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn resource_read_error_exposes_error_payload_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;
	let unknown_uri = format!("unknown://resource-{}", uuid::Uuid::new_v4());
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let _ = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(&unknown_uri))
		.await;

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_resource_name_cel", &unknown_uri),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(false)));
	assert_eq!(
		log.get("mcp_error_type_cel"),
		Some(&serde_json::json!("upstream_tool_error"))
	);
	assert!(
		log["mcp_error_msg_cel"]
			.as_str()
			.is_some_and(|m| !m.is_empty()),
		"mcp_error_msg_cel should be non-empty, got: {:?}",
		log.get("mcp_error_msg_cel")
	);
	// JSON-RPC error code -32002 (RESOURCE_NOT_FOUND)
	assert_eq!(
		log.get("mcp_error_code_cel"),
		Some(&serde_json::json!(-32002))
	);
}

/// Verify that a prompts/get JSON-RPC error stamps `mcp.success = false` and
/// `mcp.error.type = "upstream_tool_error"` with the JSON-RPC error code.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn prompt_get_error_exposes_error_payload_to_access_log_cel() {
	let mock = mock_streamable_http_server(true).await;
	let bad_prompt = format!("nonexistent-prompt-{}", uuid::Uuid::new_v4());
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let _ = client
		.get_prompt(rmcp::model::GetPromptRequestParams::new(&bad_prompt))
		.await;

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_prompt_name_cel", &bad_prompt),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(false)));
	assert_eq!(
		log.get("mcp_error_type_cel"),
		Some(&serde_json::json!("upstream_tool_error"))
	);
	assert!(
		log["mcp_error_msg_cel"]
			.as_str()
			.is_some_and(|m| !m.is_empty()),
		"mcp_error_msg_cel should be non-empty, got: {:?}",
		log.get("mcp_error_msg_cel")
	);
	// JSON-RPC error code should be present (method_not_found or similar)
	assert!(
		log.get("mcp_error_code_cel").is_some(),
		"mcp_error_code_cel should be populated for JSON-RPC errors"
	);
}

/// Verify that a successful resources/read stamps `mcp.success = true` with no error fields.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn resource_read_success_stamps_mcp_success_true() {
	let mock = mock_streamable_http_server(true).await;
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let result = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(
			"str:////Users/to/some/path/",
		))
		.await
		.unwrap();
	assert!(!result.contents.is_empty());

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_resource_name_cel", "str:////Users/to/some/path/"),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(true)));
	assert!(log.get("mcp_error_type_cel").is_none());
	assert!(log.get("mcp_error_msg_cel").is_none());
	assert!(log.get("mcp_error_code_cel").is_none());
}

/// Verify that a successful prompts/get stamps `mcp.success = true` with no error fields.
#[cfg(feature = "adobe")]
#[tokio::test]
async fn prompt_get_success_stamps_mcp_success_true() {
	let mock = mock_streamable_http_server(true).await;
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let _ = client
		.get_prompt(
			rmcp::model::GetPromptRequestParams::new("example_prompt").with_arguments(
				serde_json::json!({ "message": "hello" })
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_prompt_name_cel", "example_prompt"),
	])
	.await
	.unwrap();

	assert_eq!(log.get("mcp_success_cel"), Some(&serde_json::json!(true)));
	assert!(log.get("mcp_error_type_cel").is_none());
	assert!(log.get("mcp_error_msg_cel").is_none());
	assert!(log.get("mcp_error_code_cel").is_none());
}

#[tokio::test]
async fn prompt_request_emits_gen_ai_prompt_name() {
	let mock = mock_streamable_http_server(true).await;
	let (_t, io) = setup_access_log_mcp_proxy(&mock).await;
	let client = mcp_streamable_client(io).await;

	let _result = client
		.get_prompt(
			rmcp::model::GetPromptRequestParams::new("example_prompt").with_arguments(
				serde_json::json!({ "message": "hello" })
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.unwrap();

	let log = agent_core::telemetry::testing::eventually_find(&[
		("scope", "request"),
		("mcp_prompt_name_cel", "example_prompt"),
	])
	.await
	.unwrap();

	assert_eq!(
		log.get("mcp_method_cel"),
		Some(&serde_json::json!("prompts/get"))
	);
	assert_eq!(
		log.get("gen_ai.prompt.name"),
		Some(&serde_json::json!("example_prompt"))
	);
	assert!(log.get("gen_ai.tool.name").is_none());
	assert!(log.get("mcp_tool_name_cel").is_none());
}

async fn setup_proxy(
	mock: &MockServer,
	stateful: bool,
	legacy_sse: bool,
) -> (TestBind, SocketAddr) {
	setup_proxy_policies(mock, stateful, legacy_sse, vec![]).await
}

async fn setup_proxy_policies(
	mock: &MockServer,
	stateful: bool,
	legacy_sse: bool,
	policies: Vec<BackendTrafficPolicy>,
) -> (TestBind, SocketAddr) {
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_mcp_backend_policies(mock.addr, stateful, legacy_sse, policies)
		.with_bind(simple_bind())
		.with_route(basic_route(mock.addr));
	let io = t.serve_real_listener(BIND_KEY).await;
	(t, io)
}

// Like `setup_proxy_policies`, but also attaches `target_policies` to the MCP target
// (used by guardrails tests that exercise per-target backend transformations).
async fn setup_proxy_policies_with_target(
	mock: &MockServer,
	stateful: bool,
	legacy_sse: bool,
	policies: Vec<BackendTrafficPolicy>,
	target_policies: Vec<BackendTrafficPolicy>,
) -> (TestBind, SocketAddr) {
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_mcp_backend_and_target_policies(
			mock.addr,
			stateful,
			legacy_sse,
			policies,
			target_policies,
		)
		.with_bind(simple_bind())
		.with_route(basic_route(mock.addr));
	let io = t.serve_real_listener(BIND_KEY).await;
	(t, io)
}

/// Shared capture of every upstream request's headers, for tests that assert a
/// guardrails mutation reached the upstream MCP server.
type HeaderCapture = std::sync::Arc<std::sync::Mutex<Vec<http::HeaderMap>>>;

/// Like `mock_streamable_http_server`, but records the headers of every inbound
/// request into the returned [`HeaderCapture`] via an axum middleware layer.
async fn mock_streamable_http_server_with_capture(stateful: bool) -> (MockServer, HeaderCapture) {
	use mockserver::Counter;
	use rmcp::transport::streamable_http_server::StreamableHttpService;
	use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
	agent_core::telemetry::testing::setup_test_logging();
	let init_counter = std::sync::Arc::new(tokio::sync::Mutex::new(0_i32));
	let capture: HeaderCapture = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));

	let service = StreamableHttpService::new(
		{
			let init_counter = init_counter.clone();
			move || Ok(Counter::new(init_counter.clone()))
		},
		LocalSessionManager::default().into(),
		StreamableHttpServerConfig::default()
			.with_sse_retry(None)
			.with_sse_keep_alive(None)
			.with_stateful_mode(stateful)
			.with_json_response(false),
	);

	let (tx, rx) = tokio::sync::oneshot::channel();
	let cap = capture.clone();
	let router = axum::Router::new().nest_service("/mcp", service).layer(
		axum::middleware::from_fn(
			move |req: axum::extract::Request, next: axum::middleware::Next| {
				let cap = cap.clone();
				async move {
					cap.lock().unwrap().push(req.headers().clone());
					next.run(req).await
				}
			},
		),
	);
	let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = tcp_listener.local_addr().unwrap();
	tokio::spawn(async move {
		let _ = axum::serve(tcp_listener, router)
			.with_graceful_shutdown(async { rx.await.unwrap() })
			.await;
		info!("server stopped");
	});
	(
		MockServer {
			addr,
			init_counter,
			_cancel: tx,
		},
		capture,
	)
}

pub async fn mcp_streamable_client(
	s: SocketAddr,
) -> RunningService<RoleClient, InitializeRequestParams> {
	use rmcp::ServiceExt;
	use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
	use rmcp::transport::StreamableHttpClientTransport;
	let transport =
		StreamableHttpClientTransport::<reqwest::Client>::from_uri(format!("http://{s}/mcp"));
	let client_info = ClientInfo::new(
		ClientCapabilities::default(),
		Implementation::new("test client".to_string(), "0.0.1".to_string()),
	);

	client_info
		.serve(transport)
		.await
		.inspect_err(|e| {
			tracing::error!("client error: {:?}", e);
		})
		.unwrap()
}

type LegacyService = legacy_rmcp::service::RunningService<
	legacy_rmcp::RoleClient,
	legacy_rmcp::model::InitializeRequestParam,
>;

pub async fn mcp_sse_client(s: SocketAddr) -> LegacyService {
	use legacy_rmcp::ServiceExt;
	use legacy_rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
	use legacy_rmcp::transport::SseClientTransport;
	let transport = SseClientTransport::<legacyreqwest::Client>::start(format!("http://{s}/sse"))
		.await
		.unwrap();
	let client_info = ClientInfo {
		protocol_version: Default::default(),
		capabilities: ClientCapabilities::default(),
		client_info: Implementation {
			name: "test client".to_string(),
			version: "0.0.1".to_string(),
			title: None,
			website_url: None,
			icons: None,
		},
	};

	client_info.serve(transport).await.unwrap()
}

struct MockServer {
	addr: SocketAddr,
	init_counter: std::sync::Arc<tokio::sync::Mutex<i32>>,
	_cancel: tokio::sync::oneshot::Sender<()>,
}

impl MockServer {
	async fn init_count(&self) -> i32 {
		*self.init_counter.lock().await
	}
}

async fn mock_streamable_http_server(stateful: bool) -> MockServer {
	mock_streamable_http_server_with_delay(stateful, None).await
}

async fn mock_streamable_http_server_with_delay(
	stateful: bool,
	delay: Option<std::time::Duration>,
) -> MockServer {
	use mockserver::Counter;
	use rmcp::transport::streamable_http_server::StreamableHttpService;
	use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
	agent_core::telemetry::testing::setup_test_logging();
	let init_counter = std::sync::Arc::new(tokio::sync::Mutex::new(0_i32));

	let service = StreamableHttpService::new(
		{
			let init_counter = init_counter.clone();
			move || Ok(Counter::new(init_counter.clone()))
		},
		LocalSessionManager::default().into(),
		StreamableHttpServerConfig::default()
			.with_sse_retry(None)
			.with_sse_keep_alive(None)
			.with_stateful_mode(stateful)
			.with_json_response(false),
	);

	let (tx, rx) = tokio::sync::oneshot::channel();
	let mut router = axum::Router::new().nest_service("/mcp", service);
	if let Some(d) = delay {
		router = router.layer(axum::middleware::from_fn(
			move |req: axum::extract::Request, next: axum::middleware::Next| async move {
				tokio::time::sleep(d).await;
				next.run(req).await
			},
		));
	}
	let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = tcp_listener.local_addr().unwrap();
	tokio::spawn(async move {
		let _ = axum::serve(tcp_listener, router)
			.with_graceful_shutdown(async { rx.await.unwrap() })
			.await;
		info!("server stopped");
	});
	MockServer {
		addr,
		init_counter,
		_cancel: tx,
	}
}

/// A streamable-HTTP MCP mock that completes the `initialize` handshake (and any
/// `tools/list`) normally but returns a fixed HTTP error `status` for every
/// `tools/call` POST. Lets a directed `tools/call` reach the gateway's
/// single-target dispatch and fail at `generic_stream` with
/// `UpstreamError::Http(ClientError::Status(..))`.
#[cfg(feature = "adobe")]
async fn mock_streamable_http_server_failing_tools_call(status: ::http::StatusCode) -> MockServer {
	use mockserver::Counter;
	use rmcp::transport::streamable_http_server::StreamableHttpService;
	use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;

	agent_core::telemetry::testing::setup_test_logging();
	let init_counter = std::sync::Arc::new(tokio::sync::Mutex::new(0_i32));

	let service = StreamableHttpService::new(
		{
			let init_counter = init_counter.clone();
			move || Ok(Counter::new(init_counter.clone()))
		},
		LocalSessionManager::default().into(),
		StreamableHttpServerConfig::default()
			.with_sse_retry(None)
			.with_sse_keep_alive(None)
			.with_stateful_mode(true)
			.with_json_response(false),
	);

	let (tx, rx) = tokio::sync::oneshot::channel();
	let router = axum::Router::new()
		.nest_service("/mcp", service)
		.layer(axum::middleware::from_fn(
			move |req: axum::extract::Request, next: axum::middleware::Next| async move {
				let (parts, body) = req.into_parts();
				let bytes = axum::body::to_bytes(body, usize::MAX)
					.await
					.unwrap_or_default();
				let method = serde_json::from_slice::<serde_json::Value>(&bytes)
					.ok()
					.and_then(|v| v.get("method").and_then(|m| m.as_str()).map(str::to_string));
				let req = axum::extract::Request::from_parts(parts, axum::body::Body::from(bytes));
				if method.as_deref() == Some("tools/call") {
					::http::Response::builder()
						.status(status)
						.body(axum::body::Body::from("upstream error"))
						.unwrap()
				} else {
					next.run(req).await
				}
			},
		));

	let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = tcp_listener.local_addr().unwrap();
	tokio::spawn(async move {
		let _ = axum::serve(tcp_listener, router)
			.with_graceful_shutdown(async { rx.await.unwrap() })
			.await;
		info!("failing-tools-call server stopped");
	});
	MockServer {
		addr,
		init_counter,
		_cancel: tx,
	}
}

/// Mock that responds to every HTTP request (including the MCP `initialize`
/// POST) with the given status code.  Used to test FailClosed fanout
/// status-code passthrough without involving a real MCP server.
#[cfg(feature = "adobe")]
async fn mock_streamable_http_server_failing_initialize(status: ::http::StatusCode) -> MockServer {
	agent_core::telemetry::testing::setup_test_logging();
	let init_counter = std::sync::Arc::new(tokio::sync::Mutex::new(0_i32));
	let (tx, rx) = tokio::sync::oneshot::channel();
	let router = axum::Router::new().fallback(axum::routing::any(move || async move {
		::http::Response::builder()
			.status(status)
			.body(axum::body::Body::from(format!("upstream error {status}")))
			.unwrap()
	}));
	let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = tcp_listener.local_addr().unwrap();
	tokio::spawn(async move {
		let _ = axum::serve(tcp_listener, router)
			.with_graceful_shutdown(async { rx.await.unwrap() })
			.await;
		info!("failing-initialize server stopped");
	});
	MockServer {
		addr,
		init_counter,
		_cancel: tx,
	}
}

async fn mock_sse_server() -> MockServer {
	use legacy_rmcp::transport::sse_server::{SseServer, SseServerConfig};
	use tokio_util::sync::CancellationToken;

	agent_core::telemetry::testing::setup_test_logging();
	let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let addr = tcp_listener.local_addr().unwrap();
	let ct = CancellationToken::new();
	let (sse_server, service) = SseServer::new(SseServerConfig {
		bind: addr,
		sse_path: "/sse".to_string(),
		post_path: "/message".to_string(),
		ct: ct.child_token(),
		sse_keep_alive: None,
	});

	let (tx, rx) = tokio::sync::oneshot::channel();
	let ct2 = sse_server.with_service_directly(legacymockserver::Counter::new);
	tokio::spawn(async move {
		let _ = axum::serve(tcp_listener, service)
			.with_graceful_shutdown(async move {
				rx.await.unwrap();
				ct.cancel();
				ct2.cancel();
				tracing::info!("sse server cancelled");
			})
			.await;
	});
	MockServer {
		addr,
		init_counter: std::sync::Arc::new(tokio::sync::Mutex::new(0)),
		_cancel: tx,
	}
}
mod mockserver {
	use std::sync::Arc;

	use http::request::Parts;
	use rmcp::handler::server::wrapper::Parameters;
	use rmcp::model::*;
	use rmcp::service::RequestContext;
	use rmcp::{
		ErrorData as McpError, RoleServer, ServerHandler, prompt, prompt_handler, prompt_router,
		schemars, tool, tool_handler, tool_router,
	};
	use serde_json::json;
	use tokio::sync::Mutex;

	#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
	pub struct ExamplePromptArgs {
		/// A message to put in the prompt
		pub message: String,
	}

	#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
	pub struct CounterAnalysisArgs {
		/// The target value you're trying to reach
		pub goal: i32,
		/// Preferred strategy: 'fast' or 'careful'
		#[serde(skip_serializing_if = "Option::is_none")]
		pub strategy: Option<String>,
	}

	#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
	pub struct StructRequest {
		pub a: i32,
		pub b: i32,
	}

	#[derive(Clone)]
	pub struct Counter {
		counter: Arc<Mutex<i32>>,
		init_counter: Arc<Mutex<i32>>,
	}

	#[tool_router]
	impl Counter {
		pub fn new(init_counter: Arc<Mutex<i32>>) -> Self {
			Self {
				counter: Arc::new(Mutex::new(0)),
				init_counter,
			}
		}

		fn _create_resource_text(&self, uri: &str, name: &str) -> Resource {
			RawResource::new(uri, name.to_string()).no_annotation()
		}

		#[tool(description = "Increment the counter by 1")]
		async fn increment(&self) -> Result<CallToolResult, McpError> {
			let mut counter = self.counter.lock().await;
			*counter += 1;
			Ok(CallToolResult::success(vec![Content::text(
				counter.to_string(),
			)]))
		}

		#[tool(description = "Decrement the counter by 1")]
		async fn decrement(&self) -> Result<CallToolResult, McpError> {
			let mut counter = self.counter.lock().await;
			*counter -= 1;
			Ok(CallToolResult::success(vec![Content::text(
				counter.to_string(),
			)]))
		}

		#[tool(description = "Get the current counter value")]
		async fn get_value(&self) -> Result<CallToolResult, McpError> {
			let counter = self.counter.lock().await;
			Ok(CallToolResult::success(vec![Content::text(
				counter.to_string(),
			)]))
		}

		#[tool(description = "Say hello to the client")]
		fn say_hello(&self) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text("hello")]))
		}

		#[tool(description = "Repeat what you say")]
		fn echo(&self, Parameters(object): Parameters<JsonObject>) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text(
				serde_json::Value::Object(object).to_string(),
			)]))
		}

		#[tool(description = "Calculate the sum of two numbers")]
		fn sum(
			&self,
			Parameters(StructRequest { a, b }): Parameters<StructRequest>,
		) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text(
				(a + b).to_string(),
			)]))
		}

		#[tool(description = "Echo HTTP attributes")]
		fn echo_http(&self, rq: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
			let ext = rq.extensions.get::<Parts>();
			Ok(CallToolResult::success(vec![Content::text(
				ext
					.unwrap()
					.headers
					.get("authorization")
					.map(|s| String::from_utf8_lossy(s.as_bytes()))
					.unwrap_or_default(),
			)]))
		}

		#[tool(description = "Get initialize call count")]
		async fn get_init_count(&self) -> Result<CallToolResult, McpError> {
			let init_counter = self.init_counter.lock().await;
			Ok(CallToolResult::success(vec![Content::text(
				init_counter.to_string(),
			)]))
		}

		/// Returns a result whose `_meta.ui.resourceUri` points at an MCP App resource.
		/// Mirrors the MCP Apps spec pattern where tool calls dynamically reference a
		/// `ui://...` resource the host then loads via `resources/read`.
		#[tool(description = "Render an MCP App view")]
		fn render_app(&self) -> Result<CallToolResult, McpError> {
			let mut result = CallToolResult::success(vec![Content::text("rendered")]);
			let mut meta = Meta::new();
			meta.insert(
				"ui".into(),
				serde_json::json!({ "resourceUri": "ui://app/page.html" }),
			);
			// Mirrors `registerAppTool` dual-key normalization (modern + deprecated flat key).
			meta.insert(
				"ui/resourceUri".into(),
				serde_json::json!("ui://app/page.html"),
			);
			result.meta = Some(meta);
			Ok(result)
		}

		/// Returns an MCP App reference via `EmbeddedResource` content (A2UI sample wire form).
		#[tool(description = "Render an MCP App via embedded resource")]
		fn render_app_embedded(&self) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::resource(
				ResourceContents::TextResourceContents {
					uri: "ui://basic/app".to_string(),
					mime_type: Some("text/html;profile=mcp-app".to_string()),
					text: String::new(),
					meta: None,
				},
			)]))
		}

		#[tool(
			description = "Enqueue a task (federation routing tests)",
			execution(task_support = "optional")
		)]
		fn enqueue_task_tool(&self) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text("task queued")]))
		}
	}

	#[prompt_router]
	impl Counter {
		/// This is an example prompt that takes one required argument, message
		#[prompt(name = "example_prompt")]
		async fn example_prompt(
			&self,
			Parameters(args): Parameters<ExamplePromptArgs>,
			_ctx: RequestContext<RoleServer>,
		) -> Result<Vec<PromptMessage>, McpError> {
			let prompt = format!(
				"This is an example prompt with your message here: '{}'",
				args.message
			);
			Ok(vec![PromptMessage::new(
				PromptMessageRole::User,
				PromptMessageContent::text(prompt),
			)])
		}

		/// Analyze the current counter value and suggest next steps
		#[prompt(name = "counter_analysis")]
		async fn counter_analysis(
			&self,
			Parameters(args): Parameters<CounterAnalysisArgs>,
			_ctx: RequestContext<RoleServer>,
		) -> Result<GetPromptResult, McpError> {
			let strategy = args.strategy.unwrap_or_else(|| "careful".to_string());
			let current_value = *self.counter.lock().await;
			let difference = args.goal - current_value;

			let messages = vec![
				PromptMessage::new_text(
					PromptMessageRole::Assistant,
					"I'll analyze the counter situation and suggest the best approach.",
				),
				PromptMessage::new_text(
					PromptMessageRole::User,
					format!(
						"Current counter value: {}\nGoal value: {}\nDifference: {}\nStrategy preference: {}\n\nPlease analyze the situation and suggest the best approach to reach the goal.",
						current_value, args.goal, difference, strategy
					),
				),
			];

			Ok(GetPromptResult::new(messages).with_description(format!(
				"Counter analysis for reaching {} from {}",
				args.goal, current_value
			)))
		}
	}

	#[tool_handler]
	#[prompt_handler]
	impl ServerHandler for Counter {
		fn get_info(&self) -> ServerInfo {
			// `enable_tasks` plus subscribe/task handlers below are exercised only by `adobe_mcp_apps_integration` (needs `--features adobe`).
			ServerInfo::new(
				ServerCapabilities::builder()
					.enable_prompts()
					.enable_resources()
					.enable_resources_subscribe()
					.enable_tools()
					.enable_tasks()
					.build(),
			)
			.with_protocol_version(ProtocolVersion::V_2025_06_18)
			.with_instructions("This server provides counter tools and prompts.")
		}

		async fn list_resources(
			&self,
			_request: Option<PaginatedRequestParams>,
			_: RequestContext<RoleServer>,
		) -> Result<ListResourcesResult, McpError> {
			Ok(ListResourcesResult {
				resources: vec![
					self._create_resource_text("str:////Users/to/some/path/", "cwd"),
					self._create_resource_text("memo://insights", "memo-name"),
				],
				next_cursor: None,
				meta: None,
			})
		}

		async fn read_resource(
			&self,
			ReadResourceRequestParams { uri, .. }: ReadResourceRequestParams,
			_: RequestContext<RoleServer>,
		) -> Result<ReadResourceResult, McpError> {
			match uri.as_str() {
				"str:////Users/to/some/path/" => {
					let cwd = "/Users/to/some/path/";
					Ok(ReadResourceResult::new(vec![ResourceContents::text(
						cwd, uri,
					)]))
				},
				"memo://insights" => {
					let memo = "Business Intelligence Memo\n\nAnalysis has revealed 5 key insights ...";
					Ok(ReadResourceResult::new(vec![ResourceContents::text(
						memo, uri,
					)]))
				},
				_ => Err(McpError::resource_not_found(
					"resource_not_found",
					Some(json!({
							"uri": uri
					})),
				)),
			}
		}

		async fn subscribe(
			&self,
			SubscribeRequestParams { uri, .. }: SubscribeRequestParams,
			_: RequestContext<RoleServer>,
		) -> Result<(), McpError> {
			match uri.as_str() {
				"memo://insights" | "str:////Users/to/some/path/" => Ok(()),
				other => Err(McpError::invalid_params(
					format!("subscribe expected unwrapped upstream uri, got {other}"),
					None,
				)),
			}
		}

		async fn unsubscribe(
			&self,
			UnsubscribeRequestParams { uri, .. }: UnsubscribeRequestParams,
			_: RequestContext<RoleServer>,
		) -> Result<(), McpError> {
			match uri.as_str() {
				"memo://insights" | "str:////Users/to/some/path/" => Ok(()),
				other => Err(McpError::invalid_params(
					format!("unsubscribe expected unwrapped upstream uri, got {other}"),
					None,
				)),
			}
		}

		async fn list_tasks(
			&self,
			_request: Option<PaginatedRequestParams>,
			_: RequestContext<RoleServer>,
		) -> Result<ListTasksResult, McpError> {
			let ts = "2020-01-01T00:00:00Z";
			Ok(ListTasksResult::new(vec![Task::new(
				"t1".into(),
				TaskStatus::Working,
				ts.into(),
				ts.into(),
			)]))
		}

		async fn enqueue_task(
			&self,
			_request: CallToolRequestParams,
			_: RequestContext<RoleServer>,
		) -> Result<CreateTaskResult, McpError> {
			let ts = "2020-01-01T00:00:00Z";
			Ok(CreateTaskResult::new(Task::new(
				"job-99".into(),
				TaskStatus::Working,
				ts.into(),
				ts.into(),
			)))
		}

		async fn get_task_info(
			&self,
			GetTaskInfoParams { task_id, .. }: GetTaskInfoParams,
			_: RequestContext<RoleServer>,
		) -> Result<GetTaskResult, McpError> {
			if task_id != "t1" && task_id != "job-99" {
				return Err(McpError::invalid_params(
					format!("expected upstream task id t1 or job-99, got {task_id}"),
					None,
				));
			}
			let ts = "2020-01-01T00:00:00Z";
			Ok(GetTaskResult {
				meta: None,
				task: Task::new(task_id, TaskStatus::Working, ts.into(), ts.into()),
			})
		}

		async fn get_task_result(
			&self,
			GetTaskResultParams { task_id, .. }: GetTaskResultParams,
			_: RequestContext<RoleServer>,
		) -> Result<GetTaskPayloadResult, McpError> {
			if task_id != "t1" {
				return Err(McpError::invalid_params(
					format!("expected upstream task id t1, got {task_id}"),
					None,
				));
			}
			Ok(GetTaskPayloadResult::new(json!({"done": true})))
		}

		async fn cancel_task(
			&self,
			CancelTaskParams { task_id, .. }: CancelTaskParams,
			_: RequestContext<RoleServer>,
		) -> Result<CancelTaskResult, McpError> {
			if task_id != "t1" {
				return Err(McpError::invalid_params(
					format!("expected upstream task id t1, got {task_id}"),
					None,
				));
			}
			let ts = "2020-01-01T00:00:00Z";
			Ok(CancelTaskResult {
				meta: None,
				task: Task::new("t1".into(), TaskStatus::Cancelled, ts.into(), ts.into()),
			})
		}

		async fn list_resource_templates(
			&self,
			_request: Option<PaginatedRequestParams>,
			_: RequestContext<RoleServer>,
		) -> Result<ListResourceTemplatesResult, McpError> {
			Ok(ListResourceTemplatesResult {
				next_cursor: None,
				resource_templates: Vec::new(),
				meta: None,
			})
		}

		async fn initialize(
			&self,
			_request: InitializeRequestParams,
			_: RequestContext<RoleServer>,
		) -> Result<InitializeResult, McpError> {
			let mut init_counter = self.init_counter.lock().await;
			*init_counter += 1;
			Ok(self.get_info())
		}
	}
}

mod legacymockserver {
	use std::sync::Arc;

	use http::request::Parts;
	use legacy_rmcp as rmcp;
	use rmcp::handler::server::router::prompt::PromptRouter;
	use rmcp::handler::server::router::tool::ToolRouter;
	use rmcp::handler::server::wrapper::Parameters;
	use rmcp::model::*;
	use rmcp::service::RequestContext;
	use rmcp::{
		ErrorData as McpError, RoleServer, ServerHandler, prompt, prompt_handler, prompt_router,
		schemars, tool, tool_handler, tool_router,
	};
	use serde_json::json;
	use tokio::sync::Mutex;

	#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
	pub struct ExamplePromptArgs {
		/// A message to put in the prompt
		pub message: String,
	}

	#[derive(Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
	pub struct CounterAnalysisArgs {
		/// The target value you're trying to reach
		pub goal: i32,
		/// Preferred strategy: 'fast' or 'careful'
		#[serde(skip_serializing_if = "Option::is_none")]
		pub strategy: Option<String>,
	}

	#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
	pub struct StructRequest {
		pub a: i32,
		pub b: i32,
	}

	#[derive(Clone)]
	pub struct Counter {
		counter: Arc<Mutex<i32>>,
		tool_router: ToolRouter<Counter>,
		prompt_router: PromptRouter<Counter>,
	}

	#[tool_router]
	impl Counter {
		#[allow(dead_code)]
		pub fn new() -> Self {
			Self {
				counter: Arc::new(Mutex::new(0)),
				tool_router: Self::tool_router(),
				prompt_router: Self::prompt_router(),
			}
		}

		fn _create_resource_text(&self, uri: &str, name: &str) -> Resource {
			RawResource::new(uri, name.to_string()).no_annotation()
		}

		#[tool(description = "Increment the counter by 1")]
		async fn increment(&self) -> Result<CallToolResult, McpError> {
			let mut counter = self.counter.lock().await;
			*counter += 1;
			Ok(CallToolResult::success(vec![Content::text(
				counter.to_string(),
			)]))
		}

		#[tool(description = "Decrement the counter by 1")]
		async fn decrement(&self) -> Result<CallToolResult, McpError> {
			let mut counter = self.counter.lock().await;
			*counter -= 1;
			Ok(CallToolResult::success(vec![Content::text(
				counter.to_string(),
			)]))
		}

		#[tool(description = "Get the current counter value")]
		async fn get_value(&self) -> Result<CallToolResult, McpError> {
			let counter = self.counter.lock().await;
			Ok(CallToolResult::success(vec![Content::text(
				counter.to_string(),
			)]))
		}

		#[tool(description = "Say hello to the client")]
		fn say_hello(&self) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text("hello")]))
		}

		#[tool(description = "Repeat what you say")]
		fn echo(&self, Parameters(object): Parameters<JsonObject>) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text(
				serde_json::Value::Object(object).to_string(),
			)]))
		}

		#[tool(description = "Calculate the sum of two numbers")]
		fn sum(
			&self,
			Parameters(StructRequest { a, b }): Parameters<StructRequest>,
		) -> Result<CallToolResult, McpError> {
			Ok(CallToolResult::success(vec![Content::text(
				(a + b).to_string(),
			)]))
		}

		#[tool(description = "Echo HTTP attributes")]
		fn echo_http(&self, rq: RequestContext<RoleServer>) -> Result<CallToolResult, McpError> {
			let ext = rq.extensions.get::<Parts>();
			Ok(CallToolResult::success(vec![Content::text(
				ext
					.unwrap()
					.headers
					.get("authorization")
					.map(|s| String::from_utf8_lossy(s.as_bytes()))
					.unwrap_or_default(),
			)]))
		}
	}

	#[prompt_router]
	impl Counter {
		/// This is an example prompt that takes one required argument, message
		#[prompt(name = "example_prompt")]
		async fn example_prompt(
			&self,
			Parameters(args): Parameters<ExamplePromptArgs>,
			_ctx: RequestContext<RoleServer>,
		) -> Result<Vec<PromptMessage>, McpError> {
			let prompt = format!(
				"This is an example prompt with your message here: '{}'",
				args.message
			);
			Ok(vec![PromptMessage {
				role: PromptMessageRole::User,
				content: PromptMessageContent::text(prompt),
			}])
		}

		/// Analyze the current counter value and suggest next steps
		#[prompt(name = "counter_analysis")]
		async fn counter_analysis(
			&self,
			Parameters(args): Parameters<CounterAnalysisArgs>,
			_ctx: RequestContext<RoleServer>,
		) -> Result<GetPromptResult, McpError> {
			let strategy = args.strategy.unwrap_or_else(|| "careful".to_string());
			let current_value = *self.counter.lock().await;
			let difference = args.goal - current_value;

			let messages = vec![
				PromptMessage::new_text(
					PromptMessageRole::Assistant,
					"I'll analyze the counter situation and suggest the best approach.",
				),
				PromptMessage::new_text(
					PromptMessageRole::User,
					format!(
						"Current counter value: {}\nGoal value: {}\nDifference: {}\nStrategy preference: {}\n\nPlease analyze the situation and suggest the best approach to reach the goal.",
						current_value, args.goal, difference, strategy
					),
				),
			];

			Ok(GetPromptResult {
				description: Some(format!(
					"Counter analysis for reaching {} from {}",
					args.goal, current_value
				)),
				messages,
			})
		}
	}

	#[tool_handler]
	#[prompt_handler]
	impl ServerHandler for Counter {
		fn get_info(&self) -> ServerInfo {
			ServerInfo {
				protocol_version: ProtocolVersion::V_2025_06_18,
				capabilities: ServerCapabilities::builder()
					.enable_prompts()
					.enable_resources()
					.enable_tools()
					.build(),
				server_info: Implementation::from_build_env(),
				instructions: Some("This server provides counter tools and prompts.".to_string()),
			}
		}

		async fn list_resources(
			&self,
			_request: Option<PaginatedRequestParam>,
			_: RequestContext<RoleServer>,
		) -> Result<ListResourcesResult, McpError> {
			Ok(ListResourcesResult {
				resources: vec![
					self._create_resource_text("str:////Users/to/some/path/", "cwd"),
					self._create_resource_text("memo://insights", "memo-name"),
				],
				next_cursor: None,
			})
		}

		async fn read_resource(
			&self,
			ReadResourceRequestParam { uri }: ReadResourceRequestParam,
			_: RequestContext<RoleServer>,
		) -> Result<ReadResourceResult, McpError> {
			match uri.as_str() {
				"str:////Users/to/some/path/" => {
					let cwd = "/Users/to/some/path/";
					Ok(ReadResourceResult {
						contents: vec![ResourceContents::text(cwd, uri)],
					})
				},
				"memo://insights" => {
					let memo = "Business Intelligence Memo\n\nAnalysis has revealed 5 key insights ...";
					Ok(ReadResourceResult {
						contents: vec![ResourceContents::text(memo, uri)],
					})
				},
				_ => Err(McpError::resource_not_found(
					"resource_not_found",
					Some(json!({
							"uri": uri
					})),
				)),
			}
		}

		async fn list_resource_templates(
			&self,
			_request: Option<PaginatedRequestParam>,
			_: RequestContext<RoleServer>,
		) -> Result<ListResourceTemplatesResult, McpError> {
			Ok(ListResourceTemplatesResult {
				next_cursor: None,
				resource_templates: Vec::new(),
			})
		}

		async fn initialize(
			&self,
			_request: InitializeRequestParam,
			_: RequestContext<RoleServer>,
		) -> Result<InitializeResult, McpError> {
			Ok(self.get_info())
		}
	}
}

#[tokio::test]
async fn test_zero_targets_fail_closed() {
	let backend = McpBackendGroup {
		targets: vec![],
		..Default::default()
	};
	let client = PolicyClient {
		inputs: setup_proxy_test("{}").unwrap().pi,
		outbound: None,
	};
	let err = crate::mcp::upstream::UpstreamGroup::new(client, backend).unwrap_err();
	assert!(matches!(err, crate::mcp::Error::NoBackends));
}

#[tokio::test]
async fn test_zero_targets_fail_open() {
	let backend = McpBackendGroup {
		targets: vec![],
		failure_mode: FailureMode::FailOpen,
		..Default::default()
	};
	let client = PolicyClient {
		inputs: setup_proxy_test("{}").unwrap().pi,
		outbound: None,
	};
	crate::mcp::upstream::UpstreamGroup::new(client, backend).unwrap();
}

#[tokio::test]
async fn test_setup_partial_success_fail_open() {
	// Test skipping failed stdio targets
	let backend = McpBackendGroup {
		targets: vec![
			Arc::new(McpTarget {
				name: "bad".into(),
				spec: crate::types::agent::McpTargetSpec::Stdio {
					cmd: "this-binary-does-not-exist-agentgateway-test".into(),
					args: vec![],
					env: Default::default(),
					clear_env: false,
				},
				backend_policies: Default::default(),
				backend: None,
				always_use_prefix: false,
			}),
			Arc::new(McpTarget {
				name: "ok".into(),
				spec: crate::types::agent::McpTargetSpec::Stdio {
					cmd: "cat".into(),
					args: vec![],
					env: Default::default(),
					clear_env: false,
				},
				backend_policies: Default::default(),
				backend: None,
				always_use_prefix: false,
			}),
		],
		stateful: false,
		failure_mode: FailureMode::FailOpen,
		..Default::default()
	};
	let client = PolicyClient {
		inputs: setup_proxy_test("{}").unwrap().pi,
		outbound: None,
	};
	let group = crate::mcp::upstream::UpstreamGroup::new(client, backend).unwrap();
	assert_eq!(group.size(), 1);
}

#[tokio::test]
async fn test_all_targets_fail_open_still_errors() {
	let backend = McpBackendGroup {
		targets: vec![
			Arc::new(McpTarget {
				name: "bad-1".into(),
				spec: crate::types::agent::McpTargetSpec::Stdio {
					cmd: "this-binary-does-not-exist-agentgateway-test-1".into(),
					args: vec![],
					env: Default::default(),
					clear_env: false,
				},
				backend_policies: Default::default(),
				backend: None,
				always_use_prefix: false,
			}),
			Arc::new(McpTarget {
				name: "bad-2".into(),
				spec: crate::types::agent::McpTargetSpec::Stdio {
					cmd: "this-binary-does-not-exist-agentgateway-test-2".into(),
					args: vec![],
					env: Default::default(),
					clear_env: false,
				},
				backend_policies: Default::default(),
				backend: None,
				always_use_prefix: false,
			}),
		],
		stateful: false,
		failure_mode: FailureMode::FailOpen,
		..Default::default()
	};
	let client = PolicyClient {
		inputs: setup_proxy_test("{}").unwrap().pi,
		outbound: None,
	};
	let err = crate::mcp::upstream::UpstreamGroup::new(client, backend).unwrap_err();
	assert!(matches!(err, crate::mcp::Error::NoBackends));
}

fn fake_streamable_target(name: &str, addr: SocketAddr) -> Arc<McpTarget> {
	Arc::new(McpTarget {
		name: name.into(),
		spec: crate::types::agent::McpTargetSpec::Mcp(crate::types::agent::StreamableHTTPTargetSpec {
			backend: crate::types::agent::SimpleBackendReference::Backend(strng::format!(
				"/unused-{name}"
			)),
			path: "/mcp".to_string(),
		}),
		backend_policies: Default::default(),
		backend: Some(crate::types::agent::SimpleBackend::Opaque(
			crate::types::agent::ResourceName::new(strng::format!("backend-{name}"), "".into()),
			crate::types::agent::Target::Address(addr),
		)),
		always_use_prefix: false,
	})
}

fn fake_sse_target(name: &str, addr: SocketAddr) -> Arc<McpTarget> {
	Arc::new(McpTarget {
		name: name.into(),
		spec: crate::types::agent::McpTargetSpec::Sse(crate::types::agent::SseTargetSpec {
			backend: crate::types::agent::SimpleBackendReference::Backend(strng::format!(
				"/unused-{name}"
			)),
			path: "/sse".to_string(),
		}),
		backend_policies: Default::default(),
		backend: Some(crate::types::agent::SimpleBackend::Opaque(
			crate::types::agent::ResourceName::new(strng::format!("backend-{name}"), "".into()),
			crate::types::agent::Target::Address(addr),
		)),
		always_use_prefix: false,
	})
}

fn fake_openapi_target(name: &str, addr: SocketAddr) -> Arc<McpTarget> {
	let schema: OpenAPI = serde_json::from_value(serde_json::json!({
		"openapi": "3.0.0",
		"info": {
			"title": "Test API",
			"version": "1.0.0"
		},
		"paths": {}
	}))
	.expect("valid OpenAPI schema");

	Arc::new(McpTarget {
		name: name.into(),
		spec: crate::types::agent::McpTargetSpec::OpenAPI(crate::types::agent::OpenAPITarget {
			backend: crate::types::agent::SimpleBackendReference::Backend(strng::format!(
				"/unused-{name}"
			)),
			schema: Arc::new(schema),
		}),
		backend_policies: Default::default(),
		backend: Some(crate::types::agent::SimpleBackend::Opaque(
			crate::types::agent::ResourceName::new(strng::format!("backend-{name}"), "".into()),
			crate::types::agent::Target::Address(addr),
		)),
		always_use_prefix: false,
	})
}

fn fake_stdio_target(name: &str) -> Arc<McpTarget> {
	Arc::new(McpTarget {
		name: name.into(),
		spec: crate::types::agent::McpTargetSpec::Stdio {
			cmd: "cat".into(),
			args: vec![],
			env: Default::default(),
			clear_env: false,
		},
		backend_policies: Default::default(),
		backend: None,
		always_use_prefix: false,
	})
}

fn empty_mcp_policies() -> crate::mcp::McpAuthorizationSet {
	crate::mcp::McpAuthorizationSet::new(crate::http::authorization::RuleSets::from(Vec::new()))
}

fn persisted_session(
	target_name: &str,
	session: &str,
	backend: SocketAddr,
) -> http::sessionpersistence::MCPSession {
	http::sessionpersistence::MCPSession {
		target_name: Some(target_name.to_string()),
		session: Some(session.to_string()),
		backend: Some(backend),
	}
}

fn persisted_stateless_session(
	target_name: &str,
	backend: SocketAddr,
) -> http::sessionpersistence::MCPSession {
	http::sessionpersistence::MCPSession {
		target_name: Some(target_name.to_string()),
		session: None,
		backend: Some(backend),
	}
}

#[test]
fn test_openapi_targets_emit_stateless_session_state() {
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_openapi_target(
				"openapi",
				SocketAddr::from(([127, 0, 0, 1], 30031)),
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let sessions = relay
		.get_sessions()
		.expect("OpenAPI should support stateless sessions");
	assert_eq!(
		sessions,
		vec![MCPSession {
			target_name: Some("openapi".to_string()),
			session: None,
			backend: None,
		}]
	);

	let pinned = SocketAddr::from(([127, 0, 0, 1], 31031));
	relay
		.set_sessions(vec![persisted_stateless_session("openapi", pinned)])
		.unwrap();

	let sessions = relay
		.get_sessions()
		.expect("OpenAPI session state should still be available");
	assert_eq!(
		sessions,
		vec![MCPSession {
			target_name: Some("openapi".to_string()),
			session: None,
			backend: Some(pinned),
		}]
	);
}

#[test]
fn test_sse_targets_emit_stateless_session_state() {
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_sse_target(
				"sse",
				SocketAddr::from(([127, 0, 0, 1], 30032)),
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let sessions = relay
		.get_sessions()
		.expect("SSE should support stateless sessions");
	assert_eq!(
		sessions,
		vec![MCPSession {
			target_name: Some("sse".to_string()),
			session: None,
			backend: None,
		}]
	);

	let pinned = SocketAddr::from(([127, 0, 0, 1], 31032));
	relay
		.set_sessions(vec![persisted_stateless_session("sse", pinned)])
		.unwrap();

	let sessions = relay
		.get_sessions()
		.expect("SSE session state should still be available");
	assert_eq!(
		sessions,
		vec![MCPSession {
			target_name: Some("sse".to_string()),
			session: None,
			backend: Some(pinned),
		}]
	);
}

#[tokio::test]
async fn test_stdio_targets_remain_non_stateless() {
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_stdio_target("stdio")],
			stateful: false,
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	assert!(relay.get_sessions().is_none());
}

#[tokio::test]
async fn test_fanout_deletion_fail_open_skips_failed_upstreams() {
	let good = mock_streamable_http_server(true).await;
	let bad_addr = SocketAddr::from(([127, 0, 0, 1], 31999));
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_streamable_target("good", good.addr),
				fake_streamable_target("bad", bad_addr),
			],
			stateful: true,
			failure_mode: FailureMode::FailOpen,
			session_idle_ttl: crate::mcp::DEFAULT_SESSION_IDLE_TTL,
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	relay
		.set_sessions(vec![
			persisted_session("good", "session-good", good.addr),
			persisted_session("bad", "session-bad", bad_addr),
		])
		.unwrap();

	let response = relay
		.send_fanout_deletion(crate::mcp::upstream::IncomingRequestContext::empty())
		.await
		.unwrap();

	assert_eq!(response.status(), http::StatusCode::ACCEPTED);
}

#[test]
fn test_set_sessions_matches_by_target_name() {
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_streamable_target("alpha", SocketAddr::from(([127, 0, 0, 1], 30001))),
				fake_streamable_target("beta", SocketAddr::from(([127, 0, 0, 1], 30002))),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	relay
		.set_sessions(vec![
			persisted_session(
				"beta",
				"session-beta",
				SocketAddr::from(([127, 0, 0, 1], 31002)),
			),
			persisted_session(
				"alpha",
				"session-alpha",
				SocketAddr::from(([127, 0, 0, 1], 31001)),
			),
		])
		.unwrap();

	let sessions = relay.get_sessions().unwrap();
	assert_eq!(sessions.len(), 2);
	assert_eq!(sessions[0].target_name.as_deref(), Some("alpha"));
	assert_eq!(sessions[0].session.as_deref(), Some("session-alpha"));
	assert_eq!(
		sessions[0].backend,
		Some(SocketAddr::from(([127, 0, 0, 1], 31001)))
	);
	assert_eq!(sessions[1].target_name.as_deref(), Some("beta"));
	assert_eq!(sessions[1].session.as_deref(), Some("session-beta"));
	assert_eq!(
		sessions[1].backend,
		Some(SocketAddr::from(([127, 0, 0, 1], 31002)))
	);
}

#[test]
fn test_set_sessions_rejects_mismatched_target_set() {
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_streamable_target("alpha", SocketAddr::from(([127, 0, 0, 1], 30011))),
				fake_streamable_target("beta", SocketAddr::from(([127, 0, 0, 1], 30012))),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let err = relay
		.set_sessions(vec![
			persisted_session(
				"beta",
				"session-beta",
				SocketAddr::from(([127, 0, 0, 1], 32012)),
			),
			persisted_session(
				"gamma",
				"session-gamma",
				SocketAddr::from(([127, 0, 0, 1], 32013)),
			),
		])
		.unwrap_err();

	assert!(
		err
			.to_string()
			.contains("missing persisted session for target alpha")
	);
}

#[test]
fn test_merge_initialize_merges_upstream_instructions_when_multiplexing() {
	use rmcp::model::{
		Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerResult,
	};

	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_streamable_target("alpha", SocketAddr::from(([127, 0, 0, 1], 30101))),
				fake_streamable_target("beta", SocketAddr::from(([127, 0, 0, 1], 30102))),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let merge_fn = relay.merge_initialize(ProtocolVersion::V_2025_06_18, true);

	let results: Vec<(Strng, ServerResult)> = vec![
		(
			"alpha".into(),
			ServerResult::InitializeResult(
				InitializeResult::new(ServerCapabilities::default())
					.with_protocol_version(ProtocolVersion::V_2025_06_18)
					.with_server_info(Implementation::new("alpha-server", "1.0"))
					.with_instructions("Alpha server: handles data processing."),
			),
		),
		(
			"beta".into(),
			ServerResult::InitializeResult(
				InitializeResult::new(ServerCapabilities::default())
					.with_protocol_version(ProtocolVersion::V_2025_06_18)
					.with_server_info(Implementation::new("beta-server", "1.0"))
					.with_instructions("Beta server: handles notifications."),
			),
		),
	];

	let cel = crate::mcp::rbac::CelExecWrapper::new(::http::Request::builder().uri("http://example.com/").body(()).unwrap());
	let result = merge_fn(results, &cel).unwrap();
	let info = match result {
		ServerResult::InitializeResult(ir) => ir,
		other => panic!("expected InitializeResult, got: {:?}", other),
	};

	let instructions = info.instructions.expect("instructions should be present");
	assert!(
		instructions.contains("Alpha server: handles data processing."),
		"merged instructions should contain alpha's instructions, got: {instructions}"
	);
	assert!(
		instructions.contains("Beta server: handles notifications."),
		"merged instructions should contain beta's instructions, got: {instructions}"
	);
	assert!(
		instructions.contains("[alpha]"),
		"merged instructions should label alpha's section, got: {instructions}"
	);
	assert!(
		instructions.contains("[beta]"),
		"merged instructions should label beta's section, got: {instructions}"
	);
	assert!(
		instructions.contains("gateway"),
		"merged instructions should contain gateway preamble, got: {instructions}"
	);
}

#[test]
fn test_merge_initialize_no_instructions_when_multiplexing() {
	use rmcp::model::{
		Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerResult,
	};

	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_streamable_target(
				"alpha",
				SocketAddr::from(([127, 0, 0, 1], 30103)),
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let merge_fn = relay.merge_initialize(ProtocolVersion::V_2025_06_18, true);

	let results: Vec<(Strng, ServerResult)> = vec![(
		"alpha".into(),
		ServerResult::InitializeResult(
			InitializeResult::new(ServerCapabilities::default())
				.with_protocol_version(ProtocolVersion::V_2025_06_18)
				.with_server_info(Implementation::new("alpha-server", "1.0")),
		),
	)];

	let cel = crate::mcp::rbac::CelExecWrapper::new(::http::Request::builder().uri("http://example.com/").body(()).unwrap());
	let result = merge_fn(results, &cel).unwrap();
	let info = match result {
		ServerResult::InitializeResult(ir) => ir,
		other => panic!("expected InitializeResult, got: {:?}", other),
	};

	let instructions = info.instructions.expect("instructions should be present");
	// When no upstream provides instructions, only the gateway preamble should be present
	assert!(
		instructions.contains("gateway"),
		"should contain gateway preamble, got: {instructions}"
	);
	assert!(
		!instructions.contains("[alpha]"),
		"should not contain server sections when no instructions provided, got: {instructions}"
	);
}

#[test]
fn test_merge_initialize_forwards_single_backend_without_multiplexing() {
	use rmcp::model::{
		Implementation, InitializeResult, ProtocolVersion, ServerCapabilities, ServerResult,
	};

	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_streamable_target(
				"solo",
				SocketAddr::from(([127, 0, 0, 1], 30104)),
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let merge_fn = relay.merge_initialize(ProtocolVersion::V_2025_06_18, false);

	let results: Vec<(Strng, ServerResult)> = vec![(
		"solo".into(),
		ServerResult::InitializeResult(
			InitializeResult::new(ServerCapabilities::default())
				.with_protocol_version(ProtocolVersion::V_2025_06_18)
				.with_server_info(Implementation::new("solo-server", "1.0"))
				.with_instructions("Solo server instructions."),
		),
	)];

	let cel = crate::mcp::rbac::CelExecWrapper::new(::http::Request::builder().uri("http://example.com/").body(()).unwrap());
	let result = merge_fn(results, &cel).unwrap();
	let info = match result {
		ServerResult::InitializeResult(ir) => ir,
		other => panic!("expected InitializeResult, got: {:?}", other),
	};

	// Non-multiplexing should forward the upstream's instructions directly
	assert_eq!(
		info.instructions.as_deref(),
		Some("Solo server instructions."),
		"non-multiplexing should forward upstream instructions unchanged"
	);
	assert_eq!(info.server_info.name, "solo-server");
}

#[tokio::test]
async fn test_runtime_fanout_fail_open() {
	use futures_util::StreamExt;
	use rmcp::model::{ListToolsResult, RequestId, ServerJsonRpcMessage};

	use crate::mcp::mergestream::{MergeStream, Messages};

	let ok_msg = ServerJsonRpcMessage::response(
		rmcp::model::ServerResult::ListToolsResult(ListToolsResult {
			tools: vec![],
			next_cursor: None,
			meta: None,
		}),
		RequestId::Number(1),
	);
	let ok_stream = Messages::from(ok_msg);
	let err_stream = Messages::from(Err(crate::mcp::ClientError::new(anyhow::anyhow!(
		"bad upstream"
	))));

	let streams = vec![("ok".into(), ok_stream), ("bad".into(), err_stream)];

	let merge = Box::new(|results: Vec<(Strng, rmcp::model::ServerResult)>, _cel: &crate::mcp::rbac::CelExecWrapper| {
		// Just return the first one for simplicity in this test
		Ok(results.into_iter().next().unwrap().1)
	});

	let mut ms = MergeStream::new(streams, RequestId::Number(1), merge, crate::mcp::rbac::CelExecWrapper::new(::http::Request::builder().uri("http://example.com/").body(()).unwrap()), FailureMode::FailOpen);

	let res = ms.next().await;
	assert!(res.is_some());
	let res = res.unwrap();
	assert!(
		res.is_ok(),
		"expected success with FailOpen even if one upstream errors: {:?}",
		res.err()
	);
}

#[tokio::test]
async fn test_runtime_fanout_fail_open_all_fail() {
	use futures_util::StreamExt;
	use rmcp::model::{ListToolsResult, RequestId};

	use crate::mcp::mergestream::{MergeStream, Messages};

	let err_stream1 = Messages::from(Err(crate::mcp::ClientError::new(anyhow::anyhow!("bad 1"))));
	let err_stream2 = Messages::from(Err(crate::mcp::ClientError::new(anyhow::anyhow!("bad 2"))));

	let streams = vec![("bad1".into(), err_stream1), ("bad2".into(), err_stream2)];

	let merge = Box::new(|results: Vec<(Strng, rmcp::model::ServerResult)>, _cel: &crate::mcp::rbac::CelExecWrapper| {
		// All failed, so results should be empty.
		// Return an empty success result (idiomatic for FailOpen).
		assert!(results.is_empty());
		Ok(rmcp::model::ServerResult::ListToolsResult(
			ListToolsResult {
				tools: vec![],
				next_cursor: None,
				meta: None,
			},
		))
	});

	let mut ms = MergeStream::new(streams, RequestId::Number(1), merge, crate::mcp::rbac::CelExecWrapper::new(::http::Request::builder().uri("http://example.com/").body(()).unwrap()), FailureMode::FailOpen);

	let res = ms.next().await;
	assert!(res.is_some());
	let res = res.unwrap();
	assert!(
		res.is_ok(),
		"expected success with FailOpen even if ALL upstreams error mid-request: {:?}",
		res.err()
	);
}

#[tokio::test]
async fn mcp_local_ratelimit() {
	let mock = mock_streamable_http_server(true).await;
	let mut t = setup_proxy_test("{}")
		.unwrap()
		.with_mcp_backend(mock.addr, true, false)
		.with_bind(simple_bind())
		.with_route(basic_route(mock.addr));

	// Attach local rate limit policy
	// MCP protocol overhead: initialize + notification + SSE GET = 3 requests
	// Allow 5 total: overhead (3) + tool calls (2), then rate limit the 6th
	t.attach_route_policy(serde_json::json!({
		"localRateLimit": [{
			"maxTokens": 5,
			"tokensPerFill": 1,
			"fillInterval": "10s",
			"type": "requests"
		}]
	}))
	.await;

	let io = t.serve_real_listener(BIND_KEY).await;
	let client = mcp_streamable_client(io).await;

	// First two calls should succeed
	let result1 = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo")
				.with_arguments(serde_json::json!({"n": 1}).as_object().cloned().unwrap()),
		)
		.await;
	assert!(result1.is_ok(), "First request should succeed");

	let result2 = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo")
				.with_arguments(serde_json::json!({"n": 2}).as_object().cloned().unwrap()),
		)
		.await;
	assert!(result2.is_ok(), "Second request should succeed");

	// Third call should be rate limited
	let result3 = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo")
				.with_arguments(serde_json::json!({"n": 3}).as_object().cloned().unwrap()),
		)
		.await;
	assert!(result3.is_err(), "Third request should be rate limited");
}

#[tokio::test]
async fn mcp_extauth_deny() {
	struct DenyAllAuthz;

	#[async_trait::async_trait]
	impl crate::test_helpers::extauthmock::Handler for DenyAllAuthz {
		async fn check(
			&mut self,
			_request: &crate::http::ext_authz::proto::CheckRequest,
		) -> Result<crate::http::ext_authz::proto::CheckResponse, tonic::Status> {
			deny_response(
				crate::http::ext_authz::proto::StatusCode::Forbidden,
				"denied by mock ext_authz",
			)
		}
	}

	let authz = ExtAuthMock::new(|| DenyAllAuthz).spawn().await;

	let mock = mock_streamable_http_server(true).await;
	let mut t = setup_proxy_test("{}")
		.unwrap()
		.with_mcp_backend(mock.addr, true, false)
		.with_bind(simple_bind())
		.with_route(basic_route(mock.addr));

	// Attach extAuthz policy pointing to our mock server
	t.attach_route_policy(serde_json::json!({
		"extAuthz": {
			"host": authz.address.to_string(),
			"protocol": {
				"grpc": {}
			}
		}
	}))
	.await;

	let io = t.serve_real_listener(BIND_KEY).await;

	// Client should fail to initialize due to ext_authz denial
	let result = try_mcp_streamable_client(io).await;
	let err = result.expect_err("Client initialization should be denied by ext_authz");
	let err_msg = err.to_string();
	assert!(
		err_msg.contains("403") && err_msg.contains("denied by mock ext_authz"),
		"Expected 403 denial from ext_authz, got: {err_msg}"
	);
}

async fn try_mcp_streamable_client(
	s: SocketAddr,
) -> Result<RunningService<RoleClient, InitializeRequestParams>, rmcp::service::ClientInitializeError>
{
	use rmcp::ServiceExt;
	use rmcp::model::{ClientCapabilities, ClientInfo, Implementation};
	use rmcp::transport::StreamableHttpClientTransport;
	let transport =
		StreamableHttpClientTransport::<reqwest::Client>::from_uri(format!("http://{s}/mcp"));
	let client_info = ClientInfo::new(
		ClientCapabilities::default(),
		Implementation::new("test client".to_string(), "0.0.1".to_string()),
	);

	client_info.serve(transport).await
}

#[tokio::test]
async fn mcp_remote_ratelimit_deny() {
	struct DenyAllRateLimit;

	#[async_trait::async_trait]
	impl crate::test_helpers::ratelimitmock::Handler for DenyAllRateLimit {
		async fn should_rate_limit(
			&mut self,
			_request: &crate::http::remoteratelimit::proto::RateLimitRequest,
		) -> Result<crate::http::remoteratelimit::proto::RateLimitResponse, tonic::Status> {
			over_limit_response(b"rate limit exceeded by mock".to_vec())
		}
	}

	let ratelimit = RateLimitMock::new(|| DenyAllRateLimit).spawn().await;

	let mock = mock_streamable_http_server(true).await;
	let mut t = setup_proxy_test("{}")
		.unwrap()
		.with_mcp_backend(mock.addr, true, false)
		.with_bind(simple_bind())
		.with_route(basic_route(mock.addr));

	// Attach remoteRateLimit policy pointing to our mock server
	t.attach_route_policy(serde_json::json!({
		"remoteRateLimit": {
			"host": ratelimit.address.to_string(),
			"domain": "test",
			"descriptors": [{
				"entries": [
					{"key": "generic_key", "value": "\"test\""}
				],
				"type": "requests"
			}]
		}
	}))
	.await;

	let io = t.serve_real_listener(BIND_KEY).await;

	// Client should fail to initialize due to rate limit denial
	let result = try_mcp_streamable_client(io).await;
	let err = result.expect_err("Client initialization should be rate limited");
	let err_msg = err.to_string();
	assert!(
		err_msg.contains("429") && err_msg.contains("rate limit exceeded by mock"),
		"Expected 429 rate limit from remote service, got: {err_msg}"
	);
}

// =========================== mcpGuardrails test helpers ============================

mod guardrails_test_support {
	use std::collections::HashMap;
	use std::net::SocketAddr;
	use std::sync::Arc;

	use rmcp::model::{CallToolResult, RawContent};

	use crate::mcp::guardrails;
	use crate::types::agent::{BackendTrafficPolicy, SimpleBackendReference, Target};

	// Default test allowlist: every method exercised in this module's tests,
	// all at Phase::Full. Tests that need narrower coverage build their own.
	pub fn default_methods() -> HashMap<String, guardrails::Phase> {
		["tools/call", "tools/list", "prompts/get", "resources/read"]
			.into_iter()
			.map(|m| (m.to_string(), guardrails::Phase::Full))
			.collect()
	}

	pub fn policy(addr: SocketAddr) -> BackendTrafficPolicy {
		policy_with(
			addr,
			guardrails::FailureMode::FailClosed,
			default_methods(),
			HashMap::new(),
		)
	}

	pub fn policy_with(
		addr: SocketAddr,
		failure_mode: guardrails::FailureMode,
		methods: HashMap<String, guardrails::Phase>,
		metadata: HashMap<String, Arc<crate::cel::Expression>>,
	) -> BackendTrafficPolicy {
		let remote = guardrails::Remote {
			target: Arc::new(SimpleBackendReference::InlineBackend(Target::Address(addr))),
			policies: Vec::new(),
			failure_mode,
			metadata,
			request_headers: Default::default(),
		};
		BackendTrafficPolicy::McpGuardrails(Arc::new(guardrails::McpGuardrails {
			processors: vec![guardrails::Processor {
				methods,
				kind: guardrails::ProcessorKind::Remote(remote),
			}],
		}))
	}

	#[cfg(feature = "adobe")]
	pub fn rate_limit_policy(
		addr: SocketAddr,
		rejection_overrides: Vec<guardrails::RateLimitRejectionOverride>,
	) -> BackendTrafficPolicy {
		rate_limit_policy_with_entry(addr, "tool", "mcp.tool.name", rejection_overrides)
	}

	#[cfg(feature = "adobe")]
	pub fn rate_limit_policy_with_entry(
		addr: SocketAddr,
		key: &str,
		value: &str,
		rejection_overrides: Vec<guardrails::RateLimitRejectionOverride>,
	) -> BackendTrafficPolicy {
		rate_limit_policy_with_entry_and_peek(addr, key, value, true, rejection_overrides)
	}

	#[cfg(feature = "adobe")]
	pub fn rate_limit_policy_with_entry_and_peek(
		addr: SocketAddr,
		key: &str,
		value: &str,
		peek: bool,
		rejection_overrides: Vec<guardrails::RateLimitRejectionOverride>,
	) -> BackendTrafficPolicy {
		let rate_limit = guardrails::RateLimit {
			domain: "mcp".to_string(),
			target: Arc::new(SimpleBackendReference::InlineBackend(Target::Address(addr))),
			policies: Vec::new(),
			failure_mode: guardrails::FailureMode::FailClosed,
			descriptors: Arc::new(guardrails::RateLimitDescriptorSet(vec![
				guardrails::RateLimitDescriptorEntry {
					entries: Arc::new(vec![guardrails::RateLimitDescriptor {
						key: key.to_string(),
						value: Arc::new(crate::cel::Expression::new_strict(value).unwrap()),
					}]),
					limit_type: crate::http::localratelimit::RateLimitType::Requests,
					limit_override: None,
					peek,
				},
			])),
			rejection_overrides,
		};
		BackendTrafficPolicy::McpGuardrails(Arc::new(guardrails::McpGuardrails {
			processors: vec![guardrails::Processor {
				methods: default_methods(),
				kind: guardrails::ProcessorKind::RateLimit(rate_limit),
			}],
		}))
	}

	#[cfg(feature = "adobe")]
	pub fn rejection_override(when: &str, code: i32, message: &str) -> guardrails::RateLimitRejectionOverride {
		guardrails::RateLimitRejectionOverride {
			when: Arc::new(crate::cel::Expression::new_strict(when).unwrap()),
			response_as: guardrails::RejectionResponseAs::JsonRpcError,
			status: Some(200),
			body: Some(guardrails::RateLimitRejectionOverrideBody {
				code: Some(code),
				message: Some(Arc::new(
					crate::cel::Expression::new_strict(message).unwrap(),
				)),
			}),
			headers: Vec::new(),
		}
	}

	pub fn echo_text(r: &CallToolResult) -> String {
		r.content
			.iter()
			.find_map(|c| match c.raw {
				RawContent::Text(ref t) => Some(t.text.clone()),
				_ => None,
			})
			.expect("echo returned text")
	}
}

// ============================== mcpGuardrails tests ===============================

#[tokio::test]
async fn mcp_guardrails_pass_through() {
	use std::sync::atomic::{AtomicUsize, Ordering};

	use crate::test_helpers::extmcpmock::{closure_mock, pass_request, pass_response};

	let (req_n, resp_n) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
	let extmcp_mock = {
		let (r, p) = (req_n.clone(), resp_n.clone());
		closure_mock(
			move |_| {
				r.fetch_add(1, Ordering::SeqCst);
				pass_request()
			},
			move |_| {
				p.fetch_add(1, Ordering::SeqCst);
				pass_response()
			},
		)
		.spawn()
		.await
	};

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed when mcpGuardrails returns Pass");

	assert!(!result.content.is_empty());
	assert!(req_n.load(Ordering::SeqCst) >= 1);
	assert!(resp_n.load(Ordering::SeqCst) >= 1);
}

#[tokio::test]
async fn mcp_guardrails_reject_surfaces_jsonrpc_error() {
	use protos::ext_mcp::authorization_error::Code;

	use crate::test_helpers::extmcpmock::{closure_mock, pass_response, reject_request};

	let extmcp_mock = closure_mock(
		|_| reject_request(Code::PermissionDenied, "denied by mock mcpGuardrails"),
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect_err("tool call should fail when mcpGuardrails rejects");

	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32001, "PermissionDenied should map to -32001");
	assert_eq!(e.message.as_ref(), "denied by mock mcpGuardrails");
}

#[tokio::test]
async fn mcp_guardrails_remote_rejection_ignores_overrides() {
	use protos::ext_mcp::authorization_error::Code;
	use protos::ext_mcp::{AuthorizationError, McpRequestResult, mcp_request_result};

	use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			Ok(McpRequestResult {
				result: Some(mcp_request_result::Result::Error(AuthorizationError {
					code: Code::ResourceExhausted as i32,
					reason: "quota exhausted".to_string(),
					mcp_error: Some(
						serde_json::to_vec(&serde_json::json!({
							"headers": { "Retry-After": "9" }
						}))
						.unwrap()
						.into(),
					),
				})),
				header_mutation: None,
				metadata: None,
			})
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let policy = guardrails_test_support::policy(extmcp_mock.address);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;

	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect_err("tool call should be denied by mcpGuardrails");
	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32003);
	assert_eq!(e.message.as_ref(), "quota exhausted");
}

#[tokio::test]
async fn mcp_guardrails_invalid_mcp_error_falls_back_to_default_error() {
	use protos::ext_mcp::authorization_error::Code;
	use protos::ext_mcp::{AuthorizationError, McpRequestResult, mcp_request_result};

	use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			Ok(McpRequestResult {
				result: Some(mcp_request_result::Result::Error(AuthorizationError {
					code: Code::ResourceExhausted as i32,
					reason: "invalid mcp_error payload".to_string(),
					mcp_error: Some(bytes::Bytes::from_static(b"{not-json")),
				})),
				header_mutation: None,
				metadata: None,
			})
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let policy = guardrails_test_support::policy(extmcp_mock.address);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;

	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect_err("tool call should be denied by mcpGuardrails");
	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32003, "ResourceExhausted should map to -32003");
	assert_eq!(e.message.as_ref(), "invalid mcp_error payload");
	assert!(e.data.is_none(), "invalid mcp_error JSON should be ignored");
}

#[cfg(feature = "adobe")]
#[tokio::test]
async fn mcp_guardrails_native_rate_limit_rejection_override_uses_limit_payload() {
	use protos::ext_mcp::authorization_error::Code;
	use protos::ext_mcp::{AuthorizationError, McpRequestResult, mcp_request_result};

	use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			Ok(McpRequestResult {
				result: Some(mcp_request_result::Result::Error(AuthorizationError {
					code: Code::ResourceExhausted as i32,
					reason: "rate limit exceeded".to_string(),
					mcp_error: Some(
						serde_json::to_vec(&serde_json::json!({
							"domain": "mcp_api",
							"overallCode": "OVER_LIMIT",
							"limit": {
								"descriptor": {
									"entries": [{ "key": "tool", "value": "echo" }]
								},
								"code": "OVER_LIMIT",
								"requestsPerUnit": 3,
								"unit": "MINUTE",
								"remaining": 0,
								"resetSeconds": 43,
								"retryAfterSeconds": 42
							}
						}))
						.unwrap()
						.into(),
					),
				})),
				header_mutation: None,
				metadata: None,
			})
		},
		|_| pass_response(),
	)
	.spawn()
	.await;
	let policy = guardrails_test_support::rate_limit_policy(
		extmcp_mock.address,
		vec![guardrails_test_support::rejection_override(
			"has(mcpGuardrails.rateLimit.limit)",
			-32001,
			r#""retry after " + string(mcpGuardrails.rateLimit.limit.retryAfterSeconds)"#,
		)],
	);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;

	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect_err("rateLimit should reject before upstream");
	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32001);
	assert_eq!(e.message.as_ref(), "retry after 42");
	assert!(e.data.is_none(), "AuthorizationError.mcp_error must not be exposed in error.data");
}

#[cfg(feature = "adobe")]
#[tokio::test]
async fn mcp_guardrails_native_rate_limit_peek_exposes_quota_on_pass() {
	use crate::http::transformation_cel::{
		LocalTransform, LocalTransformationConfig, Transformation,
	};
	use crate::test_helpers::extmcpmock::{closure_mock, pass_request_with, pass_response};

	let peek_quota_metadata: prost_wkt_types::Struct = serde_json::from_value(serde_json::json!({
		"rateLimit": {
			"domain": "mcp",
			"overallCode": "OK",
			"limit": {
				"descriptor": {
					"entries": [{ "key": "tool", "value": "echo" }]
				},
				"code": "OK",
				"remaining": 3,
				"requestsPerUnit": 5,
				"unit": "MINUTE",
				"resetSeconds": 42,
				"retryAfterSeconds": 0
			}
		}
	}))
	.unwrap();

	let extmcp_mock = closure_mock(
		move |_| {
			pass_request_with(
				Vec::<(&str, &str)>::new(),
				Vec::<&str>::new(),
				Some(peek_quota_metadata.clone()),
			)
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let xfm = Transformation::try_from_local_config(
		LocalTransformationConfig {
			request: Some(LocalTransform {
				set: vec![
					(
						strng::new("x-mcp-rl-overall"),
						strng::new(
							"has(mcpGuardrails.rateLimit.overallCode) ? mcpGuardrails.rateLimit.overallCode : \"\"",
						),
					),
					(
						strng::new("x-mcp-rl-remaining"),
						strng::new(
							"has(mcpGuardrails.rateLimit.limit) ? string(mcpGuardrails.rateLimit.limit.remaining) : \"\"",
						),
					),
				],
				..Default::default()
			}),
			response: None,
		},
		true,
	)
	.unwrap();
	let target_policy = BackendTrafficPolicy::Transformation(Arc::new(xfm));

	let policy = guardrails_test_support::rate_limit_policy_with_entry(
		extmcp_mock.address,
		"tool",
		"mcp.tool.name",
		Vec::new(),
	);

	let (mock, captured) = mock_streamable_http_server_with_capture(true).await;
	let (_bind, io) = setup_proxy_policies_with_target(
		&mock,
		true,
		false,
		vec![policy],
		vec![target_policy],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect("peek pass should allow the tool call");

	let headers = captured.lock().unwrap().clone();
	let saw_overall = headers.iter().any(|h| {
		h.get("x-mcp-rl-overall")
			.map(|v| v.as_bytes())
			== Some(b"OK")
	});
	let saw_remaining = headers.iter().any(|h| {
		h.get("x-mcp-rl-remaining")
			.map(|v| v.as_bytes())
			== Some(b"3")
	});
	assert!(
		saw_overall,
		"expected x-mcp-rl-overall:OK from mcpGuardrails.rateLimit.overallCode; saw {headers:?}"
	);
	assert!(
		saw_remaining,
		"expected x-mcp-rl-remaining:3 from mcpGuardrails.rateLimit.limit.remaining; saw {headers:?}"
	);
}

#[cfg(feature = "adobe")]
#[tokio::test]
async fn mcp_guardrails_native_rate_limit_increment_runs_after_success() {
	use tokio::sync::mpsc;
	use tokio::time::{Duration, timeout};

	let (tx, mut rx) = mpsc::unbounded_channel();
	let extmcp_mock = crate::test_helpers::extmcpmock::closure_mock(
		{
			let tx = tx.clone();
			move |req| {
				tx.send((
					"request",
					serde_json::to_value(req.metadata_context.as_ref()).unwrap(),
				))
				.unwrap();
				crate::test_helpers::extmcpmock::pass_request()
			}
		},
		move |resp| {
			tx.send((
				"response",
				serde_json::to_value(resp.metadata_context.as_ref()).unwrap(),
			))
			.unwrap();
			crate::test_helpers::extmcpmock::pass_response()
		},
	)
	.spawn()
	.await;
	let policy = guardrails_test_support::rate_limit_policy_with_entry(
		extmcp_mock.address,
		"method",
		"mcp.methodName",
		Vec::new(),
	);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;

	client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect("tool call should pass");

	let peek = timeout(Duration::from_secs(5), rx.recv())
		.await
		.expect("peek request should be sent")
		.expect("peek request present");
	let increment = timeout(Duration::from_secs(5), rx.recv())
		.await
		.expect("async increment request should be sent")
		.expect("increment request present");
	assert_eq!(peek.0, "request");
	assert_eq!(increment.0, "response");
	assert_eq!(peek.1["descriptors"][0]["hitsAddend"].as_f64(), Some(0.0));
	assert_eq!(
		increment.1["descriptors"][0]["hitsAddend"].as_f64(),
		Some(1.0)
	);
}

#[cfg(feature = "adobe")]
#[tokio::test]
async fn mcp_guardrails_native_rate_limit_non_peek_increments_on_request_only() {
	use tokio::sync::mpsc;
	use tokio::time::{Duration, timeout};

	let (tx, mut rx) = mpsc::unbounded_channel();
	let extmcp_mock = crate::test_helpers::extmcpmock::closure_mock(
		{
			let tx = tx.clone();
			move |req| {
				tx.send((
					"request",
					serde_json::to_value(req.metadata_context.as_ref()).unwrap(),
				))
				.unwrap();
				crate::test_helpers::extmcpmock::pass_request()
			}
		},
		move |resp| {
			tx.send((
				"response",
				serde_json::to_value(resp.metadata_context.as_ref()).unwrap(),
			))
			.unwrap();
			crate::test_helpers::extmcpmock::pass_response()
		},
	)
	.spawn()
	.await;
	let policy = guardrails_test_support::rate_limit_policy_with_entry_and_peek(
		extmcp_mock.address,
		"method",
		"mcp.methodName",
		false,
		Vec::new(),
	);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;

	client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect("tool call should pass");

	let increment = timeout(Duration::from_secs(5), rx.recv())
		.await
		.expect("request increment should be sent")
		.expect("request increment present");
	assert_eq!(increment.0, "request");
	assert_eq!(
		increment.1["descriptors"][0]["hitsAddend"].as_f64(),
		Some(1.0)
	);
	assert!(
		timeout(Duration::from_millis(200), rx.recv())
			.await
			.is_err(),
		"non-peek descriptors must not trigger a second ExtMCP call"
	);
}

#[cfg(feature = "adobe")]
#[tokio::test]
async fn mcp_guardrails_native_rate_limit_skips_increment_on_jsonrpc_error() {
	use tokio::sync::mpsc;
	use tokio::time::{Duration, timeout};

	let (tx, mut rx) = mpsc::unbounded_channel();
	let extmcp_mock = crate::test_helpers::extmcpmock::closure_mock(
		{
			let tx = tx.clone();
			move |req| {
				tx.send((
					"request",
					serde_json::to_value(req.metadata_context.as_ref()).unwrap(),
				))
				.unwrap();
				crate::test_helpers::extmcpmock::pass_request()
			}
		},
		move |resp| {
			tx.send((
				"response",
				serde_json::to_value(resp.metadata_context.as_ref()).unwrap(),
			))
			.unwrap();
			crate::test_helpers::extmcpmock::pass_response()
		},
	)
	.spawn()
	.await;
	let policy = guardrails_test_support::rate_limit_policy_with_entry(
		extmcp_mock.address,
		"method",
		"mcp.methodName",
		Vec::new(),
	);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;

	client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("sum").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect_err("upstream should return a JSON-RPC error for invalid params");

	let peek = timeout(Duration::from_secs(5), rx.recv())
		.await
		.expect("peek request should be sent")
		.expect("peek request present");
	assert_eq!(peek.0, "request");
	assert_eq!(peek.1["descriptors"][0]["hitsAddend"].as_f64(), Some(0.0));
	assert!(
		timeout(Duration::from_millis(200), rx.recv())
			.await
			.is_err(),
		"JSON-RPC errors must not trigger async increment"
	);
}

#[tokio::test]
async fn mcp_guardrails_denies_tool_by_name() {
	use protos::ext_mcp::authorization_error::Code;

	use crate::test_helpers::extmcpmock::{
		closure_mock, pass_request, pass_response, reject_request,
	};

	let extmcp_mock = closure_mock(
		|req| {
			let name = req
				.mcp_request
				.as_deref()
				.and_then(|b| serde_json::from_slice::<serde_json::Value>(b).ok())
				.and_then(|v| v.get("name").and_then(|n| n.as_str()).map(str::to_owned))
				.unwrap_or_default();
			if name.contains("forbidden") {
				reject_request(Code::PermissionDenied, format!("tool {name} is forbidden"))
			} else {
				pass_request()
			}
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;

	// Forbidden tool is rejected at the request phase, before reaching upstream.
	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("forbidden-tool")
				.with_arguments(serde_json::Map::new()),
		)
		.await
		.expect_err("forbidden tool call should be denied by mcpGuardrails");
	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32001, "PermissionDenied should map to -32001");
	assert!(
		e.message.contains("forbidden-tool"),
		"deny message should name the tool: {}",
		e.message
	);

	// An allowed tool passes the request phase through to the upstream.
	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("allowed tool call should pass through mcpGuardrails");
	assert!(!result.content.is_empty(), "echo should return content");
}

#[tokio::test]
async fn mcp_guardrails_mutated_request_reaches_upstream() {
	use crate::test_helpers::extmcpmock::{
		closure_mock, mutated_request_json, pass_request, pass_response,
	};

	let extmcp_mock = closure_mock(
		|req| {
			if req.method != "tools/call" {
				return pass_request();
			}
			mutated_request_json(serde_json::json!({
				"name": "echo",
				"arguments": { "rewritten": true, "limit": 10, "ratio": 2.5 },
			}))
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed");

	// echo serializes its arguments verbatim; rewritten args ⇒ rewrite reached upstream.
	let text = guardrails_test_support::echo_text(&result);
	assert!(text.contains("rewritten") && text.contains("true"));
	assert!(!text.contains("\"hi\""));
	// Mutated numbers keep their JSON form: integers don't become 10.0.
	assert!(text.contains("\"limit\":10,"), "got: {text}");
	assert!(text.contains("\"ratio\":2.5"), "got: {text}");
}

#[tokio::test]
async fn mcp_guardrails_metadata_cel_evaluated_per_request() {
	use std::collections::HashMap;
	use std::sync::Mutex as StdMutex;

	use crate::cel::Expression;
	use crate::test_helpers::extmcpmock::{closure_mock, pass_request, pass_response};

	let captured: Arc<StdMutex<Option<prost_wkt_types::Struct>>> = Arc::new(StdMutex::new(None));
	let extmcp_mock = {
		let store = captured.clone();
		closure_mock(
			move |req| {
				if req.method == "tools/call"
					&& let Some(md) = req.metadata_context.as_ref()
				{
					*store.lock().unwrap() = Some(md.clone());
				}
				pass_request()
			},
			|_| pass_response(),
		)
		.spawn()
		.await
	};

	let mut metadata = HashMap::new();
	metadata.insert(
		"tenant.io".to_string(),
		Arc::new(Expression::new_strict(r#"{"path": request.path}"#).unwrap()),
	);
	let policy = guardrails_test_support::policy_with(
		extmcp_mock.address,
		guardrails::FailureMode::FailClosed,
		guardrails_test_support::default_methods(),
		metadata,
	);

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;
	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("call should succeed");

	let md = captured.lock().unwrap().clone().expect("metadata captured");
	let entry = md.fields.get("tenant.io").expect("tenant.io key present");
	assert_eq!(
		serde_json::to_value(entry).unwrap(),
		serde_json::json!({"path": "/mcp"}),
	);
}

// mcpGuardrails returns metadata in its request result; an MCP authorization rule then
// denies a tool based on that metadata (the inbound metadata -> CEL authz path).
#[tokio::test]
async fn mcp_guardrails_metadata_consumed_by_authz() {
	use crate::test_helpers::extmcpmock::{closure_mock, pass_request_with, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			pass_request_with(
				Vec::<(String, String)>::new(),
				Vec::<String>::new(),
				Some(serde_json::from_value(serde_json::json!({"tier": "free"})).unwrap()),
			)
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let deny_free_tier = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(
			cel::Expression::new_strict(r#"mcp.tool.name == "echo" && mcpGuardrails.tier == "free""#)
				.unwrap(),
		)],
		vec![],
	)));

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![
			BackendTrafficPolicy::McpAuthorization(deny_free_tier),
			guardrails_test_support::policy(extmcp_mock.address),
		],
	)
	.await;
	let client = mcp_streamable_client(io).await;

	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(serde_json::Map::new()),
		)
		.await
		.expect_err("echo should be denied when mcpGuardrails marks the caller free-tier");
	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32602, "authz denial maps to INVALID_PARAMS");
	assert_eq!(e.message.as_ref(), "Unknown tool: echo");
}

// Simiilar to mcp_guardrails_metadata_consumed_by_authz but for the fanout path.
#[tokio::test]
async fn mcp_guardrails_metadata_consumed_by_list_authz() {
	use crate::test_helpers::extmcpmock::{closure_mock, pass_request_with, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			pass_request_with(
				Vec::<(String, String)>::new(),
				Vec::<String>::new(),
				Some(serde_json::from_value(serde_json::json!({"tier": "free"})).unwrap()),
			)
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let deny_free_tier = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(
			cel::Expression::new_strict(r#"mcp.tool.name == "echo" && mcpGuardrails.tier == "free""#)
				.unwrap(),
		)],
		vec![],
	)));

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![
			BackendTrafficPolicy::McpAuthorization(deny_free_tier),
			guardrails_test_support::policy(extmcp_mock.address),
		],
	)
	.await;
	let client = mcp_streamable_client(io).await;

	let tool_names: Vec<String> = client
		.list_tools(None)
		.await
		.expect("list_tools should succeed")
		.tools
		.into_iter()
		.map(|t| t.name.to_string())
		.sorted()
		.collect();

	// `echo` is filtered only because list authz saw mcpGuardrails's `tier=free` metadata;
	// without it the deny rule would not match and `echo` would remain.
	assert!(
		!tool_names.contains(&"echo".to_string()),
		"echo should be filtered when mcpGuardrails marks the caller free-tier: {tool_names:?}"
	);
	assert!(
		tool_names.contains(&"increment".to_string()),
		"non-denied tools should remain: {tool_names:?}"
	);
}

#[tokio::test]
async fn mcp_guardrails_filtered_list_via_response_mutation() {
	use crate::test_helpers::extmcpmock::{
		closure_mock, mutated_response_json, pass_request, pass_response,
	};

	let extmcp_mock = closure_mock(
		|_| pass_request(),
		|req| {
			if req.method != "tools/list" {
				return pass_response();
			}
			mutated_response_json(serde_json::json!({
				"tools": [{
					"name": "echo",
					"description": "Repeat what you say",
					"inputSchema": { "type": "object" },
				}],
			}))
		},
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let tools = client
		.list_tools(None)
		.await
		.expect("list_tools should succeed");
	let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();
	assert_eq!(names, vec!["echo".to_string()]);
}

// Fanout (multi-backend) runs the response hook ONCE on the merged, muxed result
// rather than once per upstream: the processor sees a single checkResponse carrying
// the prefixed (`a_echo`, `b_echo`) tools and every backend in service_names.
#[tokio::test]
async fn mcp_guardrails_fanout_runs_once_on_merged_muxed_result() {
	use std::sync::atomic::{AtomicUsize, Ordering};
	use std::sync::{Arc, Mutex};

	use protos::ext_mcp::McpResponse;

	use crate::test_helpers::extmcpmock::{closure_mock, mutated_response_json, pass_request};

	type Captured = Arc<Mutex<Option<(Vec<String>, Vec<String>)>>>;

	let resp_count = Arc::new(AtomicUsize::new(0));
	let captured: Captured = Arc::new(Mutex::new(None));
	let rc = resp_count.clone();
	let cap = captured.clone();
	let extmcp_mock = closure_mock(
		move |_| pass_request(),
		move |req: &McpResponse| {
			if req.method != "tools/list" {
				return crate::test_helpers::extmcpmock::pass_response();
			}
			rc.fetch_add(1, Ordering::SeqCst);
			let names: Vec<String> = serde_json::from_slice::<serde_json::Value>(&req.mcp_response)
				.ok()
				.and_then(|v| {
					v.get("tools").and_then(|t| t.as_array()).map(|arr| {
						arr
							.iter()
							.filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
							.collect()
					})
				})
				.unwrap_or_default();
			*cap.lock().unwrap() = Some((req.service_names.clone(), names));
			// Mutating the merged list proves the hook operates on the aggregate.
			mutated_response_json(serde_json::json!({
				"tools": [{
					"name": "a_echo",
					"description": "Repeat what you say",
					"inputSchema": { "type": "object" },
				}],
			}))
		},
	)
	.spawn()
	.await;

	let mock_a = mock_streamable_http_server(true).await;
	let mock_b = mock_streamable_http_server(true).await;
	let t = setup_proxy_test("{}")
		.unwrap()
		.with_multiplex_mcp_backend_policies(
			"mcp",
			vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
			true,
			vec![guardrails_test_support::policy(extmcp_mock.address)],
		)
		.with_bind(simple_bind())
		.with_route(basic_named_route(strng::new("/mcp")));
	let io = t.serve_real_listener(strng::new("bind")).await;
	let client = mcp_streamable_client(io).await;

	let tools = client
		.list_tools(None)
		.await
		.expect("list_tools should succeed");
	let names: Vec<String> = tools.tools.iter().map(|t| t.name.to_string()).collect();

	// One RPC for the whole fanout, not one per backend.
	assert_eq!(
		resp_count.load(Ordering::SeqCst),
		1,
		"response hook should run exactly once for the merged fanout result"
	);

	let (service_names, seen) = captured
		.lock()
		.unwrap()
		.clone()
		.expect("processor saw tools/list");
	// Aggregate identity = every fanned-out backend.
	assert_eq!(service_names, vec!["a".to_string(), "b".to_string()]);
	// The processor saw the merged, muxed list (prefixed names from both backends).
	assert!(
		seen.iter().any(|n| n == "a_echo") && seen.iter().any(|n| n == "b_echo"),
		"processor should see muxed names from both backends, got: {seen:?}"
	);

	// The mutation on the merged result is what reaches the client.
	assert_eq!(names, vec!["a_echo".to_string()]);
}

// A mutated `tools/call` result must round-trip back through `ServerResult` and
// reach the client (the `*/list` case above exercises a different variant).
#[tokio::test]
async fn mcp_guardrails_mutated_tool_call_response_reaches_client() {
	use crate::test_helpers::extmcpmock::{
		closure_mock, mutated_response_json, pass_request, pass_response,
	};

	let extmcp_mock = closure_mock(
		|_| pass_request(),
		|req| {
			if req.method != "tools/call" {
				return pass_response();
			}
			mutated_response_json(serde_json::json!({
				"content": [{ "type": "text", "text": "scrubbed-by-guardrails" }],
			}))
		},
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed");

	let text = guardrails_test_support::echo_text(&result);
	assert_eq!(text, "scrubbed-by-guardrails");
	assert!(!text.contains("world"));
}

#[tokio::test]
async fn mcp_guardrails_fail_open_on_grpc_error() {
	use std::collections::HashMap;

	use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

	let extmcp_mock = closure_mock(
		|_| Err(tonic::Status::internal("simulated mcpGuardrails failure")),
		|_| pass_response(),
	)
	.spawn()
	.await;

	let policy = guardrails_test_support::policy_with(
		extmcp_mock.address,
		guardrails::FailureMode::FailOpen,
		guardrails_test_support::default_methods(),
		HashMap::new(),
	);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;
	let result = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed under failure_mode=Allow");

	let text = guardrails_test_support::echo_text(&result);
	assert!(text.contains("\"hi\"") && text.contains("\"world\""));
}

#[tokio::test]
async fn mcp_guardrails_fail_closed_on_grpc_error() {
	use std::collections::HashMap;

	use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

	let extmcp_mock = closure_mock(
		|_| Err(tonic::Status::internal("simulated mcpGuardrails failure")),
		|_| pass_response(),
	)
	.spawn()
	.await;

	let policy = guardrails_test_support::policy_with(
		extmcp_mock.address,
		guardrails::FailureMode::FailClosed,
		guardrails_test_support::default_methods(),
		HashMap::new(),
	);
	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(&mock, true, false, vec![policy]).await;
	let client = mcp_streamable_client(io).await;
	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect_err("tool call should fail under failure_mode=Deny when mcpGuardrails errors");

	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(
		e.code,
		rmcp::model::ErrorCode::INTERNAL_ERROR,
		"gRPC failure should map to internal error"
	);
	assert!(
		e.message.contains("mcpGuardrails checkRequest failed"),
		"unexpected message: {}",
		e.message
	);
}

#[tokio::test]
async fn mcp_guardrails_response_reject_surfaces_jsonrpc_error() {
	use protos::ext_mcp::authorization_error::Code;

	use crate::test_helpers::extmcpmock::{
		closure_mock, pass_request, pass_response, reject_response,
	};

	let extmcp_mock = closure_mock(
		|_| pass_request(),
		|req| {
			if req.method != "tools/call" {
				return pass_response();
			}
			reject_response(Code::PermissionDenied, "blocked on response")
		},
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect_err("tool call should fail when mcpGuardrails rejects the response");

	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(e.code.0, -32001, "PermissionDenied should map to -32001");
	assert_eq!(e.message.as_ref(), "blocked on response");
}

#[tokio::test]
async fn mcp_guardrails_protocol_violation_fails_closed() {
	use crate::mcp::guardrails::wire;
	use crate::test_helpers::extmcpmock::{closure_mock, pass_response};

	// A response with no `result` oneof set is a contract violation; under
	// failure_mode=Deny it must reject rather than pass through.
	let extmcp_mock = closure_mock(
		|_| {
			Ok(wire::McpRequestResult {
				result: None,
				header_mutation: None,
				metadata: None,
			})
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect_err(
			"tool call should fail on mcpGuardrails protocol violation under failure_mode=Deny",
		);

	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert_eq!(
		e.code,
		rmcp::model::ErrorCode::INTERNAL_ERROR,
		"protocol violation should map to internal error"
	);
	assert!(
		e.message.contains("protocol violation"),
		"unexpected message: {}",
		e.message
	);
}

#[tokio::test]
async fn mcp_guardrails_non_object_mutation_is_protocol_violation() {
	use crate::test_helpers::extmcpmock::{closure_mock, mutated_request_json, pass_response};

	// Mutated payloads must parse as the method's params; valid-but-wrong-shape
	// JSON must hit the protocol-violation path, not surface later as a
	// malformed MCP request.
	let extmcp_mock = closure_mock(
		|_| mutated_request_json(serde_json::json!([1, 2])),
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let err = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect_err("non-object mutation should reject under failure_mode=Deny");

	let rmcp::ServiceError::McpError(e) = &err else {
		panic!("expected McpError, got {err:?}");
	};
	assert!(
		e.message.contains("protocol violation"),
		"unexpected message: {}",
		e.message
	);
}

#[tokio::test]
async fn mcp_guardrails_header_mutation_reaches_upstream() {
	use crate::test_helpers::extmcpmock::{closure_mock, pass_request_with, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			pass_request_with(
				vec![("x-guardrails-test", "from-policy"), ("x-tenant", "acme")],
				vec!["user-agent"],
				None,
			)
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let (mock, captured) = mock_streamable_http_server_with_capture(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed");

	let headers = captured.lock().unwrap().clone();
	let saw_injected = headers.iter().any(|h| {
		h.get("x-guardrails-test").map(|v| v.as_bytes()) == Some(b"from-policy")
			&& h.get("x-tenant").map(|v| v.as_bytes()) == Some(b"acme")
	});
	assert!(
		saw_injected,
		"expected x-guardrails-test+x-tenant headers on upstream request; saw {headers:?}"
	);
	let tools_call_req = headers
		.iter()
		.rev()
		.find(|h| h.contains_key("x-guardrails-test"))
		.expect("found upstream request with injected header");
	assert!(
		!tools_call_req.contains_key("user-agent"),
		"expected user-agent to be removed by header_mutation.remove",
	);
}

#[tokio::test]
async fn mcp_guardrails_request_headers_visible_to_policy_server() {
	use std::sync::Mutex as StdMutex;

	use crate::test_helpers::extmcpmock::{closure_mock, pass_request, pass_response};

	let captured: Arc<StdMutex<Option<Vec<crate::mcp::guardrails::wire::McpHeader>>>> =
		Arc::new(StdMutex::new(None));
	let extmcp_mock = {
		let store = captured.clone();
		closure_mock(
			move |req| {
				if req.method == "tools/call" {
					*store.lock().unwrap() = Some(req.headers.clone());
				}
				pass_request()
			},
			|_| pass_response(),
		)
		.spawn()
		.await
	};

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed");

	let headers = captured.lock().unwrap().clone().expect("headers captured");
	// The inbound POST's headers reach the policy server without per-header CEL config.
	assert!(
		headers
			.iter()
			.any(|h| h.key.eq_ignore_ascii_case("content-type")),
		"expected incoming request headers forwarded to policy server; saw {headers:?}"
	);
}

// mcpGuardrails processor metadata is readable as `guardrails.*` in an upstream-leg transformation.
#[tokio::test]
async fn mcp_guardrails_request_metadata_usable_in_backend_transformation() {
	use crate::http::transformation_cel::{
		LocalTransform, LocalTransformationConfig, Transformation,
	};
	use crate::test_helpers::extmcpmock::{closure_mock, pass_request_with, pass_response};

	let extmcp_mock = closure_mock(
		|_| {
			let md = serde_json::from_value(serde_json::json!({ "tenant": "acme" })).unwrap();
			pass_request_with(Vec::<(&str, &str)>::new(), Vec::<&str>::new(), Some(md))
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let xfm = Transformation::try_from_local_config(
		LocalTransformationConfig {
			request: Some(LocalTransform {
				set: vec![(
					strng::new("x-from-guardrails"),
					strng::new("mcpGuardrails.tenant"),
				)],
				..Default::default()
			}),
			response: None,
		},
		true,
	)
	.unwrap();
	let target_policy = BackendTrafficPolicy::Transformation(Arc::new(xfm));

	let (mock, captured) = mock_streamable_http_server_with_capture(true).await;
	let (_bind, io) = setup_proxy_policies_with_target(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
		vec![target_policy],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let _ = client
		.call_tool(
			rmcp::model::CallToolRequestParams::new("echo").with_arguments(
				serde_json::json!({"hi": "world"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("tool call should succeed");

	let headers = captured.lock().unwrap().clone();
	let saw_metadata = headers
		.iter()
		.any(|h| h.get("x-from-guardrails").map(|v| v.as_bytes()) == Some(b"acme"));
	assert!(
		saw_metadata,
		"expected x-from-guardrails:acme set by a backend transformation reading guardrails.tenant; saw {headers:?}"
	);
}

#[tokio::test]
async fn mcp_guardrails_mutated_prompt_request_reaches_upstream() {
	use crate::test_helpers::extmcpmock::{
		closure_mock, mutated_request_json, pass_request, pass_response,
	};

	let extmcp_mock = closure_mock(
		|req| {
			if req.method != "prompts/get" {
				return pass_request();
			}
			mutated_request_json(serde_json::json!({
				"name": "example_prompt",
				"arguments": { "message": "rewritten-by-guardrails" },
			}))
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let result = client
		.get_prompt(
			rmcp::model::GetPromptRequestParams::new("example_prompt").with_arguments(
				serde_json::json!({"message": "original-message"})
					.as_object()
					.cloned()
					.unwrap(),
			),
		)
		.await
		.expect("get_prompt should succeed");

	// example_prompt echoes `message` into its text body.
	let text = result
		.messages
		.iter()
		.find_map(|m| match &m.content {
			rmcp::model::PromptMessageContent::Text { text } => Some(text.clone()),
			_ => None,
		})
		.expect("prompt should have text content");
	assert!(text.contains("rewritten-by-guardrails"));
	assert!(!text.contains("original-message"));
}

#[tokio::test]
async fn mcp_guardrails_mutated_resource_read_reaches_upstream() {
	use crate::test_helpers::extmcpmock::{
		closure_mock, mutated_request_json, pass_request, pass_response,
	};

	let extmcp_mock = closure_mock(
		|req| {
			if req.method != "resources/read" {
				return pass_request();
			}
			// Redirect the client's "cwd" request to the "memo" resource.
			mutated_request_json(serde_json::json!({ "uri": "memo://insights" }))
		},
		|_| pass_response(),
	)
	.spawn()
	.await;

	let mock = mock_streamable_http_server(true).await;
	let (_bind, io) = setup_proxy_policies(
		&mock,
		true,
		false,
		vec![guardrails_test_support::policy(extmcp_mock.address)],
	)
	.await;
	let client = mcp_streamable_client(io).await;
	let result = client
		.read_resource(rmcp::model::ReadResourceRequestParams::new(
			"str:////Users/to/some/path/",
		))
		.await
		.expect("read_resource should succeed after URI rewrite");

	let text = result
		.contents
		.iter()
		.find_map(|c| match c {
			rmcp::model::ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
			_ => None,
		})
		.expect("resource should return text");
	assert!(text.contains("Business Intelligence Memo"));
}

// ============================================================================================
// ADOBE-ONLY TESTS — keep ALL `#[cfg(feature = "adobe")]` test modules below this marker, at
// the END of this file. Upstream test additions land ABOVE this line, so the Adobe block stays
// out of the way and rebases cleanly each sync (no interleaving with upstream tests).
// New Adobe test modules: add them here (below), never above this marker.
// ============================================================================================

#[cfg(feature = "adobe")]
mod mcp_rewrite_tests {
	use super::*;
	use rmcp::model::AnnotateAble;

	fn fake_target_with_rewrite(
	name: &str,
	addr: SocketAddr,
	rewrite: crate::mcp::McpRewritePolicy,
) -> Arc<McpTarget> {
	Arc::new(McpTarget {
		name: name.into(),
		spec: crate::types::agent::McpTargetSpec::Mcp(crate::types::agent::StreamableHTTPTargetSpec {
			backend: crate::types::agent::SimpleBackendReference::Backend(strng::format!(
				"/unused-{name}"
			)),
			path: "/mcp".to_string(),
		}),
		backend_policies: crate::store::BackendPolicies {
			mcp_rewrite: Some(rewrite),
			..Default::default()
		},
		backend: Some(crate::types::agent::SimpleBackend::Opaque(
			crate::types::agent::ResourceName::new(strng::format!("backend-{name}"), "".into()),
			crate::types::agent::Target::Address(addr),
		)),
		always_use_prefix: false,
	})
}

#[test]
fn merge_tools_applies_rename_before_multiplex_prefix() {
	use std::borrow::Cow;
	use std::sync::Arc;

	use rmcp::model::{ListToolsResult, ServerResult, Tool};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::single_tool_rename("echo", "echo_renamed");
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_target_with_rewrite(
				"svc",
				SocketAddr::from(([127, 0, 0, 1], 30301)),
				rewrite,
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tools();
	let tool = Tool::new(
		Cow::Owned("echo".to_string()),
		Cow::Borrowed(""),
		Arc::new(serde_json::Map::new()),
	);
	let streams = vec![(
		"svc".into(),
		ServerResult::ListToolsResult(ListToolsResult {
			tools: vec![tool],
			next_cursor: None,
			meta: None,
		}),
	)];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListToolsResult(ltr) = out else {
		panic!("expected ListToolsResult");
	};
	assert_eq!(ltr.tools[0].name.as_ref(), "echo_renamed");
}

#[test]
fn merge_tools_auth_on_upstream_rename_invisible_to_cel() {
	use std::borrow::Cow;
	use std::sync::Arc;

	use rmcp::model::{ListToolsResult, ServerResult, Tool};

	let deny_secret = McpAuthorization::new(RuleSet::new(PolicySet::new(
		vec![],
		vec![Arc::new(
			cel::Expression::new_strict(r#"mcp.tool.name == "secret""#).unwrap(),
		)],
		vec![],
	)));
	let policies = crate::mcp::McpAuthorizationSet::new(crate::http::authorization::RuleSets::from(
		vec![deny_secret.into_inner()],
	));
	let rewrite = crate::mcp::rewrite::McpRewritePolicy::single_tool_rename("echo", "echo_renamed");
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_target_with_rewrite(
				"svc",
				SocketAddr::from(([127, 0, 0, 1], 30302)),
				rewrite,
			)],
			..Default::default()
		},
		policies,
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tools();
	let streams = vec![(
		"svc".into(),
		ServerResult::ListToolsResult(ListToolsResult {
			tools: vec![
				Tool::new(
					Cow::Owned("echo".to_string()),
					Cow::Borrowed(""),
					Arc::new(serde_json::Map::new()),
				),
				Tool::new(
					Cow::Owned("secret".to_string()),
					Cow::Borrowed(""),
					Arc::new(serde_json::Map::new()),
				),
			],
			next_cursor: None,
			meta: None,
		}),
	)];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListToolsResult(ltr) = out else {
		panic!("expected ListToolsResult");
	};
	let names: Vec<_> = ltr.tools.iter().map(|t| t.name.as_ref()).collect();
	assert_eq!(names, vec!["echo_renamed"]);
}

#[tokio::test]
async fn flat_resolve_tool_call_uses_tools_list_route_index() {
	use std::borrow::Cow;
	use std::sync::Arc;

	use rmcp::model::{ListToolsResult, ServerResult, Tool};

	let federation = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let everything_rewrite =
		crate::mcp::rewrite::McpRewritePolicy::single_tool_rename("echo", "echo_demo");
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite(
					"mcp-server-everything",
					SocketAddr::from(([127, 0, 0, 1], 30320)),
					everything_rewrite,
				),
				fake_target_with_rewrite(
					"mcp-server-threejs",
					SocketAddr::from(([127, 0, 0, 1], 30321)),
					federation.clone(),
				),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tools();
	let mk = |n: &str| {
		Tool::new(
			Cow::Owned(n.to_string()),
			Cow::Borrowed(""),
			Arc::new(serde_json::Map::new()),
		)
	};
	let _ = merge(vec![
		(
			"mcp-server-everything".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("echo")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"mcp-server-threejs".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("show_threejs_scene")],
				next_cursor: None,
				meta: None,
			}),
		),
	], &cel)
	.unwrap();

	let ctx = crate::mcp::upstream::IncomingRequestContext::empty();
	let (target, upstream) = relay
		.resolve_tool_call("show_threejs_scene", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "mcp-server-threejs");
	assert_eq!(upstream, "show_threejs_scene");

	let (target, upstream) = relay
		.resolve_tool_call("echo_demo", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "mcp-server-everything");
	assert_eq!(upstream, "echo");
}

#[tokio::test]
async fn flat_resolve_prompt_call_uses_prompts_list_route_index() {
	use rmcp::model::{ListPromptsResult, Prompt, ServerResult};

	let federation = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	// Policy assumes underscore upstream names; npm server-everything uses hyphens.
	let everything_rewrite =
		crate::mcp::rewrite::McpRewritePolicy::single_prompt_rename("simple_prompt", "simple-prompt");
	let rewrite = crate::mcp::rewrite::McpRewriteSet::merge_for_target(
		Some(&federation),
		Some(&everything_rewrite),
	);
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_target_with_rewrite(
				"mcp-server-everything",
				SocketAddr::from(([127, 0, 0, 1], 30322)),
				rewrite,
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_prompts();
	let _ = merge(
		vec![(
			"mcp-server-everything".into(),
			ServerResult::ListPromptsResult(ListPromptsResult {
				prompts: vec![Prompt::new("simple-prompt", None::<&str>, None)],
				next_cursor: None,
				meta: None,
			}),
		)],
		&cel,
	)
	.unwrap();

	let ctx = crate::mcp::upstream::IncomingRequestContext::empty();
	let (target, upstream) = relay
		.resolve_prompt_call("simple-prompt", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "mcp-server-everything");
	// Route index from prompts/list wins over static exposed_to_upstream (simple_prompt).
	assert_eq!(upstream, "simple-prompt");
}

/// Regression for the multi-target federation `tools/call` bug
/// (`TO_INVESTIGATE.md`, ambiguous flat tool name).
///
/// Federation has FOUR targets — only one (`mcp-server-everything`) advertises
/// `simulate-research-query`; the other three advertise unrelated tools and
/// have no per-target rewrite. Before the fix, `resolve_flat_tool` would either
/// drop the entry from `build_flat_tool_route_index` (on collision) or fall
/// through to the broken pass-through fallback and return "ambiguous flat tool
/// name" because the three rewrite-less targets each contributed a hit.
#[tokio::test]
async fn flat_resolve_tool_call_routes_unique_name_in_four_target_federation() {
	use std::borrow::Cow;
	use std::sync::Arc;

	use rmcp::model::{ListToolsResult, ServerResult, Tool};

	let federation = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let everything_rewrite =
		crate::mcp::rewrite::McpRewritePolicy::single_tool_rename("echo", "echo_demo");
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite(
					"mcp-server-everything",
					SocketAddr::from(([127, 0, 0, 1], 30331)),
					everything_rewrite,
				),
				fake_target_with_rewrite(
					"mcp-server-airbnb",
					SocketAddr::from(([127, 0, 0, 1], 30332)),
					federation.clone(),
				),
				fake_target_with_rewrite(
					"mcp-server-map",
					SocketAddr::from(([127, 0, 0, 1], 30333)),
					federation.clone(),
				),
				fake_target_with_rewrite(
					"mcp-server-threejs",
					SocketAddr::from(([127, 0, 0, 1], 30334)),
					federation,
				),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tools();
	let mk = |n: &str| {
		Tool::new(
			Cow::Owned(n.to_string()),
			Cow::Borrowed(""),
			Arc::new(serde_json::Map::new()),
		)
	};
	// Simulate the merged `tools/list` shape from the user's repro:
	// - everything: echo (→ echo_demo via rewrite) + the task-required tool
	// - airbnb / map / threejs: unique per-target tools
	let _ = merge(vec![
		(
			"mcp-server-everything".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("echo"), mk("simulate-research-query")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"mcp-server-airbnb".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("airbnb_search_listings")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"mcp-server-map".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("map_geocode")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"mcp-server-threejs".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("show_threejs_scene")],
				next_cursor: None,
				meta: None,
			}),
		),
	], &cel)
	.unwrap();

	let ctx = crate::mcp::upstream::IncomingRequestContext::empty();

	// The task-required tool — only advertised by mcp-server-everything.
	let (target, upstream) = relay
		.resolve_tool_call("simulate-research-query", &ctx)
		.await
		.expect("simulate-research-query must route to mcp-server-everything");
	assert_eq!(target, "mcp-server-everything");
	assert_eq!(upstream, "simulate-research-query");

	// The rewritten name — only advertised by mcp-server-everything.
	let (target, upstream) = relay
		.resolve_tool_call("echo_demo", &ctx)
		.await
		.expect("echo_demo must route to mcp-server-everything");
	assert_eq!(target, "mcp-server-everything");
	assert_eq!(upstream, "echo");

	// Unique names from each other target also route.
	let (target, _) = relay
		.resolve_tool_call("airbnb_search_listings", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "mcp-server-airbnb");
	let (target, _) = relay
		.resolve_tool_call("map_geocode", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "mcp-server-map");
	let (target, _) = relay
		.resolve_tool_call("show_threejs_scene", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "mcp-server-threejs");
}

/// Regression for [`build_flat_tool_route_index`] inconsistency: when multiple
/// federation targets share a tool name (e.g., several `server-everything`
/// clones each export `echo`), the user-visible `tools/list` keeps the first
/// via `filter_flat_tool_collisions`, but the route index used to *remove*
/// both, making the visible name unroutable.
#[tokio::test]
async fn flat_route_index_first_wins_keeps_colliding_name_callable() {
	use std::borrow::Cow;
	use std::sync::Arc;

	use rmcp::model::{ListToolsResult, ServerResult, Tool};

	let federation = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30341)), federation.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30342)), federation),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tools();
	let mk = |n: &str| {
		Tool::new(
			Cow::Owned(n.to_string()),
			Cow::Borrowed(""),
			Arc::new(serde_json::Map::new()),
		)
	};
	let out = merge(vec![
		(
			"a".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("echo")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"b".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("echo")],
				next_cursor: None,
				meta: None,
			}),
		),
	], &cel)
	.unwrap();
	let ServerResult::ListToolsResult(ltr) = out else {
		panic!("expected ListToolsResult");
	};
	// list keeps the first occurrence
	assert_eq!(ltr.tools.len(), 1);
	assert_eq!(ltr.tools[0].name.as_ref(), "echo");

	// route index must keep the SAME first occurrence so the visible name is callable.
	let ctx = crate::mcp::upstream::IncomingRequestContext::empty();
	let (target, upstream) = relay
		.resolve_tool_call("echo", &ctx)
		.await
		.expect("echo must remain callable after first-wins collision");
	assert_eq!(target, "a");
	assert_eq!(upstream, "echo");
}

#[tokio::test]
async fn resolve_tool_call_maps_exposed_to_upstream() {
	let rewrite = crate::mcp::rewrite::McpRewritePolicy::single_tool_rename("echo", "echo_renamed");
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_target_with_rewrite(
				"svc",
				SocketAddr::from(([127, 0, 0, 1], 30303)),
				rewrite,
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();
	let ctx = crate::mcp::upstream::IncomingRequestContext::empty();
	let (target, upstream) = relay
		.resolve_tool_call("echo_renamed", &ctx)
		.await
		.unwrap();
	assert_eq!(target, "svc");
	assert_eq!(upstream, "echo");
}

#[test]
fn flat_merge_tools_omits_pass_through_name_collision() {
	use std::borrow::Cow;
	use std::sync::Arc;

	use rmcp::model::{ListToolsResult, ServerResult, Tool};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30304)), rewrite.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30305)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tools();
	let mk = |n: &str| {
		Tool::new(
			Cow::Owned(n.to_string()),
			Cow::Borrowed(""),
			Arc::new(serde_json::Map::new()),
		)
	};
	let streams = vec![
		(
			"a".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("search")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"b".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![mk("search")],
				next_cursor: None,
				meta: None,
			}),
		),
	];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListToolsResult(ltr) = out else {
		panic!("expected ListToolsResult");
	};
	assert_eq!(ltr.tools.len(), 1);
	assert_eq!(ltr.tools[0].name.as_ref(), "search");
}

#[test]
fn flat_merge_resources_keeps_flat_name_and_multiplex_uri() {
	use rmcp::model::{ListResourcesResult, RawResource, ServerResult};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("alpha", SocketAddr::from(([127, 0, 0, 1], 30306)), rewrite.clone()),
				fake_target_with_rewrite("beta", SocketAddr::from(([127, 0, 0, 1], 30313)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_resources();
	let resource = RawResource::new("memo://insights/report", "memo".to_string()).no_annotation();
	let streams = vec![(
		"alpha".into(),
		ServerResult::ListResourcesResult(ListResourcesResult {
			resources: vec![resource],
			next_cursor: None,
			meta: None,
		}),
	)];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListResourcesResult(lrr) = out else {
		panic!("expected ListResourcesResult");
	};
	assert_eq!(lrr.resources.len(), 1);
	assert_eq!(lrr.resources[0].name, "memo");
	assert!(
		lrr.resources[0].uri.contains("alpha+"),
		"expected multiplexed URI, got {}",
		lrr.resources[0].uri
	);
	let (target, original) =
		crate::mcp::mcp_apps::routing::parse_resource_uri_mixed(None, &lrr.resources[0].uri)
			.unwrap();
	assert_eq!(target, "alpha");
	assert_eq!(original, "memo://insights/report");
}

#[test]
fn flat_merge_resources_omits_pass_through_name_collision() {
	use rmcp::model::{ListResourcesResult, RawResource, ServerResult};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30307)), rewrite.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30308)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_resources();
	let mk = |uri: &str| RawResource::new(uri, "memo".to_string()).no_annotation();
	let streams = vec![
		(
			"a".into(),
			ServerResult::ListResourcesResult(ListResourcesResult {
				resources: vec![mk("memo://a/1")],
				next_cursor: None,
				meta: None,
			}),
		),
		(
			"b".into(),
			ServerResult::ListResourcesResult(ListResourcesResult {
				resources: vec![mk("memo://b/1")],
				next_cursor: None,
				meta: None,
			}),
		),
	];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListResourcesResult(lrr) = out else {
		panic!("expected ListResourcesResult");
	};
	assert_eq!(lrr.resources.len(), 1);
	assert_eq!(lrr.resources[0].name, "memo");
}

#[test]
fn flat_merge_resource_templates_keeps_flat_name_and_multiplex_uri_template() {
	use rmcp::model::{ListResourceTemplatesResult, RawResourceTemplate, ServerResult};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("alpha", SocketAddr::from(([127, 0, 0, 1], 30309)), rewrite.clone()),
				fake_target_with_rewrite("beta", SocketAddr::from(([127, 0, 0, 1], 30314)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_resource_templates();
	let template =
		RawResourceTemplate::new("file://{path}", "template").no_annotation();
	let streams = vec![(
		"alpha".into(),
		ServerResult::ListResourceTemplatesResult(ListResourceTemplatesResult {
			resource_templates: vec![template],
			next_cursor: None,
			meta: None,
		}),
	)];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListResourceTemplatesResult(lrt) = out else {
		panic!("expected ListResourceTemplatesResult");
	};
	assert_eq!(lrt.resource_templates.len(), 1);
	assert_eq!(lrt.resource_templates[0].name, "template");
	assert!(
		lrt.resource_templates[0]
			.uri_template
			.starts_with("alpha+file://"),
		"expected multiplexed template URI, got {}",
		lrt.resource_templates[0].uri_template
	);
}

#[test]
fn flat_merge_tasks_omits_pass_through_id_collision() {
	use rmcp::model::{ListTasksResult, ServerResult, Task, TaskStatus};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30310)), rewrite.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30311)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tasks();
	let mk = |id: &str| {
		let ts = "2020-01-01T00:00:00Z";
		Task::new(id.to_string(), TaskStatus::Working, ts.into(), ts.into())
	};
	let streams = vec![
		(
			"a".into(),
			ServerResult::ListTasksResult(ListTasksResult::new(vec![mk("job-1")])),
		),
		(
			"b".into(),
			ServerResult::ListTasksResult(ListTasksResult::new(vec![mk("job-1")])),
		),
	];
	let out = merge(streams, &cel).unwrap();
	let ServerResult::ListTasksResult(ltr) = out else {
		panic!("expected ListTasksResult");
	};
	assert_eq!(ltr.tasks.len(), 1);
	assert_eq!(ltr.tasks[0].task_id, "job-1");
}

#[test]
fn flat_merge_tasks_populates_route_index_for_tasks_get() {
	use rmcp::model::{ListTasksResult, ServerResult, Task, TaskStatus};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30317)), rewrite.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30318)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tasks();
	let mk = |id: &str| {
		let ts = "2020-01-01T00:00:00Z";
		Task::new(id.to_string(), TaskStatus::Working, ts.into(), ts.into())
	};
	let _ = merge(vec![(
		"a".into(),
		ServerResult::ListTasksResult(ListTasksResult::new(vec![mk("job-99")])),
	)], &cel);

	let (target, upstream) = relay.resolve_task_call("job-99").unwrap();
	assert_eq!(target, "a");
	assert_eq!(upstream, "job-99");
}

/// Regression: `tasks/list` must not wipe routes that `tools/call` create
/// (`record_flat_task_route`) populated since the last fanout. Without the
/// first-wins merge in `merge_tasks`, a follow-up `tasks/get` on the just-
/// created task would error with `unknown flat task id` until the upstream's
/// own `tasks/list` caught up.
#[test]
fn flat_merge_tasks_preserves_create_recorded_route() {
	use rmcp::model::{ListTasksResult, ServerResult, Task, TaskStatus};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30351)), rewrite.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30352)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	// Simulate `tools/call` → CreateTaskResult on target `a` before any
	// federated `tasks/list` has run.
	relay.record_flat_task_route("a", "created-1");

	// Now federated `tasks/list` runs; upstream `b` returns a different task
	// and doesn't (yet) know about `created-1`.
	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tasks();
	let ts = "2020-01-01T00:00:00Z";
	let mk = |id: &str| Task::new(id.to_string(), TaskStatus::Working, ts.into(), ts.into());
	let _ = merge(vec![(
		"b".into(),
		ServerResult::ListTasksResult(ListTasksResult::new(vec![mk("job-from-b")])),
	)], &cel);

	// `created-1` must still resolve to target `a` — the new entry from `b`
	// is also routable, but the prior create-recorded entry is preserved.
	let (target, upstream) = relay
		.resolve_task_call("created-1")
		.expect("create-recorded route must survive tasks/list");
	assert_eq!(target, "a");
	assert_eq!(upstream, "created-1");

	let (target, upstream) = relay
		.resolve_task_call("job-from-b")
		.expect("tasks/list-derived route must also resolve");
	assert_eq!(target, "b");
	assert_eq!(upstream, "job-from-b");
}

#[tokio::test]
async fn ensure_flat_task_routes_loaded_noop_when_index_populated() {
	use crate::mcp::upstream::IncomingRequestContext;
	use rmcp::model::{ListTasksResult, ServerResult, Task, TaskStatus};

	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_target_with_rewrite(
				"a",
				SocketAddr::from(([127, 0, 0, 1], 30321)),
				rewrite,
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();

	let cel = crate::mcp::rbac::CelExecWrapper::new(
		::http::Request::builder()
			.uri("http://example.com/mcp")
			.body(())
			.unwrap(),
	);
	let merge = relay.merge_tasks();
	let ts = "2020-01-01T00:00:00Z";
	let _ = merge(vec![(
		"a".into(),
		ServerResult::ListTasksResult(ListTasksResult::new(vec![Task::new(
			"job-1".into(),
			TaskStatus::Working,
			ts.into(),
			ts.into(),
		)])),
	)], &cel);

	let ctx = IncomingRequestContext::empty();
	relay
		.ensure_flat_task_routes_loaded(&ctx)
		.await
		.expect("noop when index already populated");
}

#[test]
fn resolve_task_call_flat_maps_to_target() {
	let rewrite = crate::mcp::rewrite::McpRewritePolicy::flat_server();
	let relay = Relay::new(
		McpBackendGroup {
			targets: vec![fake_target_with_rewrite(
				"svc",
				SocketAddr::from(([127, 0, 0, 1], 30312)),
				rewrite.clone(),
			)],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();
	let (target, upstream) = relay.resolve_task_call("job-42").unwrap();
	assert_eq!(target, "svc");
	assert_eq!(upstream, "job-42");

	let multi = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite("a", SocketAddr::from(([127, 0, 0, 1], 30315)), rewrite.clone()),
				fake_target_with_rewrite("b", SocketAddr::from(([127, 0, 0, 1], 30316)), rewrite),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();
	let err = multi.resolve_task_call("job-42").unwrap_err();
	assert!(
		err.to_string().contains("ambiguous flat task id"),
		"{err}"
	);

	multi.record_flat_task_route("a", "job-42");
	let (target, upstream) = multi.resolve_task_call("job-42").unwrap();
	assert_eq!(target, "a");
	assert_eq!(upstream, "job-42");

	let prefix_multi = Relay::new(
		McpBackendGroup {
			targets: vec![
				fake_target_with_rewrite(
					"a",
					SocketAddr::from(([127, 0, 0, 1], 30319)),
					crate::mcp::rewrite::McpRewritePolicy::default(),
				),
				fake_target_with_rewrite(
					"b",
					SocketAddr::from(([127, 0, 0, 1], 30320)),
					crate::mcp::rewrite::McpRewritePolicy::default(),
				),
			],
			..Default::default()
		},
		empty_mcp_policies(),
		PolicyClient {
			inputs: setup_proxy_test("{}").unwrap().pi,
			outbound: None,
		},
	)
	.unwrap();
	let (target, upstream) = prefix_multi.resolve_task_call("a_job-42").unwrap();
	assert_eq!(target, "a");
	assert_eq!(upstream, "job-42");

	assert_eq!(
		crate::mcp::multiplex_naming::wrap_client_task_id(None, "a", "job-99"),
		"a_job-99"
	);
}
}

#[cfg(feature = "adobe")]
mod adobe_mcp_apps_integration {
	use std::borrow::Cow;
	use std::sync::Arc;

	use agent_core::strng;
	use rmcp::model::{
		CancelTaskParams, CancelTaskRequest, ClientRequest, GetTaskInfoParams, GetTaskInfoRequest,
		GetTaskResultParams, GetTaskResultRequest, Implementation, InitializeResult, JsonRpcRequest,
		ListToolsResult, Meta, ProtocolVersion, ReadResourceRequestParams, RequestId,
		ServerCapabilities, ServerResult, SubscribeRequestParams, TaskStatus, Tool,
		UnsubscribeRequestParams,
	};
	use serde_json::json;

	use super::*;

	fn first_sse_data_json(body: &[u8]) -> serde_json::Value {
		let text = std::str::from_utf8(body).expect("utf8 body");
		for line in text.lines() {
			let line = line.trim_end();
			if let Some(payload) = line.strip_prefix("data:") {
				let payload = payload.trim_start();
				return serde_json::from_str(payload).expect("sse data json");
			}
		}
		panic!("no data: line in SSE body: {text}");
	}

	#[test]
	fn merge_tools_wraps_ui_resource_uri_in_tool_meta_when_multiplexing() {
		let relay = Relay::new(
			McpBackendGroup {
				targets: vec![
					fake_streamable_target("alpha", SocketAddr::from(([127, 0, 0, 1], 30201))),
					fake_streamable_target("beta", SocketAddr::from(([127, 0, 0, 1], 30202))),
				],
				..Default::default()
			},
			empty_mcp_policies(),
			PolicyClient {
				inputs: setup_proxy_test("{}").unwrap().pi,
				outbound: None,
			},
		)
		.unwrap();

		let cel = crate::mcp::rbac::CelExecWrapper::new(
			::http::Request::builder()
				.uri("http://example.com/mcp")
				.body(())
				.unwrap(),
		);
		let merge = relay.merge_tools();

		let mut meta = Meta::new();
		meta.insert("ui".into(), json!({ "resourceUri": "ui://x/app.html" }));
		meta.insert("ui/resourceUri".into(), json!("ui://x/app.html"));

		let mut tool = Tool::new(
			Cow::Borrowed("t1"),
			Cow::Borrowed(""),
			Arc::new(serde_json::Map::new()),
		);
		tool.meta = Some(meta);

		let streams: Vec<(Strng, ServerResult)> = vec![(
			"alpha".into(),
			ServerResult::ListToolsResult(ListToolsResult {
				tools: vec![tool],
				next_cursor: None,
				meta: None,
			}),
		)];

		let out = merge(streams, &cel).unwrap();
		let ServerResult::ListToolsResult(ltr) = out else {
			panic!("expected ListToolsResult");
		};
		let m = ltr.tools[0].meta.as_ref().unwrap();
		let uri = m["ui"]["resourceUri"].as_str().unwrap();
		let leg = m["ui/resourceUri"].as_str().unwrap();
		assert!(
			uri.starts_with("ui://"),
			"expected federated ui URI, got {uri}"
		);
		assert!(uri.contains("u="), "expected u= query in {uri}");
		assert_eq!(
			leg, uri,
			"legacy flat key must match nested form after merge_tools"
		);
	}

	#[test]
	fn capability_cache_records_tasks_per_upstream_when_multiplexing() {
		let relay = Relay::new(
			McpBackendGroup {
				targets: vec![
					fake_streamable_target("with_tasks", SocketAddr::from(([127, 0, 0, 1], 30203))),
					fake_streamable_target("no_tasks", SocketAddr::from(([127, 0, 0, 1], 30204))),
				],
				..Default::default()
			},
			empty_mcp_policies(),
			PolicyClient {
				inputs: setup_proxy_test("{}").unwrap().pi,
				outbound: None,
			},
		)
		.unwrap();

		let merge_fn = relay.merge_initialize(ProtocolVersion::V_2025_06_18, true);
		let results: Vec<(Strng, ServerResult)> = vec![
			(
				"with_tasks".into(),
				ServerResult::InitializeResult(
					InitializeResult::new(
						ServerCapabilities::builder()
							.enable_tools()
							.enable_tasks()
							.build(),
					)
					.with_protocol_version(ProtocolVersion::V_2025_06_18)
					.with_server_info(Implementation::new("a", "1")),
				),
			),
			(
				"no_tasks".into(),
				ServerResult::InitializeResult(
					InitializeResult::new(ServerCapabilities::builder().enable_tools().build())
						.with_protocol_version(ProtocolVersion::V_2025_06_18)
						.with_server_info(Implementation::new("b", "1")),
				),
			),
		];
		let cel = crate::mcp::rbac::CelExecWrapper::new(::http::Request::builder().uri("http://example.com/").body(()).unwrap());
	let _ = merge_fn(results, &cel).unwrap();

		let all = relay.all_target_names();
		let task_targets = relay.capabilities.upstreams_with_tasks(&all);
		assert!(task_targets.contains(&"with_tasks".to_string()));
		assert!(!task_targets.contains(&"no_tasks".to_string()));
	}

	#[test]
	fn merge_tasks_merge_fn_handles_no_upstream_results() {
		let relay = Relay::new(
			McpBackendGroup {
				targets: vec![
					fake_streamable_target("a", SocketAddr::from(([127, 0, 0, 1], 30205))),
					fake_streamable_target("b", SocketAddr::from(([127, 0, 0, 1], 30206))),
				],
				..Default::default()
			},
			empty_mcp_policies(),
			PolicyClient {
				inputs: setup_proxy_test("{}").unwrap().pi,
				outbound: None,
			},
		)
		.unwrap();

		let cel = crate::mcp::rbac::CelExecWrapper::new(
			::http::Request::builder()
				.uri("http://example.com/mcp")
				.body(())
				.unwrap(),
		);
		let merge = relay.merge_tasks();
		let out = merge(vec![], &cel).unwrap();
		let ServerResult::ListTasksResult(ltr) = out else {
			panic!("expected ListTasksResult");
		};
		assert!(ltr.tasks.is_empty());
	}

	#[tokio::test]
	async fn send_fanout_to_with_zero_matching_targets_returns_empty_tasks_via_sse() {
		let relay = Relay::new(
			McpBackendGroup {
				targets: vec![
					fake_streamable_target("a", SocketAddr::from(([127, 0, 0, 1], 30210))),
					fake_streamable_target("b", SocketAddr::from(([127, 0, 0, 1], 30211))),
				],
				..Default::default()
			},
			empty_mcp_policies(),
			PolicyClient {
				inputs: setup_proxy_test("{}").unwrap().pi,
				outbound: None,
			},
		)
		.unwrap();
		let _cel = crate::mcp::rbac::CelExecWrapper::new(
			::http::Request::builder()
				.uri("http://example.com/mcp")
				.body(())
				.unwrap(),
		);
		let merge = relay.merge_tasks();
		let targets: Vec<String> = vec![];
		let req = JsonRpcRequest::new(
			RequestId::Number(99),
			ClientRequest::ListTasksRequest(Default::default()),
		);
		let resp = relay
			.send_fanout_to(
				&targets,
				req,
				crate::mcp::upstream::IncomingRequestContext::empty(),
				merge,
			)
			.await
			.expect("empty fanout should not error");
		let body = crate::http::read_resp_body(resp).await.unwrap();
		let v = first_sse_data_json(&body);
		let tasks = v["result"]["tasks"].as_array().expect("tasks array");
		assert!(tasks.is_empty(), "expected empty task list, got {v}");
	}

	#[tokio::test]
	async fn multiplex_read_resource_round_trips_federated_uri() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;
		let resources = client.list_resources(None).await.unwrap().resources;
		let memo_res = resources
			.iter()
			.find(|r| r.uri.contains("memo://insights"))
			.expect("multiplexed memo resource");
		let read = client
			.read_resource(ReadResourceRequestParams::new(memo_res.uri.clone()))
			.await
			.expect("read_resource");
		let text = match &read.contents[0] {
			rmcp::model::ResourceContents::TextResourceContents { text, .. } => text.as_str(),
			other => panic!("expected text resource, got {other:?}"),
		};
		assert!(
			text.contains("Business Intelligence"),
			"unexpected memo body: {text}"
		);
	}

	#[tokio::test]
	async fn multiplex_subscribe_unsubscribe_unwraps_resource_uri_for_upstream() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;
		let resources = client.list_resources(None).await.unwrap().resources;
		let memo_res = resources
			.iter()
			.find(|r| r.uri.contains("memo://insights"))
			.expect("multiplexed memo resource");
		client
			.subscribe(SubscribeRequestParams::new(memo_res.uri.clone()))
			.await
			.expect("subscribe");
		client
			.unsubscribe(UnsubscribeRequestParams::new(memo_res.uri.clone()))
			.await
			.expect("unsubscribe");
	}

	#[tokio::test]
	async fn multiplex_call_tool_wraps_ui_resource_uri_in_response_meta() {
		// Without wrapping the call-tool response's `_meta.ui.resourceUri`, the host
		// receives the upstream-native `ui://...` and a subsequent `resources/read`
		// fails the multiplex parser with "missing 'u' query param". This regression
		// test pins the wire form by asserting the response meta carries a federated
		// `ui://...` URI that round-trips through `parse_resource_uri_mixed`.
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		let result = client
			.call_tool(rmcp::model::CallToolRequestParams::new("a_render_app"))
			.await
			.expect("call_tool render_app");

		let meta = result.meta.expect("CallToolResult must carry _meta");
		let ui = meta
			.get("ui")
			.and_then(|v| v.as_object())
			.expect("_meta.ui object");
		let uri = ui
			.get("resourceUri")
			.and_then(|v| v.as_str())
			.expect("_meta.ui.resourceUri string");
		let legacy = meta["ui/resourceUri"]
			.as_str()
			.expect("_meta[\"ui/resourceUri\"] string (registerAppTool wire form)");

		assert!(
			uri.starts_with("ui://"),
			"federated URI must keep `ui://` scheme (MCP Apps requirement); got {uri}"
		);
		assert_eq!(
			legacy, uri,
			"legacy flat key must match modern nested wrapping"
		);
		let (target, original) = crate::mcp::mcp_apps::routing::parse_resource_uri_mixed(None, uri)
			.expect("federated URI must parse back to (target, upstream uri)");
		assert_eq!(target, "a");
		assert_eq!(original, "ui://app/page.html");
		let (t2, o2) = crate::mcp::mcp_apps::routing::parse_resource_uri_mixed(None, legacy)
			.expect("legacy key must multiplex-parse identically");
		assert_eq!((t2, o2), (target.clone(), original.clone()));
	}

	#[tokio::test]
	async fn multiplex_call_tool_wraps_ui_resource_uri_in_embedded_content() {
		// A2UI sample tools return `ui://...` via EmbeddedResource content blocks rather than
		// `_meta.ui.resourceUri`. Without wrapping, federated `resources/read` fails multiplex parsing.
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		let result = client
			.call_tool(rmcp::model::CallToolRequestParams::new(
				"a_render_app_embedded",
			))
			.await
			.expect("call_tool render_app_embedded");

		let content = result.content.first().expect("embedded resource content");
		let resource = content.as_resource().expect("resource content block");
		let uri = match &resource.resource {
			rmcp::model::ResourceContents::TextResourceContents { uri, .. } => uri.as_str(),
			other => panic!("expected TextResourceContents, got {other:?}"),
		};
		assert!(
			uri.starts_with("ui://"),
			"federated URI must keep `ui://` scheme; got {uri}"
		);
		assert!(uri.contains("u="), "expected u= query in {uri}");
		let (target, original) =
			crate::mcp::mcp_apps::routing::parse_resource_uri_mixed(None, uri)
				.expect("federated URI must parse back to (target, upstream uri)");
		assert_eq!(target, "a");
		assert_eq!(original, "ui://basic/app");
	}

	#[tokio::test]
	async fn multiplex_initialize_advertises_prompts_capability() {
		// MCP Apps hosts (e.g. MCP Inspector) gate prompt UI on the server advertising
		// the `prompts` capability. Since the gateway multiplexes prompt names via the
		// `target_` prefix, prompts are safely federated and must be advertised even
		// when fronting multiple upstreams.
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		let info = client.peer_info().expect("peer info available");
		assert!(
			info.capabilities.prompts.is_some(),
			"federated gateway must advertise prompts capability; got {:?}",
			info.capabilities
		);
		assert!(
			info.capabilities.tools.is_some(),
			"federated gateway must advertise tools capability"
		);
		assert!(
			info.capabilities.resources.is_some(),
			"federated gateway must advertise resources capability"
		);
	}

	#[tokio::test]
	async fn multiplex_initialize_advertises_full_tasks_capability() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		let info = client.peer_info().expect("peer info available");
		let tasks = info
			.capabilities
			.tasks
			.as_ref()
			.expect("tasks capability advertised");
		assert!(
			tasks.list.is_some(),
			"MCP Inspector gates Tasks UI on tasks.list; got {:?}",
			info.capabilities.tasks
		);
		assert!(tasks.cancel.is_some());
		assert!(
			tasks
				.requests
				.as_ref()
				.and_then(|r| r.tools.as_ref())
				.and_then(|t| t.call.as_ref())
				.is_some(),
			"MCP Inspector expects tasks.requests.tools.call"
		);
	}

	#[tokio::test]
	async fn multiplex_tasks_merge_list_get_and_cancel_unwrap_ids() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;
		let list = client
			.send_request(ClientRequest::ListTasksRequest(Default::default()))
			.await
			.unwrap();
		let ServerResult::ListTasksResult(ltr) = list else {
			panic!("expected ListTasksResult, got {list:?}");
		};
		let mut ids: Vec<String> = ltr.tasks.into_iter().map(|t| t.task_id).collect();
		ids.sort();
		assert_eq!(ids, vec!["a_t1".to_string(), "b_t1".to_string()]);

		let info = client
			.send_request(ClientRequest::GetTaskInfoRequest(GetTaskInfoRequest::new(
				GetTaskInfoParams {
					meta: None,
					task_id: "a_t1".into(),
				},
			)))
			.await
			.unwrap();
		let ServerResult::GetTaskResult(gtr) = info else {
			panic!("expected GetTaskResult");
		};
		assert_eq!(gtr.task.task_id, "a_t1");

		let cancel = client
			.send_request(ClientRequest::CancelTaskRequest(CancelTaskRequest::new(
				CancelTaskParams {
					meta: None,
					task_id: "b_t1".into(),
				},
			)))
			.await
			.unwrap();
		match cancel {
			ServerResult::CancelTaskResult(ctr) => {
				assert_eq!(ctr.task.task_id, "b_t1");
				assert_eq!(ctr.task.status, TaskStatus::Cancelled);
			},
			ServerResult::GetTaskResult(gtr) => {
				// `CancelTaskResult` matches the same JSON shape as `GetTaskResult` (flattened task);
				// serde may decode successful cancel responses as `GetTaskResult`.
				assert_eq!(gtr.task.task_id, "b_t1");
				assert_eq!(gtr.task.status, TaskStatus::Cancelled);
			},
			other => panic!("unexpected cancel response: {other:?}"),
		}

		let payload = client
			.send_request(ClientRequest::GetTaskResultRequest(
				GetTaskResultRequest::new(GetTaskResultParams {
					meta: None,
					task_id: "a_t1".into(),
				}),
			))
			.await
			.unwrap();
		let ServerResult::CustomResult(custom) = payload else {
			panic!("expected CustomResult for tasks/result payload, got {payload:?}");
		};
		assert_eq!(custom.0, json!({"done": true}));
	}

	#[tokio::test]
	async fn multiplex_tools_call_create_task_wraps_id_and_tasks_get_unwraps() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![("a", mock_a.addr, false), ("b", mock_b.addr, false)],
				false,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		let create = client
			.send_request(ClientRequest::CallToolRequest(
				rmcp::model::CallToolRequest::new(
					rmcp::model::CallToolRequestParams::new("a_enqueue_task_tool")
						.with_task(serde_json::Map::new()),
				),
			))
			.await
			.expect("tools/call with task");
		let ServerResult::CreateTaskResult(ctr) = create else {
			panic!("expected CreateTaskResult, got {create:?}");
		};
		assert_eq!(ctr.task.task_id, "a_job-99");

		let info = client
			.send_request(ClientRequest::GetTaskInfoRequest(GetTaskInfoRequest::new(
				GetTaskInfoParams {
					meta: None,
					task_id: "a_job-99".into(),
				},
			)))
			.await
			.expect("tasks/get");
		let ServerResult::GetTaskResult(gtr) = info else {
			panic!("expected GetTaskResult");
		};
		assert_eq!(gtr.task.task_id, "a_job-99");
	}
}

#[cfg(feature = "adobe")]
mod federated_elicitation_tests {
	use std::sync::Arc;

	use agent_core::strng;
	use rmcp::model::{
		CancelledNotificationParam, ClientCapabilities, ClientInfo, CreateElicitationRequestParams,
		ElicitationAction, ElicitationSchema, Implementation, PrimitiveSchema, RequestId,
		StringSchema,
	};
	use rmcp::service::{NotificationContext, RequestContext};
	use rmcp::transport::streamable_http_server::StreamableHttpService;
	use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
	use rmcp::{
		ClientHandler, ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler,
		tool_router,
	};
	use rmcp::{RoleClient, ServiceExt};
	use serde_json::json;
	use std::sync::atomic::{AtomicU32, Ordering};
	use tokio::sync::Mutex;

	use super::*;
	use crate::test_helpers::proxymock::{basic_named_route, simple_bind};

	#[derive(Clone)]
	struct ElicitServer;

	#[tool_router]
	impl ElicitServer {
		pub fn new() -> Self {
			Self
		}

		#[tool(description = "Elicit user input then echo it")]
		async fn elicit_echo(
			&self,
			ctx: RequestContext<RoleServer>,
		) -> Result<rmcp::model::CallToolResult, McpError> {
			let schema = ElicitationSchema::builder()
				.required_property("answer", PrimitiveSchema::String(StringSchema::new()))
				.build()
				.map_err(|e| McpError::invalid_params(e.to_string(), None))?;
			let response = ctx
				.peer
				.create_elicitation(CreateElicitationRequestParams::FormElicitationParams {
					meta: None,
					message: "provide answer".to_string(),
					requested_schema: schema,
				})
				.await
				.map_err(|e| McpError::internal_error(e.to_string(), None))?;
			let text = match response.action {
				ElicitationAction::Accept => response
					.content
					.and_then(|v| v.get("answer").and_then(|a| a.as_str()).map(str::to_string))
					.unwrap_or_else(|| "empty".to_string()),
				ElicitationAction::Decline => "declined".to_string(),
				ElicitationAction::Cancel => "cancelled".to_string(),
			};
			Ok(rmcp::model::CallToolResult::success(vec![
				rmcp::model::Content::text(text),
			]))
		}
	}

	#[tool_handler]
	impl ServerHandler for ElicitServer {
		fn get_info(&self) -> rmcp::model::ServerInfo {
			rmcp::model::ServerInfo::new(
				rmcp::model::ServerCapabilities::builder()
					.enable_tools()
					.build(),
			)
		}
	}

	async fn mock_elicitation_server() -> MockServer {
		use rmcp::transport::StreamableHttpServerConfig;
		agent_core::telemetry::testing::setup_test_logging();
		let init_counter = Arc::new(Mutex::new(0_i32));
		let service = StreamableHttpService::new(
			|| Ok(ElicitServer::new()),
			LocalSessionManager::default().into(),
			StreamableHttpServerConfig::default()
				.with_sse_retry(None)
				.with_sse_keep_alive(None)
				.with_stateful_mode(true)
				.with_json_response(false),
		);
		let (tx, rx) = tokio::sync::oneshot::channel();
		let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = tcp_listener.local_addr().unwrap();
		tokio::spawn(async move {
			let router = axum::Router::new().nest_service("/mcp", service);
			let _ = axum::serve(tcp_listener, router)
				.with_graceful_shutdown(async { rx.await.unwrap() })
				.await;
		});
		MockServer {
			addr,
			init_counter,
			_cancel: tx,
		}
	}

	#[derive(Clone)]
	struct AutoAcceptElicitation;

	impl ClientHandler for AutoAcceptElicitation {
		fn get_info(&self) -> ClientInfo {
			ClientInfo::new(
				ClientCapabilities::builder().enable_elicitation().build(),
				Implementation::new("elicitation test client".to_string(), "0.0.1".to_string()),
			)
		}

		async fn create_elicitation(
			&self,
			_request: CreateElicitationRequestParams,
			_context: rmcp::service::RequestContext<RoleClient>,
		) -> Result<rmcp::model::CreateElicitationResult, McpError> {
			Ok(rmcp::model::CreateElicitationResult {
				action: ElicitationAction::Accept,
				content: Some(json!({"answer": "federated-ok"})),
				meta: None,
			})
		}
	}

	async fn mcp_elicitation_client(
		s: SocketAddr,
	) -> rmcp::service::RunningService<RoleClient, AutoAcceptElicitation> {
		use rmcp::transport::StreamableHttpClientTransport;
		let transport =
			StreamableHttpClientTransport::<reqwest::Client>::from_uri(format!("http://{s}/mcp"));
		AutoAcceptElicitation.serve(transport).await.unwrap()
	}

	#[derive(Clone)]
	struct ElicitationActionClient {
		action: ElicitationAction,
	}

	impl ClientHandler for ElicitationActionClient {
		fn get_info(&self) -> ClientInfo {
			ClientInfo::new(
				ClientCapabilities::builder().enable_elicitation().build(),
				Implementation::new("elicitation action client".to_string(), "0.0.1".to_string()),
			)
		}

		async fn create_elicitation(
			&self,
			_request: CreateElicitationRequestParams,
			_context: rmcp::service::RequestContext<RoleClient>,
		) -> Result<rmcp::model::CreateElicitationResult, McpError> {
			Ok(rmcp::model::CreateElicitationResult {
				action: self.action.clone(),
				content: None,
				meta: None,
			})
		}
	}

	async fn mcp_elicitation_action_client(
		s: SocketAddr,
		action: ElicitationAction,
	) -> rmcp::service::RunningService<RoleClient, ElicitationActionClient> {
		use rmcp::transport::StreamableHttpClientTransport;
		let transport =
			StreamableHttpClientTransport::<reqwest::Client>::from_uri(format!("http://{s}/mcp"));
		ElicitationActionClient { action }
			.serve(transport)
			.await
			.unwrap()
	}

	#[derive(Clone)]
	struct BlockingElicitServer {
		started: Arc<tokio::sync::Notify>,
		tool_cancelled: Arc<tokio::sync::Notify>,
		call_id: Arc<Mutex<Option<RequestId>>>,
	}

	#[tool_router]
	impl BlockingElicitServer {
		pub fn new(
			started: Arc<tokio::sync::Notify>,
			tool_cancelled: Arc<tokio::sync::Notify>,
			call_id: Arc<Mutex<Option<RequestId>>>,
		) -> Self {
			Self {
				started,
				tool_cancelled,
				call_id,
			}
		}

		#[tool(description = "Block until the downstream client cancels tools/call")]
		async fn block_until_cancel(
			&self,
			ctx: RequestContext<RoleServer>,
		) -> Result<rmcp::model::CallToolResult, McpError> {
			*self.call_id.lock().await = Some(ctx.id.clone());
			self.started.notify_one();
			ctx.ct.cancelled().await;
			self.tool_cancelled.notify_one();
			Ok(rmcp::model::CallToolResult::success(vec![
				rmcp::model::Content::text("teardown"),
			]))
		}
	}

	#[tool_handler]
	impl ServerHandler for BlockingElicitServer {
		fn get_info(&self) -> rmcp::model::ServerInfo {
			rmcp::model::ServerInfo::new(
				rmcp::model::ServerCapabilities::builder()
					.enable_tools()
					.build(),
			)
		}
	}

	async fn mock_blocking_elicitation_server(
		started: Arc<tokio::sync::Notify>,
		tool_cancelled: Arc<tokio::sync::Notify>,
		call_id: Arc<Mutex<Option<RequestId>>>,
	) -> MockServer {
		use rmcp::transport::StreamableHttpServerConfig;
		agent_core::telemetry::testing::setup_test_logging();
		let init_counter = Arc::new(Mutex::new(0_i32));
		let service = StreamableHttpService::new(
			move || {
				Ok(BlockingElicitServer::new(
					started.clone(),
					tool_cancelled.clone(),
					call_id.clone(),
				))
			},
			LocalSessionManager::default().into(),
			StreamableHttpServerConfig::default()
				.with_sse_retry(None)
				.with_sse_keep_alive(None)
				.with_stateful_mode(true)
				.with_json_response(false),
		);
		let (tx, rx) = tokio::sync::oneshot::channel();
		let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = tcp_listener.local_addr().unwrap();
		tokio::spawn(async move {
			let router = axum::Router::new().nest_service("/mcp", service);
			let _ = axum::serve(tcp_listener, router)
				.with_graceful_shutdown(async { rx.await.unwrap() })
				.await;
		});
		MockServer {
			addr,
			init_counter,
			_cancel: tx,
		}
	}

	#[derive(Clone)]
	struct CancelCountingServer {
		cancel_count: Arc<AtomicU32>,
	}

	#[tool_router]
	impl CancelCountingServer {
		pub fn new(cancel_count: Arc<AtomicU32>) -> Self {
			Self { cancel_count }
		}

		#[tool(description = "No-op tool for multiplex cancel routing tests")]
		fn noop(&self) -> Result<rmcp::model::CallToolResult, McpError> {
			Ok(rmcp::model::CallToolResult::success(vec![
				rmcp::model::Content::text("ok"),
			]))
		}
	}

	#[tool_handler]
	impl ServerHandler for CancelCountingServer {
		fn get_info(&self) -> rmcp::model::ServerInfo {
			rmcp::model::ServerInfo::new(
				rmcp::model::ServerCapabilities::builder()
					.enable_tools()
					.build(),
			)
		}

		async fn on_cancelled(
			&self,
			_notification: CancelledNotificationParam,
			_context: NotificationContext<RoleServer>,
		) {
			self.cancel_count.fetch_add(1, Ordering::SeqCst);
		}
	}

	async fn mock_cancel_counting_server(cancel_count: Arc<AtomicU32>) -> MockServer {
		use rmcp::transport::StreamableHttpServerConfig;
		agent_core::telemetry::testing::setup_test_logging();
		let init_counter = Arc::new(Mutex::new(0_i32));
		let service = StreamableHttpService::new(
			{
				let cancel_count = cancel_count.clone();
				move || Ok(CancelCountingServer::new(cancel_count.clone()))
			},
			LocalSessionManager::default().into(),
			StreamableHttpServerConfig::default()
				.with_sse_retry(None)
				.with_sse_keep_alive(None)
				.with_stateful_mode(true)
				.with_json_response(false),
		);
		let (tx, rx) = tokio::sync::oneshot::channel();
		let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = tcp_listener.local_addr().unwrap();
		tokio::spawn(async move {
			let router = axum::Router::new().nest_service("/mcp", service);
			let _ = axum::serve(tcp_listener, router)
				.with_graceful_shutdown(async { rx.await.unwrap() })
				.await;
		});
		MockServer {
			addr,
			init_counter,
			_cancel: tx,
		}
	}

	// rmcp's StreamableHttpClientTransport opens a client-initiated GET /mcp SSE stream
	// after initialize (the same path production MCP clients use). Server-initiated
	// requests such as elicitation/create are delivered on that GET fanout, not via a
	// gateway-owned background upstream GET.
	#[tokio::test]
	async fn federated_elicitation_round_trip_completes() {
		let elicit = mock_elicitation_server().await;
		let plain = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("elicit", elicit.addr, false),
					("plain", plain.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_elicitation_client(io).await;

		let result = client
			.call_tool(
				rmcp::model::CallToolRequestParams::new("elicit_elicit_echo")
					.with_arguments(serde_json::Map::new()),
			)
			.await
			.expect("tools/call should complete after elicitation round-trip");

		assert_eq!(
			result.content[0].raw.as_text().unwrap().text,
			"federated-ok"
		);
	}

	#[tokio::test]
	async fn single_target_elicitation_round_trip_completes() {
		let elicit = mock_elicitation_server().await;
		let (_bind, io) = setup_proxy(&elicit, true, false).await;
		let client = mcp_elicitation_client(io).await;

		let result = client
			.call_tool(
				rmcp::model::CallToolRequestParams::new("elicit_echo")
					.with_arguments(serde_json::Map::new()),
			)
			.await
			.expect("single-target elicitation should complete");

		assert_eq!(
			result.content[0].raw.as_text().unwrap().text,
			"federated-ok"
		);
	}

	#[tokio::test]
	async fn federated_elicitation_decline_round_trip() {
		let elicit = mock_elicitation_server().await;
		let plain = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("elicit", elicit.addr, false),
					("plain", plain.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_elicitation_action_client(io, ElicitationAction::Decline).await;

		let result = client
			.call_tool(
				rmcp::model::CallToolRequestParams::new("elicit_elicit_echo")
					.with_arguments(serde_json::Map::new()),
			)
			.await
			.expect("decline elicitation should complete tools/call");

		assert_eq!(result.content[0].raw.as_text().unwrap().text, "declined");
	}

	#[tokio::test]
	async fn federated_elicitation_cancel_round_trip() {
		let elicit = mock_elicitation_server().await;
		let plain = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("elicit", elicit.addr, false),
					("plain", plain.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_elicitation_action_client(io, ElicitationAction::Cancel).await;

		let result = client
			.call_tool(
				rmcp::model::CallToolRequestParams::new("elicit_elicit_echo")
					.with_arguments(serde_json::Map::new()),
			)
			.await
			.expect("cancel elicitation should complete tools/call");

		assert_eq!(result.content[0].raw.as_text().unwrap().text, "cancelled");
	}

	#[tokio::test]
	async fn federated_tools_call_cancel_routes_to_single_upstream() {
		let started = Arc::new(tokio::sync::Notify::new());
		let tool_cancelled = Arc::new(tokio::sync::Notify::new());
		let call_id = Arc::new(Mutex::new(None));
		let plain_cancel_count = Arc::new(AtomicU32::new(0));

		let blocking = mock_blocking_elicitation_server(
			started.clone(),
			tool_cancelled.clone(),
			call_id.clone(),
		)
		.await;
		let plain = mock_cancel_counting_server(plain_cancel_count.clone()).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("blocking", blocking.addr, false),
					("plain", plain.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_elicitation_client(io).await;

		let call_fut = client.call_tool(
			rmcp::model::CallToolRequestParams::new("blocking_block_until_cancel")
				.with_arguments(serde_json::Map::new()),
		);
		tokio::pin!(call_fut);

		tokio::select! {
			_ = &mut call_fut => panic!("tools/call should not complete before cancel"),
			_ = started.notified() => {},
		}

		let request_id = call_id.lock().await.clone().expect("upstream should record call id");
		client
			.notify_cancelled(CancelledNotificationParam {
				request_id,
				reason: Some("test cancel".to_string()),
			})
			.await
			.expect("client should send notifications/cancelled");

		tokio::select! {
			result = &mut call_fut => {
				match result {
					Err(rmcp::ServiceError::Cancelled { reason }) => {
						assert_eq!(reason.as_deref(), Some("test cancel"));
					},
					other => panic!("expected client-side cancel, got {other:?}"),
				}
			},
			_ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
				panic!("tools/call did not complete after cancel");
			},
		}

		tokio::select! {
			_ = tool_cancelled.notified() => {},
			_ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
				panic!("blocking upstream did not observe cancel teardown");
			},
		}

		assert_eq!(
			plain_cancel_count.load(Ordering::SeqCst),
			0,
			"cancel should not fan out to unrelated upstream"
		);
	}
}

#[cfg(feature = "adobe")]
mod federated_progress_tests {
	use std::sync::Arc;

	use agent_core::strng;
	use rmcp::model::{
		ClientCapabilities, ClientInfo, Implementation, Meta, NumberOrString, ProgressNotificationParam,
		ProgressToken,
	};
	use rmcp::service::NotificationContext;
	use rmcp::transport::streamable_http_server::StreamableHttpService;
	use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
	use rmcp::{
		ClientHandler, ErrorData as McpError, RoleServer, ServerHandler, tool, tool_handler,
		tool_router,
	};
	use rmcp::{RoleClient, ServiceExt};
	use tokio::sync::Mutex;

	use super::*;
	use crate::test_helpers::proxymock::{basic_named_route, simple_bind};

	#[derive(Clone)]
	struct ProgressServer;

	#[tool_router]
	impl ProgressServer {
		pub fn new() -> Self {
			Self
		}

		#[tool(description = "Emit one progress notification then return")]
		async fn progress_echo(
			&self,
			ctx: rmcp::service::RequestContext<RoleServer>,
		) -> Result<rmcp::model::CallToolResult, McpError> {
			if let Some(token) = ctx.meta.get_progress_token() {
				ctx.peer
					.notify_progress(
						ProgressNotificationParam::new(token, 0.5)
							.with_message("gateway-progress-check"),
					)
					.await
					.map_err(|e| McpError::internal_error(e.to_string(), None))?;
			}
			Ok(rmcp::model::CallToolResult::success(vec![
				rmcp::model::Content::text("done"),
			]))
		}
	}

	#[tool_handler]
	impl ServerHandler for ProgressServer {
		fn get_info(&self) -> rmcp::model::ServerInfo {
			rmcp::model::ServerInfo::new(
				rmcp::model::ServerCapabilities::builder()
					.enable_tools()
					.build(),
			)
		}
	}

	async fn mock_progress_server() -> MockServer {
		use rmcp::transport::StreamableHttpServerConfig;
		agent_core::telemetry::testing::setup_test_logging();
		let init_counter = Arc::new(Mutex::new(0_i32));
		let service = StreamableHttpService::new(
			|| Ok(ProgressServer::new()),
			LocalSessionManager::default().into(),
			StreamableHttpServerConfig::default()
				.with_sse_retry(None)
				.with_sse_keep_alive(None)
				.with_stateful_mode(true)
				.with_json_response(false),
		);
		let (tx, rx) = tokio::sync::oneshot::channel();
		let tcp_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let addr = tcp_listener.local_addr().unwrap();
		tokio::spawn(async move {
			let router = axum::Router::new().nest_service("/mcp", service);
			let _ = axum::serve(tcp_listener, router)
				.with_graceful_shutdown(async { rx.await.unwrap() })
				.await;
		});
		MockServer {
			addr,
			init_counter,
			_cancel: tx,
		}
	}

	#[derive(Clone)]
	struct ProgressCapturingClient {
		seen: Arc<Mutex<Vec<ProgressNotificationParam>>>,
	}

	impl ProgressCapturingClient {
		fn new() -> Self {
			Self {
				seen: Arc::new(Mutex::new(Vec::new())),
			}
		}
	}

	impl ClientHandler for ProgressCapturingClient {
		fn get_info(&self) -> ClientInfo {
			ClientInfo::new(
				ClientCapabilities::builder().build(),
				Implementation::new("progress test client".to_string(), "0.0.1".to_string()),
			)
		}

		async fn on_progress(
			&self,
			params: ProgressNotificationParam,
			_context: NotificationContext<RoleClient>,
		) {
			self.seen.lock().await.push(params);
		}
	}

	async fn mcp_progress_client(
		s: SocketAddr,
	) -> (
		rmcp::service::RunningService<RoleClient, ProgressCapturingClient>,
		Arc<Mutex<Vec<ProgressNotificationParam>>>,
	) {
		use rmcp::transport::StreamableHttpClientTransport;
		let client = ProgressCapturingClient::new();
		let seen = client.seen.clone();
		let transport =
			StreamableHttpClientTransport::<reqwest::Client>::from_uri(format!("http://{s}/mcp"));
		let running = client.serve(transport).await.unwrap();
		(running, seen)
	}

	#[tokio::test]
	async fn federated_progress_relayed_unchanged() {
		let progress = mock_progress_server().await;
		let plain = mock_streamable_http_server(true).await;
		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("progress", progress.addr, false),
					("plain", plain.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let (client, seen) = mcp_progress_client(io).await;

		// The rmcp client injects a progressToken tied to the tools/call request id;
		// the upstream echoes that token back. We only assert the payload is relayed
		// unchanged through the federated gateway, not a specific token value.
		let mut params =
			rmcp::model::CallToolRequestParams::new("progress_progress_echo");
		params.meta = Some(Meta::with_progress_token(ProgressToken(NumberOrString::Number(
			42,
		))));

		let result = client
			.call_tool(params)
			.await
			.expect("tools/call should complete with progress on the stream");

		assert_eq!(result.content[0].raw.as_text().unwrap().text, "done");

		let notifications = seen.lock().await;
		assert_eq!(notifications.len(), 1);
		assert!((notifications[0].progress - 0.5).abs() < f64::EPSILON);
		assert_eq!(
			notifications[0].message.as_deref(),
			Some("gateway-progress-check")
		);
	}
}

#[cfg(feature = "adobe")]
mod histogram_tests {
	use agent_core::strng;
	use rmcp::model::CallToolRequestParams;
	use serde_json::json;

	use super::*;
	use crate::test_helpers::adobe_proxymock::setup_with_registry;
	use crate::test_helpers::proxymock::{BIND_KEY, basic_named_route, basic_route, simple_bind};

	const HIST: &str = "agentgateway_mcp_request_duration_seconds";

	/// I1 - single tools/call against a single-target MCP backend.
	#[tokio::test]
	async fn histogram_records_single_tools_call() {
		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;
		let client = mcp_streamable_client(io).await;

		let _ = client
			.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"hi": "world"}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();

		let out = t.scrape_metrics();
		assert!(
			out.contains(&format!("{HIST}_count")),
			"no `{HIST}_count` series emitted; got:\n{out}"
		);
		let any_nonzero_count = out
			.lines()
			.filter(|l| l.starts_with(&format!("{HIST}_count")))
			.any(|l| !l.trim_end().ends_with(" 0"));
		assert!(
			any_nonzero_count,
			"expected at least one nonzero `_count`; got:\n{out}"
		);
		let any_positive_sum = out
			.lines()
			.filter(|l| l.starts_with(&format!("{HIST}_sum")))
			.any(|l| {
				l.rsplit(' ')
					.next()
					.and_then(|n| n.parse::<f64>().ok())
					.map(|n| n > 0.0)
					.unwrap_or(false)
			});
		assert!(any_positive_sum, "expected positive `_sum`; got:\n{out}");
	}

	/// I2 - federated MCP backend; two servers must produce two distinct series.
	/// This is the central story-validating test.
	#[tokio::test]
	async fn histogram_breaks_down_by_server_in_federation() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("server-a", mock_a.addr, false),
					("server-b", mock_b.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		let _ = client
			.call_tool(
				CallToolRequestParams::new("server-a_echo")
					.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();
		let _ = client
			.call_tool(
				CallToolRequestParams::new("server-b_echo")
					.with_arguments(json!({"x": 2}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();

		let out = t.scrape_metrics();

		assert!(
			out
				.lines()
				.any(|l| l.starts_with(&format!("{HIST}_count{{"))
					&& l.contains(r#"server="mcp""#)
					&& l.contains(r#"target="server-a""#)),
			"no server=\"mcp\" target=\"server-a\" _count line; got:\n{out}"
		);
		assert!(
			out
				.lines()
				.any(|l| l.starts_with(&format!("{HIST}_count{{"))
					&& l.contains(r#"server="mcp""#)
					&& l.contains(r#"target="server-b""#)),
			"no server=\"mcp\" target=\"server-b\" _count line; got:\n{out}"
		);
	}

	/// I3 - protocol methods (initialize/tools/list/notifications/initialized) show up
	/// with `target="unknown"` and `server="mcp"` (the backend group name), matching
	/// the existing mcp_requests counter.
	#[tokio::test]
	async fn histogram_observes_non_tools_call_methods() {
		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_multiplex_mcp_backend("mcp", vec![("server-a", mock.addr, false)], true)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		let client = mcp_streamable_client(io).await;

		// `serve(transport)` already triggers `initialize` + `notifications/initialized`.
		let _ = client.list_tools(None).await.unwrap();

		let out = t.scrape_metrics();

		let has_protocol_method = out
			.lines()
			.filter(|l| l.starts_with(&format!("{HIST}_count{{")))
			.any(|l| {
				l.contains(r#"target="unknown""#)
					&& l.contains(r#"server="mcp""#)
					&& (l.contains(r#"method="initialize""#)
						|| l.contains(r#"method="tools/list""#)
						|| l.contains(r#"method="notifications/initialized""#))
			});
		assert!(
			has_protocol_method,
			"no protocol-method series with target=\"unknown\" server=\"mcp\"; got:\n{out}"
		);
	}

	/// I4 - opening an SSE stream without sending any JSON-RPC must NOT emit a histogram
	/// series. Distinguishes the new histogram from the broader HTTP request_duration.
	#[tokio::test]
	async fn histogram_excludes_sse_bootstrap_get() {
		use std::time::Duration;

		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;

		let url = format!("http://{io}/sse");
		let req = ::http::Request::builder()
			.method(::http::Method::GET)
			.uri(url)
			.header("accept", "text/event-stream")
			.body(crate::http::Body::empty())
			.unwrap();
		let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
			.build_http::<crate::http::Body>();
		let _ = tokio::time::timeout(Duration::from_secs(2), client.request(req)).await;
		tokio::time::sleep(Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		let any_nonzero_count = out
			.lines()
			.filter(|l| l.starts_with(&format!("{HIST}_count")))
			.any(|l| !l.trim_end().ends_with(" 0"));
		assert!(
			!any_nonzero_count,
			"SSE-bootstrap GET was observed by the MCP histogram; got:\n{out}"
		);
	}

	/// I5 - when the upstream returns an error, the histogram still observes the call.
	/// Validates analysis §5.3.
	#[tokio::test]
	async fn histogram_observes_upstream_error_latency() {
		let dead: std::net::SocketAddr = "127.0.0.1:1".parse().unwrap();
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(dead, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(dead));
		let io = t.serve_real_listener(BIND_KEY).await;

		// mcp_streamable_client panics internally when the upstream is refused at
		// `initialize` time; spawn it as a separate task so the JoinError is swallowed
		// without failing the test, while the gateway still logs the request and fires
		// the histogram observation via DropOnLog.
		let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
			let handle = tokio::task::spawn(async move {
				let client = mcp_streamable_client(io).await;
				let _ = client
					.call_tool(
						CallToolRequestParams::new("echo")
							.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
					)
					.await;
			});
			let _ = handle.await;
		})
		.await;

		tokio::time::sleep(std::time::Duration::from_millis(100)).await;
		let out = t.scrape_metrics();

		let any_nonzero_count = out
			.lines()
			.filter(|l| l.starts_with(&format!("{HIST}_count")))
			.any(|l| !l.trim_end().ends_with(" 0"));
		assert!(
			any_nonzero_count,
			"upstream-error call was not observed by the histogram; got:\n{out}"
		);
	}

	/// I6 - for the same MCPCall label set, `mcp_request_duration_seconds_count` must
	/// match `mcp_requests_total`. Regression guard against the two call sites diverging.
	#[tokio::test]
	async fn histogram_count_matches_counter_for_same_label_set() {
		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;
		let client = mcp_streamable_client(io).await;

		for _ in 0..5 {
			let _ = client
				.call_tool(
					CallToolRequestParams::new("echo")
						.with_arguments(json!({"hi": "world"}).as_object().cloned().unwrap()),
				)
				.await
				.unwrap();
		}

		let out = t.scrape_metrics();

		fn sum_lines(s: &str, prefix: &str) -> f64 {
			s.lines()
				.filter(|l| l.starts_with(prefix))
				.filter_map(|l| l.rsplit(' ').next())
				.filter_map(|n| n.parse::<f64>().ok())
				.sum()
		}

		let hist_count = sum_lines(&out, &format!("{HIST}_count"));
		let counter_total = sum_lines(&out, "agentgateway_mcp_requests_total");
		assert!(
			(hist_count - counter_total).abs() < f64::EPSILON,
			"histogram _count ({hist_count}) != counter total ({counter_total}); got:\n{out}"
		);
		assert!(
			hist_count >= 5.0,
			"expected at least 5 observations, got {hist_count}"
		);
	}

	/// I7 - one fast call + one slow call; the `le="0.01"` bucket holds 1, `le="0.5"` holds 2.
	#[tokio::test]
	async fn histogram_bucket_distribution_matches_observation() {
		use std::time::Duration;

		let fast = mock_streamable_http_server(true).await;
		let slow = mock_streamable_http_server_with_delay(true, Some(Duration::from_millis(200))).await;

		let t_fast = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(fast.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(fast.addr));
		let io_fast = t_fast.serve_real_listener(BIND_KEY).await;
		let c_fast = mcp_streamable_client(io_fast).await;
		let _ = c_fast
			.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();

		let t_slow = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(slow.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(slow.addr));
		let io_slow = t_slow.serve_real_listener(BIND_KEY).await;
		let c_slow = mcp_streamable_client(io_slow).await;
		let _ = c_slow
			.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"x": 2}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();

		fn bucket(out: &str, le: &str) -> f64 {
			out
				.lines()
				.find(|l| {
					l.starts_with(&format!("{HIST}_bucket"))
						&& l.contains(&format!(r#"le="{le}""#))
						&& l.contains(r#"method="tools/call""#)
				})
				.and_then(|l| l.rsplit(' ').next())
				.and_then(|n| n.parse::<f64>().ok())
				.unwrap_or(0.0)
		}

		let out_fast = t_fast.scrape_metrics();
		let out_slow = t_slow.scrape_metrics();
		// Use le=0.05 (50ms) as the fast-call boundary; on slow CI machines a
		// "fast" call can take 10–30ms, so the tighter le=0.01 (10ms) is
		// unreliable.  The 200ms-delayed slow call is safely outside le=0.05.
		assert!(
			bucket(&out_fast, "0.05") >= 1.0,
			"fast tools/call did not land in le=0.05; got:\n{out_fast}"
		);
		assert!(
			bucket(&out_slow, "0.05") < 1.0,
			"slow tools/call (200ms) unexpectedly landed in le=0.05; got:\n{out_slow}"
		);
		assert!(
			bucket(&out_slow, "0.5") >= 1.0,
			"slow tools/call did not land in le=0.5; got:\n{out_slow}"
		);
	}

	/// I8 - the histogram carries the flattened RouteIdentifier (bind/gateway/listener/route/route_rule).
	#[tokio::test]
	async fn histogram_carries_full_route_identifier_labels() {
		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;
		let client = mcp_streamable_client(io).await;

		let _ = client
			.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();

		let out = t.scrape_metrics();

		let line = out
			.lines()
			.find(|l| l.starts_with(&format!("{HIST}_count{{")))
			.unwrap_or_else(|| panic!("no _count series; got:\n{out}"));

		for key in ["bind=", "gateway=", "listener=", "route=", "route_rule="] {
			assert!(
				line.contains(key),
				"label `{key}` missing from histogram series `{line}`"
			);
		}
	}

	/// I9 - a request that finalises with no MCP context attached must produce
	/// neither `mcp_requests_total` nor `mcp_request_duration_seconds`.
	#[tokio::test]
	async fn histogram_silent_when_mcp_context_absent() {
		use std::time::Duration;

		let t = setup_with_registry("{}").unwrap().with_bind(simple_bind());
		let io = t.serve_real_listener(BIND_KEY).await;

		let req = ::http::Request::builder()
			.method(::http::Method::GET)
			.uri(format!("http://{io}/no-such-path"))
			.body(crate::http::Body::empty())
			.unwrap();
		let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
			.build_http::<crate::http::Body>();
		let _ = tokio::time::timeout(Duration::from_secs(2), client.request(req)).await;
		tokio::time::sleep(Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		assert!(
			!out.contains("agentgateway_mcp_requests_total"),
			"unexpected `mcp_requests_total` line; got:\n{out}"
		);
		assert!(
			!out.contains("agentgateway_mcp_request_duration_seconds_count"),
			"unexpected histogram `_count` line; got:\n{out}"
		);
	}
}

#[cfg(feature = "adobe")]
mod upstream_error_tests {
	use std::sync::Arc;

	use agent_core::strng;
	use rmcp::model::CallToolRequestParams;
	use serde_json::json;

	use super::*;
	use crate::http::authorization::{PolicySet, RuleSet};
	use crate::mcp::McpAuthorization;
	use crate::test_helpers::adobe_proxymock::setup_with_registry;
	use crate::test_helpers::proxymock::{BIND_KEY, basic_named_route, basic_route, simple_bind};
	use crate::types::agent::BackendTrafficPolicy;

	const CTR: &str = "agentgateway_mcp_upstream_errors_total";

	/// True if some `mcp_upstream_errors_total` series matches every `needle`
	/// label fragment and has a nonzero value.
	fn has_nonzero_series(out: &str, needles: &[&str]) -> bool {
		out.lines().any(|l| {
			l.starts_with(CTR) && needles.iter().all(|n| l.contains(n)) && !l.trim_end().ends_with(" 0")
		})
	}

	/// U2 — single-target `tools/call` whose upstream returns HTTP 502 increments
	/// the counter with `error_type="http_5xx"`. Also asserts §10 parity: the
	/// existing `mcp_requests_total` still counts the failed call.
	#[tokio::test]
	async fn counts_http_5xx_on_tools_call() {
		let mock =
			mock_streamable_http_server_failing_tools_call(::http::StatusCode::BAD_GATEWAY).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;

		let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
			let handle = tokio::task::spawn(async move {
				let client = mcp_streamable_client(io).await;
				let _ = client
					.call_tool(
						CallToolRequestParams::new("echo")
							.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
					)
					.await;
			});
			let _ = handle.await;
		})
		.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		assert!(
			has_nonzero_series(
				&out,
				&[r#"error_type="http_5xx""#, r#"method="tools/call""#]
			),
			"expected http_5xx tools/call series; got:\n{out}"
		);
		// §10 parity: the existing counter also records the failed call.
		assert!(
			out
				.lines()
				.any(|l| l.starts_with("agentgateway_mcp_requests_total")
					&& l.contains(r#"method="tools/call""#)),
			"expected mcp_requests_total tools/call series; got:\n{out}"
		);
	}

	/// U3 — upstream HTTP 404 maps to `error_type="http_4xx"`.
	#[tokio::test]
	async fn counts_http_4xx_on_tools_call() {
		let mock = mock_streamable_http_server_failing_tools_call(::http::StatusCode::NOT_FOUND).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;

		let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
			let handle = tokio::task::spawn(async move {
				let client = mcp_streamable_client(io).await;
				let _ = client
					.call_tool(
						CallToolRequestParams::new("echo")
							.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
					)
					.await;
			});
			let _ = handle.await;
		})
		.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		assert!(
			has_nonzero_series(
				&out,
				&[r#"error_type="http_4xx""#, r#"method="tools/call""#]
			),
			"expected http_4xx tools/call series; got:\n{out}"
		);
	}

	/// U4 — label correctness in a federated setup: `server` is the AIBackend name
	/// (`mcp`), `target` is the individual MCP server (`server-a`), `method="tools/call"`,
	/// and the flattened RouteIdentifier labels are present.
	#[tokio::test]
	async fn labels_carry_individual_server_and_route() {
		let mock =
			mock_streamable_http_server_failing_tools_call(::http::StatusCode::BAD_GATEWAY).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_multiplex_mcp_backend("mcp", vec![("server-a", mock.addr, false)], true)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;

		let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
			let handle = tokio::task::spawn(async move {
				let client = mcp_streamable_client(io).await;
				let _ = client
					.call_tool(
						CallToolRequestParams::new("server-a_echo")
							.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
					)
					.await;
			});
			let _ = handle.await;
		})
		.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		let line = out
			.lines()
			.find(|l| {
				l.starts_with(CTR)
					&& l.contains(r#"server="mcp""#)
					&& l.contains(r#"target="server-a""#)
			})
			.unwrap_or_else(|| {
				panic!("no server=\"mcp\" target=\"server-a\" upstream-error series; got:\n{out}")
			});
		assert!(
			line.contains(r#"method="tools/call""#),
			"wrong method on `{line}`"
		);
		assert!(
			line.contains(r#"error_type="http_5xx""#),
			"wrong error_type on `{line}`"
		);
		for key in ["bind=", "gateway=", "listener=", "route=", "route_rule="] {
			assert!(line.contains(key), "label `{key}` missing from `{line}`");
		}
	}

	/// U1 — a NON-HTTP transport failure on a dispatched `tools/call` increments the
	/// counter. The upstream completes `initialize` (establishing the session), then is
	/// killed; the subsequent `tools/call` reaches `send_single_map_response` →
	/// `generic_stream`, which fails with a transport `UpstreamError` (connection refused
	/// / broken pipe ⇒ `proxy`/`recv`/`send`) and is counted with `method="tools/call"`.
	/// The robust assertion is "some nonzero series with method=tools/call" — the exact
	/// transport variant is pinned by the Task 2 classifier unit test.
	///
	/// NOTE: an upstream unreachable *from the start* fails during the `initialize`
	/// fan-out (`session.rs` `send_fanout`), which is deliberately NOT instrumented
	/// (spec §2 non-goal: fan-out failures are not counted) — so that case correctly does
	/// NOT increment the counter. Only post-init single-target dispatch errors are counted.
	#[tokio::test]
	async fn counts_non_http_transport_failure() {
		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;
		// initialize succeeds against the healthy upstream
		let client = mcp_streamable_client(io).await;

		// Kill the upstream; the next dispatched tools/call fails at generic_stream.
		drop(mock);
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;

		let _ = tokio::time::timeout(
			std::time::Duration::from_secs(5),
			client.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
			),
		)
		.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		assert!(
			has_nonzero_series(&out, &[r#"method="tools/call""#]),
			"post-init upstream death did not increment mcp_upstream_errors for tools/call; got:\n{out}"
		);
	}

	/// REPRO (ETHOS-110815 / verify-mcp-label-schema.sh MV-2): a FEDERATED backend
	/// (two multiplex targets) establishes a stateful session against BOTH live
	/// upstreams, then ONE target is killed and a `tools/call` is dispatched to the
	/// DEAD target. This is exactly `counts_non_http_transport_failure` but
	/// multi-target — the combination the live MV-2 check exercises, where it
	/// currently produces `target="unknown"` and NO `mcp_upstream_errors_total`.
	///
	/// Asserts the two observable MV-2 failures as one in-process reproduction:
	///   1. the duration histogram records `target="server-a"` (NOT `"unknown"`) —
	///      i.e. `set_tool` took effect for the failed call;
	///   2. `mcp_upstream_errors_total` is emitted with `server="mcp"`,
	///      `target="server-a"`, `method="tools/call"`.
	#[tokio::test]
	async fn federation_dead_target_counts_and_labels() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("server-a", mock_a.addr, false),
					("server-b", mock_b.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;
		// initialize + notifications/initialized fan out to BOTH live upstreams here,
		// establishing the stateful session with both targets in `by_name`.
		let client = mcp_streamable_client(io).await;

		// Kill server-a only; server-b stays alive (matches MV-2: kill mcp-server,
		// keep mcp-server-airbnb).
		drop(mock_a);
		tokio::time::sleep(std::time::Duration::from_millis(200)).await;

		// Dispatch a single-target tools/call to the DEAD target.
		let _ = tokio::time::timeout(
			std::time::Duration::from_secs(5),
			client.call_tool(
				CallToolRequestParams::new("server-a_echo")
					.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
			),
		)
		.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();

		// (1) histogram must record target="server-a", not "unknown" — proves
		// set_tool took effect on the finalized cell for the failed call.
		assert!(
			out.lines().any(|l| l
				.starts_with("agentgateway_mcp_request_duration_seconds_count{")
				&& l.contains(r#"method="tools/call""#)
				&& l.contains(r#"target="server-a""#)),
			"duration histogram missing target=\"server-a\" for failed tools/call \
			 (live regression shows target=\"unknown\"); got:\n{out}"
		);

		// (2) upstream errors counter must be emitted with backend + target labels.
		assert!(
			has_nonzero_series(
				&out,
				&[
					r#"server="mcp""#,
					r#"target="server-a""#,
					r#"method="tools/call""#,
				],
			),
			"federation dead-target tools/call did not emit mcp_upstream_errors_total \
			 with server=mcp target=server-a method=tools/call; got:\n{out}"
		);
	}

	/// F1 — FailClosed fanout: when one target is unreachable the error surfaced
	/// to the MCP client must include the target name so operators can identify
	/// the failing backend from the access log alone.
	#[cfg(feature = "adobe")]
	#[tokio::test]
	async fn fail_closed_fanout_error_includes_target_name() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let b_addr = mock_b.addr;
		drop(mock_b); // connection refused during fanout initialize

		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("server-a", mock_a.addr, false),
					("server-b", b_addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;

		let err = try_mcp_streamable_client(io)
			.await
			.expect_err("FailClosed initialize should fail when server-b is dead");
		let err_msg = err.to_string();
		assert!(
			err_msg.contains("server-b"),
			"FailClosed fanout error should name the failing target; got: {err_msg}"
		);
	}

	/// F2 — FailClosed fanout: when one target returns a real HTTP error
	/// status during `initialize`, that status must be forwarded to the MCP
	/// client rather than being converted to a generic 500.
	#[cfg(feature = "adobe")]
	#[tokio::test]
	async fn fail_closed_fanout_preserves_upstream_status_code() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server_failing_initialize(
			::http::StatusCode::SERVICE_UNAVAILABLE,
		)
		.await;

		let t = setup_proxy_test("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("server-a", mock_a.addr, false),
					("server-b", mock_b.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;

		let err = try_mcp_streamable_client(io)
			.await
			.expect_err("FailClosed initialize should fail when server-b returns 503");
		let err_msg = err.to_string();
		// The discriminator is the *transport* status, not the substring "503":
		// without the passthrough fix the 503 is swallowed into a JSON-RPC
		// -32603 internal error (transport HTTP 500) whose text still mentions
		// "503". With the fix the client sees a real transport-level HTTP 503.
		assert!(
			err_msg.contains("HTTP 503") && !err_msg.contains("-32603"),
			"FailClosed fanout should surface a transport-level HTTP 503, not a \
			 generic JSON-RPC 500; got: {err_msg}"
		);
	}

	/// U5 — an RBAC deny-all policy rejects the `tools/call` (`UpstreamError::Authorization`)
	/// before `generic_stream`, so the counter must NOT be emitted.
	#[tokio::test]
	async fn rbac_denial_does_not_count() {
		let mock = mock_streamable_http_server(true).await;
		// Deny-all: no allow rules, one always-true deny rule (mirrors the existing
		// `authorization_denied_returns_unknown_tool_error` test). `PolicySet::new`
		// takes three rule vecs.
		let deny_all_policy = McpAuthorization::new(RuleSet::new(PolicySet::new(
			vec![], // allow
			vec![Arc::new(
				crate::cel::Expression::new_strict("true").unwrap(),
			)], // deny all
			vec![],
		)));
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend_policies(
				mock.addr,
				true,
				false,
				vec![BackendTrafficPolicy::McpAuthorization(deny_all_policy)],
			)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;
		let client = mcp_streamable_client(io).await;

		let _ = client
			.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
			)
			.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		assert!(
			!out.contains(CTR),
			"RBAC-denied call must not emit mcp_upstream_errors; got:\n{out}"
		);
	}

	/// U6 — an unknown-service `tools/call` fails with `UpstreamError::InvalidRequest`
	/// ("unknown service …"), which `send_single_map_response` returns **before** it ever
	/// calls `generic_stream` (handler.rs `let Ok(us) = self.upstreams.get(service_name) else
	/// { return Err(InvalidRequest(...)) }`). The stash runs only on the `generic_stream`
	/// `Err` branch, so it never executes and the counter must stay absent. This pins the
	/// instrumentation *placement* — a guarantee the Task 2 classifier unit test
	/// (`InvalidRequest → None`) cannot give on its own.
	///
	/// **Two** healthy upstreams are required: a single-target multiplex sets
	/// `default_target_name = Some(..)` (`upstream/mod.rs`, `targets.len() == 1` branch) and
	/// routes the whole tool name to that one target — never producing an unknown-service
	/// error. With two targets, `default_target_name` is `None`, so the `server-zzz_` prefix
	/// is parsed and resolved against the upstream set, where it is unknown. (Same pattern as
	/// the existing `stream_to_multiplex` test.)
	#[tokio::test]
	async fn unknown_service_does_not_count() {
		let mock_a = mock_streamable_http_server(true).await;
		let mock_b = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_multiplex_mcp_backend(
				"mcp",
				vec![
					("server-a", mock_a.addr, false),
					("server-b", mock_b.addr, false),
				],
				true,
			)
			.with_bind(simple_bind())
			.with_route(basic_named_route(strng::new("/mcp")));
		let io = t.serve_real_listener(strng::new("bind")).await;

		let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
			let handle = tokio::task::spawn(async move {
				let client = mcp_streamable_client(io).await;
				// "server-zzz" is not a registered upstream ⇒ InvalidRequest("unknown service …").
				let _ = client
					.call_tool(
						CallToolRequestParams::new("server-zzz_echo")
							.with_arguments(json!({"x": 1}).as_object().cloned().unwrap()),
					)
					.await;
			});
			let _ = handle.await;
		})
		.await;
		tokio::time::sleep(std::time::Duration::from_millis(100)).await;

		let out = t.scrape_metrics();
		assert!(
			!out.contains(CTR),
			"unknown-service InvalidRequest must not emit mcp_upstream_errors; got:\n{out}"
		);
	}

	/// U7 — a happy-path 200 OK `tools/call` must NOT emit the counter.
	#[tokio::test]
	async fn happy_path_does_not_count() {
		let mock = mock_streamable_http_server(true).await;
		let t = setup_with_registry("{}")
			.unwrap()
			.with_mcp_backend(mock.addr, true, false)
			.with_bind(simple_bind())
			.with_route(basic_route(mock.addr));
		let io = t.serve_real_listener(BIND_KEY).await;
		let client = mcp_streamable_client(io).await;

		let _ = client
			.call_tool(
				CallToolRequestParams::new("echo")
					.with_arguments(json!({"hi": "world"}).as_object().cloned().unwrap()),
			)
			.await
			.unwrap();

		let out = t.scrape_metrics();
		assert!(
			!out.contains(CTR),
			"successful call must not emit mcp_upstream_errors; got:\n{out}"
		);
	}
}
