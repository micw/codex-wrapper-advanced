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

use crate::wire::Event;
use crate::wire::RateLimitWindow;
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
    tokens: Tokens,
    total_ms: VecDeque<f64>,
    ttft_ms: VecDeque<f64>,
    /// Every window of the most recent turn, in arrival order.
    ///
    /// Deliberately a list: the backend sends **several** `rate_limits` events
    /// per turn — the account's 7-day quota and, separately, additional
    /// per-model limits. `wire::Event::RateLimits` does not tell them apart, so
    /// keeping only the last one would drop the account quota in favour of a
    /// limit that reads 0 %. Until the wire vocabulary distinguishes them, all of
    /// them are reported and the consumer decides.
    rate_limits: Vec<RateLimitSnapshot>,
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
}

struct RateLimitSnapshot {
    plan: Option<String>,
    primary: Option<RateLimitWindow>,
    secondary: Option<RateLimitWindow>,
    updated_at: i64,
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
            usage: None,
            outcome: None,
            rate_limits_seen: 0,
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

    /// `replace` starts a new turn's series, otherwise the window is appended to
    /// the one already running.
    fn update_rate_limit(
        &self,
        replace: bool,
        plan: Option<String>,
        primary: Option<RateLimitWindow>,
        secondary: Option<RateLimitWindow>,
    ) {
        let mut inner = self.lock();
        if replace {
            inner.rate_limits.clear();
        }
        inner.rate_limits.push(RateLimitSnapshot {
            plan,
            primary,
            secondary,
            updated_at: chrono::Utc::now().timestamp(),
        });
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
        if let Some(usage) = &turn.usage {
            let t = &mut inner.tokens;
            t.input += usage.input_tokens.unwrap_or(0);
            t.cached += usage.cached_input_tokens.unwrap_or(0);
            t.cache_write += usage.cache_write_input_tokens.unwrap_or(0);
            t.output += usage.output_tokens.unwrap_or(0);
            t.reasoning += usage.reasoning_output_tokens.unwrap_or(0);
            t.total += usage.total_tokens.unwrap_or(0);

            let model = inner.models.entry(turn.model.clone()).or_default();
            model.requests += 1;
            model.input += usage.input_tokens.unwrap_or(0);
            model.cached += usage.cached_input_tokens.unwrap_or(0);
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
            "latency_ms": {
                "total": band(&inner.total_ms),
                "ttft": band(&inner.ttft_ms),
            },
            "models": models,
            "rate_limits": inner
                .rate_limits
                .iter()
                .map(|rl| json!({
                    "plan": rl.plan,
                    "primary": rl.primary,
                    "secondary": rl.secondary,
                    "updated_at": rl.updated_at,
                }))
                .collect::<Vec<_>>(),
        })
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
    usage: Option<Usage>,
    outcome: Option<String>,
    /// Counts this turn's `RateLimits` events so the first one starts a fresh
    /// series instead of appending to the previous turn's.
    rate_limits_seen: usize,
}

impl TurnRecorder {
    /// Watches the event stream on its way to the consumer.
    pub fn observe(&mut self, event: &Event) {
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
            Event::RateLimits {
                plan,
                primary,
                secondary,
            } => {
                if let Some(metrics) = &self.metrics {
                    metrics.update_rate_limit(
                        self.rate_limits_seen == 0,
                        plan.clone(),
                        primary.clone(),
                        secondary.clone(),
                    );
                }
                self.rate_limits_seen += 1;
            }
            Event::Done {
                stop_reason, usage, ..
            } => {
                self.outcome = Some(stop_reason.clone());
                self.usage = usage.clone();
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
            "[{}] {} model={} outcome={} total={:.0}ms ttft={} in={} out={} cached={}/{} ({})",
            self.caller,
            self.surface,
            self.model,
            outcome,
            total_ms,
            ttft,
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

    fn done(input: i64, cached: i64, output: i64) -> Event {
        Event::Done {
            response_id: Some("resp_1".into()),
            stop_reason: "end_turn".into(),
            usage: Some(usage(input, cached, output)),
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

    /// The rate limit comes from the turn's own event, not from a separate call.
    #[test]
    fn rate_limit_is_taken_from_the_stream() {
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.observe(&Event::RateLimits {
                plan: Some("prolite".into()),
                primary: Some(RateLimitWindow {
                    used_percent: Some(46.0),
                    window_minutes: Some(10080),
                    resets_at: Some(1788273291),
                }),
                secondary: None,
            });
            turn.observe(&done(10, 0, 1));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["rate_limits"][0]["plan"], "prolite");
        assert_eq!(snap["rate_limits"][0]["primary"]["used_percent"], 46.0);
    }

    /// The backend sends several windows per turn. None of them may be lost, and
    /// the next turn must not append to the previous turn's series.
    #[test]
    fn every_rate_limit_window_of_a_turn_is_kept() {
        fn window(percent: f64, minutes: i64) -> Event {
            Event::RateLimits {
                plan: None,
                primary: Some(RateLimitWindow {
                    used_percent: Some(percent),
                    window_minutes: Some(minutes),
                    resets_at: None,
                }),
                secondary: None,
            }
        }
        let metrics = Arc::new(Metrics::new());
        {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.observe(&window(46.0, 10080));
            turn.observe(&window(0.0, 300));
            turn.observe(&done(10, 0, 1));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["rate_limits"].as_array().map(Vec::len), Some(2));
        assert_eq!(snap["rate_limits"][0]["primary"]["window_minutes"], 10080);
        assert_eq!(snap["rate_limits"][1]["primary"]["window_minutes"], 300);

        {
            let mut turn = metrics.start_turn(SURFACE_WIRE, "local", "gpt-5.6-sol");
            turn.observe(&window(47.0, 10080));
            turn.observe(&done(10, 0, 1));
        }
        let snap = metrics.snapshot();
        assert_eq!(snap["rate_limits"].as_array().map(Vec::len), Some(1));
        assert_eq!(snap["rate_limits"][0]["primary"]["used_percent"], 47.0);
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
        assert_eq!(snap["rate_limits"].as_array().map(Vec::len), Some(0));
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
