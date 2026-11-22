//! Cumulative token usage, counted per finished request and kept on this daemon.
//!
//! Every inference request that streams a `usage` block is recorded once, keyed by the user's
//! local date, exactly as a request log would. Counts are never sent anywhere: they live in the
//! Studio store and are read back only by the Usage view.
use crate::store::Store;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};
use chrono::{DateTime, Local};
use serde_json::{Value, json};

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

/// A usage as the Usage view reads it, with its derived `total`.
fn with_total(day: &DayUsage) -> Value {
    let mut value = serde_json::to_value(day).unwrap_or_else(|_| json!({}));
    value["total"] = json!(day.total());
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
        Self(Arc::new(Inner { store, days: Mutex::new(HashMap::new()) }))
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
        day.record(&usage.model, usage.prompt, usage.cached, usage.output, &usage.client);
        let snapshot = day.clone();
        self.0.store.set(
            Self::NAMESPACE,
            &key,
            &DayRecord { date: key.clone(), usage: day.clone() },
        )?;
        Ok(snapshot)
    }

    /// Returns the whole history plus derived summaries for the Usage view.
    pub fn summary(&self) -> Result<Value> {
        let days = self.days()?;
        let ordered: Vec<(String, DayUsage)> = days.into_iter().collect();
        let all = DayUsage::sum(&ordered.iter().map(|(_, day)| day.clone()).collect::<Vec<_>>());
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
        let all_value = with_total(&all);
        Ok(json!({
            "days": days_map,
            "all": all_value,
            "active_days": active_days,
            "busiest_day": busiest,
        }))
    }

    /// Serializes writes to a single day bucket so concurrent requests to that day don't clobber
    /// each other. The guard is held for the duration of `record`.
    fn lock_day(&self, _key: &str) -> MutexGuard<'_, HashMap<String, ()>> {
        self.0.days.lock().unwrap_or_else(|error| error.into_inner())
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
        usage.record(&RecordedUsage {
            profile_id: "profile".to_owned(),
            client: client.to_owned(),
            model: model.to_owned(),
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
        record_at(&usage, "2026-10-29T10:00:00+00:00", "mistral", 1000, 400, 200, "profile-a");
        record_at(&usage, "2026-10-29T11:00:00+00:00", "mistral", 2000, 500, 300, "profile-b");

        let all = usage.summary().unwrap();
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

        let all = usage.summary().unwrap();
        assert_eq!(all["all"]["cached"], 500);
        assert_eq!(all["all"]["prompt"], 500);
    }

    #[tokio::test]
    async fn usage_survives_reopening_the_store() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("usage.db");
        {
            let usage = UsageStore::new(Store::open(&path).unwrap());
            record_at(&usage, "2026-10-29T10:00:00+00:00", "mistral", 1200, 300, 240, "profile-a");
        }
        // A fresh UsageStore over the same database reads the persisted history back.
        let usage = UsageStore::new(Store::open(&path).unwrap());
        let all = usage.summary().unwrap();
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

        let all = usage.summary().unwrap();
        let clients = all["all"]["clients"].as_object().unwrap();
        assert_eq!(clients.len(), 1);
        assert_eq!(clients[DayUsage::UNIDENTIFIED], 220);
    }

    #[tokio::test]
    async fn empty_history_has_no_busiest_day() {
        let temp = tempfile::tempdir().unwrap();
        let usage = UsageStore::new(store(temp.path()));
        let summary = usage.summary().unwrap();
        assert!(summary["busiest_day"].is_null());
        assert_eq!(summary["all"]["requests"], 0);
    }
}
