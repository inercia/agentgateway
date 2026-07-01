//! Request-scoped GTX pinning for MCP guardrails rateLimit peek/increment.
//!
//! Adobe-only: uses public `PolicyClient` APIs so we do not patch shared proxy code.
#![cfg(feature = "adobe")]

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use http::uri::{Authority, Scheme};
use tracing::trace;

use crate::client::ResolvedDestination;
use crate::http;
use crate::http::Request;
use crate::mcp::guardrails::RateLimit;
use crate::proxy::httpproxy::PolicyClient;
use crate::proxy::{resolve_simple_backend, ProxyError};
use crate::types::agent::{BackendTrafficPolicy, SimpleBackendReference};

/// GTX backend chosen at rate-limit peek; reused for increment on the same MCP request.
#[derive(Clone, Copy, Debug)]
pub(crate) struct RateLimitGtxPin(pub SocketAddr);

pub(crate) fn channel_for(
	rate_limit: &RateLimit,
	client: PolicyClient,
	override_dest: Option<SocketAddr>,
	capture_dest: Option<Arc<Mutex<Option<SocketAddr>>>>,
) -> RateLimitGrpcChannel {
	RateLimitGrpcChannel {
		target: rate_limit.target.clone(),
		policies: Arc::new(rate_limit.policies.clone()),
		client,
		override_dest,
		capture_dest,
	}
}

async fn call_reference(
	client: &PolicyClient,
	mut req: Request,
	backend_ref: &SimpleBackendReference,
	policies: &[BackendTrafficPolicy],
	override_dest: Option<SocketAddr>,
) -> Result<http::Response, ProxyError> {
	let backend = resolve_simple_backend(backend_ref, client.inputs.as_ref())?;
	trace!("resolved {:?} to {:?}", backend_ref, &backend);

	http::modify_req_uri(&mut req, |uri| {
		if uri.authority.is_none() {
			uri.authority = Some(Authority::try_from(backend.backend.hostport())?);
		}
		if uri.scheme.is_none() {
			uri.scheme = Some(Scheme::HTTP);
		}
		Ok(())
	})
	.map_err(ProxyError::Processing)?;

	let mut pols =
		crate::proxy::tcpproxy::get_backend_policies(client.inputs.as_ref(), &backend, policies, None);
	if let Some(dest) = override_dest {
		pols.override_dest = Some(dest);
	}
	client
		.call_with_explicit_policies(req, &backend.backend, pols)
		.await
}

#[derive(Clone, Debug)]
pub(crate) struct RateLimitGrpcChannel {
	target: Arc<SimpleBackendReference>,
	client: PolicyClient,
	policies: Arc<Vec<BackendTrafficPolicy>>,
	override_dest: Option<SocketAddr>,
	capture_dest: Option<Arc<Mutex<Option<SocketAddr>>>>,
}

impl tower::Service<::http::Request<tonic::body::Body>> for RateLimitGrpcChannel {
	type Response = http::Response;
	type Error = ProxyError;
	type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send>>;

	fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
		Ok(()).into()
	}

	fn call(&mut self, req: ::http::Request<tonic::body::Body>) -> Self::Future {
		let client = self.client.clone();
		let target = self.target.clone();
		let policies = self.policies.clone();
		let override_dest = self.override_dest;
		let capture_dest = self.capture_dest.clone();
		let req = req.map(http::Body::new);
		Box::pin(async move {
			let resp = call_reference(
				&client,
				req,
				&target,
				policies.as_slice(),
				override_dest,
			)
			.await?;
			if let Some(capture) = capture_dest
				&& let Some(resolved) = resp.extensions().get::<ResolvedDestination>()
			{
				*capture.lock().unwrap() = Some(resolved.0);
			}
			Ok(resp)
		})
	}
}
