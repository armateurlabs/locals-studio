use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use serde_json::{Value, json};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};

use crate::{
    models::ModelRecord,
    types::{ServerProfile, ServerStatus},
};

#[derive(Clone)]
pub struct TelemetryCollector {
    inner: Arc<Mutex<Collector>>,
}

struct Collector {
    system: System,
    origin: Instant,
    cpu: CpuSampleCache,
}

#[derive(Default)]
struct CpuSampleCache {
    last_refresh: Option<Duration>,
    system: Option<f32>,
    processes: HashMap<(u32, u64), Option<f64>>,
}

impl CpuSampleCache {
    fn should_refresh(&self, now: Duration, minimum: Duration) -> bool {
        self.last_refresh
            .is_none_or(|last| now.saturating_sub(last) >= minimum)
    }

    fn record(&mut self, now: Duration, system: f32, processes: Vec<((u32, u64), f64)>) {
        self.system = self.last_refresh.is_some().then_some(system);
        self.processes = processes
            .into_iter()
            .map(|(identity, usage)| {
                let value = self.processes.contains_key(&identity).then_some(usage);
                (identity, value)
            })
            .collect();
        self.last_refresh = Some(now);
    }

    fn process(&self, identity: (u32, u64)) -> Option<f64> {
        self.processes.get(&identity).copied().flatten()
    }
}

impl Default for TelemetryCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl TelemetryCollector {
    pub fn new() -> Self {
        let mut system = System::new();
        system.refresh_memory();
        system.refresh_cpu_all();
        Self {
            inner: Arc::new(Mutex::new(Collector {
                system,
                origin: Instant::now(),
                cpu: CpuSampleCache::default(),
            })),
        }
    }

    pub fn snapshot(&self, statuses: &[ServerStatus], stats: &HashMap<String, Value>) -> Value {
        let mut collector = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let pids: Vec<Pid> = statuses
            .iter()
            .filter_map(|s| s.pid.map(Pid::from_u32))
            .collect();
        let now = collector.origin.elapsed();
        let refresh_cpu = collector
            .cpu
            .should_refresh(now, sysinfo::MINIMUM_CPU_UPDATE_INTERVAL);
        collector.system.refresh_memory();
        if refresh_cpu {
            collector.system.refresh_cpu_usage();
        }
        let process_refresh = ProcessRefreshKind::nothing().with_memory().without_tasks();
        let process_refresh = if refresh_cpu {
            process_refresh.with_cpu()
        } else {
            process_refresh
        };
        collector.system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&pids),
            true,
            process_refresh,
        );
        let cores = collector.system.cpus().len().max(1) as f64;
        if refresh_cpu {
            let global = collector.system.global_cpu_usage();
            let processes = pids
                .iter()
                .filter_map(|pid| collector.system.process(*pid))
                .map(|process| {
                    (
                        (process.pid().as_u32(), process.start_time()),
                        process.cpu_usage() as f64 / cores,
                    )
                })
                .collect();
            let sampled_at = collector.origin.elapsed();
            collector.cpu.record(sampled_at, global, processes);
        }
        let cpu = collector.cpu.system;
        let (mut sum_rss, mut sum_active, mut sum_cache) = (0u64, 0u64, 0u64);
        let workers: Vec<Value> = statuses.iter().map(|status| {
            let process = status.pid.and_then(|pid| collector.system.process(Pid::from_u32(pid)));
            let rss = process.map(|process| process.memory());
            let cpu=process.and_then(|process|collector.cpu.process((process.pid().as_u32(),process.start_time())));
            let stats = stats.get(&status.profile_id);
            let memory = stats.and_then(|s| s.get("memory"));
            let active = mb_to_bytes(memory.and_then(|m| m.get("gpu_active_mb")));
            let cache = mb_to_bytes(memory.and_then(|m| m.get("gpu_cache_mb")));
            sum_rss = sum_rss.saturating_add(rss.unwrap_or(0));
            sum_active = sum_active.saturating_add(active.unwrap_or(0));
            sum_cache = sum_cache.saturating_add(cache.unwrap_or(0));
            json!({
                "profile_id":status.profile_id,"alias":status.alias,"pid":status.pid,
                "cpu_percent":cpu,"rss_bytes":rss,
                "rss_peak_bytes":mb_to_bytes(memory.and_then(|m|m.get("rss_mb"))),
                "mlx_active_bytes":active,"mlx_cache_bytes":cache,
                "mlx_peak_bytes":mb_to_bytes(memory.and_then(|m|m.get("gpu_peak_mb"))),
                "active_requests":stats.and_then(|s|s.get("active")).and_then(Value::as_array).map(Vec::len).unwrap_or(status.active_requests),
                "gpu_percent":Value::Null,"gpu_percent_reason":"Locals does not expose GPU utilization per process",
                "stats":stats,
            })
        }).collect();
        json!({
            "sampled_at":chrono::Utc::now().to_rfc3339(),
            "system":{
                "ram_total_bytes":collector.system.total_memory(),"ram_used_bytes":collector.system.used_memory(),
                "ram_available_bytes":collector.system.available_memory(),
                "swap_total_bytes":collector.system.total_swap(),"swap_used_bytes":collector.system.used_swap(),
                "cpu_percent":cpu,"cpu_cores":cores as usize,
                "gpu":{"available":false,"percent":Value::Null,"reason":"Global GPU utilization requires a separate platform collector"},
            },
            "workers":workers,
            "managed_totals":{"rss_bytes":sum_rss,"mlx_active_bytes":sum_active,"mlx_cache_bytes":sum_cache},
            "memory_note":"Apple Silicon uses unified memory. RSS and MLX allocations overlap and must not be added together.",
        })
    }

    pub fn default_budget(&self) -> u64 {
        let mut collector = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        collector.system.refresh_memory();
        let total = collector.system.total_memory();
        total.saturating_sub(4 * GIB).max(total / 2)
    }

    pub fn available_memory(&self) -> u64 {
        let mut collector = self
            .inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        collector.system.refresh_memory();
        collector.system.available_memory()
    }
}

