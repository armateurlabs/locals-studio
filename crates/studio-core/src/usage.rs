//! Cumulative token usage, counted per finished request and kept on this daemon.
//!
//! Every inference request that streams a `usage` block is recorded once, keyed by the user's
//! local date, exactly as a request log would. Counts are never sent anywhere: they live in the
//! Studio store and are read back only by the Usage view.
use crate::{store::Store, types::Engine};
use anyhow::Result;
use chrono::{DateTime, Local};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Arc, Mutex, MutexGuard},
};

/// The token counts for one day, mirroring a single request log entry.
///
/// `prompt` includes cached tokens (Locals counts cached input as prompt work), `cached` is the
/// reused portion, and `output` is generated tokens. `total` is what the dashboard sums.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct DayUsage {
    pub requests: u64,
    pub prompt: u64,
    pub cached: u64,
    pub output: u64,
    /// Tokens read or written by each model, keyed by the alias the request was routed under.
    #[serde(default)]
    pub models: HashMap<String, u64>,
    /// Tokens keyed by the app or agent that made the request.
    #[serde(default)]
    pub clients: HashMap<String, u64>,
    /// The same counts for each engine, so a view filtered to one engine stays consistent. Days
    /// recorded before engines existed have none: all of their usage was Locals.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub engines: BTreeMap<String, DayUsage>,
}

impl DayUsage {
    /// Every token this day touched: prompt reading plus output written.
    pub fn total(&self) -> u64 {
        self.prompt.saturating_add(self.output)
    }

    /// Share of the prompt that was served from cache, 0.0-1.0.
    pub fn cache_fraction(&self) -> f64 {
        if self.prompt == 0 {
            0.0
        } else {
            self.cached as f64 / self.prompt as f64
        }
    }

    /// Adds one finished request to the day and to its engine's share of the day.
    pub fn record_on(
        &mut self,
        engine: Engine,
        model: &str,
        prompt: u64,
        cached: u64,
        output: u64,
        client: &str,
    ) {
        if self.engines.is_empty() && self.requests > 0 {
            // Usage from before engines were recorded is Locals'.
            let mut legacy = self.clone();
            legacy.engines.clear();
            self.engines
                .insert(Engine::Locals.as_str().to_owned(), legacy);
        }
        self.record(model, prompt, cached, output, client);
        self.engines
            .entry(engine.as_str().to_owned())
            .or_default()
            .record(model, prompt, cached, output, client);
    }

    /// This day's usage on one engine.
    pub fn on_engine(&self, engine: Engine) -> DayUsage {
        if self.engines.is_empty() {
            return if engine == Engine::Locals {
                self.clone()
            } else {
                DayUsage::default()
            };
        }
        self.engines
            .get(engine.as_str())
            .cloned()
            .unwrap_or_default()
    }

    /// Tokens per engine this day.
    pub fn engine_tokens(&self) -> BTreeMap<String, u64> {
        Engine::ALL
            .into_iter()
            .map(|engine| (engine.as_str().to_owned(), self.on_engine(engine).total()))
            .filter(|(_, tokens)| *tokens > 0)
            .collect()
    }

    /// Adds one finished request. Cached tokens never exceed the prompt they are part of.
    pub fn record(&mut self, model: &str, prompt: u64, cached: u64, output: u64, client: &str) {
        let cached = cached.min(prompt);
        self.requests += 1;
        self.prompt += prompt;
        self.cached += cached;
        self.output += output;
        let tokens = prompt.saturating_add(output);
        if !model.is_empty() {
            *self.models.entry(model.to_owned()).or_default() += tokens;
        }
        let client = if client.is_empty() || client == "-" {
            DayUsage::UNIDENTIFIED
        } else {
            client
        };
        *self.clients.entry(client.to_string()).or_default() += tokens;
    }

    const UNIDENTIFIED: &'static str = "Unidentified";

