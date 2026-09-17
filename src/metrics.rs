//! In-memory metrics for the running process.
//!
//! Modelled on the Claude wrapper's `/metrics`, minus everything that has no
//! counterpart here: there is no CLI process, so no spawn time, no CLI-internal
//! duration and no pool-savable overhead. What is left is what this daemon can
//! actually observe — latency, outcomes, tokens and, above all, the **cache hit
//! rate**.
//!
//! Nothing is persisted. A restart starts at zero, which is the honest reading:
//! these numbers describe the running process, not the account.
//!
//! # Why the cache hit rate sits in its own block
//!
//! It is the number this daemon is judged by. `cached_input_tokens` is contained
//! in `input_tokens` (upstream computes `non_cached = input - cached`), so the
//! ratio of the two is directly comparable with the Claude wrapper's, which sums
//! its own the same way.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Instant;

use serde_json::Value;
use serde_json::json;

use crate::limits::Group;
use crate::limits::TurnLimits;
use crate::wire::Event;
use crate::wire::Usage;

/// Latency samples kept per band. Bounded so a long-running process cannot grow
/// without limit; percentiles over the most recent 1000 turns say more about the
/// current state than an average over all of history.
pub const WINDOW: usize = 1000;

/// Which surface a turn came in through. `&'static str` rather than an enum:
/// the value only ever ends up as a JSON key.
pub const SURFACE_CHAT: &str = "chat_completions";
pub const SURFACE_RESPONSES: &str = "responses";
pub const SURFACE_WIRE: &str = "wire";

pub struct Metrics {
    started: Instant,
    window: usize,
    inner: Mutex<Inner>,
}

#[derive(Default)]
struct Inner {
    total_requests: u64,
    inflight: u64,
    outcomes: HashMap<String, u64>,
    /// Upstream status codes of turns rejected before the stream began. Keeps
    /// the 429 from the quota separable from the 401 of an expired token.
    rejections: HashMap<u16, u64>,
    surfaces: HashMap<String, u64>,
    models: HashMap<String, ModelStats>,
    credits: f64,
    credit_observations: u64,
    quota_groups: HashMap<String, ObservedQuota>,
    plan: Option<String>,
    tokens: Tokens,
    total_ms: VecDeque<f64>,
    ttft_ms: VecDeque<f64>,
    upstream_connect_ms: VecDeque<f64>,
    first_event_ms: VecDeque<f64>,
    max_event_gap_ms: VecDeque<f64>,
    request_decode_ms: VecDeque<f64>,
    request_encoded_bytes: u64,
    request_decoded_bytes: u64,
    compressed_requests: u64,
    /// The most recent turn's quota, resolved.
    ///
    /// One object, not a list: since 1.3.0 the daemon builds a single event per
    /// turn from the response headers, with the groups told apart and the
    /// duplicated default family already dropped (see [`crate::limits`]).
    limits: Option<TurnLimits>,
    limits_updated_at: Option<i64>,
}

#[derive(Default)]
struct Tokens {
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
    total: i64,
}

#[derive(Default)]
struct ModelStats {
    requests: u64,
    input: i64,
    cached: i64,
    cache_write: i64,
    output: i64,
    reasoning: i64,
    credits: f64,
    credit_observations: u64,
}

struct ObservedQuota {
    group: Group,
    observed_at: i64,
}

impl Metrics {
    pub fn new() -> Self {
        Self::with_window(WINDOW)
    }

