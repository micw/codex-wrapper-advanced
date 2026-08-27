//! Quota groups, in one shape from two sources.
//!
//! The backend reports quotas twice, with different strengths: as **response
//! headers of every turn** (free, and the only source that says *which* group the
//! turn charged) and via **`wham/usage`** (a request of its own, but always
//! complete and the only source of "limit reached?").
//!
//! Both are projected into the same [`Limits`] here so a consumer never has to
//! know which one an object came from. The measurements behind the projection are
//! in MESSUNGEN.md §13 and §15; the outward contract is in the README under
//! "Kontingente — ein Format aus zwei Quellen".
//!
//! # The one rule worth understanding
//!
//! Header families are keyed by limit id: `x-codex-…` is the family `codex`,
//! `x-codex-bengalfox-…` the family `codex_bengalfox`. The family `codex` is not
//! the account limit — **it is whichever limit is active for this request**. On a
//! turn against `gpt-5.3-codex-spark` it carries the same numbers as
//! `codex_bengalfox`, byte for byte.
//!
//! `x-codex-active-limit` names the active group, and that resolves it:
//!
//! * names a family that is present → `codex` is that family's copy, drop it;
//! * names nothing we have → `codex` is the only carrier of the active limit, and
//!   that limit has no name. That is what we call [`GLOBAL`].
//!
//! Decided on the **name**, never on equal values: two genuinely different groups
//! could coincide in window size, and one of them would vanish.
//!
//! The value itself (`premium`, measured identical on Free, Plus and ProLite) is
//! never interpreted, only compared — a future `standard` changes nothing.
//!
//! **Known gap:** if `active-limit` named a group whose family is *not* sent, that
//! limit would be labelled `global`. Never observed, and it would be a wrong
//! label on right numbers.

use std::collections::BTreeMap;

use http::HeaderMap;
use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// Our token for the group the backend leaves nameless.
///
/// Deliberately not `codex`, which *is* the backend's id for that header family:
/// there, `codex` means "whatever is active", so reusing it would import the
/// ambiguity this module resolves.
pub const GLOBAL: &str = "global";

/// The header family that carries the active limit.
const DEFAULT_FAMILY: &str = "codex";

/// The header every family is discovered by. A family without it is invisible —
/// that is upstream's rule, mirrored here.
const DISCOVERY_SUFFIX: &str = "-primary-used-percent";

/// One quota window. `used_percent` is the only field the backend always sends.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    pub used_percent: f64,
    /// Normalised: headers count minutes, the usage API seconds.
    pub window_seconds: Option<i64>,
    pub resets_at: Option<i64>,
    /// Remaining time as the backend counts it — independent of the local clock.
    pub resets_in_seconds: Option<i64>,
}

/// One quota group. `primary` and `secondary` both bind when both are present.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    /// [`GLOBAL`] or the backend's own id (`metered_feature` / limit id).
    pub id: String,
    /// `None` is structural: the global group has no name in either source.
    pub name: Option<String>,
    /// `None` is unknown — only the usage API answers this.
    pub reached: Option<bool>,
    pub primary: Option<Window>,
    /// `None` is structural: this group has no second window.
    pub secondary: Option<Window>,
}

/// Quotas. `None` means unknown everywhere except where a doc comment says the
/// absence is structural.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Limits {
    /// Which group the turn charged. Only the headers know; the usage API never
    /// says, and neither do we without the response headers.
    pub active_group: Option<String>,
    /// Active group first, the rest by id.
    pub groups: Vec<Group>,
}

/// The plan as **the backend reports it**, which can differ from the token's
/// claim in `/wire/v1/whoami`: that one is fixed when the token is issued.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    /// The raw value, always.
    pub id: String,
    /// `KnownPlan::display_name`, or `None` for a plan the enum does not know —
    /// an unknown tariff arrives without a label rather than with a made-up one.
    pub name: Option<String>,
}

/// Prepaid credit for overage — **not** what is left of the quota, which is
/// `100 - used_percent`. The last three fields exist only in the usage API.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Credits {
    pub has_credits: Option<bool>,
    pub unlimited: Option<bool>,
    /// A string, not a number — the backend avoids rounding money. Measured, it
    /// arrives as `"0"` on one account and empty on another in the same state, so
    /// `None` does not cleanly mean "no credit"; the backend conflates the two.
    pub balance: Option<String>,
    pub overage_limit_reached: Option<bool>,
    pub approx_cloud_messages: Option<Value>,
    pub approx_local_messages: Option<Value>,
}

