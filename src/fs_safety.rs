use crate::http::Response;
use std::fs;
use std::path::PathBuf;

/// Canonicalizes a location's configured root, or a 500 if it doesn't exist.
pub fn canonical_root(root: &str) -> Result<PathBuf, Response> {
    fs::canonicalize(root)
        .map_err(|_| Response::error(500, "Server misconfigured: location root not found"))
}

/// Strips the location's path prefix off a request path, leaving a relative
/// filesystem path with no leading slash (e.g. "/about/team" under
/// location "/about" becomes "team").
pub fn relative_path<'a>(location_path: &str, request_path: &'a str) -> &'a str {
    request_path
        .strip_prefix(location_path)
        .unwrap_or(request_path)
        .trim_start_matches('/')
}

pub fn within_root(candidate: &std::path::Path, canonical_root: &std::path::Path) -> bool {
    candidate.starts_with(canonical_root)
}