    pub fn with_window(window: usize) -> Self {
        Self {
            started: Instant::now(),
            window: window.max(1),
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Opens a turn. The returned recorder closes it — on `Done`, on `Failed`,
    /// or on being dropped, which is what a client disconnecting mid-stream
    /// looks like from here.
    pub fn start_turn(self: &Arc<Self>, surface: &str, caller: &str, model: &str) -> TurnRecorder {
        {
            let mut inner = self.lock();
            inner.total_requests += 1;
            inner.inflight += 1;
            *inner.surfaces.entry(surface.to_string()).or_default() += 1;
        }
        TurnRecorder {
            metrics: Some(self.clone()),
            surface: surface.to_string(),
            caller: caller.to_string(),
            model: model.to_string(),
            start: Instant::now(),
            ttft_ms: None,
            upstream_connect_ms: None,
            first_event_ms: None,
            last_event_at: None,
            max_event_gap_ms: 0.0,
            request: None,
            usage: None,
            credits: None,
            outcome: None,
        }
    }

    /// A turn the backend refused before a single event arrived. Counted as a
    /// request, because it consumed one.
    pub fn record_rejected(&self, surface: &str, status: Option<u16>) {
        let mut inner = self.lock();
        inner.total_requests += 1;
        *inner.surfaces.entry(surface.to_string()).or_default() += 1;
        *inner.outcomes.entry("rejected".to_string()).or_default() += 1;
        if let Some(status) = status {
            *inner.rejections.entry(status).or_default() += 1;
        }
    }

    fn update_limits(&self, limits: TurnLimits) {
        let mut inner = self.lock();
        let observed_at = chrono::Utc::now().timestamp();
        if let Some(plan) = &limits.account.plan {
            inner.plan = Some(plan.id.clone());
        }
        for group in &limits.limits.groups {
            inner.quota_groups.insert(
                group.id.clone(),
                ObservedQuota {
                    group: group.clone(),
                    observed_at,
                },
            );
        }
        inner.limits = Some(limits);
        inner.limits_updated_at = Some(observed_at);
    }

    fn finish_turn(&self, turn: &TurnRecorder, outcome: &str) -> String {
        let total_ms = turn.start.elapsed().as_secs_f64() * 1000.0;
        let window = self.window;
        let mut inner = self.lock();
        inner.inflight = inner.inflight.saturating_sub(1);
        *inner.outcomes.entry(outcome.to_string()).or_default() += 1;
        push(&mut inner.total_ms, total_ms, window);
        if let Some(ttft) = turn.ttft_ms {
            push(&mut inner.ttft_ms, ttft, window);
        }
        if let Some(connect) = turn.upstream_connect_ms {
            push(&mut inner.upstream_connect_ms, connect, window);
        }
        if let Some(first_event) = turn.first_event_ms {
            push(&mut inner.first_event_ms, first_event, window);
        }
        let tail_gap_ms = turn
            .last_event_at
            .map(|last| last.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        push(
            &mut inner.max_event_gap_ms,
            turn.max_event_gap_ms.max(tail_gap_ms),
            window,
        );
        if let Some(request) = &turn.request {
            inner.request_encoded_bytes += request.encoded_bytes as u64;
            inner.request_decoded_bytes += request.decoded_bytes as u64;
            inner.compressed_requests += u64::from(request.compressed);
            push(&mut inner.request_decode_ms, request.decode_ms, window);
        }
        let model = inner.models.entry(turn.model.clone()).or_default();
        model.requests += 1;
        if let Some(usage) = &turn.usage {
            let t = &mut inner.tokens;
            t.input += usage.input_tokens.unwrap_or(0);
            t.cached += usage.cached_input_tokens.unwrap_or(0);
            t.cache_write += usage.cache_write_input_tokens.unwrap_or(0);
            t.output += usage.output_tokens.unwrap_or(0);
            t.reasoning += usage.reasoning_output_tokens.unwrap_or(0);
            t.total += usage.total_tokens.unwrap_or(0);

            let model = inner.models.entry(turn.model.clone()).or_default();
            model.input += usage.input_tokens.unwrap_or(0);
            model.cached += usage.cached_input_tokens.unwrap_or(0);
            model.cache_write += usage.cache_write_input_tokens.unwrap_or(0);
            model.output += usage.output_tokens.unwrap_or(0);
            model.reasoning += usage.reasoning_output_tokens.unwrap_or(0);
        }
        if let Some(credits) = turn.credits.as_deref().and_then(parse_credits) {
            inner.credits += credits;
            inner.credit_observations += 1;
            let model = inner.models.entry(turn.model.clone()).or_default();
            model.credits += credits;
            model.credit_observations += 1;
        }
        drop(inner);
        turn.log_line(outcome, total_ms)
    }

    pub fn snapshot(&self) -> Value {
        let inner = self.lock();
        let done: u64 = inner.outcomes.values().sum();
        // Deliberately narrow: `dropped` is the client hanging up and `aborted`
        // is the backend ending early — neither is a failure of this daemon.
        let errors = inner.outcomes.get("failed").copied().unwrap_or(0)
            + inner.outcomes.get("rejected").copied().unwrap_or(0);

        let t = &inner.tokens;
        let models: HashMap<&String, Value> = inner
            .models
            .iter()
            .map(|(name, stats)| {
                (
                    name,
                    json!({
                        "requests": stats.requests,
                        "input_tokens": stats.input,
                        "cached_tokens": stats.cached,
                        "cache_write_tokens": stats.cache_write,
                        "output_tokens": stats.output,
                        "reasoning_tokens": stats.reasoning,
                        "credits": stats.credits,
                        "credit_observations": stats.credit_observations,
                        "hit_rate": ratio(stats.cached, stats.input),
                    }),
                )
            })
            .collect();

        json!({
            "uptime_seconds": round1(self.started.elapsed().as_secs_f64()),
            "total_requests": inner.total_requests,
            "inflight": inner.inflight,
            "outcomes": inner.outcomes,
            "error_rate": if done > 0 { ratio(errors as i64, done as i64) } else { json!(0.0) },
            "rejections_by_status": inner
                .rejections
                .iter()
                .map(|(status, count)| (status.to_string(), *count))
                .collect::<HashMap<_, _>>(),
            "surfaces": inner.surfaces,
            "cache": {
                "hit_rate": ratio(t.cached, t.input),
                "read_tokens": t.cached,
                // Measured against the subscription backend: always 0. The field
                // exists in the protocol, the backend does not fill it on this
                // path. Reported anyway rather than hidden — a value appearing
                // here would be news.
                "write_tokens": t.cache_write,
                "input_tokens": t.input,
            },
            "tokens": {
                "input": t.input,
                "output": t.output,
                "reasoning": t.reasoning,
                "total": t.total,
            },
            "credits": {
                "total": inner.credits,
                "observations": inner.credit_observations,
            },
            "latency_ms": {
                "total": band(&inner.total_ms),
                "ttft": band(&inner.ttft_ms),
                "upstream_connect": band(&inner.upstream_connect_ms),
                "first_event": band(&inner.first_event_ms),
                "max_event_gap": band(&inner.max_event_gap_ms),
                "request_decode": band(&inner.request_decode_ms),
            },
            "request_body": {
                "encoded_bytes": inner.request_encoded_bytes,
                "decoded_bytes": inner.request_decoded_bytes,
                "compression_ratio": ratio(
                    inner.request_encoded_bytes as i64,
                    inner.request_decoded_bytes as i64,
                ),
                "compressed_requests": inner.compressed_requests,
            },
            "models": models,
            "limits": inner.limits,
            "limits_updated_at": inner.limits_updated_at,
        })
    }

    /// Prometheus text format for durable scraping by an external time-series database.
    pub fn prometheus(&self) -> String {
        let inner = self.lock();
        let mut lines = vec![
            "# HELP subscription_wrapper_info Static wrapper identity.".to_string(),
            "# TYPE subscription_wrapper_info gauge".to_string(),
            sample(
                "subscription_wrapper_info",
                1,
                &[
                    ("provider", "codex"),
                    ("plan", inner.plan.as_deref().unwrap_or("unknown")),
                ],
            ),
            "# HELP subscription_wrapper_uptime_seconds Process uptime.".to_string(),
            "# TYPE subscription_wrapper_uptime_seconds gauge".to_string(),
            sample(
                "subscription_wrapper_uptime_seconds",
                self.started.elapsed().as_secs_f64(),
                &[("provider", "codex")],
            ),
            "# HELP subscription_wrapper_inflight_requests Requests currently running.".to_string(),
            "# TYPE subscription_wrapper_inflight_requests gauge".to_string(),
            sample(
                "subscription_wrapper_inflight_requests",
                inner.inflight,
                &[("provider", "codex")],
            ),
            "# HELP subscription_wrapper_requests_total Requests by terminal outcome.".to_string(),
            "# TYPE subscription_wrapper_requests_total counter".to_string(),
        ];
        let mut outcomes: Vec<_> = inner.outcomes.iter().collect();
        outcomes.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (outcome, count) in outcomes {
            lines.push(sample(
                "subscription_wrapper_requests_total",
                count,
                &[("provider", "codex"), ("outcome", outcome.as_str())],
            ));
        }
        lines.extend([
            "# HELP subscription_wrapper_tokens_total Provider-reported tokens by model and category.".to_string(),
            "# TYPE subscription_wrapper_tokens_total counter".to_string(),
        ]);
        let mut models: Vec<_> = inner.models.iter().collect();
        models.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (model, stats) in models {
            let uncached = (stats.input - stats.cached - stats.cache_write).max(0);
            for (kind, value) in [
                ("input_uncached", uncached),
                ("cache_read", stats.cached),
                ("cache_write", stats.cache_write),
                ("output", stats.output),
                ("reasoning", stats.reasoning),
            ] {
                lines.push(sample(
                    "subscription_wrapper_tokens_total",
                    value,
                    &[
                        ("provider", "codex"),
                        ("model", model.as_str()),
                        ("type", kind),
                    ],
                ));
            }
        }
        lines.extend([
            "# HELP subscription_wrapper_credits_total Provider-reported per-response credits by model.".to_string(),
            "# TYPE subscription_wrapper_credits_total counter".to_string(),
            "# HELP subscription_wrapper_credit_observations_total Responses carrying a valid credit amount.".to_string(),
            "# TYPE subscription_wrapper_credit_observations_total counter".to_string(),
        ]);
        let mut models: Vec<_> = inner.models.iter().collect();
        models.sort_by(|(left, _), (right, _)| left.cmp(right));
        for (model, stats) in models {
            if stats.credit_observations == 0 {
                continue;
            }
            let labels = [("provider", "codex"), ("model", model.as_str())];
            lines.push(sample(
                "subscription_wrapper_credits_total",
                stats.credits,
                &labels,
            ));
            lines.push(sample(
                "subscription_wrapper_credit_observations_total",
                stats.credit_observations,
                &labels,
            ));
        }
        lines.extend([
            "# HELP subscription_wrapper_quota_used_ratio Used fraction of a subscription quota window.".to_string(),
            "# TYPE subscription_wrapper_quota_used_ratio gauge".to_string(),
            "# HELP subscription_wrapper_quota_window_seconds Configured quota window length.".to_string(),
            "# TYPE subscription_wrapper_quota_window_seconds gauge".to_string(),
            "# HELP subscription_wrapper_quota_reset_timestamp_seconds Quota window reset time.".to_string(),
            "# TYPE subscription_wrapper_quota_reset_timestamp_seconds gauge".to_string(),
            "# HELP subscription_wrapper_quota_observed_timestamp_seconds Last backend observation time.".to_string(),
            "# TYPE subscription_wrapper_quota_observed_timestamp_seconds gauge".to_string(),
        ]);
        let mut groups: Vec<_> = inner.quota_groups.values().collect();
        groups.sort_by(|left, right| left.group.id.cmp(&right.group.id));
        for observed in groups {
            for (window_name, window) in [
                ("primary", observed.group.primary.as_ref()),
                ("secondary", observed.group.secondary.as_ref()),
            ] {
                let Some(window) = window else {
                    continue;
                };
                let labels = [
                    ("provider", "codex"),
                    ("group", observed.group.id.as_str()),
                    ("window", window_name),
                ];
                lines.push(sample(
                    "subscription_wrapper_quota_used_ratio",
                    window.used_percent / 100.0,
                    &labels,
                ));
                if let Some(duration) = window.window_seconds {
                    lines.push(sample(
                        "subscription_wrapper_quota_window_seconds",
                        duration,
                        &labels,
                    ));
                }
                if let Some(reset) = window.resets_at {
                    lines.push(sample(
                        "subscription_wrapper_quota_reset_timestamp_seconds",
                        reset,
                        &labels,
                    ));
                }
                lines.push(sample(
                    "subscription_wrapper_quota_observed_timestamp_seconds",
                    observed.observed_at,
                    &labels,
                ));
            }
        }
        lines.push(String::new());
        lines.join("\n")
    }

    /// A poisoned lock must not take the server down: metrics are diagnostics,
    /// not the product.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|err| err.into_inner())
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}

/// Open turn. Closes itself when dropped, so a client hanging up mid-stream
/// still lands in the statistics instead of leaking an `inflight`.
pub struct TurnRecorder {
    /// `None` after finishing, so `Drop` does not count a second time.
    metrics: Option<Arc<Metrics>>,
    surface: String,
    caller: String,
    model: String,
    start: Instant,
    ttft_ms: Option<f64>,
    upstream_connect_ms: Option<f64>,
    first_event_ms: Option<f64>,
    last_event_at: Option<Instant>,
    max_event_gap_ms: f64,
    request: Option<RequestObservation>,
    usage: Option<Usage>,
    credits: Option<String>,
    outcome: Option<String>,
}

struct RequestObservation {
    encoded_bytes: usize,
    decoded_bytes: usize,
    decode_ms: f64,
    compressed: bool,
}

impl TurnRecorder {
    pub fn record_request(
        &mut self,
        encoded_bytes: usize,
        decoded_bytes: usize,
        decode_ms: f64,
        compressed: bool,
    ) {
        self.request = Some(RequestObservation {
            encoded_bytes,
            decoded_bytes,
            decode_ms,
            compressed,
        });
    }

    pub fn record_upstream_connect(&mut self, elapsed_ms: f64) {
        self.upstream_connect_ms = Some(elapsed_ms);
    }

    /// Watches the event stream on its way to the consumer.
    pub fn observe(&mut self, event: &Event) {
        if !matches!(event, Event::RateLimits(_)) {
            let now = Instant::now();
            if self.first_event_ms.is_none() {
                self.first_event_ms = Some(self.start.elapsed().as_secs_f64() * 1000.0);
            }
            if let Some(last) = self.last_event_at {
                self.max_event_gap_ms = self
                    .max_event_gap_ms
                    .max(now.duration_since(last).as_secs_f64() * 1000.0);
            }
            self.last_event_at = Some(now);
        }
        match event {
            // Time to first token counts the first thing a user sees — text,
            // thinking, or a tool call for turns that produce no text at all.
            // The guard belongs in the pattern rather than in the body: only the
            // first of these events sets the mark, every later one falls through
            // to the catch-all and does nothing.
            Event::TextDelta { .. } | Event::ThinkingDelta { .. } | Event::ToolCall { .. }
                if self.ttft_ms.is_none() =>
            {
                self.ttft_ms = Some(self.start.elapsed().as_secs_f64() * 1000.0);
            }
            Event::RateLimits(limits) => {
                if let Some(metrics) = &self.metrics {
                    metrics.update_limits(limits.clone());
                }
            }
            Event::Done {
                stop_reason,
                usage,
                credits,
                ..
            } => {
                self.outcome = Some(stop_reason.clone());
                self.usage = usage.clone();
                self.credits = credits.clone();
            }
            Event::Failed { .. } => self.outcome = Some("failed".to_string()),
            _ => {}
        }
    }
}

impl Drop for TurnRecorder {
    fn drop(&mut self) {
        let Some(metrics) = self.metrics.take() else {
            return;
        };
        // No terminal event seen: the consumer went away mid-stream.
        let outcome = self
            .outcome
            .clone()
            .unwrap_or_else(|| "dropped".to_string());
        let line = metrics.finish_turn(self, &outcome);
        eprintln!("{line}");
    }
}

impl TurnRecorder {
    fn log_line(&self, outcome: &str, total_ms: f64) -> String {
        let ttft = match self.ttft_ms {
            Some(ms) => format!("{ms:.0}ms"),
            None => "-".to_string(),
        };
        let connect = self
            .upstream_connect_ms
            .map(|ms| format!("{ms:.0}ms"))
            .unwrap_or_else(|| "-".to_string());
        let first_event = self
            .first_event_ms
            .map(|ms| format!("{ms:.0}ms"))
            .unwrap_or_else(|| "-".to_string());
        let tail_gap_ms = self
            .last_event_at
            .map(|last| last.elapsed().as_secs_f64() * 1000.0)
            .unwrap_or(0.0);
        let max_gap = self.max_event_gap_ms.max(tail_gap_ms);
        let request = self
            .request
            .as_ref()
            .map(|request| format!("{}/{}", request.encoded_bytes, request.decoded_bytes))
            .unwrap_or_else(|| "-".to_string());
        let (input, cached, output, hit) = match &self.usage {
            Some(usage) => {
                let input = usage.input_tokens.unwrap_or(0);
                let cached = usage.cached_input_tokens.unwrap_or(0);
                (
                    input,
                    cached,
                    usage.output_tokens.unwrap_or(0),
                    if input > 0 {
                        format!("{:.1}%", 100.0 * cached as f64 / input as f64)
                    } else {
                        "-".to_string()
                    },
                )
            }
            None => (0, 0, 0, "-".to_string()),
        };
        format!(
            "[{}] {} model={} outcome={} total={:.0}ms connect={} first={} ttft={} gap={:.0}ms req={} in={} out={} cached={}/{} ({})",
            self.caller,
            self.surface,
            self.model,
            outcome,
            total_ms,
            connect,
            first_event,
            ttft,
            max_gap,
            request,
            input,
            output,
            cached,
            input,
            hit
        )
    }
}

// --- Helpers ---------------------------------------------------------------

fn push(buffer: &mut VecDeque<f64>, value: f64, window: usize) {
    if buffer.len() == window {
        buffer.pop_front();
    }
    buffer.push_back(value);
}

fn parse_credits(value: &str) -> Option<f64> {
    value
        .parse::<f64>()
        .ok()
        .filter(|credits| credits.is_finite() && *credits >= 0.0)
}

fn sample<T: std::fmt::Display>(name: &str, value: T, labels: &[(&str, &str)]) -> String {
    let labels = labels
        .iter()
        .map(|(key, value)| format!(r#"{key}="{}""#, escape_label(value)))
        .collect::<Vec<_>>()
        .join(",");
    format!("{name}{{{labels}}} {value}")
}

fn escape_label(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n").replace('"', "\\\"")
}

fn ratio(part: i64, whole: i64) -> Value {
    if whole <= 0 {
        return json!(0.0);
    }
    json!(((part as f64 / whole as f64) * 10_000.0).round() / 10_000.0)
}

fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// Nearest-rank percentile, same rule as the Claude wrapper's `_pct` so the two
/// `/metrics` outputs stay comparable.
fn percentile(sorted: &[f64], p: usize) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let index = ((p as f64 / 100.0) * sorted.len() as f64) as usize;
    Some(round1(sorted[index.min(sorted.len() - 1)]))
}

fn band(samples: &VecDeque<f64>) -> Value {
    let mut sorted: Vec<f64> = samples.iter().copied().collect();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    json!({
        "p50": percentile(&sorted, 50),
        "p95": percentile(&sorted, 95),
        "p99": percentile(&sorted, 99),
        "n": sorted.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(input: i64, cached: i64, output: i64) -> Usage {
        Usage {
            input_tokens: Some(input),
            cached_input_tokens: Some(cached),
            cache_write_input_tokens: Some(0),
            output_tokens: Some(output),
            reasoning_output_tokens: Some(0),
            total_tokens: Some(input + output),
        }
    }

    fn limits(plan: &str, active: &str) -> crate::limits::TurnLimits {
        crate::limits::TurnLimits {
            account: crate::limits::Account {
                plan: Some(crate::limits::Plan {
                    id: plan.to_string(),
                    name: None,
                }),
                ..Default::default()
            },
            limits: crate::limits::Limits {
                active_group: Some(active.to_string()),
                groups: vec![crate::limits::Group {
                    id: active.to_string(),
                    name: None,
                    reached: None,
                    primary: None,
                    secondary: None,
                }],
            },
        }
    }

    fn done(input: i64, cached: i64, output: i64) -> Event {
        Event::Done {
            response_id: Some("resp_1".into()),
            stop_reason: "end_turn".into(),
            usage: Some(usage(input, cached, output)),
            credits: None,
        }
    }

    /// One finished turn lands in every block a consumer reads.
    #[test]
    fn finished_turn_is_counted() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_CHAT, "local", "gpt-5.6-sol");
            assert_eq!(metrics.snapshot()["inflight"], 1);
            turn.observe(&Event::TextDelta { text: "hi".into() });
            turn.observe(&done(1000, 800, 20));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["total_requests"], 1);
        assert_eq!(snap["inflight"], 0);
        assert_eq!(snap["outcomes"]["end_turn"], 1);
        assert_eq!(snap["cache"]["hit_rate"], 0.8);
        assert_eq!(snap["cache"]["read_tokens"], 800);
        assert_eq!(snap["tokens"]["output"], 20);
        assert_eq!(snap["surfaces"][SURFACE_CHAT], 1);
        assert_eq!(snap["models"]["gpt-5.6-sol"]["requests"], 1);
        assert_eq!(snap["latency_ms"]["ttft"]["n"], 1);
    }

    #[test]
    fn credits_are_counted_by_model_when_the_backend_reports_them() {
        let metrics = Arc::new(Metrics::new());
        for amount in [Some("24.125"), None, Some("not-a-number")] {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.observe(&Event::Done {
                response_id: Some("resp_credit".into()),
                stop_reason: "end_turn".into(),
                usage: None,
                credits: amount.map(str::to_string),
            });
        }

        let snap = metrics.snapshot();
        assert_eq!(snap["credits"]["total"], 24.125);
        assert_eq!(snap["credits"]["observations"], 1);
        assert_eq!(snap["models"]["gpt-5.6-sol"]["credits"], 24.125);
        assert_eq!(
            snap["models"]["gpt-5.6-sol"]["credit_observations"],
            1
        );

        let text = metrics.prometheus();
        assert!(text.contains(
            r#"subscription_wrapper_credits_total{provider="codex",model="gpt-5.6-sol"} 24.125"#
        ));
        assert!(text.contains(
            r#"subscription_wrapper_credit_observations_total{provider="codex",model="gpt-5.6-sol"} 1"#
        ));
    }

    #[test]
    fn prometheus_splits_tokens_and_keeps_observed_quota_groups() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "model\"quoted");
            let mut quota = limits("prolite", "global");
            quota.limits.groups[0].primary = Some(crate::limits::Window {
                used_percent: 12.0,
                window_seconds: Some(604_800),
                resets_at: Some(1234),
                resets_in_seconds: Some(100),
            });
            turn.observe(&Event::RateLimits(quota));
            turn.observe(&done(1000, 800, 20));
        }

        let text = metrics.prometheus();
        assert!(text.contains(
            r#"subscription_wrapper_tokens_total{provider="codex",model="model\"quoted",type="input_uncached"} 200"#
        ));
        assert!(text.contains(
            r#"subscription_wrapper_tokens_total{provider="codex",model="model\"quoted",type="cache_read"} 800"#
        ));
        assert!(text.contains(
            r#"subscription_wrapper_quota_used_ratio{provider="codex",group="global",window="primary"} 0.12"#
        ));
        assert!(text.contains("subscription_wrapper_quota_window_seconds"));
        assert!(text.contains("subscription_wrapper_quota_reset_timestamp_seconds"));
    }

