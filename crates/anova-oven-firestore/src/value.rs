//! Firestore `Value` union type used by the REST API.
//!
//! Every field in a Firestore document is wrapped in a single-variant object
//! that tags its type (e.g. `{ "stringValue": "foo" }`,
//! `{ "booleanValue": false }`, `{ "mapValue": { "fields": {...} } }`).
//!
//! We represent this as a struct where each type is an `Option`, matching how
//! Firestore serializes it on the wire, and provide [`FirestoreValue::to_json`]
//! to convert into a plain `serde_json::Value` for ergonomic deserialization.

use alloc::string::String;
use alloc::vec::Vec;
use serde::Deserialize;
use serde_json::{Map, Value as JsonValue};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FirestoreValue {
    pub null_value: Option<serde_json::Value>,
    pub boolean_value: Option<bool>,
    /// REST API serializes integers as strings.
    pub integer_value: Option<String>,
    pub double_value: Option<f64>,
    pub timestamp_value: Option<String>,
    pub string_value: Option<String>,
    /// Bytes (base64-encoded in REST API).
    pub bytes_value: Option<String>,
    /// Full path like `projects/.../documents/user-profiles/{uid}`.
    pub reference_value: Option<String>,
    pub geo_point_value: Option<GeoPoint>,
    pub array_value: Option<ArrayValue>,
    pub map_value: Option<MapValue>,
}

#[derive(Debug, Deserialize)]
pub struct GeoPoint {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Debug, Default, Deserialize)]
pub struct ArrayValue {
    #[serde(default)]
    pub values: Vec<FirestoreValue>,
}

#[derive(Debug, Default, Deserialize)]
pub struct MapValue {
    #[serde(default)]
    pub fields: Map<String, JsonValue>,
}

impl FirestoreValue {
    /// Convert this Firestore value into a plain JSON value.
    ///
    /// - Timestamps become their ISO 8601 strings.
    /// - References become their full document path strings.
    /// - Integers are parsed back into JSON numbers where possible.
    /// - Maps with unknown/empty types become `null`.
    pub fn to_json(&self) -> JsonValue {
        if self.null_value.is_some() {
            return JsonValue::Null;
        }
        if let Some(b) = self.boolean_value {
            return JsonValue::Bool(b);
        }
        if let Some(i) = &self.integer_value {
            if let Ok(n) = i.parse::<i64>() {
                return JsonValue::Number(n.into());
            }
            return JsonValue::String(i.clone());
        }
        if let Some(f) = self.double_value {
            if let Some(n) = serde_json::Number::from_f64(f) {
                return JsonValue::Number(n);
            }
        }
        if let Some(s) = &self.string_value {
            return JsonValue::String(s.clone());
        }
        if let Some(s) = &self.timestamp_value {
            return JsonValue::String(s.clone());
        }
        if let Some(s) = &self.reference_value {
            return JsonValue::String(s.clone());
        }
        if let Some(s) = &self.bytes_value {
            return JsonValue::String(s.clone());
        }
        if let Some(arr) = &self.array_value {
            let items: Vec<JsonValue> = arr.values.iter().map(|v| v.to_json()).collect();
            return JsonValue::Array(items);
        }
        if let Some(map) = &self.map_value {
            return map_fields_to_json(&map.fields);
        }
        if let Some(gp) = &self.geo_point_value {
            let mut m = Map::new();
            m.insert(
                "latitude".into(),
                serde_json::Number::from_f64(gp.latitude)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null),
            );
            m.insert(
                "longitude".into(),
                serde_json::Number::from_f64(gp.longitude)
                    .map(JsonValue::Number)
                    .unwrap_or(JsonValue::Null),
            );
            return JsonValue::Object(m);
        }
        JsonValue::Null
    }
}

/// Convert a map of Firestore fields (serialized as raw JSON with type
/// wrappers) into a plain JSON object by unwrapping each value.
///
/// The map's values are accepted as `serde_json::Value` so that this function
/// works equally well on the top-level `document.fields` map (which comes in
/// as raw JSON) and on nested `mapValue.fields` sub-maps.
pub fn map_fields_to_json(fields: &Map<String, JsonValue>) -> JsonValue {
    let mut out = Map::with_capacity(fields.len());
    for (k, v) in fields {
        // Each value is a Firestore Value union; re-deserialize through
        // FirestoreValue to unwrap it.
        let fv: FirestoreValue = serde_json::from_value(v.clone()).unwrap_or_default();
        out.insert(k.clone(), fv.to_json());
    }
    JsonValue::Object(out)
}
