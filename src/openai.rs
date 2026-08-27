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

/// Reasoning levels `/models` advertises that the API then rejects.
///
/// Measured 2026-08-27 against `gpt-5.6-{sol,terra,luna}`: `ultra` is listed in
/// `supported_reasoning_levels` for sol and terra, and every request carrying it
/// comes back `400 Invalid value: 'ultra'`. Passing it on would put a value into
/// a model picker that cannot be picked — the same trap the tool list avoids by
/// rejecting unsupported types outright instead of dropping them silently.
const REJECTED_REASONING_LEVELS: &[&str] = &["ultra"];

/// Accepted by the API but missing from `supported_reasoning_levels`.
///
/// Measured 2026-08-27 across **all nine** models the subscription serves: eight
/// take `none` and switch reasoning off (`reasoning_output_tokens: 0`). It is the
/// one level a caller cannot otherwise reach, and the fastest one — worth listing.
///
/// Deliberately not added: `minimal`. The backend names it in its generic error
/// message, but answers `Unsupported value: 'minimal' is not supported with the
/// 'gpt-5.6-sol' model` for these models.
const UNADVERTISED_REASONING_LEVEL: &str = "none";

/// Models that reject [`UNADVERTISED_REASONING_LEVEL`].
///
/// The ninth model. `gpt-5.3-codex-spark` answers `Unsupported value: 'none' is
/// not supported with the 'gpt-5.3-codex-spark' model. Supported values are:
/// 'low', 'medium', 'high', and 'xhigh'` — for it, advertised and accepted
/// already agree, so nothing is added.
///
/// Keyed by slug on purpose. `supports_reasoning_summary_parameter: false` is set
/// on exactly this model too, and tempting to use as the rule — but that field is
/// about summaries, not about effort levels, and tying the two together would be
/// a guess dressed up as a mechanism. A measured list of slugs says what it is:
/// an observation with a date, to be re-checked when models change.
const NONE_UNSUPPORTED_MODELS: &[&str] = &["gpt-5.3-codex-spark"];

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
    // Corrected against what the API actually takes, not passed through: the
    // backend's own catalogue disagrees with its own validation in both
    // directions. See the two constants above for the measurements. A model that
    // advertises nothing keeps advertising nothing — `none` alone would be an
    // invention, and this module invents nothing.
    let advertised: Vec<&str> = model
        .get("supported_reasoning_levels")
        .and_then(Value::as_array)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|entry| entry.get("effort").and_then(Value::as_str))
                .collect()
        })
        .unwrap_or_default();
    if !advertised.is_empty() {
        let takes_none = !NONE_UNSUPPORTED_MODELS.contains(&slug);
        // `none` first: the list stays ordered by ascending effort.
        let levels: Vec<&str> = takes_none
            .then_some(UNADVERTISED_REASONING_LEVEL)
            .into_iter()
            .chain(
                advertised
                    .into_iter()
                    .filter(|level| !REJECTED_REASONING_LEVELS.contains(level)),
            )
            .collect();
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
                { "effort": "low" }, { "effort": "high" }, { "effort": "ultra" }
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

    /// The backend's catalogue disagrees with its own validation. A picker fed
    /// straight from it offers `ultra`, which every request carrying it answers
    /// with a 400, and hides `none`, which works.
    #[test]
    fn reasoning_levels_match_what_the_api_accepts() {
        let models = [backend_model("gpt-5.6-sol", "list")];
        let response = models_response(&models, false);
        let levels = response["data"][0]["reasoning_levels"].clone();

        assert_eq!(levels, json!(["none", "low", "high"]));
        assert!(
            !levels.as_array().unwrap().iter().any(|l| l == "ultra"),
            "ultra is advertised by the backend but rejected by it"
        );
    }

    /// Measured 2026-08-27: `gpt-5.3-codex-spark` is the one model that rejects
    /// `none`. Adding it there would put the same unusable entry into the picker
    /// that removing `ultra` was meant to get rid of.
    #[test]
    fn none_is_left_off_where_the_model_rejects_it() {
        let models = [
            backend_model("gpt-5.6-sol", "list"),
            backend_model("gpt-5.3-codex-spark", "list"),
        ];
        let data = models_response(&models, false);
        assert_eq!(
            data["data"][0]["reasoning_levels"],
            json!(["none", "low", "high"])
        );
        assert_eq!(data["data"][1]["reasoning_levels"], json!(["low", "high"]));
    }

    /// A model that advertises no levels keeps advertising none. Listing `none`
    /// on its own would be an invention.
    #[test]
    fn a_model_without_levels_gets_no_list() {
        let mut model = backend_model("gpt-5.6-sol", "list");
        model
            .as_object_mut()
            .unwrap()
            .remove("supported_reasoning_levels");
        let response = models_response(&[model], false);
        assert!(response["data"][0].get("reasoning_levels").is_none());
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