    #[test]
    fn transport_and_request_observations_are_reported() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.record_request(25, 100, 1.5, true);
            turn.record_upstream_connect(42.0);
            turn.observe(&Event::Started { model: None });
            std::thread::sleep(std::time::Duration::from_millis(2));
            turn.observe(&done(10, 0, 1));
        }

        let snap = metrics.snapshot();
        assert_eq!(snap["request_body"]["encoded_bytes"], 25);
        assert_eq!(snap["request_body"]["decoded_bytes"], 100);
        assert_eq!(snap["request_body"]["compression_ratio"], 0.25);
        assert_eq!(snap["request_body"]["compressed_requests"], 1);
        assert_eq!(snap["latency_ms"]["request_decode"]["n"], 1);
        assert_eq!(snap["latency_ms"]["upstream_connect"]["n"], 1);
        assert_eq!(snap["latency_ms"]["first_event"]["n"], 1);
        assert_eq!(snap["latency_ms"]["max_event_gap"]["n"], 1);
        assert!(snap["latency_ms"]["max_event_gap"]["p50"].as_f64().unwrap() >= 1.0);
    }

    /// The mark is set by the **first** content event and never moved. With the
    /// guard sitting in the pattern, a later delta falls through to the
    /// catch-all — this pins that it does not reset the measurement.
    #[test]
    fn ttft_is_the_first_content_event_only() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_CHAT, "local", "gpt-5.6-sol");
            turn.observe(&Event::TextDelta { text: "a".into() });
            let first = turn.ttft_ms.expect("first delta sets the mark");
            std::thread::sleep(std::time::Duration::from_millis(5));
            turn.observe(&Event::TextDelta { text: "b".into() });
            turn.observe(&Event::ToolCall {
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments: "{}".into(),
            });
            assert_eq!(turn.ttft_ms, Some(first));
            turn.observe(&done(10, 0, 1));
        }
        assert_eq!(metrics.snapshot()["latency_ms"]["ttft"]["n"], 1);
    }

    /// A turn that only calls a tool produces no text — the mark must still be
    /// set, otherwise agent turns would never report a ttft.
    #[test]
    fn tool_call_alone_sets_ttft() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_RESPONSES, "local", "gpt-5.6-sol");
            turn.observe(&Event::ToolCall {
                call_id: "call_1".into(),
                name: "get_weather".into(),
                arguments: "{}".into(),
            });
            assert!(turn.ttft_ms.is_some());
            turn.observe(&done(10, 0, 1));
        }
        assert_eq!(metrics.snapshot()["latency_ms"]["ttft"]["n"], 1);
    }

    /// A consumer hanging up mid-stream must not leak an `inflight`. This is the
    /// case the recorder's `Drop` exists for.
    #[test]
    fn dropped_turn_is_closed() {
        let metrics = Arc::new(Metrics::new());
        drop(metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol"));
        let snap = metrics.snapshot();
        assert_eq!(snap["inflight"], 0);
        assert_eq!(snap["outcomes"]["dropped"], 1);
        // Not an error of ours: the client went away, the turn did not fail.
        assert_eq!(snap["error_rate"], 0.0);
    }

    /// A rejection never opens a stream, but it did cost a request.
    #[test]
    fn rejection_is_counted_by_status() {
        let metrics = Arc::new(Metrics::new());
        metrics.record_rejected(SURFACE_CHAT, Some(429));
        let snap = metrics.snapshot();
        assert_eq!(snap["total_requests"], 1);
        assert_eq!(snap["outcomes"]["rejected"], 1);
        assert_eq!(snap["rejections_by_status"]["429"], 1);
        assert_eq!(snap["error_rate"], 1.0);
    }

    #[test]
    fn failure_counts_as_error() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_RESPONSES, "local", "gpt-5.6-sol");
            turn.observe(&Event::Failed {
                message: "upstream said no".into(),
                retryable: false,
            });
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["outcomes"]["failed"], 1);
        assert_eq!(snap["error_rate"], 1.0);
    }

    /// The quota comes from the turn itself, already resolved: one object with
    /// the groups told apart, not a list of nameless windows.
    #[test]
    fn quota_is_taken_from_the_turn() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.observe(&Event::RateLimits(limits("prolite", "global")));
            turn.observe(&done(10, 0, 1));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["limits"]["account"]["plan"]["id"], "prolite");
        assert_eq!(snap["limits"]["limits"]["active_group"], "global");
        assert!(snap["limits_updated_at"].is_i64());
    }

    /// The next turn replaces the previous state instead of piling onto it.
    #[test]
    fn a_later_turn_replaces_the_quota() {
        let metrics = Arc::new(Metrics::new());
        for group in ["global", "codex_bengalfox"] {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.observe(&Event::RateLimits(limits("prolite", group)));
            turn.observe(&done(10, 0, 1));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["limits"]["limits"]["active_group"], "codex_bengalfox");
        assert_eq!(
            snap["limits"]["limits"]["groups"].as_array().map(Vec::len),
            Some(1),
            "not accumulated across turns"
        );
    }

    /// Turns without usage must not drag the hit rate towards zero — a request
    /// the backend refused says nothing about the cache.
    #[test]
    fn turns_without_usage_do_not_dilute_the_hit_rate() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_CHAT, "local", "gpt-5.6-sol");
            turn.observe(&done(1000, 900, 10));
        }
        drop(metrics.start_turn(SURFACE_CHAT, "local", "gpt-5.6-sol"));
        assert_eq!(metrics.snapshot()["cache"]["hit_rate"], 0.9);
    }

    /// Empty is empty: no samples must not become a fabricated zero.
    #[test]
    fn empty_bands_report_null() {
        let snap = Metrics::new().snapshot();
        assert!(snap["latency_ms"]["total"]["p50"].is_null());
        assert_eq!(snap["latency_ms"]["total"]["n"], 0);
        assert_eq!(snap["cache"]["hit_rate"], 0.0);
        assert!(snap["limits"].is_null());
    }

    #[test]
    fn percentiles_follow_nearest_rank() {
        let sorted: Vec<f64> = (1..=100).map(f64::from).collect();
        assert_eq!(percentile(&sorted, 50), Some(51.0));
        assert_eq!(percentile(&sorted, 95), Some(96.0));
        assert_eq!(percentile(&sorted, 99), Some(100.0));
        assert_eq!(percentile(&[], 50), None);
    }

    /// The window is a ring: old samples fall out instead of growing forever.
    #[test]
    fn latency_window_is_bounded() {
        let metrics = Arc::new(Metrics::with_window(3));
        for _ in 0..10 {
            drop(metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol"));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["latency_ms"]["total"]["n"], 3);
        assert_eq!(snap["total_requests"], 10);
    }
}
