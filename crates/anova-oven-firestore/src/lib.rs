#![cfg_attr(not(feature = "std"), no_std)]

//! Transport-agnostic Firebase Auth + Firestore client for the Anova Oven
//! app's Firestore backend.
//!
//! This crate builds HTTP request bodies and parses response bodies as pure
//! serde operations. The caller provides the actual HTTP transport (e.g.
//! `reqwest` on the CLI, `embassy-net` + `embedded-tls` on the Pico W).
//!
//! The primary entry points are:
//! - [`auth::sign_in_url`] / [`auth::SignInRequest`] / [`auth::SignInResponse`]
//!   for email+password authentication via Firebase Identity Toolkit.
//! - [`auth::refresh_token_url`] / [`auth::build_refresh_form`] /
//!   [`auth::RefreshTokenResponse`] for exchanging a refresh token for a
//!   fresh ID token.
//! - [`firestore::run_query_url`] plus the query builders in
//!   [`queries`] for sending structured queries to Firestore via REST.
//! - [`recipe::OvenRecipe`] and friends for deserializing the final
//!   recipe documents.

extern crate alloc;

pub mod auth;
pub mod firestore;
pub mod queries;
pub mod recipe;
pub mod value;

pub use auth::{RefreshTokenResponse, SignInRequest, SignInResponse};
pub use firestore::{Document, RunQueryItem, RunQueryRequest, RunQueryResponse};
pub use recipe::{FavoriteOvenRecipe, Ingredient, OvenCook, OvenRecipe, Step};
pub use value::FirestoreValue;

/// Identifiers for the Anova oven's Firebase project.
pub const ANOVA_PROJECT_ID: &str = "anova-app";

/// Firebase API key extracted from the Anova **oven** app (`com.anovaculinary.anovaoven`).
///
/// This key is public — it is shipped inside the mobile app — and is safe to
/// embed in clients. It authenticates requests to the `anova-app` Firebase
/// project alongside Firestore security rules.
pub const ANOVA_OVEN_API_KEY: &str = "AIzaSyCGJwHXUhkNBdPkH3OAkjc9-3xMMjvanfU";

/// Firebase API key extracted from the general Anova Culinary app.
///
/// Produces tokens for the same `anova-app` project and the same UID as the
/// oven app key, so either works.
pub const ANOVA_GENERAL_API_KEY: &str = "AIzaSyB0VNqmJVAeR1fn_NbqqhwSytyMOZ_JO9c";
