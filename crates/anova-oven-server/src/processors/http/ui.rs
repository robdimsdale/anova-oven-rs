//! The web UI: a status page at `/` and recipe pages under `/recipes`.
//!
//! Static pages compiled into the binary. They fetch everything from `api/`
//! in the browser, so there is no templating here, except for one thing:
//! every link is relative to the page's `<base href="/">`, and behind a
//! reverse proxy that serves the UI under a prefix, the base becomes that
//! prefix (see [`path_prefix`]).

use std::sync::Arc;

use axum::http::header::VARY;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::{routing, Router};

use super::{build_response, HttpState};

const INDEX: &str = include_str!("ui/index.html");
const RECIPES: &str = include_str!("ui/recipes.html");
const RECIPE: &str = include_str!("ui/recipe.html");
const STYLE: &str = include_str!("ui/style.css");
const SCRIPT: &str = include_str!("ui/common.js");
const STEPS: &str = include_str!("ui/steps.js");

/// The `<base>` every page carries, rewritten for a path prefix.
const BASE_TAG: &str = r#"<base href="/">"#;

/// The headers a reverse proxy uses to say which prefix it serves the UI
/// under: the common one (Traefik, nginx and others can set it) and Home
/// Assistant ingress's. Only the first one present counts; see
/// [`path_prefix`].
const PREFIX_HEADERS: [&str; 2] = ["x-forwarded-prefix", "x-ingress-path"];

pub(super) fn router() -> Router<Arc<HttpState>> {
    Router::new()
        .route(
            "/",
            routing::get(|h: HeaderMap| async move { html(INDEX, &h) }),
        )
        .route(
            "/recipes",
            routing::get(|h: HeaderMap| async move { html(RECIPES, &h) }),
        )
        // `new` and an ID share a page; it reads which from the path.
        .route(
            "/recipes/{id}",
            routing::get(|h: HeaderMap| async move { html(RECIPE, &h) }),
        )
        .route(
            "/ui/style.css",
            routing::get(|| async { asset("text/css; charset=utf-8", STYLE) }),
        )
        .route(
            "/ui/common.js",
            routing::get(|| async { asset("text/javascript; charset=utf-8", SCRIPT) }),
        )
        .route(
            "/ui/steps.js",
            routing::get(|| async { asset("text/javascript; charset=utf-8", STEPS) }),
        )
}

fn html(page: &'static str, headers: &HeaderMap) -> Response {
    let body = with_base(page, path_prefix(headers));
    let mut response = build_response(
        StatusCode::OK,
        "text/html; charset=utf-8",
        body.into_bytes(),
    );
    // The page depends on these headers: a cache in front must not serve
    // one prefix's page for another.
    response.headers_mut().insert(
        VARY,
        HeaderValue::from_static("x-forwarded-prefix, x-ingress-path"),
    );
    response
}

/// The page with its `<base>` pointing at `prefix`, if there is one.
fn with_base(page: &str, prefix: Option<&str>) -> String {
    match prefix {
        Some(prefix) => page.replacen(BASE_TAG, &format!(r#"<base href="{prefix}/">"#), 1),
        None => page.to_string(),
    }
}

/// The prefix a reverse proxy serves the UI under, such as `/oven` or
/// `/api/hassio_ingress/<token>`, without a trailing slash.
///
/// The value goes into the page's HTML, so only a plain absolute path is
/// used: letters, digits and `/ _ - .`, no `..`, and no `//`. A URL that
/// starts with `//` names another site, so `<base href="//evil.example/">`
/// would load the scripts and send every API call there.
///
/// Only the first header present counts, in the order of
/// [`PREFIX_HEADERS`]. If that one is invalid, the result is `None`, and
/// the next header is not tried: a bad value is a sign of a wrong setup,
/// so the page falls back to `/` rather than guess.
fn path_prefix(headers: &HeaderMap) -> Option<&str> {
    let value = PREFIX_HEADERS.iter().find_map(|name| headers.get(*name))?;
    let path = value.to_str().ok()?.trim_end_matches('/');
    let plain = path
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"/_-.".contains(&b));
    let safe = path.starts_with('/') && !path.contains("//") && !path.contains("..");
    (plain && safe).then_some(path)
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    build_response(StatusCode::OK, content_type, body.as_bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(name: &'static str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(name, value.parse().unwrap());
        h
    }

    #[test]
    fn every_page_has_the_base_tag_once() {
        for page in [INDEX, RECIPES, RECIPE] {
            assert_eq!(page.matches(BASE_TAG).count(), 1);
        }
    }

    #[test]
    fn no_page_or_script_links_to_an_absolute_path() {
        // An absolute link escapes the prefix. `api()` adds its own `api`.
        for file in [INDEX, RECIPES, RECIPE, SCRIPT, STEPS] {
            let file = file.replacen(BASE_TAG, "", 1);
            for bad in [
                r#"href="/"#,
                r#"src="/"#,
                "location.href = \"/",
                "location.href = `/",
                "fetch(\"/",
                "fetch(`/",
            ] {
                assert!(!file.contains(bad), "{bad}");
            }
        }
    }

    #[test]
    fn api_paths_start_with_a_slash() {
        // `api()` puts `api` in front, so `recipes/x` would ask for
        // `apirecipes/x`. `act(path)` passes its path on to `api()`.
        for file in [INDEX, RECIPES, RECIPE, SCRIPT, STEPS] {
            // The path comes after the method: `api("GET", "/status")`.
            let api = file
                .split("api(\"")
                .skip(1)
                .map(|call| call.split_once(", ").map_or("", |(_, rest)| rest));
            let act = file.split("act(").skip(1);
            for path in api.chain(act) {
                // A path in a variable is checked where it's written.
                if !path.starts_with(['"', '`']) {
                    continue;
                }
                assert!(
                    path.starts_with("\"/") || path.starts_with("`/"),
                    "{}",
                    &path[..path.len().min(40)]
                );
            }
        }
    }

    #[test]
    fn a_prefix_from_either_header() {
        let h = headers("x-forwarded-prefix", "/oven/");
        assert_eq!(path_prefix(&h), Some("/oven"));
        let h = headers("x-ingress-path", "/api/hassio_ingress/AbC-12_x");
        assert_eq!(path_prefix(&h), Some("/api/hassio_ingress/AbC-12_x"));
    }

    #[test]
    fn a_prefix_must_be_a_plain_absolute_path() {
        assert_eq!(path_prefix(&HeaderMap::new()), None);
        for bad in [
            "relative",
            r#"/x"><script>"#,
            "/a/../b",
            "/a b",
            "/",
            "//evil.example",
            "/a//b",
        ] {
            assert_eq!(
                path_prefix(&headers("x-forwarded-prefix", bad)),
                None,
                "{bad}"
            );
        }
    }

    #[test]
    fn only_the_first_header_present_counts() {
        let mut h = headers("x-forwarded-prefix", "//evil.example");
        h.insert("x-ingress-path", "/api/hassio_ingress/x".parse().unwrap());
        assert_eq!(path_prefix(&h), None);
    }

    #[test]
    fn the_page_varies_with_the_prefix_headers() {
        let response = html(INDEX, &HeaderMap::new());
        assert_eq!(
            response.headers()[VARY],
            "x-forwarded-prefix, x-ingress-path"
        );
    }

    #[test]
    fn the_prefix_goes_into_the_base() {
        let page = with_base(INDEX, Some("/oven"));
        assert!(page.contains(r#"<base href="/oven/">"#));
        assert!(!page.contains(BASE_TAG));
        assert_eq!(with_base(INDEX, None), INDEX);
    }
}