/// Account-wide state. Deliberately without identity: `account_id`, `user_id` and
/// `email` sit in the token and therefore in `/wire/v1/whoami`, without a request.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Account {
    pub plan: Option<Plan>,
    pub credits: Option<Credits>,
    pub spend_control: Option<Value>,
    pub reset_credits: Option<Value>,
}

/// What a turn's headers carry.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct TurnLimits {
    pub account: Account,
    pub limits: Limits,
}

/// What `GET /wire/v1/usage` carries.
///
/// `promo` sits outside [`Account`] on purpose: there it holds marketing copy, and
/// `null` would mean "no campaign" from the usage API but "unknown" from a turn.
/// As a key that only exists here it cannot be misread.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct UsageReport {
    pub account: Account,
    pub limits: Limits,
    pub promo: Option<Value>,
}

// --- From response headers --------------------------------------------------

impl TurnLimits {
    /// Projects a turn's response headers.
    ///
    /// Without `x-codex-active-limit` the default family cannot be resolved. It
    /// then keeps the backend's own id `codex` and `active_group` stays `None` —
    /// unknown, rather than a guess at `global`.
    pub fn from_headers(headers: &HeaderMap) -> Self {
        let mut families: BTreeMap<String, Group> = BTreeMap::new();
        for id in family_ids(headers) {
            if let Some(group) = read_family(headers, &id) {
                families.insert(id, group);
            }
        }

        let active = header_str(headers, "x-codex-active-limit").map(normalize_id);
        let names_a_family = active
            .as_ref()
            .is_some_and(|active| active != DEFAULT_FAMILY && families.contains_key(active));

        let active_group = if names_a_family {
            // The default family is that group's copy.
            families.remove(DEFAULT_FAMILY);
            active
        } else if let Some(mut group) = families.remove(DEFAULT_FAMILY) {
            match active {
                // Nothing we have is named, so the active limit is the nameless
                // one: the account's own.
                Some(_) => {
                    group.id = GLOBAL.to_string();
                    families.insert(GLOBAL.to_string(), group);
                    Some(GLOBAL.to_string())
                }
                // No `active-limit` header: unresolvable, so say so.
                None => {
                    families.insert(DEFAULT_FAMILY.to_string(), group);
                    None
                }
            }
        } else {
            active.filter(|active| families.contains_key(active))
        };

        Self {
            account: Account {
                plan: header_str(headers, "x-codex-plan-type").map(plan),
                credits: read_credits(headers),
                spend_control: None,
                reset_credits: None,
            },
            limits: Limits {
                groups: ordered(families, active_group.as_deref()),
                active_group,
            },
        }
    }
}

/// Every limit id the headers mention, via the discovery anchor.
fn family_ids(headers: &HeaderMap) -> Vec<String> {
    headers
        .keys()
        .filter_map(|name| {
            let name = name.as_str().to_ascii_lowercase();
            let prefix = name.strip_suffix(DISCOVERY_SUFFIX)?;
            Some(normalize_id(prefix.strip_prefix("x-")?.to_string()))
        })
        .collect()
}

fn read_family(headers: &HeaderMap, id: &str) -> Option<Group> {
    let prefix = format!("x-{}", id.replace('_', "-"));
    let primary = read_window(headers, &prefix, "primary");
    let secondary = read_window(headers, &prefix, "secondary");
    if primary.is_none() && secondary.is_none() {
        return None;
    }
    Some(Group {
        id: id.to_string(),
        name: header_str(headers, &format!("{prefix}-limit-name")),
        reached: None,
        primary,
        secondary,
    })
}

/// Mirrors upstream's rule: a window exists once `used_percent` parses and any of
/// the three values is non-trivial. A free account sends `secondary` as all
/// zeroes with an empty `reset-at` — that is "no second window", not a window at
/// zero.
fn read_window(headers: &HeaderMap, prefix: &str, which: &str) -> Option<Window> {
    let used_percent = header_f64(headers, &format!("{prefix}-{which}-used-percent"))?;
    let window_minutes = header_i64(headers, &format!("{prefix}-{which}-window-minutes"));
    let resets_at = header_i64(headers, &format!("{prefix}-{which}-reset-at"));

    let has_data = used_percent != 0.0
        || window_minutes.is_some_and(|minutes| minutes != 0)
        || resets_at.is_some();

    has_data.then(|| Window {
        used_percent,
        window_seconds: window_minutes.map(|minutes| minutes * 60),
        resets_at,
        resets_in_seconds: header_i64(headers, &format!("{prefix}-{which}-reset-after-seconds")),
    })
}

