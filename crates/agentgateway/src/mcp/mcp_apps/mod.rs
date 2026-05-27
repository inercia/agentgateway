//! MCP Apps multiplexing (resources, templates, tasks, capability hints, `ui://` wrapping).

pub(crate) mod capabilities;
pub(crate) mod routing;

#[cfg(all(test, feature = "adobe"))]
mod tests;
