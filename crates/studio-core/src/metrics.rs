//! Per-daemon metrics. Scrapes encode cached observations and never poll inference workers.
use crate::{models::ModelRecord, types::ServerStatus};
use anyhow::Result;
use prometheus::{CounterVec, GaugeVec, HistogramOpts, HistogramVec, Opts, Registry, TextEncoder};
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex, MutexGuard},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

const PREFIX: &str = "locals_studio_";
const CACHE_TTL: Duration = Duration::from_secs(10);
const ENDPOINTS: &[&str] = &[
    "/v1/chat/completions",
    "/v1/completions",
    "/v1/messages",
    "/v1/messages/count_tokens",
    "/api/chat",
    "/api/generate",
    "/api/show",
    "/api/embed",
    "/api/embeddings",
    "other",
];
const OUTCOMES: &[&str] = &[
    "success",
    "http_error",
    "inference_error",
    "transport_error",
    "cancelled",
];
const STATES: &[&str] = &[
    "stopped", "starting", "loading", "ready", "running", "draining", "stopping", "error",
    "failed", "unknown",
];
const PHASES: &[&str] = &["preparing", "thinking", "generating", "decoding", "unknown"];
const BUCKETS: &[f64] = &[
    0.01, 0.05, 0.1, 0.25, 0.5, 1., 2.5, 5., 10., 30., 60., 120., 300.,
];

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}
fn opts(name: &str, help: &str) -> Opts {
    Opts::new(format!("{PREFIX}{name}"), help)
}
fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|value| value.is_finite() && *value >= 0.)
}
fn endpoint(value: &str) -> &'static str {
    let value = match value {
        "/chat/completions" => "/v1/chat/completions",
        "/completions" => "/v1/completions",
        _ => value,
    };
    ENDPOINTS
        .iter()
        .copied()
        .find(|known| *known == value)
        .unwrap_or("other")
}

#[derive(Clone)]
pub(crate) struct Metrics(Arc<Inner>);
struct Inner {
    registry: Registry,
    requests: CounterVec,
    duration: HistogramVec,
    first_token: HistogramVec,
    in_flight: GaugeVec,
    // Keep zero-count profiles until a render can retire deleted label sets safely.
    live: Mutex<HashMap<String, usize>>,
    sample: Mutex<Option<Sample>>,
    created: Instant,
    started_at: f64,
}
struct Sample {
    started: Instant,
    value: Value,
}

impl Metrics {
    pub(crate) fn new() -> Result<Self> {
        let registry = Registry::new();
        let labels = &["profile_id", "endpoint", "outcome"];
        let requests = CounterVec::new(
            opts(
                "inference_requests_total",
                "Studio gateway inference requests finalized by outcome; client or server abandonment is cancelled.",
            ),
            labels,
        )?;
        let duration = HistogramVec::new(HistogramOpts::new(format!("{PREFIX}inference_request_duration_seconds"), "Studio gateway request lifetime through response-body completion, failure or abandonment, including queueing and streaming.").buckets(BUCKETS.to_vec()), labels)?;
        let first_token = HistogramVec::new(HistogramOpts::new(format!("{PREFIX}inference_time_to_first_token_seconds"), "Time from gateway admission to first observed nonempty reasoning, content or tool fragment; unobserved and non-streamed first tokens are omitted.").buckets(BUCKETS.to_vec()), &["profile_id", "endpoint"])?;
        let in_flight = GaugeVec::new(
            opts(
                "inference_requests_in_flight",
                "Requests currently observed by the Studio gateway, including response-body streaming.",
            ),
            &["profile_id", "endpoint"],
        )?;
        registry.register(Box::new(requests.clone()))?;
        registry.register(Box::new(duration.clone()))?;
        registry.register(Box::new(first_token.clone()))?;
        registry.register(Box::new(in_flight.clone()))?;
        Ok(Self(Arc::new(Inner {
            registry,
            requests,
            duration,
            first_token,
            in_flight,
            live: Mutex::new(HashMap::new()),
            sample: Mutex::new(None),
            created: Instant::now(),
            started_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs_f64(),
        })))
    }

    pub(crate) fn record_telemetry(&self, sampled_start: Instant, value: Value) {
        let mut sample = lock(&self.0.sample);
        if sample
            .as_ref()
            .is_none_or(|previous| sampled_start > previous.started)
        {
            *sample = Some(Sample {
                started: sampled_start,
                value,
            });
        }
    }

    pub(crate) fn begin(&self, endpoint_name: &str) -> RequestMetrics {
        let endpoint = endpoint(endpoint_name);
        let mut live = lock(&self.0.live);
        *live.entry("unresolved".into()).or_default() += 1;
        self.0
            .in_flight
            .with_label_values(&["unresolved", endpoint])
            .inc();
        RequestMetrics {
            metrics: self.clone(),
            endpoint,
            started: Instant::now(),
            state: Mutex::new(RequestState {
                profile: "unresolved".into(),
                status: 200,
                first_token: false,
                inference_error: false,
                transport_error: false,
                finished: false,
            }),
        }
    }

    fn retire_profiles(&self, known: &HashSet<&str>) {
        let mut live = lock(&self.0.live);
        live.retain(|profile, active| {
            if profile == "unresolved" || known.contains(profile.as_str()) || *active > 0 {
                return true;
            }
            for endpoint in ENDPOINTS {
                let _ = self
                    .0
                    .in_flight
                    .remove_label_values(&[profile.as_str(), endpoint]);
                let _ = self
                    .0
                    .first_token
                    .remove_label_values(&[profile.as_str(), endpoint]);
                for outcome in OUTCOMES {
                    let labels = &[profile.as_str(), endpoint, outcome];
                    let _ = self.0.requests.remove_label_values(labels);
                    let _ = self.0.duration.remove_label_values(labels);
                }
            }
            false
        });
    }

