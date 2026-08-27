//! The model catalogue, projected.
//!
//! **Not a pass-through.** The backend objects carry an `instructions_template`
//! of ~17 KB each — 169 KB for a list that usually fills a picker — and the
//! catalogue disagrees with the backend's own behaviour in three measured places
//! (MESSUNGEN.md §14 and §16). Neither belongs in a consumer's contract.
//!
//! Two shapes come out of here:
//!
//! * [`catalogue`] for `/wire/v1/models` — **complete**, every model with a
//!   `hidden` flag, both context values, the corrected reasoning levels.
//! * [`openai_list`] for `/v1/models` — **opinionated**: hidden models dropped,
//!   one context length per entry, and a `:long` variant where the extra budget
//!   is worth offering.
//!
//! # Corrections are a ceiling, never a lift
//!
//! Where a measurement contradicts the catalogue, it may only **cap** the
//! published value. Raising one would promise something the backend does not,
//! and today's measurement is tomorrow's lie. Everything measured is dated in
//! MESSUNGEN.md; only the cap lives here.

use serde_json::Value;
use serde_json::json;

/// Reasoning levels the catalogue advertises that the API then rejects.
///
/// Measured 2026-08-27 against `gpt-5.6-{sol,terra,luna}`: `ultra` is listed for
/// sol and terra, and every request carrying it comes back
/// `400 Invalid value: 'ultra'` — on luna too, which does not even advertise it.
/// Passing it on would put a value into a picker that cannot be picked.
const REJECTED_REASONING_LEVELS: &[&str] = &["ultra"];

/// Accepted by the API but missing from `supported_reasoning_levels`.
///
/// Measured 2026-08-27 across all nine models: eight take `none` and switch
/// reasoning off (`reasoning_output_tokens: 0`). It is the one level a caller
/// cannot otherwise reach, and the fastest one.
///
/// Deliberately not added: `minimal`. The backend names it in its generic error
/// message but answers `Unsupported value: 'minimal' is not supported with the
/// 'gpt-5.6-sol' model` for these models.
const UNADVERTISED_REASONING_LEVEL: &str = "none";

/// Models that reject [`UNADVERTISED_REASONING_LEVEL`].
///
/// `gpt-5.3-codex-spark` answers `Unsupported value: 'none' is not supported with
/// the 'gpt-5.3-codex-spark' model` — for it, advertised and accepted already
/// agree. Keyed by slug on purpose: `supports_reasoning_summary_parameter: false`
/// happens to be set on the same model and is tempting as the rule, but that
/// field is about summaries, not effort levels, and tying them together would be
/// a guess dressed up as a mechanism.
const NONE_UNSUPPORTED_MODELS: &[&str] = &["gpt-5.3-codex-spark"];

/// Caps on `max_context_window` where the catalogue over-promises.
///
/// Measured 2026-08-28 (MESSUNGEN.md §16): `gpt-5.4` declares 1 000 000 and
/// refuses ~950 000. 872 000 is the value the rest of the same tier declares and
/// that four models accepted verbatim (871 963 tokens through on terra, luna,
/// gpt-reserve and codex-auto-review), so it caps without inventing a number.
///
/// Only ever lowers. A model missing here keeps its catalogue value.
const MAX_CONTEXT_CAPS: &[(&str, i64)] = &[("gpt-5.4", 872_000)];

/// Models that get a second `/v1/models` entry for the larger budget.
///
/// Six models would qualify by `max > default`, but a variant doubles a picker
/// entry for **no functional difference** — both ids produce byte-identical
/// requests, since the daemon sends no context field at all. `gpt-5.6-sol` is the
/// workhorse; for everything else `/wire/v1/models` carries
/// `max_context_length` and a consumer can budget it itself.
const LONG_VARIANTS: &[&str] = &["gpt-5.6-sol"];

/// Separator between model and variant. Matches the house convention on the
/// Claude side (`opus:high`).
const VARIANT_SEPARATOR: char = ':';

/// The suffix that selects the larger budget.
const LONG_SUFFIX: &str = "long";

/// Strips a variant suffix, yielding the name the backend knows.
///
/// `gpt-5.6-sol:long` and `gpt-5.6-sol` are the same model — the variant only
/// says which context budget the caller intends to keep to.
pub fn wire_model(id: &str) -> &str {
    id.split_once(VARIANT_SEPARATOR)
        .map_or(id, |(model, _variant)| model)
}

/// `GET /wire/v1/models` — every model, nothing filtered.
pub fn catalogue(models: &[Value]) -> Value {
    json!({ "models": models.iter().map(entry).collect::<Vec<_>>() })
}

