//! Integration tests that verify parsing of Firestore REST responses into
//! the high-level types this crate exposes.
//!
//! The test inputs mirror the shape of real Anova Firestore documents.

use anova_oven_firestore::firestore::{Document, RunQueryResponse};
use anova_oven_firestore::{queries, OvenRecipe};

#[test]
fn parse_run_query_response() {
    let body = r#"[
      {
        "document": {
          "name": "projects/anova-app/databases/(default)/documents/oven-recipes/abc123",
          "fields": {
            "title": { "stringValue": "Freezer reheat" },
            "description": { "stringValue": "" },
            "draft": { "booleanValue": false },
            "published": { "booleanValue": false },
            "createdTimestamp": { "timestampValue": "2023-02-14T15:23:57.759Z" },
            "updatedTimestamp": { "timestampValue": "2024-01-07T23:42:28.669Z" },
            "userProfileRef": { "referenceValue": "projects/anova-app/databases/(default)/documents/user-profiles/uid1" },
            "servings": { "integerValue": "0" },
            "numberOfCooks": { "integerValue": "116" },
            "steps": {
              "arrayValue": {
                "values": [
                  {
                    "mapValue": {
                      "fields": {
                        "stepType": { "stringValue": "stage" },
                        "type": { "stringValue": "preheat" },
                        "rackPosition": { "integerValue": "3" },
                        "userActionRequired": { "booleanValue": false },
                        "fan": { "mapValue": { "fields": { "speed": { "integerValue": "100" } } } },
                        "vent": { "mapValue": { "fields": { "open": { "booleanValue": false } } } },
                        "temperatureBulbs": {
                          "mapValue": {
                            "fields": {
                              "mode": { "stringValue": "dry" },
                              "dry": {
                                "mapValue": {
                                  "fields": {
                                    "setpoint": {
                                      "mapValue": {
                                        "fields": {
                                          "fahrenheit": { "integerValue": "230" },
                                          "celsius": { "integerValue": "110" }
                                        }
                                      }
                                    }
                                  }
                                }
                              }
                            }
                          }
                        }
                      }
                    }
                  },
                  {
                    "mapValue": {
                      "fields": {
                        "stepType": { "stringValue": "direction" },
                        "title": { "stringValue": "Plate and serve" },
                        "description": { "stringValue": "" }
                      }
                    }
                  }
                ]
              }
            }
          },
          "createTime": "2023-02-14T15:23:57.759Z",
          "updateTime": "2024-01-07T23:42:28.669Z"
        },
        "readTime": "2024-01-03T22:00:00Z"
      }
    ]"#;

    let items: RunQueryResponse = serde_json::from_str(body).unwrap();
    assert_eq!(items.len(), 1);

    let doc = items[0].document.as_ref().expect("document");
    assert_eq!(doc.id(), "abc123");

    let recipe = OvenRecipe::from_document(doc).unwrap();
    assert_eq!(recipe.title, "Freezer reheat");
    assert_eq!(recipe.draft, false);
    assert_eq!(recipe.published, false);
    assert_eq!(recipe.firestore_id.as_deref(), Some("abc123"));
    assert_eq!(recipe.created_timestamp.as_deref(), Some("2023-02-14T15:23:57.759Z"));
    assert_eq!(
        recipe.user_profile_ref.as_deref(),
        Some("projects/anova-app/databases/(default)/documents/user-profiles/uid1")
    );
    assert_eq!(recipe.steps.len(), 2);
    assert_eq!(recipe.cook_stages().len(), 1);

    let stage = recipe.cook_stages()[0];
    assert_eq!(stage["stepType"], "stage");
    assert_eq!(stage["type"], "preheat");
    assert_eq!(stage["rackPosition"], 3);
    assert_eq!(stage["fan"]["speed"], 100);
    assert_eq!(stage["temperatureBulbs"]["mode"], "dry");
    assert_eq!(stage["temperatureBulbs"]["dry"]["setpoint"]["fahrenheit"], 230);
}

#[test]
fn user_recipes_query_matches_app_query_shape() {
    let q = queries::user_recipes("anova-app", "uid-foo", 10);
    let json = serde_json::to_value(&q.body).unwrap();
    // Collection
    assert_eq!(json["structuredQuery"]["from"][0]["collectionId"], "oven-recipes");
    // Filters: userProfileRef reference + draft boolean, combined with AND
    let composite = &json["structuredQuery"]["where"]["compositeFilter"];
    assert_eq!(composite["op"], "AND");
    assert_eq!(composite["filters"][0]["fieldFilter"]["field"]["fieldPath"], "userProfileRef");
    assert_eq!(composite["filters"][0]["fieldFilter"]["op"], "EQUAL");
    assert_eq!(
        composite["filters"][0]["fieldFilter"]["value"]["referenceValue"],
        "projects/anova-app/databases/(default)/documents/user-profiles/uid-foo"
    );
    assert_eq!(composite["filters"][1]["fieldFilter"]["field"]["fieldPath"], "draft");
    assert_eq!(composite["filters"][1]["fieldFilter"]["value"]["booleanValue"], false);
    // Ordering + limit
    assert_eq!(json["structuredQuery"]["orderBy"][0]["field"]["fieldPath"], "createdTimestamp");
    assert_eq!(json["structuredQuery"]["orderBy"][0]["direction"], "DESCENDING");
    assert_eq!(json["structuredQuery"]["limit"], 10);
}

#[test]
fn single_document_parsing_is_the_same() {
    // getDocument returns a single Document (not wrapped in a RunQueryItem)
    let body = r#"{
      "name": "projects/anova-app/databases/(default)/documents/oven-recipes/xyz789",
      "fields": {
        "title": { "stringValue": "Sourdough" },
        "draft": { "booleanValue": false },
        "published": { "booleanValue": true }
      },
      "createTime": "2023-01-01T00:00:00Z",
      "updateTime": "2023-01-01T00:00:00Z"
    }"#;

    let doc: Document = serde_json::from_str(body).unwrap();
    assert_eq!(doc.id(), "xyz789");
    let recipe = OvenRecipe::from_document(&doc).unwrap();
    assert_eq!(recipe.title, "Sourdough");
    assert_eq!(recipe.published, true);
}

#[test]
fn refresh_form_is_urlencoded() {
    use anova_oven_firestore::auth::build_refresh_form;
    let body = build_refresh_form("abc/def+ghi=jkl");
    assert_eq!(body, "grant_type=refresh_token&refresh_token=abc%2Fdef%2Bghi%3Djkl");
}