    pub(crate) fn render(
        &self,
        statuses: &[ServerStatus],
        models: &[ModelRecord],
    ) -> Result<String> {
        let known: HashSet<_> = statuses
            .iter()
            .map(|status| status.profile_id.as_str())
            .collect();
        self.retire_profiles(&known);
        let mut families = self.0.registry.gather();
        for family in &mut families {
            family.mut_metric().retain(|metric| {
                metric.get_label().iter().all(|label| {
                    label.name() != "profile_id"
                        || label.value() == "unresolved"
                        || known.contains(label.value())
                })
            });
        }
        families.retain(|family| !family.get_metric().is_empty());
        let mut snapshot = SnapshotMetrics::new();
        snapshot.gauge(
            "start_time_seconds",
            "Unix time at which this Studio daemon metrics registry started.",
            &[],
            &[],
            Some(self.0.started_at),
        )?;
        snapshot.gauge(
            "uptime_seconds",
            "Uptime of this Studio daemon metrics registry.",
            &[],
            &[],
            Some(self.0.created.elapsed().as_secs_f64()),
        )?;
        snapshot.gauge(
            "build_info",
            "Studio management service package version.",
            &["version"],
            &[env!("CARGO_PKG_VERSION")],
            Some(1.),
        )?;
        let sample = lock(&self.0.sample);
        let age = sample
            .as_ref()
            .and_then(|sample| Instant::now().checked_duration_since(sample.started));
        let value = sample
            .as_ref()
            .filter(|_| age.is_some_and(|age| age <= CACHE_TTL))
            .map(|sample| &sample.value);
        snapshot.gauge(
            "telemetry_up",
            "Whether a cached telemetry collection started within the last 10 seconds.",
            &[],
            &[],
            Some(f64::from(value.is_some())),
        )?;
        if let Some(sample) = sample.as_ref() {
            if let Some(timestamp) = sample.value["sampled_at"]
                .as_str()
                .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
            {
                snapshot.gauge(
                    "telemetry_last_sample_timestamp_seconds",
                    "Unix timestamp of the latest accepted telemetry sample, including when stale.",
                    &[],
                    &[],
                    Some(timestamp.timestamp_millis() as f64 / 1000.),
                )?;
            }
        }
        if let Some(value) = value {
            snapshot.system(&value["system"])?;
        }
        for status in statuses {
            let worker = value
                .and_then(|value| value["workers"].as_array())
                .and_then(|workers| workers.iter().find(|worker| matches_worker(worker, status)));
            snapshot.worker(status, worker)?;
        }
        drop(sample);
        families.extend(snapshot.registry.gather());
        enrich_model_labels(&mut families, statuses, models);
        Ok(TextEncoder::new().encode_to_string(&families)?)
    }
}

// Mutable display metadata belongs to exposition, not to the counter/histogram
// identities: renaming a model or alias must not replace the accumulated values.
fn enrich_model_labels(
    families: &mut [prometheus::proto::MetricFamily],
    statuses: &[ServerStatus],
    models: &[ModelRecord],
) {
    let profiles: HashMap<_, _> = statuses
        .iter()
        .map(|status| (status.profile_id.as_str(), status))
        .collect();
    let names: HashMap<_, _> = models
        .iter()
        .map(|model| (model.id.as_str(), model.name.as_str()))
        .collect();
    for family in families {
        for metric in family.mut_metric() {
            let status = metric
                .get_label()
                .iter()
                .find(|label| label.name() == "profile_id" && label.value() != "unresolved")
                .and_then(|label| profiles.get(label.value()));
            let Some(status) = status else {
                continue;
            };
            let name = names
                .get(status.model_id.as_str())
                .copied()
                .unwrap_or(status.alias.as_str());
            let mut labels = metric.take_label();
            labels.retain(|label| !matches!(label.name(), "model_id" | "model" | "alias"));
            for (key, value) in [
                ("model_id", status.model_id.as_str()),
                ("model", name),
                ("alias", status.alias.as_str()),
            ] {
                let mut label = prometheus::proto::LabelPair::new();
                label.set_name(key.into());
                label.set_value(value.into());
                labels.push(label);
            }
            labels.sort_by(|left, right| left.name().cmp(right.name()));
            metric.set_label(labels);
        }
    }
}