    /// Sums a set of days into one usage, like the dashboard's "all time" tile.
    pub fn sum(days: &[DayUsage]) -> DayUsage {
        let mut sum = DayUsage::default();
        for day in days {
            sum.requests += day.requests;
            sum.prompt += day.prompt;
            sum.cached += day.cached;
            sum.output += day.output;
            for (model, tokens) in &day.models {
                *sum.models.entry(model.clone()).or_default() += *tokens;
            }
            for (client, tokens) in &day.clients {
                *sum.clients.entry(client.clone()).or_default() += *tokens;
            }
        }
        sum
    }
}

/// A usage as the Usage view reads it, with its derived `total` and tokens per engine.
fn with_total(day: &DayUsage) -> Value {
    let mut flat = day.clone();
    let engines = flat.engine_tokens();
    flat.engines.clear();
    let mut value = serde_json::to_value(&flat).unwrap_or_else(|_| json!({}));
    value["total"] = json!(day.total());
    value["engines"] = json!(engines);
    value
}

/// A day's usage together with the date key it was stored under.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DayRecord {
    pub date: String,
    #[serde(flatten)]
    pub usage: DayUsage,
}

/// Per-request usage captured while a response streamed, before it is folded into a day.
#[derive(Clone, Debug)]
pub struct RecordedUsage {
    pub profile_id: String,
    pub model: String,
    /// The app or agent that made the request, empty when it did not identify itself.
    pub client: String,
    pub engine: Engine,
    pub prompt: u64,
    pub cached: u64,
    pub output: u64,
    pub at: DateTime<Local>,
}

/// Accumulates usage into dated buckets and persists them in the store.
#[derive(Clone)]
pub struct UsageStore(Arc<Inner>);

struct Inner {
    store: Store,
    /// The day key currently being mutated, so concurrent requests to the same day are serialized.
    days: Mutex<HashMap<String, ()>>,
}

impl UsageStore {
    pub fn new(store: Store) -> Self {
        Self(Arc::new(Inner {
            store,
            days: Mutex::new(HashMap::new()),
        }))
    }

    const NAMESPACE: &'static str = "usage";

    /// The date key a timestamp falls under, in the daemon's local time.
    fn key_for(at: &DateTime<Local>) -> String {
        at.format("%Y-%m-%d").to_string()
    }

    /// Loads the persisted day map. A missing or unreadable file yields an empty history rather
    /// than an error, so a fresh install or a corrupted file never stops new requests from counting.
    pub fn days(&self) -> Result<HashMap<String, DayUsage>> {
        let records: Vec<DayRecord> = self.0.store.list(Self::NAMESPACE)?;
        let mut map = HashMap::new();
        for record in records {
            map.insert(record.date, record.usage);
        }
        Ok(map)
    }

    /// Records one finished request, updating and persisting its day bucket.
    pub fn record(&self, usage: &RecordedUsage) -> Result<DayUsage> {
        let key = Self::key_for(&usage.at);
        let _guard = self.lock_day(&key);
        let mut days = self.days()?;
        let day = days.entry(key.clone()).or_default();
        day.record_on(
            usage.engine,
            &usage.model,
            usage.prompt,
            usage.cached,
            usage.output,
            &usage.client,
        );
        let snapshot = day.clone();
        self.0.store.set(
            Self::NAMESPACE,
            &key,
            &DayRecord {
                date: key.clone(),
                usage: day.clone(),
            },
        )?;
        Ok(snapshot)
    }

