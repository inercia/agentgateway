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

#[cfg(test)]
mod tests {
	use super::parse_resource_name;
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
}