fn mb_to_bytes(value: Option<&Value>) -> Option<u64> {
    value
        .and_then(Value::as_f64)
        .filter(|v| v.is_finite() && *v >= 0.)
        .map(|v| (v * 1_000_000.) as u64)
}

const GIB: u64 = 1 << 30;

/// A conservative configured-load scenario, not a guaranteed process memory cap.
/// Unknown KV architectures and automatic cache/context remain explicitly partial.
pub async fn estimate_memory(
    profiles: &[ServerProfile],
    models: &[ModelRecord],
    budget: u64,
) -> Result<Value> {
    let mut items = Vec::new();
    let mut total = 0u64;
    let mut complete = true;
    let mut automatic = false;
    let mut warnings = Vec::new();
    for profile in profiles {
        let model = models.iter().find(|model| {
            model.id == profile.model_id || model.repo_id.as_deref() == Some(&profile.model_id)
        });
        let weights = model.map(|m| m.weights_bytes).unwrap_or(0);
        let engine_count = option(profile, "engines")
            .map(|value| {
                let names: Vec<&str> = match value {
                    Value::String(text) => text
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect(),
                    Value::Array(values) => values.iter().filter_map(Value::as_str).collect(),
                    _ => Vec::new(),
                };
                names.into_iter().collect::<HashSet<_>>().len().max(1) as u64
            })
            .unwrap_or(1);
        let context = option(profile, "max_context")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0);
        let cache = option(profile, "cache_gb")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v > 0.)
            .map(|v| (v * GIB as f64) as u64);
        let slots = option(profile, "slots")
            .and_then(Value::as_u64)
            .unwrap_or(16)
            .max(1);
        let config = if let Some(model) = model {
            tokio::fs::read(model.path.join("config.json"))
                .await
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        } else {
            None
        };
        let kv_per_token = config
            .as_ref()
            .and_then(|config| kv_bytes_per_token(config, profile));
        let kv = context.zip(kv_per_token).map(|(context, kv)| {
            context
                .saturating_mul(kv)
                .saturating_mul(slots)
                .saturating_mul(engine_count)
        });
        let is_auto = context.is_none() || cache.is_none();
        automatic |= is_auto;
        let item_complete = weights > 0 && !is_auto && kv.is_some();
        complete &= item_complete;
        let baseline = weights
            .saturating_mul(engine_count)
            .saturating_add(cache.unwrap_or(0).saturating_mul(engine_count))
            .saturating_add(2 * GIB);
        let estimate = baseline.saturating_add(kv.unwrap_or(0));
        total = total.saturating_add(estimate);
        items.push(json!({
            "profile_id":profile.id,"model_id":profile.model_id,"alias":profile.alias,
            "weights_bytes":if weights>0 {Some(weights)} else {None},"engine_count":engine_count,
            "cache_budget_bytes":cache,"max_context":context,"scenario_concurrency":slots,
            "kv_bytes_per_token":kv_per_token,"scenario_kv_bytes":kv,
            "runtime_allowance_bytes":2*GIB,"estimated_bytes":estimate,"baseline_estimated_bytes":baseline,
            "complete":item_complete,"confidence":if item_complete {"estimated"} else {"partial"},"automatic":is_auto,
        }));
    }
    if profiles.len() > 1 && automatic {
        warnings.push(json!({"code":"multiple_automatic_workers","message":"Multiple models use independent automatic cache/context sizing. Set explicit limits or confirm this launch."}));
    }
    if total > budget {
        warnings.push(json!({"code":"memory_budget_exceeded","message":"The selected models' configured-load estimate exceeds the machine budget."}));
    }
    if !complete {
        warnings.push(json!({"code":"partial_estimate","message":"Automatic settings or unknown model architecture make this a partial estimate, not an upper bound."}));
    }
    let requires_override = warnings.iter().any(|w| {
        w["code"] == "multiple_automatic_workers" || w["code"] == "memory_budget_exceeded"
    });
    Ok(
        json!({"budget_bytes":budget,"total_estimated_bytes":total,"complete":complete,"models":items,"warnings":warnings,"requires_override":requires_override,
        "note":"Includes weights, configured prompt cache, a 2 GiB runtime allowance and KV at maximum context/concurrency when identifiable. Activations, checkpoints and batching can differ; this is not a hard memory cap."}),
    )
}

