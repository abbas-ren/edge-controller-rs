use std::{
    collections::{BTreeMap, VecDeque},
    fmt,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock, RwLock,
    },
};

use anyhow::{anyhow, Result};
use serde::Serialize;
use tracing::{field::Visit, Event, Subscriber};
use tracing_subscriber::{
    registry::{LookupSpan, Registry},
    reload, EnvFilter, Layer,
};

const MAX_RECENT_LOGS: usize = 5_000;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeLogEntry {
    pub sequence: u64,
    pub timestamp: String,
    pub level: String,
    pub target: String,
    pub message: String,
    pub fields: BTreeMap<String, serde_json::Value>,
}

struct RuntimeLogStore {
    sequence: AtomicU64,
    entries: Mutex<VecDeque<RuntimeLogEntry>>,
}

impl RuntimeLogStore {
    fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            entries: Mutex::new(VecDeque::with_capacity(MAX_RECENT_LOGS)),
        }
    }

    fn push(&self, mut entry: RuntimeLogEntry) {
        entry.sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if entries.len() == MAX_RECENT_LOGS {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    fn recent(&self, limit: usize) -> Vec<RuntimeLogEntry> {
        let entries = self
            .entries
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let count = limit.clamp(1, MAX_RECENT_LOGS).min(entries.len());
        entries
            .iter()
            .skip(entries.len() - count)
            .cloned()
            .collect()
    }
}

#[derive(Default)]
struct EventVisitor {
    message: Option<String>,
    fields: BTreeMap<String, serde_json::Value>,
}

impl EventVisitor {
    fn insert(&mut self, field: &tracing::field::Field, value: serde_json::Value) {
        if field.name() == "message" {
            self.message = value
                .as_str()
                .map(str::to_owned)
                .or_else(|| Some(value.to_string()));
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

impl Visit for EventVisitor {
    fn record_f64(&mut self, field: &tracing::field::Field, value: f64) {
        if let Some(number) = serde_json::Number::from_f64(value) {
            self.insert(field, serde_json::Value::Number(number));
        }
    }

    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.insert(field, value.into());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.insert(field, value.into());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.insert(field, value.into());
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.insert(field, value.into());
    }

    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn fmt::Debug) {
        self.insert(field, format!("{value:?}").into());
    }
}

pub(super) struct RuntimeLogLayer {
    store: Arc<RuntimeLogStore>,
}

impl<S> Layer<S> for RuntimeLogLayer
where
    S: Subscriber + for<'lookup> LookupSpan<'lookup>,
{
    fn on_event(&self, event: &Event<'_>, _context: tracing_subscriber::layer::Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = EventVisitor::default();
        event.record(&mut visitor);
        self.store.push(RuntimeLogEntry {
            sequence: 0,
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: metadata.level().as_str().to_ascii_lowercase(),
            target: metadata.target().to_owned(),
            message: visitor
                .message
                .unwrap_or_else(|| metadata.name().to_owned()),
            fields: visitor.fields,
        });
    }
}

pub(super) struct PendingRuntimeLogControl {
    level: String,
    store: Arc<RuntimeLogStore>,
    reload: reload::Handle<EnvFilter, Registry>,
}

struct RuntimeLogControl {
    level: RwLock<String>,
    store: Arc<RuntimeLogStore>,
    reload: reload::Handle<EnvFilter, Registry>,
}

static RUNTIME_LOG_CONTROL: OnceLock<RuntimeLogControl> = OnceLock::new();

pub(super) fn prepare(
    filter: EnvFilter,
    initial_level: &str,
) -> (
    reload::Layer<EnvFilter, Registry>,
    RuntimeLogLayer,
    PendingRuntimeLogControl,
) {
    let (filter_layer, reload_handle) = reload::Layer::new(filter);
    let store = Arc::new(RuntimeLogStore::new());
    (
        filter_layer,
        RuntimeLogLayer {
            store: store.clone(),
        },
        PendingRuntimeLogControl {
            level: initial_level.trim().to_ascii_lowercase(),
            store,
            reload: reload_handle,
        },
    )
}

pub(super) fn install(control: PendingRuntimeLogControl) -> Result<()> {
    RUNTIME_LOG_CONTROL
        .set(RuntimeLogControl {
            level: RwLock::new(control.level),
            store: control.store,
            reload: control.reload,
        })
        .map_err(|_| anyhow!("runtime logging was already initialized"))
}

pub fn current_level() -> Result<String> {
    Ok(control()?
        .level
        .read()
        .unwrap_or_else(|error| error.into_inner())
        .clone())
}

pub fn set_level(level: &str) -> Result<String> {
    let normalized = level.trim().to_ascii_lowercase();
    let filter = runtime_filter(&normalized)?;
    let control = control()?;
    control
        .reload
        .reload(filter)
        .map_err(|error| anyhow!("cannot reload log filter: {error}"))?;
    *control
        .level
        .write()
        .unwrap_or_else(|error| error.into_inner()) = normalized.clone();
    Ok(normalized)
}

pub fn recent(limit: usize) -> Result<Vec<RuntimeLogEntry>> {
    Ok(control()?.store.recent(limit))
}

fn control() -> Result<&'static RuntimeLogControl> {
    RUNTIME_LOG_CONTROL
        .get()
        .ok_or_else(|| anyhow!("runtime logging is not initialized"))
}

fn runtime_filter(level: &str) -> Result<EnvFilter> {
    match level {
        "trace" | "debug" | "info" | "warn" | "error" | "off" => {
            EnvFilter::try_new(level).map_err(|error| anyhow!("invalid log filter: {error}"))
        }
        _ => Err(anyhow!(
            "unsupported log level {level}; expected trace, debug, info, warn, error, or off"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_filter_accepts_supported_levels_only() {
        assert!(runtime_filter("debug").is_ok());
        assert!(runtime_filter("verbose").is_err());
    }

    #[test]
    fn runtime_log_store_evicts_oldest_entries() {
        let store = RuntimeLogStore::new();
        for index in 0..=MAX_RECENT_LOGS {
            store.push(RuntimeLogEntry {
                sequence: 0,
                timestamp: String::new(),
                level: "info".to_owned(),
                target: "test".to_owned(),
                message: index.to_string(),
                fields: BTreeMap::new(),
            });
        }
        let entries = store.recent(MAX_RECENT_LOGS);
        assert_eq!(entries.len(), MAX_RECENT_LOGS);
        assert_eq!(
            entries.first().map(|entry| entry.message.as_str()),
            Some("1")
        );
    }
}
