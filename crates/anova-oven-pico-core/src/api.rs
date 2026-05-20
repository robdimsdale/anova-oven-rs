//! Pure pre-flight logic for outbound API calls. The actual HTTP transport
//! lives in `anova-oven-pico::api` and depends on `reqwless`/`embassy-net`.

use alloc::format;
use alloc::string::String;

/// Normalizes the configured server URL: trims trailing `/`, and prepends
/// `http://` if the input has no scheme. The result is computed once at
/// startup and reused for every API call rather than re-allocating per
/// request (review §2.1).
pub fn normalize_server_url(url: &str) -> String {
    let trimmed = url.trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.into()
    } else {
        format!("http://{trimmed}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_http_scheme() {
        assert_eq!(normalize_server_url("http://example.com"), "http://example.com");
    }

    #[test]
    fn preserves_https_scheme() {
        assert_eq!(normalize_server_url("https://example.com"), "https://example.com");
    }

    #[test]
    fn adds_http_to_bare_host() {
        assert_eq!(normalize_server_url("example.com"), "http://example.com");
    }

    #[test]
    fn adds_http_to_host_with_port() {
        assert_eq!(normalize_server_url("192.168.1.10:8080"), "http://192.168.1.10:8080");
    }

    #[test]
    fn trims_single_trailing_slash() {
        assert_eq!(normalize_server_url("http://example.com/"), "http://example.com");
    }

    #[test]
    fn trims_multiple_trailing_slashes() {
        assert_eq!(normalize_server_url("http://example.com///"), "http://example.com");
        assert_eq!(normalize_server_url("example.com///"), "http://example.com");
    }

    #[test]
    fn preserves_path_segments() {
        assert_eq!(
            normalize_server_url("http://example.com/api/v1"),
            "http://example.com/api/v1"
        );
    }

    #[test]
    fn empty_input() {
        // Edge case: bare empty string becomes `"http://"`. Not useful, but
        // documented behaviour rather than a crash.
        assert_eq!(normalize_server_url(""), "http://");
    }
}
