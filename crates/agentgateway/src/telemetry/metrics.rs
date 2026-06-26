use std::fmt::Debug;

use agent_core::metrics::{CustomField, DefaultedUnknown, EncodeArc, EncodeDebug, EncodeDisplay};
use agent_core::strng::RichStrng;
use agent_core::version;
use frozen_collections::FzHashSet;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram as PromHistogram;
use prometheus_client::metrics::info::Info;
use prometheus_client::registry::{Metric, Registry, Unit};
use tracing::{debug, trace};

use crate::mcp::MCPOperation;
use crate::proxy::ProxyResponseReason;
use crate::types::agent::TransportProtocol;

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct RouteIdentifier {
	pub bind: DefaultedUnknown<RichStrng>,
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub route: DefaultedUnknown<RichStrng>,
	pub route_rule: DefaultedUnknown<RichStrng>,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum GuardrailPhase {
	#[default]
	Request,
	Response,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum GuardrailAction {
	#[default]
	Allow,
	Mask,
	Reject,
	FailOpen,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GuardrailLabels {
	pub phase: GuardrailPhase,
	pub action: GuardrailAction,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct MinimalHTTPLabels {
	pub backend: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

impl From<HTTPLabels> for MinimalHTTPLabels {
	fn from(value: HTTPLabels) -> Self {
		Self {
			backend: value.backend,
			route: value.route,
			custom: value.custom,
		}
	}
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct HTTPLabels {
	pub backend: DefaultedUnknown<RichStrng>,
	pub protocol: DefaultedUnknown<EncodeDebug<crate::cel::BackendProtocol>>,

	pub method: DefaultedUnknown<EncodeDisplay<http::Method>>,
	pub status: DefaultedUnknown<EncodeDisplay<u16>>,
	pub reason: DefaultedUnknown<EncodeDisplay<ProxyResponseReason>>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GenAILabels {
	pub gen_ai_operation_name: DefaultedUnknown<RichStrng>,
	pub gen_ai_system: DefaultedUnknown<RichStrng>,
	pub gen_ai_request_model: DefaultedUnknown<RichStrng>,
	pub gen_ai_response_model: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct GenAILabelsTokenUsage {
	pub gen_ai_token_type: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub common: EncodeArc<GenAILabels>,
}

#[derive(Clone, Hash, Default, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct CostCatalogLookupLabels {
	pub status: crate::llm::cost::CostLookupStatus,

	#[prometheus(flatten)]
	pub common: EncodeArc<GenAILabels>,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct MCPCall {
	pub method: DefaultedUnknown<RichStrng>,

	pub resource_type: DefaultedUnknown<MCPOperation>,
	pub server: DefaultedUnknown<RichStrng>,
	pub resource: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[cfg(feature = "adobe")]
#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct MCPCallAdobe {
	pub server: DefaultedUnknown<RichStrng>,
	pub target: DefaultedUnknown<RichStrng>,
	pub method: DefaultedUnknown<RichStrng>,
	pub resource_type: DefaultedUnknown<MCPOperation>,
	pub resource: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,

	#[prometheus(flatten)]
	pub custom: CustomField,
}

#[cfg(feature = "adobe")]
#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct MCPUpstreamError {
	pub server: DefaultedUnknown<RichStrng>,
	pub target: DefaultedUnknown<RichStrng>,
	pub method: DefaultedUnknown<RichStrng>,
	pub error_type: DefaultedUnknown<RichStrng>,

	#[prometheus(flatten)]
	pub route: RouteIdentifier,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct TCPLabels {
	pub bind: DefaultedUnknown<RichStrng>,
	pub gateway: DefaultedUnknown<RichStrng>,
	pub listener: DefaultedUnknown<RichStrng>,
	pub protocol: TransportProtocol,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct ConnectLabels {
	pub transport: DefaultedUnknown<RichStrng>,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum OutboundCallKind {
	/// The primary backend call
	#[default]
	Primary,
	/// A callout as part of a policy execution
	Policy,
	/// A mirrored call
	Mirror,
}

#[derive(
	Copy, Clone, Hash, Debug, PartialEq, Eq, prometheus_client::encoding::EncodeLabelValue, Default,
)]
pub enum OutboundCallSubtype {
	// Primary
	#[default]
	Http,
	Llm,
	Mcp,

	// Policy
	ExtAuthz,
	ExtProc,
	Guardrail,
	RateLimit,
	Oidc,
}

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct OutboundCallLabels {
	pub kind: OutboundCallKind,
	pub subtype: OutboundCallSubtype,
}

type Counter = Family<HTTPLabels, counter::Counter>;
type Histogram<T> = Family<T, prometheus_client::metrics::histogram::Histogram>;
type TCPCounter = Family<TCPLabels, counter::Counter>;

#[derive(Clone, Hash, Debug, PartialEq, Eq, EncodeLabelSet)]
pub struct BuildLabel {
	tag: &'static str,
}

#[derive(Debug)]
pub struct Metrics {
	pub requests: Counter,
	pub request_duration: Histogram<HTTPLabels>,
	pub request_processing_duration: Histogram<MinimalHTTPLabels>,
	pub response_processing_duration: Histogram<MinimalHTTPLabels>,
	pub response_bytes: Family<HTTPLabels, counter::Counter>,

	pub mcp_requests: Family<MCPCall, counter::Counter>,

	pub gen_ai_token_usage: Histogram<GenAILabelsTokenUsage>,
	pub gen_ai_cost: Family<GenAILabels, counter::Counter<f64>>,
	pub gen_ai_request_duration: Histogram<GenAILabels>,
	pub gen_ai_time_per_output_token: Histogram<GenAILabels>,
	pub gen_ai_time_to_first_token: Histogram<GenAILabels>,

	pub tls_handshake_duration: Histogram<TCPLabels>,

	pub downstream_connection: TCPCounter,
	pub tcp_downstream_rx_bytes: Family<TCPLabels, counter::Counter>,
	pub tcp_downstream_tx_bytes: Family<TCPLabels, counter::Counter>,

	pub upstream_connect_duration: Histogram<ConnectLabels>,
	pub upstream_call_duration: Histogram<OutboundCallLabels>,

	// metrics for guardrail checks (allow/mask/reject) for request/response
	pub guardrail_checks: Family<GuardrailLabels, counter::Counter>,

	pub cost_catalog_lookups: Family<CostCatalogLookupLabels, counter::Counter>,

	// metrics for request retries
	pub retries: Counter,

	#[cfg(feature = "adobe")]
	pub mcp_request_duration: Histogram<MCPCallAdobe>,

	#[cfg(feature = "adobe")]
	pub mcp_upstream_errors: Family<MCPUpstreamError, counter::Counter>,
}

// FilteredRegistry is a wrapper around Registry that allows to filter out certain metrics.
// Note: this currently only excludes them from the registry, but the underlying metrics are still
// stored. This can result in memory cost, etc to store the labels.
// A more robust future solution would be to have a sort of `Disabled` metric that does not store;
// note that even still, we would be computing the labels (and then dropping them), but in many cases
// the same labels are shared by many metrics, and are cheap to construct, so likely not a major concern.
struct FilteredRegistry<'a> {
	registry: &'a mut Registry,
	removes: FzHashSet<String>,
}

impl<'a> FilteredRegistry<'a> {
	fn should_skip(&self, name: &str, unit: Option<&Unit>) -> bool {
		let mut names = vec![
			name.to_string(),
			format!("{}_total", name),
			format!("{}_{}_total", agent_core::metrics::PREFIX, name),
			format!("{}_{}", agent_core::metrics::PREFIX, name),
		];
		if let Some(unit) = unit {
			names.extend_from_slice(&[
				format!("{}_{}", name, unit.as_str()),
				format!("{}_{}_total", name, unit.as_str()),
				format!(
					"{}_{}_{}_total",
					agent_core::metrics::PREFIX,
					name,
					unit.as_str()
				),
				format!("{}_{}_{}", agent_core::metrics::PREFIX, name, unit.as_str()),
			])
		}

		for n in names.into_iter() {
			let exclude = self.removes.contains(&n);
			trace!(name = n, exclude, "check metric for exclusion");
			if exclude {
				return true;
			}
		}
		false
	}
	fn register(&mut self, name: impl Into<String>, help: impl Into<String>, metric: impl Metric) {
		let name = name.into();
		if self.should_skip(&name, None) {
			debug!("skip register metric: {}", name);
			return;
		}
		self.registry.register(name, help, metric);
	}

	fn register_with_unit(
		&mut self,
		name: impl Into<String>,
		help: impl Into<String>,
		unit: Unit,
		metric: impl Metric,
	) {
		let name = name.into();
		if self.should_skip(&name, Some(&unit)) {
			debug!("skip register metric: {}_{}", name, unit.as_str());
			return;
		}
		self.registry.register_with_unit(name, help, unit, metric);
	}
}

impl Metrics {
	pub fn new(registry: &mut Registry, removes: FzHashSet<String>) -> Self {
		let mut registry = FilteredRegistry { registry, removes };
		registry.register(
			"build",
			"Agentgateway build information",
			Info::new(BuildLabel {
				tag: version::BuildInfo::new().version,
			}),
		);

		let gen_ai_token_usage = Family::<GenAILabelsTokenUsage, _>::new_with_constructor(move || {
			PromHistogram::new(TOKEN_USAGE_BUCKET)
		});
		registry.register(
			"gen_ai_client_token_usage",
			"Number of tokens used per request",
			gen_ai_token_usage.clone(),
		);

		let gen_ai_cost = Family::<GenAILabels, _>::default();
		registry.register_with_unit(
			"gen_ai_client_cost",
			"Cumulative USD cost of generative AI requests",
			Unit::Other("usd".to_string()),
			gen_ai_cost.clone(),
		);

		// TODO: add error attribute if it ends with an error
		let gen_ai_request_duration = Family::<GenAILabels, _>::new_with_constructor(move || {
			PromHistogram::new(REQUEST_DURATION_BUCKET)
		});
		registry.register(
			"gen_ai_server_request_duration",
			"Duration of generative AI request",
			gen_ai_request_duration.clone(),
		);

		let gen_ai_time_per_output_token = Family::<GenAILabels, _>::new_with_constructor(move || {
			PromHistogram::new(OUTPUT_TOKEN_BUCKET)
		});
		registry.register(
			"gen_ai_server_time_per_output_token",
			"Time to generate each output token for a given request",
			gen_ai_time_per_output_token.clone(),
		);

		let gen_ai_time_to_first_token = Family::<GenAILabels, _>::new_with_constructor(move || {
			PromHistogram::new(FIRST_TOKEN_BUCKET)
		});
		registry.register(
			"gen_ai_server_time_to_first_token",
			"Time to generate the first token for a given request",
			gen_ai_time_to_first_token.clone(),
		);

		#[cfg(feature = "adobe")]
		let mcp_request_duration = {
			let m = Family::<MCPCallAdobe, _>::new_with_constructor(move || {
				PromHistogram::new(HTTP_REQUEST_DURATION_BUCKET)
			});
			registry.register_with_unit(
				"mcp_request_duration",
				"Duration of MCP calls (seconds)",
				Unit::Seconds,
				m.clone(),
			);
			m
		};

		Metrics {
			requests: build(
				&mut registry,
				"requests",
				"The total number of HTTP requests sent",
			),
			guardrail_checks: {
				let m = Family::<GuardrailLabels, _>::default();
				registry.register(
					"guardrail_checks",
					"Total number of guardrail checks",
					m.clone(),
				);
				m
			},
			cost_catalog_lookups: {
				let m = Family::<CostCatalogLookupLabels, _>::default();
				registry.register(
					"cost_catalog_lookups",
					"Total number of model cost catalog lookups by resolution status",
					m.clone(),
				);
				m
			},
			downstream_connection: build(
				&mut registry,
				"downstream_connections",
				"The total number of downstream connections established",
			),

			mcp_requests: build(
				&mut registry,
				"mcp_requests",
				"Total number of MCP tool calls",
			),

			#[cfg(feature = "adobe")]
			mcp_request_duration,

			#[cfg(feature = "adobe")]
			mcp_upstream_errors: build(
				&mut registry,
				"mcp_upstream_errors",
				"Total number of MCP upstream/transport errors",
			),

			gen_ai_token_usage,
			gen_ai_cost,
			gen_ai_request_duration,
			gen_ai_time_per_output_token,
			gen_ai_time_to_first_token,

			response_bytes: {
				let m = Family::<HTTPLabels, _>::default();
				registry.register_with_unit(
					"response",
					"Total HTTP response bytes received",
					Unit::Bytes,
					m.clone(),
				);
				m
			},
			request_duration: {
				let m = Family::<HTTPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(HTTP_REQUEST_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"request_duration",
					"Duration of HTTP requests (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			request_processing_duration: {
				let m = Family::<MinimalHTTPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(PROCESSING_DURATION_BUCKETS)
				});
				registry.register_with_unit(
					"request_processing",
					"Duration from receiving an HTTP request to sending the primary outbound call (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			response_processing_duration: {
				let m = Family::<MinimalHTTPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(PROCESSING_DURATION_BUCKETS)
				});
				registry.register_with_unit(
					"response_processing",
					"Duration from receiving the primary outbound response to sending the HTTP response (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			tcp_downstream_rx_bytes: {
				let m = Family::<TCPLabels, _>::default();
				registry.register_with_unit(
					"downstream_received",
					"Total TCP bytes received per connection labels",
					Unit::Bytes,
					m.clone(),
				);
				m
			},
			tcp_downstream_tx_bytes: {
				let m = Family::<TCPLabels, _>::default();
				registry.register_with_unit(
					"downstream_sent",
					"Total TCP bytes transmitted per connection labels",
					Unit::Bytes,
					m.clone(),
				);
				m
			},
			upstream_connect_duration: {
				let m = Family::<ConnectLabels, _>::new_with_constructor(move || {
					PromHistogram::new(CONNECT_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"upstream_connect_duration",
					"Duration to establish upstream connection (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			upstream_call_duration: {
				let m = Family::<OutboundCallLabels, _>::new_with_constructor(move || {
					PromHistogram::new(HTTP_REQUEST_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"upstream_call_duration",
					"Duration of outbound calls made by agentgateway (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			tls_handshake_duration: {
				let m = Family::<TCPLabels, _>::new_with_constructor(move || {
					PromHistogram::new(CONNECT_DURATION_BUCKET)
				});
				registry.register_with_unit(
					"tls_handshake_duration",
					"Duration to complete inbound TLS/HTTPS handshake (seconds)",
					Unit::Seconds,
					m.clone(),
				);
				m
			},
			retries: build(
				&mut registry,
				"retries",
				"The total number of request retries",
			),
		}
	}
}

fn build<'a, T: Clone + std::hash::Hash + Eq + Send + Sync + Debug + EncodeLabelSet + 'static>(
	registry: &mut FilteredRegistry<'a>,
	name: &str,
	help: &str,
) -> Family<T, counter::Counter> {
	let m = Family::<T, _>::default();
	registry.register(name, help, m.clone());
	m
}

// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiclienttokenusage
const TOKEN_USAGE_BUCKET: [f64; 14] = [
	1., 4., 16., 64., 256., 1024., 4096., 16384., 65536., 262144., 1048576., 4194304., 16777216.,
	67108864.,
];
// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiserverrequestduration
const REQUEST_DURATION_BUCKET: [f64; 14] = [
	0.01, 0.02, 0.04, 0.08, 0.16, 0.32, 0.64, 1.28, 2.56, 5.12, 10.24, 20.48, 40.96, 81.92,
];
// Finer-grained, exponentially growing buckets for TCP/TLS connect.
// Keep in seconds (Prometheus convention). Prioritize sub-second resolution, with a few larger outlier buckets.
const CONNECT_DURATION_BUCKET: [f64; 10] = [
	0.0005, // 0.5 ms
	0.0015, // 1.5 ms
	0.0043, // 4.3 ms
	0.0126, // 12.6 ms
	0.0368, // 36.8 ms
	0.108,  // 108 ms
	0.316,  // 316 ms
	0.924,  // 924 ms
	2.71,   // 2.71 s
	8.0,    // 8 s
];
// HTTP request duration buckets - general purpose for all HTTP traffic
// Covers 1ms to ~80 seconds with exponential growth
const HTTP_REQUEST_DURATION_BUCKET: [f64; 14] = [
	0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 80.0,
];
// Internal processing time
// Covers 50us to 250ms with growth.
const PROCESSING_DURATION_BUCKETS: [f64; 10] = [
	0.00005, // 50us
	0.0001,  // 100us
	0.00025, // 250us
	0.0005,  // 500us
	0.001,   // 1ms
	0.0025,  // 2.5ms
	0.005,   // 5ms
	0.01,    // 10ms
	0.05,    // 50ms
	0.25,    // 250ms
];

// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiservertime_per_output_token
// NOTE: the spec has SHOULD, but is not smart enough to handle the faster LLMs.
// We have added 0.001 (1000 TPS)
const OUTPUT_TOKEN_BUCKET: [f64; 14] = [
	0.001, 0.01, 0.025, 0.05, 0.075, 0.1, 0.15, 0.2, 0.3, 0.4, 0.5, 0.75, 1.0, 2.5,
];
// https://opentelemetry.io/docs/specs/semconv/gen-ai/gen-ai-metrics/#metric-gen_aiservertime_to_first_token
const FIRST_TOKEN_BUCKET: [f64; 16] = [
	0.001, 0.005, 0.01, 0.02, 0.04, 0.06, 0.08, 0.1, 0.25, 0.5, 0.75, 1.0, 2.5, 5.0, 7.5, 10.0,
];

#[cfg(feature = "adobe")]
pub(crate) mod adobe_metrics {
	use std::time::Duration;

	use super::{MCPCallAdobe, Metrics};

	/// Records the duration of an MCP call into the Adobe-only histogram.
	/// Increments the Adobe-only `mcp_request_duration` histogram. Aggregate count parity
	/// with `mcp_requests_total` across all MCPCall series is guarded by the I6 integration test.
	#[allow(dead_code)]
	pub fn record_mcp_call(metrics: &Metrics, call: &MCPCallAdobe, duration: Duration) {
		metrics
			.mcp_request_duration
			.get_or_create(call)
			.observe(duration.as_secs_f64());
	}

	/// Classifies an [`UpstreamError`] into a bounded `error_type` label for the
	/// `mcp_upstream_errors_total` counter. Returns `None` for the non-transport
	/// variants (RBAC denial + client/protocol errors) which must NOT be counted.
	///
	/// The match is exhaustive over `UpstreamError` (no wildcard arm): if a future
	/// upstream sync adds a variant, this fails to compile until it is classified
	/// here — a deliberate forcing function. Do not add a `_ =>` arm.
	pub fn classify_upstream_error(e: &crate::mcp::UpstreamError) -> Option<&'static str> {
		use crate::mcp::{ClientError, UpstreamError};

		Some(match e {
			UpstreamError::ServiceError(_) => "service_error",
			UpstreamError::OpenAPIError(_) => "openapi_error",
			UpstreamError::Proxy(_) => "proxy",
			UpstreamError::Stdio(_) => "stdio",
			UpstreamError::StdioShutdown => "stdio_shutdown",
			UpstreamError::Send => "send",
			UpstreamError::Recv => "recv",
			UpstreamError::Http(ClientError::Status(resp)) => match resp.status().as_u16() {
				400..=499 => "http_4xx",
				500..=599 => "http_5xx",
				_ => "http_other",
			},
			// ClientError::General | ClientError::Proxy — no upstream status code available.
			UpstreamError::Http(_) => "http_other",
			// Not upstream/transport failures — do not count:
			UpstreamError::McpGuardrails(_)
			| UpstreamError::Authorization { .. }
			| UpstreamError::InvalidRequest(_)
			| UpstreamError::InvalidMethod(_)
			| UpstreamError::InvalidMethodWithMultiplexing(_) => return None,
		})
	}
}

#[cfg(all(test, feature = "adobe"))]
mod adobe_tests {
	use agent_core::metrics::{CustomField, DefaultedUnknown};
	use frozen_collections::FzHashSet;
	use prometheus_client::encoding::text::encode;
	use prometheus_client::registry::Registry;

	use super::{MCPCallAdobe, Metrics, RouteIdentifier};

	#[test]
	fn mcp_request_duration_registered_under_adobe_feature() {
		let mut registry = Registry::default();
		let sub = agent_core::metrics::sub_registry(&mut registry);
		let metrics = Metrics::new(sub, FzHashSet::default());

		// prometheus_client v0.24 only encodes non-empty families — seed one observation so
		// the # TYPE / # UNIT / # HELP headers appear in the scraped output.
		metrics
			.mcp_request_duration
			.get_or_create(&MCPCallAdobe {
				server: DefaultedUnknown::default(),
				target: DefaultedUnknown::default(),
				method: DefaultedUnknown::default(),
				resource_type: DefaultedUnknown::default(),
				resource: DefaultedUnknown::default(),
				route: RouteIdentifier::default(),
				custom: CustomField::default(),
			})
			.observe(0.001);

		let mut out = String::new();
		encode(&mut out, &registry).unwrap();

		assert!(
			out.contains("# TYPE agentgateway_mcp_request_duration_seconds histogram"),
			"missing TYPE line for histogram; got:\n{out}"
		);
		assert!(
			out.contains("# UNIT agentgateway_mcp_request_duration_seconds seconds"),
			"missing UNIT line for histogram; got:\n{out}"
		);
		assert!(
			out.contains("# HELP agentgateway_mcp_request_duration_seconds"),
			"missing HELP line for histogram; got:\n{out}"
		);
	}

	#[test]
	fn mcp_upstream_errors_registered_under_adobe_feature() {
		use super::MCPUpstreamError;

		let mut registry = Registry::default();
		let sub = agent_core::metrics::sub_registry(&mut registry);
		let metrics = Metrics::new(sub, FzHashSet::default());

		// prometheus_client only encodes non-empty families — seed one increment so
		// the # TYPE / # HELP headers appear in the scraped output.
		metrics
			.mcp_upstream_errors
			.get_or_create(&MCPUpstreamError {
				server: DefaultedUnknown::default(),
				target: DefaultedUnknown::default(),
				method: DefaultedUnknown::default(),
				error_type: DefaultedUnknown::default(),
				route: RouteIdentifier::default(),
			})
			.inc();

		let mut out = String::new();
		encode(&mut out, &registry).unwrap();

		assert!(
			out.contains("# TYPE agentgateway_mcp_upstream_errors counter"),
			"missing TYPE line for counter; got:\n{out}"
		);
		assert!(
			out.contains("# HELP agentgateway_mcp_upstream_errors"),
			"missing HELP line for counter; got:\n{out}"
		);
		assert!(
			out
				.lines()
				.any(|l| l.starts_with("agentgateway_mcp_upstream_errors_total{")),
			"missing _total sample line; got:\n{out}"
		);
	}

	#[test]
	fn mcp_call_adobe_has_server_and_target_labels() {
		use super::MCPCallAdobe;

		let mut registry = Registry::default();
		let sub = agent_core::metrics::sub_registry(&mut registry);
		let metrics = Metrics::new(sub, FzHashSet::default());

		// Seed one observation so prometheus_client emits the TYPE/UNIT/HELP headers.
		metrics
			.mcp_request_duration
			.get_or_create(&MCPCallAdobe {
				server: DefaultedUnknown::default(),
				target: DefaultedUnknown::default(),
				method: DefaultedUnknown::default(),
				resource_type: DefaultedUnknown::default(),
				resource: DefaultedUnknown::default(),
				route: RouteIdentifier::default(),
				custom: CustomField::default(),
			})
			.observe(0.001);

		let mut out = String::new();
		encode(&mut out, &registry).unwrap();

		assert!(
			out.contains("# TYPE agentgateway_mcp_request_duration_seconds histogram"),
			"missing TYPE line; got:\n{out}"
		);
		// Both new labels must appear in the scraped line.
		assert!(
			out.lines().any(|l| l.contains(r#"server=""#) && l.contains(r#"target=""#)),
			"server= or target= label missing from histogram output; got:\n{out}"
		);
	}

	#[test]
	fn classify_upstream_error_maps_transport_and_skips_non_transport() {
		use super::adobe_metrics::classify_upstream_error;
		use crate::mcp::{ClientError, UpstreamError};

		// Non-transport variants must NOT be counted (classifier returns None).
		assert_eq!(
			classify_upstream_error(&UpstreamError::Authorization {
				resource_type: "tool".into(),
				resource_name: "echo".into(),
			}),
			None
		);
		assert_eq!(
			classify_upstream_error(&UpstreamError::InvalidRequest("x".into())),
			None
		);
		assert_eq!(
			classify_upstream_error(&UpstreamError::InvalidMethod("x".into())),
			None
		);
		assert_eq!(
			classify_upstream_error(&UpstreamError::InvalidMethodWithMultiplexing("x".into())),
			None
		);
		assert_eq!(
			classify_upstream_error(&UpstreamError::McpGuardrails(
				crate::mcp::guardrails::Rejection::json_rpc(rmcp::model::ErrorData {
					code: rmcp::model::ErrorCode::METHOD_NOT_FOUND,
					message: "test".into(),
					data: None,
				})
			)),
			None,
			"McpGuardrails should not be counted"
		);

		// Simple transport variants.
		assert_eq!(classify_upstream_error(&UpstreamError::Send), Some("send"));
		assert_eq!(classify_upstream_error(&UpstreamError::Recv), Some("recv"));
		assert_eq!(
			classify_upstream_error(&UpstreamError::StdioShutdown),
			Some("stdio_shutdown")
		);

		// Direct `Proxy(_)` transport failure ⇒ "proxy". This is the outer variant U1
		// (connection-refused) produces end-to-end; the U1 integration test only asserts
		// "some nonzero series exists", so the `proxy` mapping is pinned here. The inner
		// `ProxyError` variant is irrelevant — the classifier matches `Proxy(_)`.
		assert_eq!(
			classify_upstream_error(&UpstreamError::Proxy(
				crate::proxy::ProxyError::UpstreamCallTimeout
			)),
			Some("proxy")
		);

		// HTTP status split.
		let http_status = |code: u16| {
			let resp = ::http::Response::builder()
				.status(code)
				.body(crate::http::Body::empty())
				.unwrap();
			UpstreamError::Http(ClientError::Status(Box::new(resp)))
		};
		assert_eq!(classify_upstream_error(&http_status(404)), Some("http_4xx"));
		assert_eq!(classify_upstream_error(&http_status(502)), Some("http_5xx"));
		assert_eq!(
			classify_upstream_error(&http_status(302)),
			Some("http_other")
		);
	}
}
