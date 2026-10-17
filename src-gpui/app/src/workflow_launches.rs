//! APP-side launch memory for dynamic workflows (batch M) — the desktop
//! port of the TS localStorage table (`src/lib/workflow-launches.ts`).
//!
//! `POST …/start` is the ONLY moment a run's backend id and its ACP session
//! id are both in the app's hands — history rows carry the backend
//! `parentSessionId`, and nothing server-side maps it back for a remote
//! client. So every successful start is recorded here, and this table is
//! the single source for "which session owns this run" (open session, run
//! detail, resume). Entries go stale (the bridge restarts, the session is
//! closed): consumers treat an unknown session as a read-only row, the TS
//! `lookupLaunch(runId)?.acpSessionId ?? row.acpSessionId` ladder.
//!
//! File: `workflow-launches.json` beside the server book
//! (`protocol::profiles::config_dir`), written the same temp-file + rename
//! way as `pending_store.rs`. An unreadable file reads as empty and the
//! feature degrades to memory-only — persistence never takes the page down.

use std::path::Path;

use protocol::workflow::WorkflowLaunch;

/// Newest-first launch list, capped at 200 (`zcode.workflowLaunches`' CAP —
/// 200 runs ≈ months of launches at any realistic pace).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct WorkflowLaunchStore {
    entries: Vec<WorkflowLaunch>,
}

/// The store's file location: `workflow-launches.json` beside the server
/// book (the same directory `pending_store.rs` writes in).
pub(crate) fn launches_path() -> std::path::PathBuf {
    protocol::profiles::config_dir().join("workflow-launches.json")
}

impl WorkflowLaunchStore {
    /// Load from `path`; a missing or invalid file is an empty store (the
    /// TS `catch { return [] }` semantics).
    pub(crate) fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return Self::default(),
        };
        Self::from_json(&text)
    }

    /// The load's permissive parse: a non-array body or a bad entry drops
    /// out instead of failing the whole table (TS casts the parsed array;
    /// the port keeps only entries that decode).
    fn from_json(text: &str) -> Self {
        let raw: Vec<serde_json::Value> = match serde_json::from_str(text) {
            Ok(raw) => raw,
            Err(_) => return Self::default(),
        };
        Self {
            entries: raw
                .into_iter()
                .filter_map(|v| serde_json::from_value(v).ok())
                .collect(),
        }
    }

    /// Persist to `path` (pretty JSON + trailing newline, parent dirs
    /// created) — temp-file + rename, so a crash mid-write cannot truncate
    /// the table (an in-place partial write would read back empty and lose
    /// EVERY launch's join, the exact data this store exists to protect).
    fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let body = format!(
            "{}\n",
            serde_json::to_string_pretty(&self.entries).map_err(|e| e.to_string())?
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

    /// Record one successful launch (TS `rememberLaunch`): re-inserting a
    /// runId moves it to the front, the cap trims the tail. `at` stamps the
    /// caller's clock.
    pub(crate) fn remember(
        &mut self,
        launch: WorkflowLaunch,
        at: u64,
        path: &Path,
    ) -> Result<(), String> {
        let mut launch = launch;
        launch.at = at;
        self.entries.retain(|e| e.run_id != launch.run_id);
        self.entries.insert(0, launch);
        self.entries.truncate(LAUNCH_CAP);
        self.save(path)
    }

    /// The launch one run row resolves through (TS `lookupLaunch`).
    pub(crate) fn lookup(&self, run_id: &str) -> Option<&WorkflowLaunch> {
        self.entries.iter().find(|e| e.run_id == run_id)
    }
}

/// The table's size cap (TS `CAP = 200`).
const LAUNCH_CAP: usize = 200;

#[cfg(test)]
mod tests {
    // Explicit imports only — the sibling test modules' shape.
    use super::{WorkflowLaunch, WorkflowLaunchStore};
    use protocol::workflow::WorkflowScope;
    use std::path::PathBuf;
    fn launch(run_id: &str, session: &str, instance: &str) -> WorkflowLaunch {
        WorkflowLaunch {
            run_id: run_id.to_string(),
            acp_session_id: session.to_string(),
            instance_id: instance.to_string(),
            name: "deploy".to_string(),
            scope: WorkflowScope::Project,
            at: 0,
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "zcode-launches-{tag}-{}-{}",
            std::process::id(),
            tag
        ))
    }

    /// Round trip, plus the missing-file and garbage-file reads: both read
    /// as empty (persistence never takes the feature down).
    #[test]
    fn store_round_trips_and_bad_files_read_empty() {
        let path = tmp_path("rt");
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            WorkflowLaunchStore::load(&path),
            WorkflowLaunchStore::default()
        );
        let mut store = WorkflowLaunchStore::default();
        store
            .remember(launch("run-1", "s-1", "inst-a"), 7, &path)
            .expect("remember saves");
        // Missing parent dirs are created on the way.
        let nested = path.parent().unwrap().join("nested/launches.json");
        store
            .remember(launch("run-2", "s-2", "inst-b"), 8, &nested)
            .expect("nested save");
        let reloaded = WorkflowLaunchStore::load(&nested);
        assert_eq!(reloaded.lookup("run-1").map(|l| l.acp_session_id.as_str()), Some("s-1"));
        assert_eq!(reloaded.lookup("run-2").map(|l| l.instance_id.as_str()), Some("inst-b"));
        // The stamp came from the caller's clock, and a garbage file is empty.
        assert_eq!(reloaded.lookup("run-2").map(|l| l.at), Some(8));
        assert_eq!(
            WorkflowLaunchStore::from_json("not json{"),
            WorkflowLaunchStore::default()
        );
        // A list with one bad entry keeps the good one (TS casts; the port
        // drops only the undecodable).
        let mixed = WorkflowLaunchStore::from_json(
            r#"[{"runId":"r","acpSessionId":"s","instanceId":"i","name":"n","scope":"global","at":1},42]"#,
        );
        assert!(mixed.lookup("r").is_some());
        assert_eq!(mixed.lookup("42"), None);
        let _ = std::fs::remove_dir_all(path.parent().unwrap().join("nested"));
        let _ = std::fs::remove_file(&path);
    }

    /// Re-inserting a runId moves it to the front (the newest-first list IS
    /// the recency), and the cap trims the TAIL — the oldest launches go.
    #[test]
    fn remember_moves_to_front_and_trims_the_tail() {
        let mut store = WorkflowLaunchStore::default();
        store
            .remember(launch("run-1", "s-1", "inst-a"), 1, &tmp_path("nul1"))
            .expect("save");
        store
            .remember(launch("run-2", "s-2", "inst-a"), 2, &tmp_path("nul2"))
            .expect("save");
        store
            .remember(launch("run-1", "s-1-new", "inst-a"), 3, &tmp_path("nul3"))
            .expect("save");
        assert_eq!(store.lookup("run-1").map(|l| l.acp_session_id.as_str()), Some("s-1-new"));

        let mut capped = WorkflowLaunchStore::default();
        for i in 0..205 {
            capped
                .remember(
                    launch(&format!("run-{i}"), "s", "inst"),
                    i as u64,
                    &tmp_path("cap"),
                )
                .expect("save");
        }
        let count = capped.entries_count_for_test();
        assert_eq!(count, 200, "the cap holds at 200");
        assert!(capped.lookup("run-204").is_some(), "newest kept");
        assert!(capped.lookup("run-0").is_none(), "the oldest five trimmed");
    }
}

#[cfg(test)]
impl WorkflowLaunchStore {
    /// Test-only size probe (the cap assertion).
    fn entries_count_for_test(&self) -> usize {
        self.entries.len()
    }
}