fn read_credits(headers: &HeaderMap) -> Option<Credits> {
    let credits = Credits {
        has_credits: header_bool(headers, "x-codex-credits-has-credits"),
        unlimited: header_bool(headers, "x-codex-credits-unlimited"),
        balance: header_str(headers, "x-codex-credits-balance"),
        ..Credits::default()
    };
    (credits != Credits::default()).then_some(credits)
}

// --- From the usage API -----------------------------------------------------

impl UsageReport {
    /// Projects the raw `wham/usage` body.
    pub fn from_usage(body: &Value) -> Self {
        let mut groups = Vec::new();
        if let Some(group) = usage_group(GLOBAL, None, body.get("rate_limit")) {
            groups.push(group);
        }
        for extra in body
            .get("additional_rate_limits")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(id) = extra.get("metered_feature").and_then(Value::as_str) else {
                continue;
            };
            let name = extra.get("limit_name").and_then(Value::as_str);
            if let Some(group) = usage_group(id, name, extra.get("rate_limit")) {
                groups.push(group);
            }
        }

        Self {
            account: Account {
                plan: body
                    .get("plan_type")
                    .and_then(Value::as_str)
                    .map(|raw| plan(raw.to_string())),
                credits: body.get("credits").map(usage_credits),
                spend_control: cloned(body.get("spend_control")),
                reset_credits: cloned(body.get("rate_limit_reset_credits")),
            },
            limits: Limits {
                // The usage API never says which group is active.
                active_group: None,
                groups,
            },
            promo: cloned(body.get("promo")),
        }
    }
}

fn usage_group(id: &str, name: Option<&str>, limit: Option<&Value>) -> Option<Group> {
    let limit = limit.filter(|limit| !limit.is_null())?;
    let primary = usage_window(limit.get("primary_window"));
    let secondary = usage_window(limit.get("secondary_window"));
    if primary.is_none() && secondary.is_none() {
        return None;
    }
    Some(Group {
        id: id.to_string(),
        name: name.map(str::to_string),
        // Reported per limit, not per window — which is why it sits here.
        reached: limit.get("limit_reached").and_then(Value::as_bool),
        primary,
        secondary,
    })
}

fn usage_window(window: Option<&Value>) -> Option<Window> {
    let window = window.filter(|window| !window.is_null())?;
    Some(Window {
        used_percent: window
            .get("used_percent")
            .and_then(Value::as_f64)
            .unwrap_or(0.0),
        window_seconds: window.get("limit_window_seconds").and_then(Value::as_i64),
        resets_at: window.get("reset_at").and_then(Value::as_i64),
        resets_in_seconds: window.get("reset_after_seconds").and_then(Value::as_i64),
    })
}

fn usage_credits(credits: &Value) -> Credits {
    Credits {
        has_credits: credits.get("has_credits").and_then(Value::as_bool),
        unlimited: credits.get("unlimited").and_then(Value::as_bool),
        balance: credits
            .get("balance")
            .and_then(Value::as_str)
            .map(str::to_string),
        overage_limit_reached: credits
            .get("overage_limit_reached")
            .and_then(Value::as_bool),
        approx_cloud_messages: cloned(credits.get("approx_cloud_messages")),
        approx_local_messages: cloned(credits.get("approx_local_messages")),
    }
}

// --- Helpers ----------------------------------------------------------------

/// Active group first, the rest by id — a stable order a consumer can rely on.
fn ordered(families: BTreeMap<String, Group>, active: Option<&str>) -> Vec<Group> {
    let mut groups: Vec<Group> = families.into_values().collect();
    groups.sort_by(|a, b| {
        let rank = |group: &Group| u8::from(Some(group.id.as_str()) != active);
        rank(a).cmp(&rank(b)).then_with(|| a.id.cmp(&b.id))
    });
    groups
}

fn plan(raw: String) -> Plan {
    use codex_protocol::auth::PlanType;
    let name = match PlanType::from_raw_value(&raw) {
        PlanType::Known(known) => Some(known.display_name().to_string()),
        PlanType::Unknown(_) => None,
    };
    Plan { id: raw, name }
}

fn normalize_id(name: impl Into<String>) -> String {
    name.into().trim().to_ascii_lowercase().replace('-', "_")
}

fn cloned(value: Option<&Value>) -> Option<Value> {
    value.filter(|value| !value.is_null()).cloned()
}

fn header_str(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(name)?.to_str().ok()?.trim();
    (!raw.is_empty()).then(|| raw.to_string())
}

