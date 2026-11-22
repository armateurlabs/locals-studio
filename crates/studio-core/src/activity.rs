//! Observed response phases and sampled worker output counters. Response bytes are never changed.
use crate::{
    chat::SseParser,
    events::{self, EventSender, emit},
    metrics::RequestMetrics,
    types::ServerStatus,
};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex},
    time::Duration,
};

#[derive(Clone, Default)]
pub(crate) struct ActivityTracker(Arc<Mutex<Tracked>>);
struct Tracked {
    epoch: String,
    revision: u64,
    requests: HashMap<String, (ServerStatus, &'static str)>,
    events: Option<EventSender>,
}
impl Default for Tracked {
    fn default() -> Self {
        Self {
            epoch: uuid::Uuid::new_v4().to_string(),
            revision: 0,
            requests: HashMap::new(),
            events: None,
        }
    }
}
pub(crate) fn same_worker(left: &ServerStatus, right: &ServerStatus) -> bool {
    left.profile_id == right.profile_id
        && left.pid == right.pid
        && left.started_at == right.started_at
        && left.port == right.port
}
impl ActivityTracker {
    pub fn with_events(events: EventSender) -> Self {
        Self(Arc::new(Mutex::new(Tracked {
            events: Some(events),
            ..Default::default()
        })))
    }
    pub fn begin(&self, status: ServerStatus) -> ActivityGuard {
        let guard = ActivityGuard {
            tracker: self.clone(),
            id: uuid::Uuid::new_v4().to_string(),
            status,
        };
        self.change(&guard, Some("preparing"));
        guard
    }
    fn change(&self, guard: &ActivityGuard, phase: Option<&'static str>) {
        let mut state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        if state.requests.get(&guard.id).map(|(_, current)| *current) == phase {
            return;
        }
        match phase {
            Some(phase) => {
                state
                    .requests
                    .insert(guard.id.clone(), (guard.status.clone(), phase));
            }
            None => {
                state.requests.remove(&guard.id);
            }
        }
        state.revision = state.revision.wrapping_add(1);
        if let Some(events) = &state.events {
            let activity = summary(&state, &guard.status, None, true);
            events::emit(
                events,
                "server.activity",
                &json!({"profile_id":guard.status.profile_id,"pid":guard.status.pid,"started_at":guard.status.started_at,"port":guard.status.port,"activity":activity}),
            );
        }
    }
    pub fn summary(&self, status: &ServerStatus, stats: Option<&Value>) -> Value {
        let state = self.0.lock().unwrap_or_else(|poison| poison.into_inner());
        summary(&state, status, stats, false)
    }
}
pub(crate) struct ActivityGuard {
    tracker: ActivityTracker,
    id: String,
    status: ServerStatus,
}
impl ActivityGuard {
    fn phase(&self, phase: &'static str) {
        self.tracker.change(self, Some(phase));
    }
}
impl Drop for ActivityGuard {
    fn drop(&mut self) {
        self.tracker.change(self, None);
    }
}

fn summary(
    state: &Tracked,
    status: &ServerStatus,
    stats: Option<&Value>,
    gateway_only: bool,
) -> Value {
    let gateway: Vec<_> = state
        .requests
        .values()
        .filter(|(worker, _)| same_worker(worker, status))
        .map(|(_, phase)| *phase)
        .collect();
    let active = stats.and_then(|stats| stats["active"].as_array());
    let gateway_count = gateway.len();
    let count = active.map_or(
        gateway_count.max(if stats.is_none() && !gateway_only {
            status.active_requests
        } else {
            0
        }),
        |active| active.len().max(gateway_count),
    );
    let mut phases = BTreeMap::<&str, usize>::new();
    for phase in gateway
        .iter()
        .filter(|phase| matches!(**phase, "thinking" | "generating"))
    {
        *phases.entry(phase).or_default() += 1;
    }
    let observed = phases.values().sum::<usize>();
    let mut remaining = count.saturating_sub(observed);
    if let Some(active) = active {
        let decoding = active
            .iter()
            .filter(|request| request["tokens"].as_u64().is_some_and(|tokens| tokens > 0))
            .count()
            .saturating_sub(observed)
            .min(remaining);
        if decoding > 0 {
            phases.insert("decoding", decoding);
            remaining -= decoding;
        }
        let preparing = active
            .iter()
            .filter(|request| request["tokens"].as_u64() == Some(0))
            .count()
            .min(remaining);
        if preparing > 0 {
            phases.insert("preparing", preparing);
            remaining -= preparing;
        }
    }
    if remaining > 0 {
        let preparing = gateway
            .iter()
            .filter(|phase| **phase == "preparing")
            .count()
            .min(remaining);
        if preparing > 0 {
            *phases.entry("preparing").or_default() += preparing;
            remaining -= preparing;
        }
        if remaining > 0 {
            phases.insert("unknown", remaining);
        }
    }
    let phase = if count == 0 {
        if stats.is_some() || gateway_only || status.pid.is_none() {
            "idle"
        } else {
            "unknown"
        }
    } else if phases.len() == 1 {
        phases.keys().next().copied().unwrap_or("unknown")
    } else {
        "mixed"
    };
    let sum = |key: &str| {
        active.and_then(|active| {
            active
                .iter()
                .try_fold(0u64, |sum, request| sum.checked_add(request[key].as_u64()?))
        })
    };
    json!({"phase":phase,"phases":phases,"active_requests":count,"total_requests":stats.and_then(|stats|stats["requests"].as_u64()),"tokens_per_second":null,"prompt_tokens":sum("prompt_tokens"),"output_tokens":sum("tokens"),"phase_source":if gateway_only || gateway_count > 0 {if active.is_some() {"combined"} else {"gateway"}} else if active.is_some() {"worker"} else {"unavailable"},"stats_available":stats.is_some(),"revision":state.revision,"tracker_epoch":state.epoch,"observed_at_ms":chrono::Utc::now().timestamp_millis()})
}

/// Inspect only bounded metadata from chunks; a parser failure never affects inference.
pub(crate) struct ResponseObserver {
    guard: Option<ActivityGuard>,
    metrics: Option<Arc<RequestMetrics>>,
    sse: Option<SseParser>,
    ndjson: bool,
    /// A plain JSON body (a non-streamed reply), buffered only to read its final `usage`.
    json: bool,
    bytes: Vec<u8>,
    usage: Option<UsageCapture>,
}

/// Bodies larger than this are not buffered for usage: a reply's usage is never worth more memory.
const USAGE_BODY_LIMIT: usize = 8 * 1024 * 1024;

/// The token counts a response reports, captured as it streams and reported once when it ends.
#[derive(Default)]
struct UsageCapture {
    events: Option<EventSender>,
    profile_id: String,
    model: String,
    client: String,
    prompt: u64,
    cached: u64,
    output: u64,
    seen: bool,
}

impl UsageCapture {
    /// Reads token counts from one frame, in any of the dialects the gateway serves:
    /// OpenAI (`usage.prompt_tokens`, `prompt_tokens_details.cached_tokens`, `completion_tokens`),
    /// Anthropic (`usage` or `message.usage` with `input_tokens` excluding cache reads), and
    /// Ollama (`prompt_eval_count`/`eval_count` on the final record). Later frames refine earlier
    /// ones field by field, as Anthropic reports input in `message_start` and output at the end.
    fn observe(&mut self, value: &Value) {
        let count = |value: &Value, key: &str| value.get(key).and_then(Value::as_u64);
        let usage = value
            .get("usage")
            .filter(|usage| usage.is_object())
            .or_else(|| {
                value["message"]
                    .get("usage")
                    .filter(|usage| usage.is_object())
            });
        if let Some(usage) = usage {
            if let Some(prompt) = count(usage, "prompt_tokens") {
                self.prompt = prompt;
                self.cached = count(&usage["prompt_tokens_details"], "cached_tokens")
                    .or_else(|| count(usage, "cached_tokens"))
                    .unwrap_or(0);
                self.seen = true;
            } else if let Some(input) = count(usage, "input_tokens") {
                let read = count(usage, "cache_read_input_tokens").unwrap_or(0);
                let created = count(usage, "cache_creation_input_tokens").unwrap_or(0);
                self.prompt = input.saturating_add(read).saturating_add(created);
                self.cached = read;
                self.seen = true;
            }
            if let Some(output) =
                count(usage, "completion_tokens").or_else(|| count(usage, "output_tokens"))
            {
                self.output = output;
                self.seen = true;
            }
        } else if value["done"] == true
            && (value.get("prompt_eval_count").is_some() || value.get("eval_count").is_some())
        {
            self.prompt = count(value, "prompt_eval_count").unwrap_or(0);
            self.cached = 0;
            self.output = count(value, "eval_count").unwrap_or(0);
            self.seen = true;
        }
    }
}

impl Drop for UsageCapture {
    /// Reports once whenever the response ends, including when the client hangs up after reading
    /// the final frame but before the proxy stream is polled to completion.
    fn drop(&mut self) {
        if !self.seen {
            return;
        }
        if let Some(events) = &self.events {
            let _ = emit(
                events,
                "usage.recorded",
                &json!({
                    "profile_id": self.profile_id,
                    "model": self.model,
                    "client": self.client,
                    "prompt": self.prompt,
                    "cached": self.cached,
                    "output": self.output,
                    "at": chrono::Local::now().to_rfc3339(),
                }),
            );
        }
    }
}

impl ResponseObserver {
    pub fn new(guard: Option<ActivityGuard>, content_type: &str) -> Self {
        let sse = content_type.contains("text/event-stream");
        let ndjson = content_type.contains("ndjson");
        Self {
            guard,
            metrics: None,
            sse: sse.then(SseParser::default),
            ndjson,
            json: !sse && !ndjson && content_type.contains("json"),
            bytes: Vec::new(),
            usage: None,
        }
    }