fn entry(model: &Value) -> Value {
    let slug = model
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default();
    json!({
        "id": slug,
        "model": slug,
        "display_name": model.get("display_name").and_then(Value::as_str),
        "description": model.get("description").and_then(Value::as_str),
        "hidden": model.get("visibility").and_then(Value::as_str) != Some("list"),
        "input_modalities": model.get("input_modalities").cloned().unwrap_or(json!([])),
        "default_context_length": context_default(model),
        "max_context_length": context_max(model),
        "reasoning": {
            "levels": reasoning_levels(model),
            "default": model.get("default_reasoning_level").and_then(Value::as_str),
            // Absent means supported: only the one model that cannot says so.
            "summary_supported": model
                .get("supports_reasoning_summary_parameter")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        },
    })
}

/// What Codex itself runs with.
pub fn context_default(model: &Value) -> Option<i64> {
    model.get("context_window").and_then(Value::as_i64)
}

/// The usable ceiling: the catalogue's, capped where it over-promises.
pub fn context_max(model: &Value) -> Option<i64> {
    let slug = model
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let declared = model
        .get("max_context_window")
        .and_then(Value::as_i64)
        .or_else(|| context_default(model))?;
    let cap = MAX_CONTEXT_CAPS
        .iter()
        .find(|(name, _)| *name == slug)
        .map(|(_, cap)| *cap);
    Some(cap.map_or(declared, |cap| declared.min(cap)))
}

/// The corrected level list — see the constants for what moves and why.
pub fn reasoning_levels(model: &Value) -> Vec<&str> {
    let slug = model
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
    if advertised.is_empty() {
        // A model that advertises nothing keeps advertising nothing: `none` on
        // its own would be an invention.
        return Vec::new();
    }
    let takes_none = !NONE_UNSUPPORTED_MODELS.contains(&slug);
    takes_none
        .then_some(UNADVERTISED_REASONING_LEVEL)
        .into_iter()
        .chain(
            advertised
                .into_iter()
                .filter(|level| !REJECTED_REASONING_LEVELS.contains(level)),
        )
        .collect()
}

/// Does this model get a `:long` entry, and is there anything to gain?
fn long_variant(model: &Value) -> Option<i64> {
    let slug = model.get("slug").and_then(Value::as_str)?;
    if !LONG_VARIANTS.contains(&slug) {
        return None;
    }
    let (default, max) = (context_default(model)?, context_max(model)?);
    (max > default).then_some(max)
}

/// `GET /v1/models` — hidden models dropped, one context length per entry.
pub fn openai_list(models: &[Value]) -> Value {
    let mut data = Vec::new();
    for model in models
        .iter()
        .filter(|model| model.get("visibility").and_then(Value::as_str) == Some("list"))
    {
        let Some(slug) = model.get("slug").and_then(Value::as_str) else {
            continue;
        };
        data.push(openai_entry(model, slug, context_default(model)));
        if let Some(max) = long_variant(model) {
            let id = format!("{slug}{VARIANT_SEPARATOR}{LONG_SUFFIX}");
            data.push(openai_entry(model, &id, Some(max)));
        }
    }
    json!({ "object": "list", "data": data })
}