fn header_f64(headers: &HeaderMap, name: &str) -> Option<f64> {
    header_str(headers, name)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn header_i64(headers: &HeaderMap, name: &str) -> Option<i64> {
    header_str(headers, name)?.parse::<i64>().ok()
}

fn header_bool(headers: &HeaderMap, name: &str) -> Option<bool> {
    let raw = header_str(headers, name)?;
    match raw.to_ascii_lowercase().as_str() {
        "true" | "1" => Some(true),
        "false" | "0" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderName;
    use http::HeaderValue;
    use serde_json::json;

    /// The fixtures are verbatim captures from the live backend — two accounts,
    /// three turns. Written out rather than typed into the tests so the shapes
    /// stay comparable with what MESSUNGEN.md quotes.
    fn headers(fixture: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        for line in fixture.lines() {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.insert(
                name.trim().parse::<HeaderName>().expect("header name"),
                HeaderValue::from_str(value.trim()).expect("header value"),
            );
        }
        headers
    }

    const SOL: &str = include_str!("../fixtures/headers-prolite-sol.txt");
    const SPARK: &str = include_str!("../fixtures/headers-prolite-spark.txt");
    const FREE: &str = include_str!("../fixtures/headers-free-terra.txt");
    const USAGE_PROLITE: &str = include_str!("../fixtures/usage-prolite.json");
    const USAGE_FREE: &str = include_str!("../fixtures/usage-free.json");

    fn ids(limits: &Limits) -> Vec<&str> {
        limits.groups.iter().map(|g| g.id.as_str()).collect()
    }

    /// `active-limit: premium` names no family, so the default family is the
    /// nameless account limit — and the Spark group rides along untouched.
    #[test]
    fn sol_turn_resolves_the_default_family_to_global() {
        let turn = TurnLimits::from_headers(&headers(SOL));

        assert_eq!(turn.limits.active_group.as_deref(), Some(GLOBAL));
        assert_eq!(ids(&turn.limits), ["global", "codex_bengalfox"]);

        let global = &turn.limits.groups[0];
        assert_eq!(
            global.name, None,
            "the account limit is nameless everywhere"
        );
        assert_eq!(
            global.primary,
            Some(Window {
                used_percent: 0.0,
                window_seconds: Some(604_800), // 10080 min normalised
                resets_at: Some(1_788_457_743),
                resets_in_seconds: Some(604_372),
            })
        );
        assert_eq!(global.secondary, None, "no second window on this account");

        let spark = &turn.limits.groups[1];
        assert_eq!(spark.name.as_deref(), Some("GPT-5.3-Codex-Spark"));
        assert_eq!(spark.primary.as_ref().unwrap().window_seconds, Some(18_000));
        assert_eq!(
            spark.secondary.as_ref().unwrap().window_seconds,
            Some(604_800)
        );
    }

    /// The case the whole module exists for: on a Spark turn the default family
    /// is a byte-identical copy of the named one. Kept, it would be counted twice.
    #[test]
    fn spark_turn_drops_the_duplicated_default_family() {
        let turn = TurnLimits::from_headers(&headers(SPARK));

        assert_eq!(turn.limits.active_group.as_deref(), Some("codex_bengalfox"));
        assert_eq!(ids(&turn.limits), ["codex_bengalfox"]);
        assert_eq!(
            turn.limits.groups[0].name.as_deref(),
            Some("GPT-5.3-Codex-Spark")
        );
        // And the account limit is simply not in this response.
        assert!(!ids(&turn.limits).contains(&GLOBAL));
    }

    /// A free account has exactly one family, and a 30-day window instead of 7.
    #[test]
    fn free_turn_has_a_single_group() {
        let turn = TurnLimits::from_headers(&headers(FREE));

        assert_eq!(turn.limits.active_group.as_deref(), Some(GLOBAL));
        assert_eq!(ids(&turn.limits), ["global"]);
        assert_eq!(
            turn.limits.groups[0]
                .primary
                .as_ref()
                .unwrap()
                .window_seconds,
            Some(2_592_000)
        );
        assert_eq!(
            turn.limits.groups[0].secondary, None,
            "all-zero secondary headers mean no window, not a window at zero"
        );
        assert_eq!(turn.account.plan.as_ref().unwrap().id, "free");
        assert_eq!(
            turn.account.plan.as_ref().unwrap().name.as_deref(),
            Some("Free")
        );
    }

    /// Without the header the default family cannot be resolved. Saying so beats
    /// guessing at `global`.
    #[test]
    fn without_active_limit_the_default_family_stays_unresolved() {
        let mut map = headers(SOL);
        map.remove("x-codex-active-limit");
        let turn = TurnLimits::from_headers(&map);

        assert_eq!(turn.limits.active_group, None);
        assert!(ids(&turn.limits).contains(&"codex"));
        assert!(!ids(&turn.limits).contains(&GLOBAL));
    }

    /// Resolution reads the name, never the numbers. Here the two families carry
    /// identical windows but different ids — both must survive.
    #[test]
    fn equal_windows_alone_do_not_drop_a_group() {
        let mut map = headers(SPARK);
        map.insert(
            "x-codex-active-limit",
            HeaderValue::from_static("something-else"),
        );
        let turn = TurnLimits::from_headers(&map);

        assert_eq!(ids(&turn.limits), ["global", "codex_bengalfox"]);
        assert_eq!(turn.limits.active_group.as_deref(), Some(GLOBAL));
    }

    #[test]
    fn credits_come_from_the_headers_too() {
        let sol = TurnLimits::from_headers(&headers(SOL)).account.credits;
        assert_eq!(sol.as_ref().unwrap().balance.as_deref(), Some("0"));
        assert_eq!(sol.as_ref().unwrap().has_credits, Some(false));
        // Only the usage API has these.
        assert_eq!(sol.as_ref().unwrap().overage_limit_reached, None);

        // An empty balance header is not "0" — the backend sends both.
        let free = TurnLimits::from_headers(&headers(FREE)).account.credits;
        assert_eq!(free.unwrap().balance, None);
    }

    /// Same shape from the other source — and `reached`, which only it answers.
    #[test]
    fn usage_projects_into_the_same_shape() {
        let report = UsageReport::from_usage(&serde_json::from_str(USAGE_PROLITE).unwrap());

        assert_eq!(report.limits.active_group, None, "usage never says which");
        assert_eq!(ids(&report.limits), ["global", "codex_bengalfox"]);
        assert_eq!(report.limits.groups[0].reached, Some(false));
        assert_eq!(
            report.limits.groups[0]
                .primary
                .as_ref()
                .unwrap()
                .window_seconds,
            Some(604_800)
        );
        assert_eq!(
            report.limits.groups[1].name.as_deref(),
            Some("GPT-5.3-Codex-Spark")
        );
        assert_eq!(
            report.account.plan.as_ref().unwrap().name.as_deref(),
            Some("Pro Lite")
        );
        assert_eq!(report.promo, None);
    }

    /// The free account's usage body is where `null` instead of `[]` bites.
    #[test]
    fn usage_survives_a_null_additional_list() {
        let report = UsageReport::from_usage(&serde_json::from_str(USAGE_FREE).unwrap());

        assert_eq!(ids(&report.limits), ["global"]);
        assert_eq!(report.account.plan.as_ref().unwrap().id, "free");
        assert_eq!(report.account.credits.unwrap().balance, None);
        assert_eq!(
            report.promo.as_ref().and_then(|p| p.get("campaign_id")),
            Some(&json!("plus-1-month-free")),
            "promo is populated here, and only the usage API carries it"
        );
    }

    /// The window of the same group must come out equal from both sources.
    #[test]
    fn both_sources_agree_on_the_spark_group() {
        let from_turn = TurnLimits::from_headers(&headers(SOL));
        let from_usage = UsageReport::from_usage(&serde_json::from_str(USAGE_PROLITE).unwrap());

        let turn_spark = from_turn
            .limits
            .groups
            .iter()
            .find(|g| g.id == "codex_bengalfox")
            .unwrap();
        let usage_spark = from_usage
            .limits
            .groups
            .iter()
            .find(|g| g.id == "codex_bengalfox")
            .unwrap();

        assert_eq!(turn_spark.name, usage_spark.name);
        assert_eq!(
            turn_spark.primary.as_ref().unwrap().window_seconds,
            usage_spark.primary.as_ref().unwrap().window_seconds
        );
        assert_eq!(
            turn_spark.secondary.as_ref().unwrap().resets_at,
            usage_spark.secondary.as_ref().unwrap().resets_at
        );
    }

    /// An unknown tariff arrives without a label instead of with a made-up one.
    #[test]
    fn an_unknown_plan_gets_no_name() {
        let resolved = plan("prolite".to_string());
        assert_eq!(resolved.name.as_deref(), Some("Pro Lite"));

        let unknown = plan("tarif-von-morgen".to_string());
        assert_eq!(unknown.id, "tarif-von-morgen");
        assert_eq!(unknown.name, None);
    }
}
