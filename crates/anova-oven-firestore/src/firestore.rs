//! Firestore REST API structured query request/response types.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value as JsonValue};

/// Build the URL for `POST .../documents:runQuery`.
///
/// `parent_path` is the document path to run the query against — for
/// top-level collections pass an empty string; for subcollections under a
/// user, pass e.g. `"users/<uid>"`.
pub fn run_query_url(project_id: &str, parent_path: &str) -> String {
    if parent_path.is_empty() {
        format!(
            "https://firestore.googleapis.com/v1/projects/{project_id}/databases/(default)/documents:runQuery"
        )
    } else {
        format!(
            "https://firestore.googleapis.com/v1/projects/{project_id}/databases/(default)/documents/{parent_path}:runQuery"
        )
    }
}

/// Build the URL for fetching a single document via `GET`.
pub fn get_document_url(project_id: &str, document_path: &str) -> String {
    format!(
        "https://firestore.googleapis.com/v1/projects/{project_id}/databases/(default)/documents/{document_path}"
    )
}

/// Build the full document name used for `referenceValue` fields in queries.
pub fn document_name(project_id: &str, document_path: &str) -> String {
    format!(
        "projects/{project_id}/databases/(default)/documents/{document_path}"
    )
}

// --- runQuery wire types ---

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunQueryRequest {
    pub structured_query: StructuredQuery,
}

#[derive(Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StructuredQuery {
    pub from: Vec<CollectionSelector>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub where_: Option<Filter>,
    #[serde(rename = "orderBy", skip_serializing_if = "Vec::is_empty", default)]
    pub order_by: Vec<Order>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CollectionSelector {
    pub collection_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_descendants: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(untagged)]
pub enum Filter {
    Composite {
        #[serde(rename = "compositeFilter")]
        composite_filter: CompositeFilter,
    },
    Field {
        #[serde(rename = "fieldFilter")]
        field_filter: FieldFilter,
    },
}

#[derive(Debug, Serialize)]
pub struct CompositeFilter {
    pub op: &'static str,
    pub filters: Vec<Filter>,
}

#[derive(Debug, Serialize)]
pub struct FieldFilter {
    pub field: FieldReference,
    pub op: &'static str,
    pub value: JsonValue,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FieldReference {
    pub field_path: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Order {
    pub field: FieldReference,
    pub direction: &'static str,
}

// --- runQuery response types ---

/// A `runQuery` response is a JSON array of [`RunQueryItem`] objects.
pub type RunQueryResponse = Vec<RunQueryItem>;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunQueryItem {
    pub document: Option<Document>,
    pub read_time: Option<String>,
    /// Present on the final item if the query was skipped entirely.
    pub skipped_results: Option<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    /// Full document name like
    /// `projects/anova-app/databases/(default)/documents/oven-recipes/{id}`.
    pub name: String,
    /// Each value is a Firestore [`crate::FirestoreValue`] wrapped in an
    /// object — we keep them as raw JSON here and unwrap lazily.
    #[serde(default)]
    pub fields: Map<String, JsonValue>,
    pub create_time: Option<String>,
    pub update_time: Option<String>,
}

impl Document {
    /// Return the Firestore document ID (last path segment of `name`).
    pub fn id(&self) -> &str {
        self.name.rsplit('/').next().unwrap_or("")
    }

    /// Convert the document's `fields` into a plain JSON object by unwrapping
    /// each Firestore value.
    pub fn to_json(&self) -> JsonValue {
        crate::value::map_fields_to_json(&self.fields)
    }
}

// --- Constructors for filter building ---

impl Filter {
    pub fn and(filters: Vec<Filter>) -> Self {
        Filter::Composite {
            composite_filter: CompositeFilter {
                op: "AND",
                filters,
            },
        }
    }

    pub fn equal_bool(field_path: impl Into<String>, value: bool) -> Self {
        Filter::Field {
            field_filter: FieldFilter {
                field: FieldReference {
                    field_path: field_path.into(),
                },
                op: "EQUAL",
                value: JsonValue::Object({
                    let mut m = Map::new();
                    m.insert("booleanValue".into(), JsonValue::Bool(value));
                    m
                }),
            },
        }
    }

    pub fn equal_reference(field_path: impl Into<String>, reference: impl Into<String>) -> Self {
        Filter::Field {
            field_filter: FieldFilter {
                field: FieldReference {
                    field_path: field_path.into(),
                },
                op: "EQUAL",
                value: JsonValue::Object({
                    let mut m = Map::new();
                    m.insert("referenceValue".into(), JsonValue::String(reference.into()));
                    m
                }),
            },
        }
    }
}

impl Order {
    pub fn descending(field_path: impl Into<String>) -> Self {
        Order {
            field: FieldReference {
                field_path: field_path.into(),
            },
            direction: "DESCENDING",
        }
    }

    pub fn ascending(field_path: impl Into<String>) -> Self {
        Order {
            field: FieldReference {
                field_path: field_path.into(),
            },
            direction: "ASCENDING",
        }
    }
}
