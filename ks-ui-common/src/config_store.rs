//! Persistent Lua script configuration (`config.json`).
//!
//! Lua scripts have no direct filesystem access; they read and write typed
//! config entries through the `config` API. The store lives in the GUI
//! process, survives hot reload, and writes the whole table back to
//! `config.json` at most once per flush interval while dirty (plus on demand
//! through `config.save()` and once at exit).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Upper bounds keep a buggy or hostile script from growing the file without
/// bound. They mirror the validation rules used at every other trust boundary.
const MAX_KEY_BYTES: usize = 128;
const MAX_VALUE_BYTES: usize = 4096;
const MAX_ENTRIES: usize = 256;
const FLUSH_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ConfigValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
}

pub type SharedConfigStore = Arc<Mutex<ConfigStore>>;

pub struct ConfigStore {
    path: PathBuf,
    entries: BTreeMap<String, ConfigValue>,
    dirty: bool,
    last_flush: Instant,
}

impl ConfigStore {
    /// Loads (or creates) the store for `config.json` beside the executable.
    pub fn shared(exe_dir: &Path) -> SharedConfigStore {
        let mut store = Self {
            path: exe_dir.join("config.json"),
            entries: BTreeMap::new(),
            dirty: false,
            last_flush: Instant::now(),
        };
        store.load();
        Arc::new(Mutex::new(store))
    }

    fn load(&mut self) {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return;
        };
        match serde_json::from_str::<BTreeMap<String, ConfigValue>>(&text) {
            Ok(entries) => self.entries = entries,
            Err(error) => {
                tracing::warn!(%error, path = %self.path.display(), "config.json parse failed; starting empty");
            }
        }
    }

    pub fn set(&mut self, key: &str, value: ConfigValue) -> bool {
        if key.is_empty() || key.len() > MAX_KEY_BYTES {
            return false;
        }
        if let ConfigValue::Str(text) = &value {
            if text.len() > MAX_VALUE_BYTES {
                return false;
            }
        }
        if !self.entries.contains_key(key) && self.entries.len() >= MAX_ENTRIES {
            tracing::warn!("config store full ({MAX_ENTRIES} entries)");
            return false;
        }
        self.entries.insert(key.to_owned(), value);
        self.dirty = true;
        true
    }

    pub fn get(&self, key: &str) -> Option<ConfigValue> {
        self.entries.get(key).cloned()
    }

    pub fn remove(&mut self, key: &str) -> bool {
        if self.entries.remove(key).is_some() {
            self.dirty = true;
            true
        } else {
            false
        }
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Writes the table to `config.json` (temp file + rename so a crash never
    /// leaves a half-written file behind).
    pub fn flush(&mut self) {
        self.last_flush = Instant::now();
        if !self.dirty {
            return;
        }
        let text = match serde_json::to_string_pretty(&self.entries) {
            Ok(text) => text,
            Err(error) => {
                tracing::error!(%error, "config serialize failed");
                return;
            }
        };
        let temp = self.path.with_extension("json.tmp");
        let write = std::fs::write(&temp, text.as_bytes())
            .and_then(|()| std::fs::rename(&temp, &self.path));
        match write {
            Ok(()) => self.dirty = false,
            Err(error) => {
                tracing::error!(%error, path = %self.path.display(), "config write failed");
                let _ = std::fs::remove_file(&temp);
            }
        }
    }

    /// Flushes at most once per [`FLUSH_INTERVAL`] while dirty; called every
    /// frame from the GUI loop.
    pub fn flush_if_due(&mut self) {
        if self.dirty && self.last_flush.elapsed() >= FLUSH_INTERVAL {
            self.flush();
        }
    }
}

impl Drop for ConfigStore {
    fn drop(&mut self) {
        self.flush();
    }
}
