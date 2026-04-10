//! Pre-built structured queries matching the exact shapes required by the
//! Anova oven's Firestore security rules.

use alloc::string::{String, ToString};
use alloc::vec;

use crate::firestore::{
    document_name, CollectionSelector, Filter, Order, RunQueryRequest, StructuredQuery,
};

/// Parent path (under `.../documents/`) that a query's REST URL targets.
///
/// Top-level queries (like `oven-recipes`) use an empty parent; subcollection
/// queries (like `users/{uid}/favorite-oven-recipes`) use the owning document.
pub struct Query {
    pub parent_path: String,
    pub body: RunQueryRequest,
}

/// Build the exact query the iOS/Android oven app uses for **My Recipes**:
///
/// ```text
/// collection("oven-recipes")
///   .where("userProfileRef", "==", doc("user-profiles", uid))
///   .where("draft", "==", false)
///   .orderBy("createdTimestamp", "desc")
///   .limit(limit)
/// ```
///
/// This is one of the only query shapes the Firestore security rules allow.
pub fn user_recipes(project_id: &str, uid: &str, limit: u32) -> Query {
    let user_profile_ref = document_name(project_id, &format_user_profile_path(uid));
    Query {
        parent_path: String::new(),
        body: RunQueryRequest {
            structured_query: StructuredQuery {
                from: vec![CollectionSelector {
                    collection_id: "oven-recipes".to_string(),
                    all_descendants: None,
                }],
                where_: Some(Filter::and(vec![
                    Filter::equal_reference("userProfileRef", user_profile_ref),
                    Filter::equal_bool("draft", false),
                ])),
                order_by: vec![Order::descending("createdTimestamp")],
                limit: Some(limit),
            },
        },
    }
}

/// Build the query for the user's draft recipes.
pub fn user_draft_recipes(project_id: &str, uid: &str, limit: u32) -> Query {
    let user_profile_ref = document_name(project_id, &format_user_profile_path(uid));
    Query {
        parent_path: String::new(),
        body: RunQueryRequest {
            structured_query: StructuredQuery {
                from: vec![CollectionSelector {
                    collection_id: "oven-recipes".to_string(),
                    all_descendants: None,
                }],
                where_: Some(Filter::and(vec![
                    Filter::equal_reference("userProfileRef", user_profile_ref),
                    Filter::equal_bool("draft", true),
                ])),
                order_by: vec![Order::descending("createdTimestamp")],
                limit: Some(limit),
            },
        },
    }
}

/// Build the query for community/published recipes.
pub fn published_recipes(limit: u32) -> Query {
    Query {
        parent_path: String::new(),
        body: RunQueryRequest {
            structured_query: StructuredQuery {
                from: vec![CollectionSelector {
                    collection_id: "oven-recipes".to_string(),
                    all_descendants: None,
                }],
                where_: Some(Filter::equal_bool("published", true)),
                order_by: vec![Order::descending("publishedTimestamp")],
                limit: Some(limit),
            },
        },
    }
}

/// Build the query for a user's bookmarked recipes (`favorite-oven-recipes`
/// subcollection under `users/{uid}`).
pub fn favorite_recipes(uid: &str, limit: u32) -> Query {
    Query {
        parent_path: format_user_path(uid),
        body: RunQueryRequest {
            structured_query: StructuredQuery {
                from: vec![CollectionSelector {
                    collection_id: "favorite-oven-recipes".to_string(),
                    all_descendants: None,
                }],
                where_: None,
                order_by: vec![Order::descending("addedTimestamp")],
                limit: Some(limit),
            },
        },
    }
}

/// Build the query for a user's cook history (`oven-cooks` subcollection).
pub fn oven_cooks(uid: &str, limit: u32) -> Query {
    Query {
        parent_path: format_user_path(uid),
        body: RunQueryRequest {
            structured_query: StructuredQuery {
                from: vec![CollectionSelector {
                    collection_id: "oven-cooks".to_string(),
                    all_descendants: None,
                }],
                where_: None,
                order_by: vec![Order::descending("endedTimestamp")],
                limit: Some(limit),
            },
        },
    }
}

pub(crate) fn format_user_profile_path(uid: &str) -> String {
    let mut s = String::with_capacity(uid.len() + 14);
    s.push_str("user-profiles/");
    s.push_str(uid);
    s
}

pub(crate) fn format_user_path(uid: &str) -> String {
    let mut s = String::with_capacity(uid.len() + 6);
    s.push_str("users/");
    s.push_str(uid);
    s
}
