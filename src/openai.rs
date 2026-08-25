//! Translation into the OpenAI format.
//!
//! **Mapping only** — no networking, no state. The handlers live in
//! [`crate::serve`], the upstream in [`crate::client`]. That keeps the
//! translation testable and the layering intact: `wire` is the neutral
//! vocabulary, this module one of potentially several outward shapes.
//!
//! Guiding rule: serve what OpenAI clients expect, but **invent nothing** we do
//! not know. Where the two collide, the reasoning is stated inline.

use serde_json::Value;
use serde_json::json;

/// Builds the response for `GET /v1/models`.
///
/// **A slim projection, not a pass-through.** Each backend object carries a
/// `model_messages.instructions_template` of roughly 17 KB; passed through raw
/// that would be ~170 KB for a list that usually just fills a picker. Callers
/// who need the raw data use `GET /wire/v1/models`.
///
/// `include_hidden` also lists models with `visibility != "list"` — on the
/// subscription that is `codex-auto-review`, an internal review model. Omitting
/// it by default is a filter, so it is switchable and documented here instead of
/// happening silently.
pub fn models_response(models: &[Value], include_hidden: bool) -> Value {
    let data: Vec<Value> = models
        .iter()
        .filter(|model| {
            include_hidden || model.get("visibility").and_then(Value::as_str) != Some("hide")
        })
        .map(model_object)
        .collect();

    json!({ "object": "list", "data": data })
}

fn model_object(model: &Value) -> Value {
    let slug = model
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let mut object = json!({
        "id": slug,
        "object": "model",
        // The backend states no creation date. `0` rather than `now()`: the
        // number is wrong either way, but at least stable — a moving `now()`
        // would make caches and comparisons differ on every request. Omitting it
        // would be more honest but breaks clients that treat the field as
        // required.
        "created": 0,
        "owned_by": "openai",
    });

    let map = object.as_object_mut().expect("object");

    // Beyond the standard, but harmless: clients ignore what they do not know.
    // `context_length` is the OpenRouter spelling and closes the "context size"
    // gap from KONTEXT-HARNESS.md §6.
    if let Some(name) = model.get("display_name").and_then(Value::as_str) {
        map.insert("display_name".to_string(), json!(name));
    }
    if let Some(window) = model.get("context_window").and_then(Value::as_i64) {
        map.insert("context_length".to_string(), json!(window));
    }
    let levels: Vec<&str> = model
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("effort").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !levels.is_empty() {
        map.insert("reasoning_levels".to_string(), json!(levels));
    }

    object
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend_model(slug: &str, visibility: &str) -> Value {
        json!({
            "slug": slug,
            "display_name": "Display Name",
            "visibility": visibility,
            "context_window": 272_000,
            "supported_reasoning_levels": [
                { "effort": "low" }, { "effort": "high" }
            ],
            // The reason for the projection: large, and unwanted here.
            "model_messages": { "instructions_template": "x".repeat(17_000) },
        })
    }

    #[test]
    fn maps_the_required_fields() {
        let models = [backend_model("gpt-5.6-sol", "list")];
        let response = models_response(&models, false);

        assert_eq!(response["object"], "list");
        let entry = &response["data"][0];
        assert_eq!(entry["id"], "gpt-5.6-sol");
        assert_eq!(entry["object"], "model");
        assert_eq!(entry["owned_by"], "openai");
        assert!(entry.get("created").is_some());
    }

    #[test]
    fn drops_the_huge_prompt() {
        let models = [backend_model("gpt-5.6-sol", "list")];
        let response = models_response(&models, false);
        assert!(response["data"][0].get("model_messages").is_none());
        assert!(serde_json::to_string(&response).unwrap().len() < 500);
    }

    #[test]
    fn hidden_models_only_on_request() {
        let models = [
            backend_model("gpt-5.6-sol", "list"),
            backend_model("codex-auto-review", "hide"),
        ];
        assert_eq!(
            models_response(&models, false)["data"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            models_response(&models, true)["data"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn extra_fields_only_when_known() {
        let sparse = json!({ "slug": "slug-only" });
        let response = models_response(&[sparse], false);
        let entry = &response["data"][0];
        assert_eq!(entry["id"], "slug-only");
        // Nothing invented where the backend says nothing.
        assert!(entry.get("context_length").is_none());
        assert!(entry.get("reasoning_levels").is_none());
        assert!(entry.get("display_name").is_none());
    }
}