    /// Returns the whole history plus derived summaries for the Usage view.
    /// The history and its summaries, for every engine or only `engine`.
    pub fn summary(&self, engine: Option<Engine>) -> Result<Value> {
        let days = self.days()?;
        let engine_tokens: BTreeMap<String, u64> = Engine::ALL
            .into_iter()
            .map(|engine| {
                let tokens = days
                    .values()
                    .map(|day| day.on_engine(engine).total())
                    .sum::<u64>();
                (engine.as_str().to_owned(), tokens)
            })
            .filter(|(_, tokens)| *tokens > 0)
            .collect();
        let ordered: Vec<(String, DayUsage)> = days
            .into_iter()
            .map(|(date, day)| match engine {
                Some(engine) => (date, day.on_engine(engine)),
                None => (date, day),
            })
            .collect();
        let all = DayUsage::sum(
            &ordered
                .iter()
                .map(|(_, day)| day.clone())
                .collect::<Vec<_>>(),
        );
        let active: Vec<&DayUsage> = ordered
            .iter()
            .filter(|(_, day)| day.total() > 0)
            .map(|(_, day)| day)
            .collect();
        let active_days = active.len();
        let busiest = ordered
            .iter()
            .filter(|(_, day)| day.total() > 0)
            .max_by_key(|(_, day)| day.total())
            .map(|(date, day)| json!({ "date": date, "usage": with_total(day) }));
        let days_map: serde_json::Map<String, Value> = ordered
            .iter()
            .map(|(date, day)| (date.clone(), with_total(day)))
            .collect();
        let mut all_value = with_total(&all);
        all_value["engines"] = json!(engine_tokens);
        Ok(json!({
            "engine": engine,
            "days": days_map,
            "all": all_value,
            "active_days": active_days,
            "busiest_day": busiest,
        }))
    }

    /// Serializes writes to a single day bucket so concurrent requests to that day don't clobber
    /// each other. The guard is held for the duration of `record`.
    fn lock_day(&self, _key: &str) -> MutexGuard<'_, HashMap<String, ()>> {
        self.0
            .days
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn store(root: &Path) -> Store {
        Store::open(&root.join("usage.db")).unwrap()
    }

    fn record_at(
        usage: &UsageStore,
        date: &str,
        model: &str,
        prompt: u64,
        cached: u64,
        output: u64,
        client: &str,
    ) {
        let at = chrono::DateTime::parse_from_rfc3339(date)
            .unwrap()
            .with_timezone(&Local);
        usage
            .record(&RecordedUsage {
                profile_id: "profile".to_owned(),
                client: client.to_owned(),
                model: model.to_owned(),
                engine: if model.starts_with("kapitsa") {
                    Engine::Kapitsa
                } else {
                    Engine::Locals
                },
                prompt,
                cached,
                output,
                at,
            })
            .unwrap();
    }

    #[tokio::test]
    async fn records_accumulate_into_a_day_bucket() {
        let temp = tempfile::tempdir().unwrap();
        let usage = UsageStore::new(store(temp.path()));
        record_at(
            &usage,
            "2026-10-29T10:00:00+00:00",
            "mistral",
            1000,
            400,
            200,
            "profile-a",
        );
        record_at(
            &usage,
            "2026-10-29T11:00:00+00:00",
            "mistral",
            2000,
            500,
            300,
            "profile-b",
        );

        let all = usage.summary(None).unwrap();
        assert_eq!(all["all"]["requests"], 2);
        assert_eq!(all["all"]["prompt"], 3000);
        assert_eq!(all["all"]["cached"], 900);
        assert_eq!(all["all"]["output"], 500);
        assert_eq!(all["all"]["total"], 3500);
        assert_eq!(all["all"]["models"]["mistral"], 3500);
        assert_eq!(all["active_days"], 1);
        assert_eq!(all["days"]["2026-10-29"]["total"], 3500);
    }

    #[tokio::test]
    async fn cached_tokens_never_exceed_the_prompt() {
        let temp = tempfile::tempdir().unwrap();
        let usage = UsageStore::new(store(temp.path()));
        record_at(&usage, "2026-10-29T10:00:00+00:00", "m", 500, 9999, 10, "p");

        let all = usage.summary(None).unwrap();
        assert_eq!(all["all"]["cached"], 500);
        assert_eq!(all["all"]["prompt"], 500);
    }