/// Compare only new allocations with current OS availability. Existing workers are already
/// reflected in available RAM; counting their estimate again would produce a false alarm.
pub(crate) fn account_available_memory(
    estimate: &mut Value,
    active_profiles: &HashSet<String>,
    available: u64,
) {
    let (mut additional, mut allowance) = (0u64, 0u64);
    let mut additional_complete = true;
    let mut seen = HashSet::new();
    if let Some(models) = estimate.get_mut("models").and_then(Value::as_array_mut) {
        for model in models {
            let id = model["profile_id"].as_str().unwrap_or_default().to_owned();
            let active = active_profiles.contains(&id);
            let include = !active && seen.insert(id);
            let total = model["estimated_bytes"].as_u64().unwrap_or(0);
            let runtime = model["runtime_allowance_bytes"]
                .as_u64()
                .unwrap_or(0)
                .min(total);
            if include {
                additional = additional.saturating_add(total - runtime);
                allowance = allowance.saturating_add(runtime);
                additional_complete &= model["complete"].as_bool() == Some(true);
            }
            model["already_active"] = json!(active);
            model["additional_required_bytes"] = json!(if include { total } else { 0 });
        }
    }
    let required = additional.saturating_add(allowance);
    estimate["available_memory_bytes"] = json!(available);
    estimate["additional_estimated_bytes"] = json!(additional);
    estimate["additional_runtime_allowance_bytes"] = json!(allowance);
    estimate["additional_required_bytes"] = json!(required);
    estimate["additional_complete"] = json!(additional_complete);
    estimate["available_memory_note"] = json!(
        "Current available RAM includes memory used by other applications and external model servers. Existing Studio workers are excluded from the additional estimate. The estimate and runtime allowance are not a guaranteed memory cap."
    );
    if required > available {
        let warning = json!({
            "code":"available_memory_exceeded",
            "message":"The additional memory estimate, including runtime allowance, exceeds currently available RAM. It is an estimate, not a guarantee of actual memory usage.",
            "additional_estimated_bytes":additional,"runtime_allowance_bytes":allowance,
            "additional_required_bytes":required,"available_memory_bytes":available,
        });
        if let Some(warnings) = estimate.get_mut("warnings").and_then(Value::as_array_mut) {
            warnings.push(warning);
        }
        estimate["requires_override"] = json!(true);
    }
}

fn option<'a>(profile: &'a ServerProfile, key: &str) -> Option<&'a Value> {
    profile
        .options
        .get(key)
        .or_else(|| profile.options.get(&key.replace('_', "-")))
}

