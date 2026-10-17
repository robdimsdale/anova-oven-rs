//! The web UI: a status page at `/` and recipe pages under `/recipes`.
//!
//! Static pages compiled into the binary. They fetch everything from `/api`
//! in the browser, so there is no templating here.

use std::sync::Arc;

use axum::http::StatusCode;
use axum::response::Response;
use axum::{routing, Router};

use super::{build_response, HttpState};

const INDEX: &str = include_str!("ui/index.html");
const RECIPES: &str = include_str!("ui/recipes.html");
const RECIPE: &str = include_str!("ui/recipe.html");
const STYLE: &str = include_str!("ui/style.css");
const SCRIPT: &str = include_str!("ui/common.js");
const STEPS: &str = include_str!("ui/steps.js");

pub(super) fn router() -> Router<Arc<HttpState>> {
    Router::new()
        .route("/", routing::get(|| async { html(INDEX) }))
        .route("/recipes", routing::get(|| async { html(RECIPES) }))
        // `new` and an ID share a page; it reads which from the path.
        .route("/recipes/{id}", routing::get(|| async { html(RECIPE) }))
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

fn html(page: &'static str) -> Response {
    build_response(
        StatusCode::OK,
        "text/html; charset=utf-8",
        page.as_bytes().to_vec(),
    )
}

fn asset(content_type: &'static str, body: &'static str) -> Response {
    build_response(StatusCode::OK, content_type, body.as_bytes().to_vec())
}