    /// Reports this response's token usage on `events` when it ends, attributed to the model it
    /// was routed to and the client that asked.
    pub fn with_usage(
        mut self,
        events: EventSender,
        profile_id: String,
        model: String,
        client: String,
    ) -> Self {
        self.usage = Some(UsageCapture {
            events: Some(events),
            profile_id,
            model,
            client,
            ..Default::default()
        });
        self
    }

    pub fn with_metrics(mut self, metrics: Option<Arc<RequestMetrics>>) -> Self {
        self.metrics = metrics;
        self
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if self.guard.is_none() && self.metrics.is_none() && self.usage.is_none() {
            return;
        }
        if let Some(parser) = self.sse.as_mut() {
            match parser.push(bytes) {
                Ok(events) => {
                    for event in events {
                        self.observe(&event);
                    }
                }
                Err(_) => {
                    if let Some(guard) = &self.guard {
                        guard.phase("unknown");
                    }
                    self.sse = None;
                }
            }
        } else if self.json && self.usage.is_some() {
            if self.bytes.len().saturating_add(bytes.len()) > USAGE_BODY_LIMIT {
                self.json = false;
                self.bytes = Vec::new();
                return;
            }
            self.bytes.extend_from_slice(bytes);
        } else if self.ndjson {
            if self.bytes.len().saturating_add(bytes.len()) > 1024 * 1024 {
                self.ndjson = false;
                self.bytes.clear();
                if let Some(guard) = &self.guard {
                    guard.phase("unknown");
                }
                return;
            }
            self.bytes.extend_from_slice(bytes);
            while let Some(end) = self.bytes.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.bytes.drain(..=end).collect();
                if let Ok(line) = std::str::from_utf8(&line) {
                    self.observe(line);
                }
            }
        }
    }
    pub fn complete(&mut self) {
        // NDJSON permits the last record without a final newline.
        if self.ndjson && !self.bytes.is_empty() {
            let bytes = std::mem::take(&mut self.bytes);
            if let Ok(frame) = std::str::from_utf8(&bytes) {
                self.observe(frame);
            }
        }
        // Only the usage of a whole JSON reply is read: it has no phases or first token to time.
        if self.json && !self.bytes.is_empty() {
            let bytes = std::mem::take(&mut self.bytes);
            if let (Some(usage), Ok(value)) =
                (self.usage.as_mut(), serde_json::from_slice::<Value>(&bytes))
            {
                usage.observe(&value);
            }
        }
    }
    fn observe(&mut self, frame: &str) {
        let Ok(value) = serde_json::from_str::<Value>(frame) else {
            return;
        };
        if let Some(usage) = &mut self.usage {
            usage.observe(&value);
        }
        let nonempty = |value: &Value| value.as_str().is_some_and(|text| !text.is_empty());
        if (value.get("error").is_some_and(|error| !error.is_null()) || value["type"] == "error")
            && let Some(metrics) = &self.metrics
        {
            metrics.inference_error();
        }
        let delta = &value["delta"];
        let thinking = value["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                nonempty(&choice["delta"]["reasoning_content"])
                    || nonempty(&choice["delta"]["reasoning"])
            })
        }) || (delta["type"] == "thinking_delta" && nonempty(&delta["thinking"]))
            || nonempty(&value["message"]["thinking"])
            || nonempty(&value["thinking"]);
        let generating = value["choices"].as_array().is_some_and(|choices| {
            choices.iter().any(|choice| {
                nonempty(&choice["delta"]["content"])
                    || nonempty(&choice["text"])
                    || choice["delta"]["tool_calls"]
                        .as_array()
                        .is_some_and(|calls| !calls.is_empty())
            })
        }) || (delta["type"] == "text_delta" && nonempty(&delta["text"]))
            || (delta["type"] == "input_json_delta" && nonempty(&delta["partial_json"]))
            || (value["type"] == "content_block_start"
                && value["content_block"]["type"] == "tool_use"
                && nonempty(&value["content_block"]["name"]))
            || nonempty(&value["message"]["content"])
            || value["message"]["tool_calls"]
                .as_array()
                .is_some_and(|calls| !calls.is_empty())
            || nonempty(&value["response"]);
        if (thinking || generating)
            && let Some(metrics) = &self.metrics
        {
            metrics.first_token();
        }
        if let Some(guard) = &self.guard {
            if generating {
                guard.phase("generating");
            } else if thinking {
                guard.phase("thinking");
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct RateSamples(HashMap<String, RateSample>);
struct RateSample {
    worker: ServerStatus,
    counters: BTreeMap<String, u64>,
    at: Duration,
    uptime: f64,
    rate: Option<f64>,
}
impl RateSamples {
    pub fn retain(&mut self, statuses: &[ServerStatus]) {
        self.0.retain(|id, _| {
            statuses
                .iter()
                .any(|status| &status.profile_id == id && status.pid.is_some())
        });
    }
    pub fn sample(
        &mut self,
        status: &ServerStatus,
        stats: Option<&Value>,
        now: Duration,
    ) -> Option<f64> {
        let read = || -> Option<(BTreeMap<String, u64>, f64)> {
            let stats = stats?;
            let uptime = stats["uptime_s"]
                .as_f64()
                .filter(|uptime| uptime.is_finite() && *uptime >= 0.)?;
            let mut counters = BTreeMap::new();
            for (engine, value) in stats["engines"].as_object()? {
                if value["loaded"].as_bool() == Some(false) {
                    continue;
                }
                counters.insert(engine.clone(), value["tokens_out"].as_u64()?);
            }
            (!counters.is_empty()).then_some((counters, uptime))
        };
        let Some((counters, uptime)) = read() else {
            self.0.remove(&status.profile_id);
            return None;
        };
        if status.pid.is_none() {
            self.0.remove(&status.profile_id);
            return None;
        }
        let mut rate = None;
        if let Some(previous) = self.0.get(&status.profile_id) {
            let elapsed = now.saturating_sub(previous.at);
            let continuous = same_worker(&previous.worker, status)
                && uptime >= previous.uptime
                && counters.keys().eq(previous.counters.keys())
                && counters
                    .iter()
                    .all(|(key, value)| *value >= previous.counters[key]);
            if continuous && elapsed < Duration::from_millis(250) {
                return previous.rate;
            }
            if continuous
                && elapsed <= Duration::from_secs(10)
                && elapsed >= Duration::from_millis(250)
                && uptime > previous.uptime
            {
                let delta = counters.iter().try_fold(0u64, |sum, (key, value)| {
                    sum.checked_add(value - previous.counters[key])
                });
                rate = delta.map(|delta| delta as f64 / elapsed.as_secs_f64());
            }
        }
        self.0.insert(
            status.profile_id.clone(),
            RateSample {
                worker: status.clone(),
                counters,
                at: now,
                uptime,
                rate,
            },
        );
        rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn worker() -> ServerStatus {
        ServerStatus {
            profile_id: "one".into(),
            alias: "model".into(),
            model_id: "model".into(),
            state: "ready".into(),
            pid: Some(42),
            port: Some(1234),
            backend_model: None,
            backend_models: Vec::new(),
            backend_model_contexts: Default::default(),
            error: None,
            active_requests: 0,
            started_at: Some("first-start".into()),
        }
    }
    fn stats(uptime: f64, tokens: u64) -> Value {
        json!({"uptime_s":uptime,"requests":9,"engines":{"locals":{"loaded":true,"tokens_out":tokens,"completion_tokens":9999,"decode_s":1}},"active":[{"engine":"locals","started":1234.5,"tokens":tokens,"prompt_tokens":32}]})
    }
    fn frame(value: Value) -> Vec<u8> {
        format!("data: {value}\r\n\r\n").into_bytes()
    }

    #[test]
    fn fragmented_stream_observes_only_actual_reasoning_and_content_and_clears_on_drop() {
        let (events, mut receiver) = tokio::sync::broadcast::channel(32);
        let tracker = ActivityTracker::with_events(events);
        let status = worker();
        let mut observer =
            ResponseObserver::new(Some(tracker.begin(status.clone())), "text/event-stream");
        assert_eq!(tracker.summary(&status, None)["phase"], "preparing");
        observer
            .push(b": keepalive\n\ndata: {\"choices\":[{\"delta\":{\"role\":\"assistant\"}}]}\n\n");
        assert_eq!(tracker.summary(&status, None)["phase"], "preparing");
        let reasoning = frame(json!({"choices":[{"delta":{"reasoning_content":"Think 🌍"}}]}));
        for byte in reasoning {
            observer.push(&[byte]);
        }
        assert_eq!(tracker.summary(&status, None)["phase"], "thinking");
        observer.push(&frame(json!({"usage":{"completion_tokens":9}})));
        assert_eq!(tracker.summary(&status, None)["phase"], "thinking");
        observer.push(&frame(json!({"choices":[{"delta":{"content":"Answer"}}]})));
        observer.push(&frame(json!({"choices":[{"delta":{"content":"more"}}]})));
        assert_eq!(tracker.summary(&status, None)["phase"], "generating");
        drop(observer);
        let emitted: Vec<_> = std::iter::from_fn(|| receiver.try_recv().ok()).collect();
        let phases: Vec<_> = emitted
            .iter()
            .map(|event| event["data"]["activity"]["phase"].as_str().unwrap())
            .collect();
        assert_eq!(phases, vec!["preparing", "thinking", "generating", "idle"]);
        assert!(
            emitted
                .windows(2)
                .all(|pair| pair[0]["data"]["activity"]["revision"].as_u64()
                    < pair[1]["data"]["activity"]["revision"].as_u64())
        );
    }

    #[test]
    fn metrics_observer_ignores_metadata_and_observes_fragmented_protocols_without_activity_guard()
    {
        for (content_type, payload) in [
            (
                "text/event-stream",
                frame(json!({"choices":[{"delta":{"reasoning_content":"reason"}}]})),
            ),
            (
                "text/event-stream",
                frame(
                    json!({"choices":[{"delta":{"tool_calls":[{"function":{"arguments":"{"}}]}}]}),
                ),
            ),
            (
                "text/event-stream",
                frame(
                    json!({"type":"content_block_start","content_block":{"type":"tool_use","name":"search"}}),
                ),
            ),
            (
                "application/x-ndjson",
                b"{\"message\":{\"content\":\"answer\"}}".to_vec(),
            ),
        ] {
            let metrics = crate::metrics::Metrics::new().unwrap();
            let request = Arc::new(metrics.begin("/v1/chat/completions"));
            request.set_profile("one");
            let mut observer =
                ResponseObserver::new(None, content_type).with_metrics(Some(request.clone()));
            if content_type == "text/event-stream" {
                observer.push(b": keepalive\n\n");
                observer.push(&frame(json!({"choices":[{"delta":{"role":"assistant"}}]})));
                observer.push(&frame(json!({"usage":{"completion_tokens":10}})));
            }
            assert!(
                !metrics
                    .render(&[worker()], &[])
                    .unwrap()
                    .contains("locals_studio_inference_time_to_first_token_seconds_count{")
            );
            for chunk in payload.chunks(3) {
                observer.push(chunk);
            }
            observer.complete();
            request.finish();
            let text = metrics.render(&[worker()], &[]).unwrap();
            assert!(
                text.lines().any(|line| line
                    .starts_with("locals_studio_inference_time_to_first_token_seconds_count{")
                    && line.ends_with(" 1")),
                "{text}"
            );
        }
    }

    #[test]
    fn streamed_errors_are_counted_and_json_does_not_invent_a_first_token() {
        for (content_type, payload, outcome) in [
            (
                "text/event-stream",
                frame(json!({"type":"error","error":{"message":"failed"}})),
                "inference_error",
            ),
            (
                "application/x-ndjson",
                b"{\"error\":\"failed\"}\n".to_vec(),
                "inference_error",
            ),
            (
                "application/json",
                br#"{"choices":[{"message":{"content":"answer"}}]}"#.to_vec(),
                "success",
            ),
        ] {
            let metrics = crate::metrics::Metrics::new().unwrap();
            let request = Arc::new(metrics.begin("/v1/chat/completions"));
            request.set_profile("one");
            let mut observer =
                ResponseObserver::new(None, content_type).with_metrics(Some(request.clone()));
            observer.push(&payload);
            observer.complete();
            request.finish();
            let text = metrics.render(&[worker()], &[]).unwrap();
            assert!(
                text.lines().any(|line| line
                    .starts_with("locals_studio_inference_requests_total{")
                    && line.contains(&format!("outcome=\"{outcome}\""))
                    && line.ends_with(" 1")),
                "{text}"
            );
            assert!(!text.contains("locals_studio_inference_time_to_first_token_seconds_count{"));
        }
    }

    #[test]
    fn anthropic_ollama_text_and_tool_protocols_are_observed_without_rewriting_bytes() {
        for (content_type, thinking, generating) in [
            (
                "text/event-stream",
                frame(json!({"delta":{"type":"thinking_delta","thinking":"think"}})),
                frame(json!({"delta":{"type":"text_delta","text":"answer"}})),
            ),
            (
                "application/x-ndjson",
                b"{\"message\":{\"thinking\":\"reason\"}}\n".to_vec(),
                b"{\"message\":{\"content\":\"answer\"}}\n".to_vec(),
            ),
            (
                "application/x-ndjson",
                b"{\"thinking\":\"reason\"}\n".to_vec(),
                b"{\"response\":\"answer\"}\n".to_vec(),
            ),
            (
                "application/x-ndjson",
                b"{\"message\":{\"thinking\":\"reason\"}}\n".to_vec(),
                b"{\"message\":{\"tool_calls\":[{\"function\":{\"name\":\"search\"}}]}}\n".to_vec(),
            ),
        ] {
            let tracker = ActivityTracker::default();
            let status = worker();
            let mut observer =
                ResponseObserver::new(Some(tracker.begin(status.clone())), content_type);
            for chunk in thinking.chunks(3) {
                observer.push(chunk);
            }
            assert_eq!(tracker.summary(&status, None)["phase"], "thinking");
            for chunk in generating.chunks(2) {
                observer.push(chunk);
            }
            assert_eq!(tracker.summary(&status, None)["phase"], "generating");
        }
        for value in [
            json!({"choices":[{"text":"completion"}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{"}}]}}]}),
            json!({"delta":{"type":"input_json_delta","partial_json":"{"}}),
        ] {
            let tracker = ActivityTracker::default();
            let status = worker();
            let mut observer =
                ResponseObserver::new(Some(tracker.begin(status.clone())), "text/event-stream");
            observer.push(&frame(value));
            assert_eq!(tracker.summary(&status, None)["phase"], "generating");
        }
    }

    #[test]
    fn concurrent_requests_and_worker_restarts_keep_phases_scoped_to_their_instance() {
        let tracker = ActivityTracker::default();
        let status = worker();
        let first = tracker.begin(status.clone());
        let second = tracker.begin(status.clone());
        first.phase("thinking");
        second.phase("generating");
        let summary = tracker.summary(&status, Some(&stats(1., 10)));
        assert_eq!(summary["phase"], "mixed");
        assert_eq!(summary["active_requests"], 2);
        assert_eq!(summary["phases"]["thinking"], 1);
        assert_eq!(summary["phases"]["generating"], 1);
        let mut restarted = status.clone();
        restarted.started_at = Some("second-start".into());
        assert_eq!(
            tracker.summary(&restarted, Some(&json!({"active":[]})))["phase"],
            "idle"
        );
        drop(first);
        assert_eq!(tracker.summary(&status, None)["phase"], "generating");
        drop(second);
        assert_eq!(
            tracker.summary(&status, Some(&json!({"active":[]})))["phase"],
            "idle"
        );
    }

    #[test]
    fn worker_stats_expose_decoding_without_claiming_a_reasoning_phase_or_prefill_stage() {
        let tracker = ActivityTracker::default();
        let status = worker();
        let _request = tracker.begin(status.clone());
        assert_eq!(
            tracker.summary(&status, Some(&stats(1., 0)))["phase"],
            "preparing"
        );
        let summary = tracker.summary(&status, Some(&stats(2., 20)));
        assert_eq!(summary["phase"], "decoding");
        assert_eq!(summary["prompt_tokens"], 32);
        assert_eq!(summary["output_tokens"], 20);
        assert_eq!(summary["total_requests"], 9);
        assert_eq!(
            ActivityTracker::default().summary(&status, None)["phase"],
            "unknown"
        );
        let mut observer =
            ResponseObserver::new(Some(tracker.begin(status.clone())), "application/x-ndjson");
        observer.push(&vec![b'x'; 1024 * 1024 + 1]);
        assert!(observer.bytes.is_empty());
        assert!(!observer.ndjson);
    }

    #[test]
    fn live_rate_uses_monotonic_engine_tokens_and_retains_the_sample_between_nearby_polls() {
        let mut samples = RateSamples::default();
        let status = worker();
        assert_eq!(
            samples.sample(&status, Some(&stats(1., 10)), Duration::from_secs(1)),
            None
        );
        assert_eq!(
            samples.sample(&status, Some(&stats(3., 50)), Duration::from_secs(3)),
            Some(20.)
        );
        assert_eq!(
            samples.sample(&status, Some(&stats(3.01, 50)), Duration::from_millis(3010)),
            Some(20.)
        );
        let mut next = stats(4., 80);
        next["active"] = json!([]);
        assert_eq!(
            samples.sample(&status, Some(&next), Duration::from_secs(4)),
            Some(30.),
            "completion/removal of active rows must not reset the output counter"
        );
        assert_eq!(
            samples.sample(&status, Some(&stats(5., 1)), Duration::from_secs(5)),
            None,
            "same-PID engine restart resets its baseline"
        );
        assert_eq!(
            samples.sample(&status, Some(&stats(6., 11)), Duration::from_secs(6)),
            Some(10.)
        );
        assert_eq!(samples.sample(&status, None, Duration::from_secs(7)), None);
        assert_eq!(
            samples.sample(&status, Some(&stats(8., 31)), Duration::from_secs(8)),
            None
        );
        let mut restarted = status.clone();
        restarted.started_at = Some("restart".into());
        assert_eq!(
            samples.sample(&restarted, Some(&stats(9., 50)), Duration::from_secs(9)),
            None
        );
        assert_eq!(
            samples.sample(&restarted, Some(&stats(1., 60)), Duration::from_secs(10)),
            None,
            "older uptime is not continuous"
        );
        assert_eq!(samples.sample(&restarted,Some(&json!({"uptime_s":2,"engines":{"locals":{"completion_tokens":999,"decode_s":1}}})),Duration::from_secs(11)),None,"lifetime averages are not a live counter fallback");
    }

    fn recorded_usage(content_type: &str, chunks: &[Vec<u8>]) -> Option<Value> {
        let (events, mut receiver) = tokio::sync::broadcast::channel(32);
        let mut observer = ResponseObserver::new(None, content_type).with_usage(
            events,
            "profile-42".into(),
            "model".into(),
            "claude-cli".into(),
        );
        for chunk in chunks {
            observer.push(chunk);
        }
        observer.complete();
        drop(observer);
        std::iter::from_fn(|| receiver.try_recv().ok())
            .find(|event| event["type"] == "usage.recorded")
            .map(|event| event["data"].clone())
    }

    fn tokens(data: &Value) -> (u64, u64, u64) {
        (
            data["prompt"].as_u64().unwrap(),
            data["cached"].as_u64().unwrap(),
            data["output"].as_u64().unwrap(),
        )
    }

    #[test]
    fn openai_stream_usage_is_recorded_with_its_cached_prompt() {
        let data = recorded_usage(
            "text/event-stream",
            &[
                frame(json!({"choices":[{"delta":{"content":"Answer"}}]})),
                frame(json!({"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"completion_tokens":20,"prompt_tokens_details":{"cached_tokens":40}}})),
            ],
        )
        .expect("usage.recorded");
        assert_eq!(tokens(&data), (100, 40, 20));
        assert_eq!(data["profile_id"], "profile-42");
        assert_eq!(data["client"], "claude-cli");
    }

    #[test]
    fn anthropic_stream_usage_counts_cache_reads_as_prompt() {
        let data = recorded_usage(
            "text/event-stream",
            &[
                frame(json!({"type":"message_start","message":{"usage":{"input_tokens":100,"output_tokens":0}}})),
                frame(json!({"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}})),
                frame(json!({"type":"message_delta","usage":{"input_tokens":60,"output_tokens":25,"cache_read_input_tokens":40}})),
            ],
        )
        .expect("usage.recorded");
        assert_eq!(tokens(&data), (100, 40, 25));
    }

    #[test]
    fn ollama_ndjson_usage_is_read_from_the_final_record() {
        let data = recorded_usage(
            "application/x-ndjson",
            &[
                b"{\"message\":{\"content\":\"Hi\"},\"done\":false}\n".to_vec(),
                b"{\"done\":true,\"prompt_eval_count\":70,\"eval_count\":9}".to_vec(),
            ],
        )
        .expect("usage.recorded");
        assert_eq!(tokens(&data), (70, 0, 9));
    }

    #[test]
    fn a_non_streamed_json_reply_records_its_usage() {
        let body = serde_json::to_vec(&json!({"choices":[{"message":{"content":"Hi"}}],"usage":{"prompt_tokens":12,"completion_tokens":3}})).unwrap();
        let (head, tail) = body.split_at(10);
        let data = recorded_usage("application/json", &[head.to_vec(), tail.to_vec()])
            .expect("usage.recorded");
        assert_eq!(tokens(&data), (12, 0, 3));
    }

    #[test]
    fn a_response_without_usage_records_nothing() {
        assert!(
            recorded_usage(
                "text/event-stream",
                &[frame(json!({"choices":[{"delta":{"content":"Answer"}}]}))],
            )
            .is_none()
        );
    }
}