fn kv_bytes_per_token(config: &Value, profile: &ServerProfile) -> Option<u64> {
    let config = config.get("text_config").unwrap_or(config);
    // Recurrent/hybrid caches need model-specific state accounting. Do not call an all-layer formula complete.
    if config.get("linear_num_value_heads").is_some()
        || config.get("layer_types").is_some()
        || config.get("full_attention_interval").is_some()
    {
        return None;
    }
    let layers = config.get("num_hidden_layers")?.as_u64()?;
    let heads = config
        .get("num_key_value_heads")
        .or_else(|| config.get("num_attention_heads"))?
        .as_u64()?;
    let dim = config.get("head_dim").and_then(Value::as_u64).or_else(|| {
        let attention = config.get("num_attention_heads")?.as_u64()?;
        (attention > 0).then(|| {
            config
                .get("hidden_size")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                / attention
        })
    })?;
    if layers == 0 || heads == 0 || dim == 0 {
        return None;
    }
    let elements = 2u64
        .checked_mul(layers)?
        .checked_mul(heads)?
        .checked_mul(dim)?;
    if profile.env.get("LOCALS_KV_BITS").is_some_and(|v| v == "8") {
        elements.checked_add(elements / 16)
    } else {
        elements.checked_mul(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_cache_keeps_valid_samples_between_refreshes_and_resets_reused_pids() {
        let mut cache = CpuSampleCache::default();
        let interval = Duration::from_millis(200);
        let first = (42, 1000);
        assert_eq!(cache.system, None);
        cache.record(Duration::ZERO, 0., vec![(first, 0.)]);
        assert_eq!(cache.system, None);
        assert_eq!(cache.process(first), None);
        assert!(!cache.should_refresh(Duration::from_millis(100), interval));
        assert!(cache.should_refresh(Duration::from_millis(200), interval));
        cache.record(Duration::from_millis(200), 23., vec![(first, 5.5)]);
        assert_eq!(cache.system, Some(23.));
        assert_eq!(cache.process(first), Some(5.5));
        assert!(!cache.should_refresh(Duration::from_millis(250), interval));
        assert!(!cache.should_refresh(Duration::from_millis(350), interval));
        assert_eq!(cache.system, Some(23.));
        assert_eq!(cache.process(first), Some(5.5));
        assert!(
            cache.should_refresh(Duration::from_millis(400), interval),
            "Read-only calls must not move the last CPU refresh clock"
        );
        let reused = (42, 2000);
        assert_eq!(cache.process(reused), None);
        cache.record(Duration::from_millis(400), 31., vec![(reused, 0.)]);
        assert_eq!(
            cache.process(reused),
            None,
            "A reused PID needs a new CPU baseline"
        );
        assert_eq!(cache.process(first), None);
        cache.record(Duration::from_millis(600), 18., vec![(reused, 2.)]);
        assert_eq!(cache.process(reused), Some(2.));
    }

    #[test]
    fn kv_scenario_accounts_for_grouped_heads_and_rejects_hybrid_architectures() {
        let profile = ServerProfile::default();
        let config = json!({"num_hidden_layers":32,"num_key_value_heads":8,"num_attention_heads":32,"hidden_size":4096});
        // K and V, 32 attention layers, 8 grouped heads, 128 elements, 2 bytes.
        assert_eq!(kv_bytes_per_token(&config, &profile), Some(131_072));
        let mut hybrid = config;
        hybrid["full_attention_interval"] = json!(4);
        assert_eq!(kv_bytes_per_token(&hybrid, &profile), None);
    }

    #[tokio::test]
    async fn simultaneous_automatic_workers_require_override_and_are_not_complete_estimates() {
        let profiles = vec![
            ServerProfile {
                id: "one".into(),
                alias: "one".into(),
                ..Default::default()
            },
            ServerProfile {
                id: "two".into(),
                alias: "two".into(),
                ..Default::default()
            },
        ];
        let estimate = estimate_memory(&profiles, &[], 64 * GIB).await.unwrap();
        assert_eq!(estimate["complete"], false);
        assert_eq!(estimate["requires_override"], true);
        let single = estimate_memory(&profiles[..1], &[], 64 * GIB)
            .await
            .unwrap();
        assert_eq!(single["requires_override"], false);
    }

    #[test]
    fn available_memory_alarm_counts_only_new_workers_and_includes_their_runtime_allowance() {
        let original = json!({"requires_override":false,"warnings":[],"models":[
            {"profile_id":"running","estimated_bytes":12*GIB,"runtime_allowance_bytes":2*GIB,"complete":true},
            {"profile_id":"new","estimated_bytes":6*GIB,"runtime_allowance_bytes":2*GIB,"complete":true}
        ]});
        let active = HashSet::from(["running".to_owned()]);
        let mut fits = original.clone();
        account_available_memory(&mut fits, &active, 7 * GIB);
        assert_eq!(fits["additional_estimated_bytes"], 4 * GIB);
        assert_eq!(fits["additional_runtime_allowance_bytes"], 2 * GIB);
        assert_eq!(fits["additional_required_bytes"], 6 * GIB);
        assert_eq!(fits["models"][0]["additional_required_bytes"], 0);
        assert_eq!(
            fits["requires_override"], false,
            "The running worker is already included in OS memory consumption"
        );
        let mut exceeds = original;
        account_available_memory(&mut exceeds, &active, 5 * GIB);
        assert_eq!(exceeds["requires_override"], true);
        assert_eq!(exceeds["warnings"][0]["code"], "available_memory_exceeded");
    }
}
