//! The persisted per-session turn readouts (batch T) — the desktop port of
//! the TS session-cache's readout half (`src/lib/storage.ts`
//! `writeSessionCache` stores usage/cacheHit/turnStats per session, so the
//! mobile app's FIRST open of a session still shows them). The desktop's
//! `SessionCore` snapshot LRU (`protocol::replay::SessionCache`) is
//! memory-only: it covers switches and wake-reconnects within one process,
//! but a fresh process starts blind — the exact gap this store closes.
//!
//! Only the readouts persist: messages replay from the bridge on attach
//! anyway (a cold start has no snapshot to paint), and a warm in-memory
//! cache hit already restores everything, these fields included.
//!
//! File: `session-readouts.json` beside the server book
//! (`protocol::profiles::config_dir`). Same doctrine as `pending_store.rs`:
//! permissive load (a bad entry drops, never the file), temp+rename writes,
//! no file once empty. Session ids are hub-issued UUIDs — the map is NOT
//! reset on a hub switch (unlike the pending drafts); a session keeps its
//! readouts wherever it lives.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use protocol::acp::ContextUsage;
use protocol::chat::TurnStats;
use serde::{Deserialize, Serialize};

/// One session's last-known readouts — the snapshot's tail three fields.
#[derive(Debug, Default, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub(crate) struct SessionReadout {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) usage: Option<ContextUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) cache_hit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) turn_stats: Option<TurnStats>,
}

impl SessionReadout {
    fn is_empty(&self) -> bool {
        self.usage.is_none() && self.cache_hit.is_none() && self.turn_stats.is_none()
    }
}

/// The persisted map: session id → last-known readouts.
#[derive(Debug, Default, Clone, PartialEq)]
pub(crate) struct ReadoutStore {
    map: HashMap<String, SessionReadout>,
}

impl ReadoutStore {
    /// Load from `path`; a missing or invalid file is an empty store, not an
    /// error (persistence never takes the readouts down with it).
    pub(crate) fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return Self::default(),
        };
        // Value-level first: a typed map would let ONE bad session fail the
        // entire file.
        let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(&text) {
            Ok(raw) => raw,
            Err(_) => return Self::default(),
        };
        let mut map = HashMap::new();
        for (sid, value) in raw {
            if let Ok(readout) = serde_json::from_value::<SessionReadout>(value) {
                if !readout.is_empty() {
                    map.insert(sid, readout);
                }
            }
        }
        Self { map }
    }

    /// Persist to `path` (pretty JSON + trailing newline, parent dirs
    /// created), removing the file when the map is empty. Temp-file +
    /// rename: a crash mid-write must not truncate the store.
    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        if self.map.is_empty() {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e.to_string()),
            };
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(&self.map).map_err(|e| e.to_string())?
        );
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, body) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.to_string());
        }
        match std::fs::rename(&tmp, path) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                Err(e.to_string())
            }
        }
    }

    /// This session's persisted readouts, if any (the attach-time seed
    /// source).
    pub(crate) fn get(&self, session_id: &str) -> Option<SessionReadout> {
        self.map.get(session_id).copied()
    }

    /// Record one session's current readouts (a turn settled). An
    /// all-`None` readout drops the key — a session that never reported
    /// numbers leaves no stale entry behind.
    pub(crate) fn record(&mut self, session_id: &str, readout: SessionReadout) {
        if readout.is_empty() {
            self.map.remove(session_id);
        } else {
            self.map.insert(session_id.to_owned(), readout);
        }
    }

    /// Drop a deleted session's entry (the tombstone delete).
    pub(crate) fn remove(&mut self, session_id: &str) {
        self.map.remove(session_id);
    }
}

/// The store's file location: `session-readouts.json` beside the server
/// book.
pub(crate) fn readouts_path() -> PathBuf {
    protocol::profiles::config_dir().join("session-readouts.json")
}

#[cfg(test)]
mod tests {
    use super::{readouts_path, ReadoutStore, SessionReadout};
    use protocol::acp::ContextUsage;
    use protocol::chat::TurnStats;

    fn readout() -> SessionReadout {
        SessionReadout {
            usage: Some(ContextUsage {
                used: 175_000,
                size: 1_000_000,
            }),
            cache_hit: Some(82),
            turn_stats: Some(TurnStats {
                tps: 41.0,
                output_tokens: 1_234,
                seconds: 30.1,
            }),
        }
    }

    /// The round trip that fixes first-open blindness: record + save in one
    /// process, load in the next.
    #[test]
    fn readouts_survive_a_restart_round_trip() {
        let dir = std::env::temp_dir().join(format!(
            "zcode-readouts-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-readouts.json");

        let mut first = ReadoutStore::default();
        first.record("sess_1", readout());
        first.save(&path).unwrap();

        let second = ReadoutStore::load(&path);
        assert_eq!(second.get("sess_1"), Some(readout()));
        assert_eq!(second.get("sess_other"), None);

        // An all-None readout drops the key, and the emptied map removes the
        // file (nothing stale left behind).
        let mut third = second.clone();
        third.record("sess_1", SessionReadout::default());
        third.save(&path).unwrap();
        assert!(!path.exists(), "the emptied store removes its file");
        assert_eq!(ReadoutStore::load(&path).get("sess_1"), None);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The permissive load: one bad entry drops alone, an unreadable file is
    /// an empty store.
    #[test]
    fn readouts_load_is_permissive() {
        let dir = std::env::temp_dir().join(format!(
            "zcode-readouts-bad-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("session-readouts.json");

        std::fs::write(&path, "not json at all").unwrap();
        assert_eq!(ReadoutStore::load(&path), ReadoutStore::default());

        std::fs::write(
            &path,
            r#"{"good": {"cache_hit": 50}, "bad": {"usage": "bogus"}}"#,
        )
        .unwrap();
        let store = ReadoutStore::load(&path);
        assert_eq!(
            store.get("good").and_then(|r| r.cache_hit),
            Some(50),
            "the good entry survives"
        );
        assert_eq!(store.get("bad"), None, "the bad entry drops alone");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The store never shares the OS temp default: it lives beside the
    /// server book (same rule as the pending store).
    #[test]
    fn readouts_path_sits_in_the_config_dir() {
        let path = readouts_path();
        assert_eq!(
            path.parent(),
            Some(protocol::profiles::config_dir().as_path())
        );
        assert_eq!(
            path.file_name().and_then(|n| n.to_str()),
            Some("session-readouts.json")
        );
    }
}
