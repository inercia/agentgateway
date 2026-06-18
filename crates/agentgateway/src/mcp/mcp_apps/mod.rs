//! MCP Apps multiplexing (resources, templates, tasks, capability hints, `ui://` wrapping).

pub(crate) mod capabilities;
pub(crate) mod routing;
pub(crate) mod server_info;

#[cfg(all(test, feature = "adobe"))]
mod tests;