struct RequestState {
    profile: String,
    status: u16,
    first_token: bool,
    inference_error: bool,
    transport_error: bool,
    finished: bool,
}
pub(crate) struct RequestMetrics {
    metrics: Metrics,
    endpoint: &'static str,
    started: Instant,
    state: Mutex<RequestState>,
}
impl RequestMetrics {
    pub(crate) fn set_profile(&self, profile_id: &str) {
        let mut state = lock(&self.state);
        let profile = if profile_id.is_empty() {
            "unresolved"
        } else {
            profile_id
        };
        if state.finished || state.profile == profile {
            return;
        }
        let mut live = lock(&self.metrics.0.live);
        self.metrics
            .0
            .in_flight
            .with_label_values(&[state.profile.as_str(), self.endpoint])
            .dec();
        if let Some(active) = live.get_mut(&state.profile) {
            *active = active.saturating_sub(1);
        }
        *live.entry(profile.into()).or_default() += 1;
        self.metrics
            .0
            .in_flight
            .with_label_values(&[profile, self.endpoint])
            .inc();
        state.profile = profile.into();
    }
    pub(crate) fn http_status(&self, status: u16) {
        let mut state = lock(&self.state);
        if !state.finished {
            state.status = status;
        }
    }
    pub(crate) fn first_token(&self) {
        let mut state = lock(&self.state);
        if state.finished || state.first_token {
            return;
        }
        self.metrics
            .0
            .first_token
            .with_label_values(&[state.profile.as_str(), self.endpoint])
            .observe(self.started.elapsed().as_secs_f64());
        state.first_token = true;
    }
    pub(crate) fn inference_error(&self) {
        let mut state = lock(&self.state);
        if !state.finished {
            state.inference_error = true;
        }
    }
    pub(crate) fn transport_error(&self) {
        let mut state = lock(&self.state);
        if !state.finished {
            state.transport_error = true;
        }
    }
    pub(crate) fn finish(&self) {
        self.finalize(true);
    }
    fn finalize(&self, completed: bool) {
        let mut state = lock(&self.state);
        if state.finished {
            return;
        }
        let outcome = if state.transport_error {
            "transport_error"
        } else if !completed {
            "cancelled"
        } else if state.inference_error {
            "inference_error"
        } else if !(200..300).contains(&state.status) {
            "http_error"
        } else {
            "success"
        };
        let labels = &[state.profile.as_str(), self.endpoint, outcome];
        self.metrics.0.requests.with_label_values(labels).inc();
        self.metrics
            .0
            .duration
            .with_label_values(labels)
            .observe(self.started.elapsed().as_secs_f64());
        let mut live = lock(&self.metrics.0.live);
        self.metrics
            .0
            .in_flight
            .with_label_values(&[state.profile.as_str(), self.endpoint])
            .dec();
        if let Some(active) = live.get_mut(&state.profile) {
            *active = active.saturating_sub(1);
        }
        state.finished = true;
    }
}
impl Drop for RequestMetrics {
    fn drop(&mut self) {
        self.finalize(false);
    }
}

fn matches_worker(worker: &Value, status: &ServerStatus) -> bool {
    status.pid.is_some()
        && worker["profile_id"].as_str() == Some(status.profile_id.as_str())
        && worker["pid"].as_u64() == status.pid.map(u64::from)
        && worker["port"].as_u64() == status.port.map(u64::from)
        && worker["started_at"].as_str() == status.started_at.as_deref()
}