    #[tokio::test]
    async fn usage_survives_reopening_the_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.db");
        {
            let usage = UsageStore::new(Store::open(&path).unwrap());
            record_at(
                &usage,
                "2026-10-29T10:00:00+00:00",
                "mistral",
                1200,
                300,
                240,
                "profile-a",
            );
        }
        // A fresh UsageStore over the same database reads the persisted history back.
        let usage = UsageStore::new(Store::open(&path).unwrap());
        let all = usage.summary(None).unwrap();
        assert_eq!(all["all"]["requests"], 1);
        assert_eq!(all["all"]["prompt"], 1200);
        assert_eq!(all["all"]["output"], 240);
        assert_eq!(all["all"]["models"]["mistral"], 1440);
    }

    #[tokio::test]
    async fn unidentified_clients_are_grouped_under_one_label() {
        let temp = tempfile::tempdir().unwrap();
        let usage = UsageStore::new(store(temp.path()));
        record_at(&usage, "2026-10-29T10:00:00+00:00", "m", 100, 0, 10, "");
        record_at(&usage, "2026-10-29T11:00:00+00:00", "m", 100, 0, 10, "-");

        let all = usage.summary(None).unwrap();
        let clients = all["all"]["clients"].as_object().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[DayUsage::UNIDENTIFIED], 220);
    }

    #[tokio::test]
    async fn empty_history_has_no_busiest_day() {
        let temp = tempfile::tempdir().unwrap();
        let usage = UsageStore::new(store(temp.path()));
        let summary = usage.summary(None).unwrap();
        assert!(summary["busiest_day"].is_null());
        assert_eq!(summary["all"]["requests"], 0);
    }

    #[tokio::test]
    async fn usage_filtered_by_engine_keeps_totals_models_and_clients_consistent() {
        let temp = tempfile::tempdir().unwrap();
        let usage = UsageStore::new(store(temp.path()));
        // A day recorded before engines existed, as older versions stored it: all Locals.
        let mut legacy = DayUsage::default();
        legacy.record("mistral", 1000, 0, 100, "Claude Code");
        usage
            .0
            .store
            .set(
                "usage",
                "2026-10-29",
                &DayRecord {
                    date: "2026-10-29".into(),
                    usage: legacy,
                },
            )
            .unwrap();
        record_at(
            &usage,
            "2026-10-29T12:00:00+00:00",
            "kapitsa-35b",
            400,
            300,
            50,
            "opencode",
        );
        record_at(
            &usage,
            "2026-10-30T12:00:00+00:00",
            "mistral",
            200,
            0,
            20,
            "Claude Code",
        );

        let all = usage.summary(None).unwrap();
        assert_eq!(all["all"]["total"], 1770);
        assert_eq!(all["all"]["engines"]["locals"], 1320);
        assert_eq!(all["all"]["engines"]["kapitsa"], 450);
        assert_eq!(all["days"]["2026-10-29"]["engines"]["kapitsa"], 450);

        let kapitsa = usage.summary(Some(Engine::Kapitsa)).unwrap();
        assert_eq!(kapitsa["all"]["total"], 450);
        assert_eq!(kapitsa["all"]["requests"], 1);
        assert_eq!(kapitsa["all"]["cached"], 300);
        assert_eq!(kapitsa["all"]["clients"]["opencode"], 450);
        assert!(kapitsa["all"]["clients"].get("Claude Code").is_none());
        assert_eq!(kapitsa["days"]["2026-10-30"]["total"], 0);
        assert_eq!(kapitsa["active_days"], 1);

        let locals = usage.summary(Some(Engine::Locals)).unwrap();
        assert_eq!(locals["all"]["total"], 1320);
        assert_eq!(locals["all"]["models"]["mistral"], 1320);
        assert_eq!(locals["all"]["requests"], 2);
    }
}
