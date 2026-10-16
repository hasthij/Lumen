//! Remembers what you launch so frequent picks rise to the top.

use std::collections::HashMap;
use std::path::PathBuf;

pub struct History {
    counts: HashMap<String, u32>,
    dirty: bool,
}

fn path() -> PathBuf {
    crate::log::dir().join("launcher-history.txt")
}

impl History {
    pub fn load() -> Self {
        let mut counts = HashMap::new();
        if let Ok(text) = std::fs::read_to_string(path()) {
            for line in text.lines() {
                if let Some((n, key)) = line.split_once('\t') {
                    if let Ok(n) = n.parse() {
                        counts.insert(key.to_string(), n);
                    }
                }
            }
        }
        Self { counts, dirty: false }
    }

    pub fn boost(&self, key: &str) -> i32 {
        self.counts.get(key).map_or(0, |&n| (n.min(30) * 40) as i32)
    }

    pub fn record(&mut self, key: &str) {
        *self.counts.entry(key.to_string()).or_insert(0) += 1;
        self.dirty = true;
    }

    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        // Keep the 500 most used entries.
        let mut entries: Vec<_> = self.counts.iter().collect();
        entries.sort_by(|a, b| b.1.cmp(a.1));
        let text: String = entries.iter().take(500).map(|(k, n)| format!("{n}\t{k}\n")).collect();
        let tmp = path().with_extension("tmp");
        if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, path()).is_ok() {
            self.dirty = false;
        }
    }
}
