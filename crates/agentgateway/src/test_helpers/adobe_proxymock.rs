use std::net::SocketAddr;
use std::ops::Deref;
use std::sync::{Arc, Mutex};

use agent_core::metrics::sub_registry;
use frozen_collections::FzHashSet;
use prometheus_client::encoding::text::encode;
use prometheus_client::registry::Registry;

use crate::telemetry::metrics::Metrics;
use crate::test_helpers::proxymock::{TestBind, setup_proxy_test};
use crate::types::agent::{Bind, Route};

/// Drop-in replacement for `setup_proxy_test` that retains the Prometheus
/// `Registry` backing `pi.metrics`. Adobe-only — keeps the shared
/// `proxymock.rs` byte-identical to upstream.
///
/// The returned `MetricsTestBind` delegates the builders used by the histogram
/// tests (`with_mcp_backend`, `with_multiplex_mcp_backend`, `with_bind`,
/// `with_route`) and derefs to `TestBind` for everything else, so call sites
/// only need the constructor swap.
pub fn setup_with_registry(cfg: &str) -> anyhow::Result<MetricsTestBind> {
	let mut tb = setup_proxy_test(cfg)?;
	let registry = Arc::new(Mutex::new(Registry::default()));
	let metrics = Arc::new(Metrics::new(
		sub_registry(&mut registry.lock().unwrap()),
		FzHashSet::default(),
	));
	// `tb.pi` has refcount = 1 at this point (we just received it from
	// `setup_proxy_test`), so `make_mut` returns `&mut ProxyInputs` directly
	// without cloning.
	Arc::make_mut(&mut tb.pi).metrics = metrics;
	Ok(MetricsTestBind { tb, registry })
}

pub struct MetricsTestBind {
	pub tb: TestBind,
	pub registry: Arc<Mutex<Registry>>,
}

impl MetricsTestBind {
	pub fn scrape_metrics(&self) -> String {
		let mut out = String::new();
		encode(&mut out, &self.registry.lock().unwrap()).unwrap();
		out
	}

	pub fn with_mcp_backend(self, b: SocketAddr, stateful: bool, legacy_sse: bool) -> Self {
		Self {
			tb: self.tb.with_mcp_backend(b, stateful, legacy_sse),
			registry: self.registry,
		}
	}

	pub fn with_multiplex_mcp_backend(
		self,
		name: &str,
		servers: Vec<(&str, SocketAddr, bool)>,
		stateful: bool,
	) -> Self {
		Self {
			tb: self.tb.with_multiplex_mcp_backend(name, servers, stateful),
			registry: self.registry,
		}
	}

	pub fn with_bind(self, bind: Bind) -> Self {
		Self {
			tb: self.tb.with_bind(bind),
			registry: self.registry,
		}
	}

	pub fn with_route(self, r: Route) -> Self {
		Self {
			tb: self.tb.with_route(r),
			registry: self.registry,
		}
	}
}

impl Deref for MetricsTestBind {
	type Target = TestBind;

	fn deref(&self) -> &TestBind {
		&self.tb
	}
}