// A fresh snapshot registry prevents a missing observation from reusing a former
// gauge and exposes raw upstream counters exactly once, rather than adding them on polls.
struct SnapshotMetrics {
    registry: Registry,
    gauges: HashMap<&'static str, GaugeVec>,
    counters: HashMap<&'static str, CounterVec>,
}
impl SnapshotMetrics {
    fn new() -> Self {
        Self {
            registry: Registry::new(),
            gauges: HashMap::new(),
            counters: HashMap::new(),
        }
    }
    fn gauge(
        &mut self,
        name: &'static str,
        help: &str,
        names: &[&str],
        labels: &[&str],
        value: Option<f64>,
    ) -> Result<()> {
        let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.) else {
            return Ok(());
        };
        if !self.gauges.contains_key(name) {
            let gauge = GaugeVec::new(opts(name, help), names)?;
            self.registry.register(Box::new(gauge.clone()))?;
            self.gauges.insert(name, gauge);
        }
        self.gauges[name].with_label_values(labels).set(value);
        Ok(())
    }
    fn counter(
        &mut self,
        name: &'static str,
        help: &str,
        names: &[&str],
        labels: &[&str],
        value: Option<f64>,
    ) -> Result<()> {
        let Some(value) = value.filter(|value| value.is_finite() && *value >= 0.) else {
            return Ok(());
        };
        if !self.counters.contains_key(name) {
            let counter = CounterVec::new(opts(name, help), names)?;
            self.registry.register(Box::new(counter.clone()))?;
            self.counters.insert(name, counter);
        }
        self.counters[name].with_label_values(labels).inc_by(value);
        Ok(())
    }
    fn system(&mut self, system: &Value) -> Result<()> {
        for (field, name, help) in [
            (
                "ram_total_bytes",
                "system_memory_total_bytes",
                "Total host unified memory in bytes.",
            ),
            (
                "ram_used_bytes",
                "system_memory_used_bytes",
                "Used host unified memory in bytes.",
            ),
            (
                "ram_available_bytes",
                "system_memory_available_bytes",
                "Available host unified memory in bytes.",
            ),
            (
                "swap_total_bytes",
                "system_swap_total_bytes",
                "Total host swap in bytes.",
            ),
            (
                "swap_used_bytes",
                "system_swap_used_bytes",
                "Used host swap in bytes.",
            ),
            (
                "cpu_cores",
                "system_cpu_cores",
                "Number of host logical CPU cores.",
            ),
        ] {
            self.gauge(name, help, &[], &[], number(&system[field]))?;
        }
        self.gauge(
            "system_cpu_usage_ratio",
            "Sampled host CPU usage as a fraction of all host CPU capacity.",
            &[],
            &[],
            number(&system["cpu_percent"]).map(|value| value / 100.),
        )?;
        Ok(())
    }
    fn worker(&mut self, status: &ServerStatus, worker: Option<&Value>) -> Result<()> {
        let labels = &[status.profile_id.as_str()];
        let stats = worker.and_then(|worker| worker["stats"].as_object().map(|_| &worker["stats"]));
        self.gauge(
            "worker_ready",
            "Whether the current worker has a PID and Studio reports ready or running.",
            &["profile_id"],
            labels,
            Some(f64::from(
                status.pid.is_some() && matches!(status.state.as_str(), "ready" | "running"),
            )),
        )?;
        self.gauge(
            "worker_stats_up",
            "Whether the latest fresh, identity-matched worker stats collection succeeded.",
            &["profile_id"],
            labels,
            Some(f64::from(stats.is_some())),
        )?;
        let state = if STATES.contains(&status.state.as_str()) {
            status.state.as_str()
        } else {
            "unknown"
        };
        for candidate in STATES {
            self.gauge(
                "worker_state",
                "Current Studio worker state; one-hot over bounded state names.",
                &["profile_id", "state"],
                &[status.profile_id.as_str(), candidate],
                Some(f64::from(*candidate == state)),
            )?;
        }
        if let Some(timestamp) = status
            .started_at
            .as_deref()
            .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        {
            self.gauge(
                "worker_start_time_seconds",
                "Unix timestamp of the current worker launch.",
                &["profile_id"],
                labels,
                Some(timestamp.timestamp_millis() as f64 / 1000.),
            )?;
        }
        if status.pid.is_none() {
            self.gauge("worker_active_requests", "Active requests reported by the worker, zero for a stopped worker; unavailable live measurements are omitted.", &["profile_id"], labels, Some(0.))?;
        }
        if let Some(worker) = worker {
            for (field, name, help) in [
                (
                    "rss_bytes",
                    "worker_resident_memory_bytes",
                    "Current worker RSS; overlaps with MLX allocations and must not be added to them.",
                ),
                (
                    "rss_peak_bytes",
                    "worker_resident_memory_peak_bytes",
                    "Peak worker RSS reported by Locals.",
                ),
                (
                    "mlx_active_bytes",
                    "worker_mlx_active_memory_bytes",
                    "Worker memory currently allocated by MLX; overlaps with process RSS.",
                ),
                (
                    "mlx_cache_bytes",
                    "worker_mlx_cache_memory_bytes",
                    "Worker memory retained in the MLX allocator cache; overlaps with process RSS.",
                ),
                (
                    "mlx_peak_bytes",
                    "worker_mlx_peak_memory_bytes",
                    "Peak worker MLX allocation reported by Locals.",
                ),
            ] {
                self.gauge(name, help, &["profile_id"], labels, number(&worker[field]))?;
            }
            self.gauge(
                "worker_cpu_usage_ratio",
                "Sampled worker CPU usage as a fraction of all host CPU capacity.",
                &["profile_id"],
                labels,
                number(&worker["cpu_percent"]).map(|value| value / 100.),
            )?;
            let activity = &worker["activity"];
            if (stats.is_some()
                || matches!(
                    activity["phase_source"].as_str(),
                    Some("gateway" | "combined")
                ))
                && activity["phases"].is_object()
            {
                for phase in PHASES {
                    self.gauge("worker_activity_requests", "Sampled request phase counts combining observed gateway output with worker activity; preparing includes queueing and prompt preparation, decoding does not distinguish thinking.", &["profile_id", "phase"], &[status.profile_id.as_str(), phase], Some(number(&activity["phases"][*phase]).unwrap_or(0.)))?;
                }
            }
        }
        let Some(stats) = stats else {
            return Ok(());
        };
        self.gauge("worker_active_requests", "Active requests reported by the worker, zero for a stopped worker; unavailable live measurements are omitted.", &["profile_id"], labels, stats["active"].as_array().map(|active| active.len() as f64))?;
        self.gauge(
            "worker_uptime_seconds",
            "Worker uptime reported by Locals.",
            &["profile_id"],
            labels,
            number(&stats["uptime_s"]),
        )?;
        self.counter("worker_requests_total", "Raw Locals worker request counter; admission rules differ by protocol and exclude some rejected requests.", &["profile_id"], labels, stats["requests"].as_u64().map(|value| value as f64))?;
        self.counter("worker_errors_total", "Raw Locals worker errors, including validation and streaming errors; not an HTTP status counter and not directly comparable to worker_requests_total.", &["profile_id"], labels, stats["errors"].as_u64().map(|value| value as f64))?;
        for engine in ["locals", "mlx"] {
            let value = &stats["engines"][engine];
            if !value.is_object() {
                continue;
            }
            let labels = &[status.profile_id.as_str(), engine];
            self.gauge(
                "worker_engine_loaded",
                "Whether Locals reports the inference engine loaded.",
                &["profile_id", "engine"],
                labels,
                value["loaded"].as_bool().map(f64::from),
            )?;
            for (field, name, help) in [
                (
                    "requests",
                    "worker_engine_requests_total",
                    "Raw Locals per-engine accounted request completions; excludes requests which never reach final accounting.",
                ),
                (
                    "prompt_tokens",
                    "worker_prompt_tokens_total",
                    "Raw engine prompt-token counter for admitted jobs; may include internal attempts or cache priming and is not billable client usage.",
                ),
                (
                    "cached_tokens",
                    "worker_cached_tokens_total",
                    "Raw engine cached-token counter; includes engine job accounting and is not billable client usage.",
                ),
                (
                    "completion_tokens",
                    "worker_completion_tokens_total",
                    "Raw Locals completion tokens recorded at final request accounting; internal retries and aborted requests may differ from generated tokens.",
                ),
                (
                    "tokens_out",
                    "worker_generated_tokens_total",
                    "Raw engine generated tokens including reasoning, tool output and internal attempts; resets when the worker or engine restarts.",
                ),
                (
                    "tool_calls",
                    "worker_tool_calls_total",
                    "Raw Locals tool calls recorded at final request accounting.",
                ),
                (
                    "forced_tokens",
                    "worker_forced_tokens_total",
                    "Raw Locals grammar-forced tokens recorded at final request accounting.",
                ),
            ] {
                self.counter(
                    name,
                    help,
                    &["profile_id", "engine"],
                    labels,
                    value[field].as_u64().map(|value| value as f64),
                )?;
            }
            self.counter("worker_accounted_job_duration_seconds_total", "Cumulative Locals accounted job time from submission to completion, including queueing, prompt work and decoding; concurrent jobs overlap and this is not GPU utilization.", &["profile_id", "engine"], labels, number(&value["decode_s"]))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn worker(id: &str, pid: Option<u32>, state: &str) -> ServerStatus {
        ServerStatus {
            profile_id: id.into(),
            alias: format!("private-alias-{id}"),
            model_id: "private/model".into(),
            engine: Default::default(),
            runtime_version: None,
            state: state.into(),
            pid,
            port: pid.map(|_| 8322),
            backend_model: None,
            backend_models: Vec::new(),
            backend_model_contexts: Default::default(),
            error: None,
            active_requests: 0,
            started_at: pid.map(|_| "2026-10-29T10:00:00Z".into()),
        }
    }
    fn telemetry(status: &ServerStatus, generated: u64) -> Value {
        json!({"sampled_at":"2026-10-29T10:00:01.250Z","system":{"ram_total_bytes":16000,"ram_used_bytes":8000,"cpu_percent":25.0,"cpu_cores":8},"workers":[{
            "profile_id":status.profile_id,"pid":status.pid,"port":status.port,"started_at":status.started_at,
            "rss_bytes":4000,"rss_peak_bytes":5000,"mlx_active_bytes":2000,"mlx_cache_bytes":1000,"mlx_peak_bytes":3000,"cpu_percent":50.0,
            "stats":{"model":"/private/model/path","uptime_s":5.0,"requests":7,"errors":2,"active":[{},{}],"engines":{"locals":{"loaded":true,"requests":5,"prompt_tokens":100,"cached_tokens":40,"completion_tokens":20,"tokens_out":generated,"decode_s":2.5},"unbounded-engine-name":{"tokens_out":999}}},
            "activity":{"phase_source":"combined","phases":{"thinking":1,"generating":1}}
        }]})
    }
    fn value(output: &str, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        let name = format!("{PREFIX}{name}");
        output.lines().find_map(|line| {
            if !line.starts_with(&name)
                || !matches!(line.as_bytes().get(name.len()), Some(b' ' | b'{'))
            {
                return None;
            }
            if !labels
                .iter()
                .all(|(key, value)| line.contains(&format!("{key}=\"{value}\"")))
            {
                return None;
            }
            line.split_whitespace().last()?.parse().ok()
        })
    }

    #[test]
    fn registries_are_isolated_and_labels_are_escaped_without_exporting_paths() {
        let first = Metrics::new().unwrap();
        let second = Metrics::new().unwrap();
        let status = worker("quote\" backslash\\\nline", Some(1), "ready");
        let guard = first.begin("/chat/completions");
        guard.set_profile(&status.profile_id);
        guard.finish();
        let output = first.render(std::slice::from_ref(&status), &[]).unwrap();
        assert!(output.contains("profile_id=\"quote\\\" backslash\\\\\\nline\""));
        assert!(output.contains("endpoint=\"/v1/chat/completions\""));
        assert!(output.contains("model_id=\"private/model\""));
        assert!(!output.contains("/private/model/path"));
        assert!(
            !second
                .render(&[status], &[])
                .unwrap()
                .contains("inference_requests_total")
        );
        let invalid = first.begin("/private/unbounded/request/path");
        invalid.http_status(404);
        invalid.finish();
        let output = first.render(&[], &[]).unwrap();
        assert!(output.contains("endpoint=\"other\""));
        assert!(!output.contains("/private/unbounded/request/path"));
    }

    #[test]
    fn newest_collection_start_wins_and_raw_counters_are_not_added_on_scrapes() {
        let metrics = Metrics::new().unwrap();
        let status = worker("one", Some(42), "ready");
        let now = Instant::now();
        let newer = now - Duration::from_secs(1);
        metrics.record_telemetry(newer, telemetry(&status, 23));
        metrics.record_telemetry(now - Duration::from_secs(2), telemetry(&status, 999));
        metrics.record_telemetry(newer, telemetry(&status, 888));
        for _ in 0..2 {
            let output = metrics.render(std::slice::from_ref(&status), &[]).unwrap();
            assert_eq!(
                value(
                    &output,
                    "worker_generated_tokens_total",
                    &[("profile_id", "one"), ("engine", "locals")]
                ),
                Some(23.)
            );
            assert_eq!(
                value(
                    &output,
                    "worker_completion_tokens_total",
                    &[("profile_id", "one")]
                ),
                Some(20.)
            );
            assert_eq!(value(&output, "system_cpu_usage_ratio", &[]), Some(0.25));
            assert_eq!(
                value(&output, "worker_cpu_usage_ratio", &[("profile_id", "one")]),
                Some(0.5)
            );
            assert_eq!(
                value(&output, "worker_active_requests", &[("profile_id", "one")]),
                Some(2.)
            );
            assert_eq!(
                value(
                    &output,
                    "worker_activity_requests",
                    &[("phase", "thinking")]
                ),
                Some(1.)
            );
            assert!(!output.contains("unbounded-engine-name"));
            assert!(!output.contains("/private/model/path"));
            assert!(output.contains("# TYPE locals_studio_worker_generated_tokens_total counter"));
        }
    }

    #[test]
    fn absent_or_stale_cache_does_not_make_live_workers_zero_or_ready_states_stale() {
        let metrics = Metrics::new().unwrap();
        let status = worker("one", Some(42), "ready");
        let cold = metrics.render(std::slice::from_ref(&status), &[]).unwrap();
        assert_eq!(value(&cold, "telemetry_up", &[]), Some(0.));
        assert_eq!(value(&cold, "worker_ready", &[]), Some(1.));
        assert_eq!(value(&cold, "worker_stats_up", &[]), Some(0.));
        assert_eq!(value(&cold, "worker_active_requests", &[]), None);
        metrics.record_telemetry(
            Instant::now() - Duration::from_secs(11),
            telemetry(&status, 5),
        );
        let stale = metrics.render(std::slice::from_ref(&status), &[]).unwrap();
        assert_eq!(value(&stale, "worker_stats_up", &[]), Some(0.));
        assert_eq!(value(&stale, "worker_resident_memory_bytes", &[]), None);
        assert_eq!(value(&stale, "system_memory_used_bytes", &[]), None);
        assert_eq!(value(&stale, "worker_generated_tokens_total", &[]), None);
        assert!(value(&stale, "telemetry_last_sample_timestamp_seconds", &[]).is_some());
        let stopped = worker("one", None, "stopped");
        let output = metrics.render(&[stopped], &[]).unwrap();
        assert_eq!(value(&output, "worker_ready", &[]), Some(0.));
        assert_eq!(value(&output, "worker_active_requests", &[]), Some(0.));
        assert_eq!(
            value(&output, "worker_state", &[("state", "stopped")]),
            Some(1.)
        );
    }

    #[test]
    fn worker_identity_changes_and_profile_removal_never_reuse_old_measurements() {
        let status = worker("one", Some(42), "ready");
        let metrics = Metrics::new().unwrap();
        metrics.record_telemetry(Instant::now(), telemetry(&status, 5));
        for changed in [
            ServerStatus {
                pid: Some(43),
                ..status.clone()
            },
            ServerStatus {
                port: Some(9000),
                ..status.clone()
            },
            ServerStatus {
                started_at: Some("2026-10-29T10:01:00Z".into()),
                ..status.clone()
            },
        ] {
            let output = metrics.render(&[changed], &[]).unwrap();
            assert_eq!(value(&output, "worker_stats_up", &[]), Some(0.));
            assert_eq!(value(&output, "worker_generated_tokens_total", &[]), None);
            assert_eq!(value(&output, "worker_resident_memory_bytes", &[]), None);
        }
        let output = metrics.render(&[], &[]).unwrap();
        assert!(!output.contains("profile_id=\"one\""));
        // A request can outlive profile deletion without publishing removed labels
        // or decrementing a gauge which was already deleted from the registry.
        let guard = metrics.begin("/api/chat");
        guard.set_profile("one");
        assert!(
            !metrics
                .render(&[], &[])
                .unwrap()
                .contains("profile_id=\"one\"")
        );
        drop(guard);
        assert!(
            !metrics
                .render(&[], &[])
                .unwrap()
                .contains("profile_id=\"one\"")
        );
        assert!(!lock(&metrics.0.live).contains_key("one"));
    }

    #[test]
    fn two_workers_keep_state_and_missing_measurements_independent() {
        let first = worker("one", Some(1), "ready");
        let second = worker("two", Some(2), "starting");
        let mut data = telemetry(&first, 17);
        data["workers"].as_array_mut().unwrap().push(json!({"profile_id":"two","pid":2,"port":8322,"started_at":second.started_at,"stats":null,"rss_bytes":6000,"cpu_percent":null,"mlx_active_bytes":null}));
        let metrics = Metrics::new().unwrap();
        metrics.record_telemetry(Instant::now(), data);
        let output = metrics.render(&[first, second], &[]).unwrap();
        assert_eq!(
            value(&output, "worker_stats_up", &[("profile_id", "one")]),
            Some(1.)
        );
        assert_eq!(
            value(&output, "worker_stats_up", &[("profile_id", "two")]),
            Some(0.)
        );
        assert_eq!(
            value(&output, "worker_ready", &[("profile_id", "two")]),
            Some(0.)
        );
        assert_eq!(
            value(
                &output,
                "worker_state",
                &[("profile_id", "two"), ("state", "starting")]
            ),
            Some(1.)
        );
        assert_eq!(
            value(
                &output,
                "worker_resident_memory_bytes",
                &[("profile_id", "two")]
            ),
            Some(6000.)
        );
        assert_eq!(
            value(
                &output,
                "worker_mlx_active_memory_bytes",
                &[("profile_id", "two")]
            ),
            None
        );
        assert_eq!(
            value(&output, "worker_active_requests", &[("profile_id", "two")]),
            None
        );
        assert_eq!(
            value(
                &output,
                "worker_generated_tokens_total",
                &[("profile_id", "two")]
            ),
            None
        );
    }

    #[test]
    fn request_guards_finalize_once_preserve_errors_and_only_count_observed_first_tokens() {
        let metrics = Metrics::new().unwrap();
        let status = worker("one", Some(1), "ready");
        for outcome in OUTCOMES {
            let guard = metrics.begin("/v1/chat/completions");
            guard.set_profile("one");
            assert_eq!(
                value(
                    &metrics.render(std::slice::from_ref(&status), &[]).unwrap(),
                    "inference_requests_in_flight",
                    &[("profile_id", "one")]
                ),
                Some(1.)
            );
            match *outcome {
                "success" => {
                    guard.first_token();
                    guard.first_token();
                    guard.finish();
                    guard.finish();
                }
                "http_error" => {
                    guard.http_status(503);
                    guard.finish();
                }
                "inference_error" => {
                    guard.http_status(200);
                    guard.inference_error();
                    guard.finish();
                }
                "transport_error" => {
                    guard.transport_error();
                    guard.http_status(502);
                }
                "cancelled" => {}
                _ => unreachable!(),
            }
            drop(guard);
        }
        let output = metrics.render(std::slice::from_ref(&status), &[]).unwrap();
        for outcome in OUTCOMES {
            let labels = &[("profile_id", "one"), ("outcome", *outcome)];
            assert_eq!(
                value(&output, "inference_requests_total", labels),
                Some(1.),
                "{outcome}"
            );
            assert_eq!(
                value(&output, "inference_request_duration_seconds_count", labels),
                Some(1.),
                "{outcome}"
            );
        }
        assert_eq!(
            value(
                &output,
                "inference_time_to_first_token_seconds_count",
                &[("profile_id", "one")]
            ),
            Some(1.)
        );
        assert_eq!(
            value(
                &output,
                "inference_requests_in_flight",
                &[("profile_id", "one")]
            ),
            Some(0.)
        );
        let json_only = metrics.begin("/v1/completions");
        json_only.set_profile("one");
        json_only.finish();
        assert_eq!(
            value(
                &metrics.render(&[status], &[]).unwrap(),
                "inference_time_to_first_token_seconds_count",
                &[("endpoint", "/v1/completions")]
            ),
            None
        );
    }

    #[test]
    fn last_arc_drop_finishes_cancelled_requests_and_completed_arcs_do_not_count_again() {
        let metrics = Metrics::new().unwrap();
        let status = worker("one", Some(1), "ready");
        let guard = Arc::new(metrics.begin("/api/generate"));
        guard.set_profile("one");
        let observer = guard.clone();
        drop(guard);
        assert_eq!(
            value(
                &metrics.render(std::slice::from_ref(&status), &[]).unwrap(),
                "inference_requests_in_flight",
                &[("profile_id", "one")]
            ),
            Some(1.)
        );
        observer.first_token();
        drop(observer);
        let output = metrics.render(std::slice::from_ref(&status), &[]).unwrap();
        assert_eq!(
            value(
                &output,
                "inference_requests_total",
                &[("outcome", "cancelled")]
            ),
            Some(1.)
        );
        assert_eq!(
            value(&output, "inference_time_to_first_token_seconds_count", &[]),
            Some(1.)
        );
        assert_eq!(
            value(
                &output,
                "inference_requests_in_flight",
                &[("profile_id", "one")]
            ),
            Some(0.)
        );
        let guard = Arc::new(metrics.begin("/api/generate"));
        guard.set_profile("one");
        let observer = guard.clone();
        guard.finish();
        drop(guard);
        drop(observer);
        assert_eq!(
            value(
                &metrics.render(&[status], &[]).unwrap(),
                "inference_requests_total",
                &[("outcome", "success")]
            ),
            Some(1.)
        );
    }

    fn model(id: &str, name: &str) -> ModelRecord {
        ModelRecord {
            id: id.into(),
            name: name.into(),
            source: "import".into(),
            repo_id: None,
            revision: None,
            path: "/private/never-export/model-weights".into(),
            weights_bytes: 1024,
            compatible: true,
            engines: vec![crate::types::Engine::Locals],
            architecture: None,
            quantization: None,
            context_length: None,
        }
    }

    #[test]
    fn model_labels_cover_worker_and_request_metrics_and_group_shared_models() {
        let mut first = worker("one", Some(1), "ready");
        first.model_id = "model-a".into();
        first.alias = "alpha".into();
        let mut second = worker("two", Some(2), "ready");
        second.model_id = "model-a".into();
        second.alias = "alpha-tools".into();
        let mut third = worker("three", Some(3), "ready");
        third.model_id = "model-b".into();
        third.alias = "beta".into();
        let mut stopped = worker("four", None, "stopped");
        stopped.model_id = "model-a".into();
        stopped.alias = "alpha-paused".into();
        let statuses = [first, second, third, stopped];
        let models = [model("model-a", "Qwen 0.8B"), model("model-b", "Gemma 4B")];
        let metrics = Metrics::new().unwrap();
        let mut sample = telemetry(&statuses[0], 7);
        for (status, tokens) in [(&statuses[1], 11), (&statuses[2], 13)] {
            sample["workers"]
                .as_array_mut()
                .unwrap()
                .push(telemetry(status, tokens)["workers"][0].clone());
        }
        metrics.record_telemetry(Instant::now(), sample);
        for status in &statuses[..3] {
            let guard = metrics.begin("/v1/chat/completions");
            guard.set_profile(&status.profile_id);
            guard.first_token();
            guard.finish();
        }
        let output = metrics.render(&statuses, &models).unwrap();
        for (status, name) in
            statuses
                .iter()
                .zip(["Qwen 0.8B", "Qwen 0.8B", "Gemma 4B", "Qwen 0.8B"])
        {
            let profile_label = format!("profile_id=\"{}\"", status.profile_id);
            let lines: Vec<_> = output
                .lines()
                .filter(|line| !line.starts_with('#') && line.contains(&profile_label))
                .collect();
            assert!(!lines.is_empty());
            for line in lines {
                assert!(
                    line.contains(&format!("model_id=\"{}\"", status.model_id)),
                    "{line}"
                );
                assert!(line.contains(&format!("model=\"{name}\"")), "{line}");
                assert!(
                    line.contains(&format!("alias=\"{}\"", status.alias)),
                    "{line}"
                );
                for label in ["model_id=", "model=", "alias="] {
                    assert_eq!(line.matches(label).count(), 1, "{line}");
                }
            }
        }
        let shared_total: f64 = output
            .lines()
            .filter(|line| {
                line.starts_with("locals_studio_worker_generated_tokens_total{")
                    && line.contains("model_id=\"model-a\"")
            })
            .map(|line| {
                line.split_whitespace()
                    .last()
                    .unwrap()
                    .parse::<f64>()
                    .unwrap()
            })
            .sum();
        assert_eq!(shared_total, 18.);
        assert_eq!(
            value(
                &output,
                "worker_state",
                &[
                    ("profile_id", "four"),
                    ("state", "stopped"),
                    ("model", "Qwen 0.8B")
                ]
            ),
            Some(1.)
        );
        assert!(!output.contains("/private/never-export/model-weights"));
    }

    #[test]
    fn metadata_renames_preserve_accumulated_values_and_labels_are_escaped_and_unique() {
        let mut status = worker("one", Some(1), "ready");
        status.model_id = "model-a".into();
        status.alias = "alias\\path\nnext".into();
        let mut record = model("model-a", "Qwen \"quoted\"");
        let metrics = Metrics::new().unwrap();
        metrics.record_telemetry(Instant::now(), telemetry(&status, 17));
        let guard = metrics.begin("/api/chat");
        guard.set_profile("one");
        guard.first_token();
        guard.finish();
        let before = metrics
            .render(std::slice::from_ref(&status), std::slice::from_ref(&record))
            .unwrap();
        assert!(before.contains("model=\"Qwen \\\"quoted\\\"\""));
        assert!(before.contains("alias=\"alias\\\\path\\nnext\""));
        status.alias = "renamed-alias".into();
        record.name = "Renamed model".into();
        let after = metrics
            .render(std::slice::from_ref(&status), std::slice::from_ref(&record))
            .unwrap();
        for name in [
            "inference_requests_total",
            "inference_request_duration_seconds_count",
            "inference_time_to_first_token_seconds_count",
            "worker_generated_tokens_total",
        ] {
            assert_eq!(
                value(&before, name, &[("profile_id", "one")]),
                value(&after, name, &[("profile_id", "one")]),
                "{name}"
            );
            assert!(
                value(
                    &after,
                    name,
                    &[
                        ("profile_id", "one"),
                        ("model", "Renamed model"),
                        ("alias", "renamed-alias")
                    ]
                )
                .is_some()
            );
        }
        let mut families = metrics.0.registry.gather();
        assert!(
            families
                .iter()
                .flat_map(|family| family.get_metric())
                .flat_map(|metric| metric.get_label())
                .all(|label| !matches!(label.name(), "model" | "model_id" | "alias")),
            "display metadata must not become an internal counter identity"
        );
        // Enrichment is idempotent and replaces its own metadata without duplicate keys.
        for _ in 0..2 {
            enrich_model_labels(
                &mut families,
                std::slice::from_ref(&status),
                std::slice::from_ref(&record),
            );
        }
        for metric in families.iter().flat_map(|family| family.get_metric()) {
            let names: HashSet<_> = metric
                .get_label()
                .iter()
                .map(|label| label.name())
                .collect();
            assert_eq!(names.len(), metric.get_label().len());
            assert!(
                metric
                    .get_label()
                    .windows(2)
                    .all(|pair| pair[0].name() < pair[1].name())
            );
        }
        assert!(TextEncoder::new().encode_to_string(&families).is_ok());
    }

    #[test]
    fn missing_catalogue_uses_alias_while_unresolved_and_global_metrics_stay_unenriched() {
        let mut status = worker("one", None, "stopped");
        status.model_id = "unknown-model".into();
        status.alias = "Friendly fallback".into();
        let metrics = Metrics::new().unwrap();
        let unresolved = metrics.begin("/api/chat");
        unresolved.http_status(503);
        unresolved.finish();
        let known = metrics.begin("/api/chat");
        known.set_profile("one");
        known.finish();
        let output = metrics.render(std::slice::from_ref(&status), &[]).unwrap();
        assert_eq!(
            value(
                &output,
                "worker_ready",
                &[
                    ("profile_id", "one"),
                    ("model_id", "unknown-model"),
                    ("model", "Friendly fallback"),
                    ("alias", "Friendly fallback")
                ]
            ),
            Some(0.)
        );
        for line in output.lines().filter(|line| !line.starts_with('#')) {
            if !line.contains("profile_id=") || line.contains("profile_id=\"unresolved\"") {
                assert!(!line.contains("model_id="), "{line}");
                assert!(!line.contains("model="), "{line}");
                assert!(!line.contains("alias="), "{line}");
            }
        }
        let removed = metrics
            .render(&[], &[model("unknown-model", "Still in library")])
            .unwrap();
        assert!(!removed.contains("profile_id=\"one\""));
        assert_eq!(
            value(
                &removed,
                "inference_requests_total",
                &[("profile_id", "unresolved"), ("outcome", "http_error")]
            ),
            Some(1.)
        );
    }
}