fn openai_entry(model: &Value, id: &str, context_length: Option<i64>) -> Value {
    let mut object = json!({
        "id": id,
        "object": "model",
        // The backend names no date, and a moving `now()` would be worse than an
        // honestly wrong constant — it would make every response differ.
        "created": 0,
        "owned_by": "openai",
    });
    let map = object.as_object_mut().expect("object");
    if let Some(name) = model.get("display_name").and_then(Value::as_str) {
        map.insert("display_name".to_string(), json!(name));
    }
    if let Some(length) = context_length {
        map.insert("context_length".to_string(), json!(length));
    }
    let levels = reasoning_levels(model);
    if !levels.is_empty() {
        map.insert("reasoning_levels".to_string(), json!(levels));
    }
    object
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real catalogue, verbatim except for the ~17 KB instruction templates,
    /// which are stubbed to a marker so the projection can be shown to drop them.
    const CATALOGUE: &str = include_str!("../fixtures/models.json");

    fn models() -> Vec<Value> {
        serde_json::from_str(CATALOGUE).expect("fixture")
    }

    fn find<'a>(list: &'a Value, id: &str) -> Option<&'a Value> {
        list.as_array()?.iter().find(|entry| entry["id"] == id)
    }

    #[test]
    fn the_catalogue_keeps_every_model_and_flags_the_hidden_ones() {
        let wire = catalogue(&models());
        let list = &wire["models"];

        assert_eq!(list.as_array().map(Vec::len), Some(9));
        assert_eq!(find(list, "gpt-5.6-sol").unwrap()["hidden"], false);
        assert_eq!(find(list, "gpt-reserve").unwrap()["hidden"], true);
        assert_eq!(find(list, "codex-auto-review").unwrap()["hidden"], true);
    }

    /// The reason the projection exists: 169 KB of instruction templates for a
    /// list that fills a picker.
    #[test]
    fn the_catalogue_drops_the_instruction_templates() {
        let wire = serde_json::to_string(&catalogue(&models())).unwrap();
        assert!(!wire.contains("instructions_template"));
        assert!(wire.len() < 8_000, "was {} bytes", wire.len());
    }

    #[test]
    fn both_context_values_are_reported() {
        let wire = catalogue(&models());
        let sol = find(&wire["models"], "gpt-5.6-sol").unwrap();

        assert_eq!(sol["default_context_length"], 272_000);
        assert_eq!(sol["max_context_length"], 872_000);
        // No derived threshold: when to compact is the consumer's policy, and
        // Codex' 90 % rule is Codex' business. The two numbers above suffice.
        assert!(sol.get("compact_at").is_none());
    }

    /// The one correction. `gpt-5.4` declares a million and refuses ~950 000.
    #[test]
    fn an_over_promised_maximum_is_capped() {
        let wire = catalogue(&models());
        let five_four = find(&wire["models"], "gpt-5.4").unwrap();

        assert_eq!(
            five_four["max_context_length"], 872_000,
            "capped, not 1000000"
        );
        // Everyone else keeps the catalogue's word.
        assert_eq!(
            find(&wire["models"], "gpt-5.6-terra").unwrap()["max_context_length"],
            872_000
        );
        assert_eq!(
            find(&wire["models"], "gpt-5.5").unwrap()["max_context_length"],
            272_000
        );
    }

    /// A correction may only lower. Raising one would promise what the backend
    /// does not, and a measurement ages worse than a declaration.
    #[test]
    fn a_cap_never_raises() {
        for model in models() {
            let declared = model["max_context_window"].as_i64();
            let served = context_max(&model);
            if let (Some(declared), Some(served)) = (declared, served) {
                assert!(served <= declared, "{} was raised", model["slug"]);
            }
        }
    }

    #[test]
    fn reasoning_levels_match_what_the_api_accepts() {
        let list = models();
        let sol = list.iter().find(|m| m["slug"] == "gpt-5.6-sol").unwrap();
        let spark = list
            .iter()
            .find(|m| m["slug"] == "gpt-5.3-codex-spark")
            .unwrap();

        // `ultra` is advertised for sol and rejected by the backend; `none` is
        // accepted and not advertised.
        assert_eq!(
            reasoning_levels(sol),
            ["none", "low", "medium", "high", "xhigh", "max"]
        );
        // Spark is the one model that refuses `none`.
        assert_eq!(reasoning_levels(spark), ["low", "medium", "high", "xhigh"]);
    }

    #[test]
    fn a_model_without_levels_gets_none_invented() {
        let mut model = models()[0].clone();
        model
            .as_object_mut()
            .unwrap()
            .remove("supported_reasoning_levels");
        assert!(reasoning_levels(&model).is_empty());
    }

    #[test]
    fn the_openai_list_hides_what_the_catalogue_flags() {
        let list = openai_list(&models());
        assert_eq!(list["object"], "list");

        assert!(find(&list["data"], "gpt-5.6-sol").is_some());
        assert!(find(&list["data"], "gpt-reserve").is_none());
        assert!(find(&list["data"], "codex-auto-review").is_none());
    }

    /// The variant is a budget label, so it exists only where it buys something
    /// and only for the model people actually run.
    #[test]
    fn only_sol_gets_a_long_variant() {
        let list = openai_list(&models());
        let data = &list["data"];

        assert_eq!(
            find(data, "gpt-5.6-sol").unwrap()["context_length"],
            272_000
        );
        assert_eq!(
            find(data, "gpt-5.6-sol:long").unwrap()["context_length"],
            872_000
        );

        // Would qualify by `max > default`, but is not offered.
        assert!(find(data, "gpt-5.6-terra:long").is_none());
        assert!(find(data, "gpt-5.4:long").is_none());
        // No headroom at all — a variant would be pure noise.
        assert!(find(data, "gpt-5.5:long").is_none());
        assert!(find(data, "gpt-5.3-codex-spark:long").is_none());
    }

    /// Both ids reach the same model: the daemon sends no context field, so the
    /// requests are byte-identical.
    #[test]
    fn the_variant_suffix_is_stripped_for_the_backend() {
        assert_eq!(wire_model("gpt-5.6-sol:long"), "gpt-5.6-sol");
        assert_eq!(wire_model("gpt-5.6-sol"), "gpt-5.6-sol");
        assert_eq!(wire_model("gpt-5.3-codex-spark"), "gpt-5.3-codex-spark");
    }
}
