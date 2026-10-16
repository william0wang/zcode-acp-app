//! The persisted pending-prompt queue (batch H1) — the desktop port of the
//! TS localStorage map (`src/lib/storage.ts` `loadPending`/`savePending`):
//! one draft queue per session id, surviving restarts and session switches.
//! `SessionCore` owns the in-memory queues (`pending_prompts`, keyed by
//! session id); this store is the app-layer persistence the protocol crate
//! deliberately leaves to its caller (module doc in `protocol/src/session.rs`).
//!
//! File: `pending-prompts.json` beside the server book
//! (`protocol::profiles::config_dir`). Entries are validated on load the TS
//! way — invalid drafts drop out, an unreadable file reads as empty, and the
//! queue feature degrades to memory-only rather than failing the app.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use protocol::chat::PromptDraft;

/// The persisted map: session id → queued drafts, in send order.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct PendingStore {
    map: HashMap<String, Vec<PromptDraft>>,
}

impl PendingStore {
    /// Load from `path`; a missing or invalid file is an empty store, not an
    /// error (the TS `catch { return {} }` semantics — persistence never
    /// takes the queue down with it).
    pub(crate) fn load(path: &Path) -> Self {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(_) => return Self::default(),
        };
        Self::from_json(&text)
    }

    /// The load's permissive parse: unknown shapes and invalid drafts drop
    /// out instead of failing the whole map (TS `isDraft` filtering); empty
    /// queues lose their key, as everywhere else.
    fn from_json(text: &str) -> Self {
        // Value-level first: a typed map would let ONE bad session (or one
        // bad draft) fail the entire file.
        let raw: HashMap<String, serde_json::Value> = match serde_json::from_str(text) {
            Ok(raw) => raw,
            Err(_) => return Self::default(),
        };
        let mut map = HashMap::new();
        for (sid, value) in raw {
            let Some(drafts) = value.as_array() else {
                continue; // legacy/unknown shape — dropped, not fatal
            };
            let queue: Vec<PromptDraft> = drafts
                .iter()
                .filter_map(|v| serde_json::from_value(v.clone()).ok())
                .collect();
            if !queue.is_empty() {
                map.insert(sid, queue);
            }
        }
        Self { map }
    }

    /// Persist to `path` (pretty JSON + trailing newline, parent dirs
    /// created), removing the file when the map is empty — the TS
    /// `removeItem`-on-empty behavior, so a fully drained queue leaves no
    /// stale file behind. The write is temp-file + rename: a crash
    /// mid-write must not truncate the file the drafts live in (an
    /// in-place partial write would read back as empty and lose EVERY
    /// session's queue, the exact data this store exists to protect).
    pub(crate) fn save(&self, path: &Path) -> Result<(), String> {
        if self.map.is_empty() {
            return match std::fs::remove_file(path) {
                Ok(()) => Ok(()),
                // Nothing to remove is success (the TS removeItem no-ops).
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

    /// This session's persisted queue, if any (the attach-time seed source).
    pub(crate) fn queue(&self, session_id: &str) -> Option<&[PromptDraft]> {
        self.map.get(session_id).map(Vec::as_slice)
    }

    /// The whole map by reference (the driver loop's merge input).
    pub(crate) fn snapshot(&self) -> &HashMap<String, Vec<PromptDraft>> {
        &self.map
    }

    /// Replace the whole map and persist it at [`pending_path`].
    pub(crate) fn adopt(&mut self, next: HashMap<String, Vec<PromptDraft>>) -> Result<(), String> {
        self.map = next;
        self.save(&pending_path())
    }

    /// Clear every entry and persist the empty map (a Connection Profile
    /// switch — the drafts belong to the previous hub's sessions, the TS
    /// `savePending({})` on switchServer/disconnectHub).
    pub(crate) fn reset(&mut self) {
        let _ = self.adopt(HashMap::new());
    }
}

/// The store's file location: `pending-prompts.json` beside the server book.
pub(crate) fn pending_path() -> PathBuf {
    protocol::profiles::config_dir().join("pending-prompts.json")
}

/// One sync's merge rule (batch H1): every live queue overwrites its
/// session's entry; a session attached WITHOUT a queue drops its entry (the
/// queue drained, was discarded, or was edited away while attached); queues
/// of sessions no tab holds survive — drafts wait for the next attach, the
/// TS localStorage semantics. Returns the next map only when it differs
/// (callers skip the disk write otherwise).
pub(crate) fn merge_pending_queues(
    persisted: &HashMap<String, Vec<PromptDraft>>,
    live: &[(Option<&str>, &HashMap<String, Vec<PromptDraft>>)],
) -> Option<HashMap<String, Vec<PromptDraft>>> {
    // Cheap pre-pass: nothing queued anywhere and nothing to drop means the
    // merge is a no-op — skip the deep comparison (image drafts are large).
    let any_live = live.iter().any(|(_, queues)| !queues.is_empty());
    let any_drop = live.iter().any(|(active, queues)| {
        active
            .map(|sid| !queues.contains_key(sid) && persisted.contains_key(sid))
            .unwrap_or(false)
    });
    if !any_live && !any_drop {
        return None;
    }
    let mut changed = false;
    for (active, queues) in live {
        for (sid, queue) in queues.iter() {
            if persisted.get(sid) != Some(queue) {
                changed = true;
            }
        }
        if let Some(sid) = *active {
            if !queues.contains_key(sid) && persisted.contains_key(sid) {
                changed = true;
            }
        }
    }
    if !changed {
        return None;
    }
    let mut next = persisted.clone();
    for (active, queues) in live {
        for (sid, queue) in queues.iter() {
            next.insert(sid.clone(), queue.clone());
        }
        if let Some(sid) = *active {
            if !queues.contains_key(sid) {
                next.remove(sid);
            }
        }
    }
    Some(next)
}

#[cfg(test)]
mod tests {
    // Explicit imports only — sibling test modules' shape (the kit glob
    // shadowing note; this crate's tests must keep the built-in #[test]).
    use super::{merge_pending_queues, PendingStore};
    use protocol::chat::{AttachmentDraft, PromptDraft};
    use std::collections::HashMap;

    fn draft(text: &str) -> PromptDraft {
        PromptDraft {
            text: text.to_string(),
            images: Vec::new(),
        }
    }

    fn image_draft(text: &str, data: &str) -> PromptDraft {
        PromptDraft {
            text: text.to_string(),
            images: vec![AttachmentDraft {
                data: data.to_string(),
                mime_type: "image/png".to_string(),
            }],
        }
    }

    fn queues_of(
        entries: &[(&str, &[PromptDraft])],
    ) -> HashMap<String, Vec<PromptDraft>> {
        entries
            .iter()
            .map(|(sid, q)| (sid.to_string(), q.to_vec()))
            .collect()
    }

    #[test]
    fn store_round_trips_and_missing_file_reads_empty() {
        let dir = std::env::temp_dir().join(format!("zcode-pending-{}", std::process::id()));
        let path = dir.join("nested").join("pending-prompts.json");
        let _ = std::fs::remove_file(&path);
        assert_eq!(PendingStore::load(&path), PendingStore::default());

        let store = PendingStore {
            map: queues_of(&[("sess_1", &[draft("queued while offline"), image_draft("shot", "aGk=")])]),
        };
        store.save(&path).expect("save creates dirs");
        assert_eq!(PendingStore::load(&path), store);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn corrupt_entries_drop_instead_of_failing_the_map() {
        // A half-written file: one valid entry, one invalid draft, one
        // unknown session shape. The valid queue survives; the rest fall
        // away (the TS isDraft filter), and garbage reads as empty.
        let text = r#"{
            "sess_1": [{"text": "kept", "images": []}, {"text": 7}],
            "sess_2": "not-a-list"
        }"#;
        let store = PendingStore::from_json(text);
        assert_eq!(store.queue("sess_1"), Some(&[draft("kept")][..]));
        assert!(store.queue("sess_2").is_none());
        assert_eq!(PendingStore::from_json("not json{"), PendingStore::default());
    }

    #[test]
    fn empty_store_save_removes_the_file() {
        let dir = std::env::temp_dir().join(format!("zcode-pending-empty-{}", std::process::id()));
        let path = dir.join("pending-prompts.json");
        let _ = std::fs::remove_dir_all(&dir);
        let store = PendingStore::default();
        store.save(&path).expect("empty save with no file is fine");
        // After draining: the file disappears (TS removeItem on empty).
        let store = PendingStore {
            map: queues_of(&[("sess_1", &[draft("x")])]),
        };
        store.save(&path).expect("seed file");
        let drained = PendingStore::default();
        drained.save(&path).expect("drain save");
        assert!(!path.exists(), "an empty map must leave no stale file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn live_queues_overwrite_and_drains_drop_while_attached() {
        let persisted = queues_of(&[
            ("sess_1", &[draft("old head")]),
            ("sess_2", &[draft("waits for re-attach")]),
        ]);
        // sess_1 attached with a NEW queue; sess_3 attached with none
        // (nothing persisted — no drop); a detached sess_2 keeps its entry.
        let tab1 = queues_of(&[("sess_1", &[draft("new head"), draft("second")])]);
        let tab2 = queues_of(&[]);
        let live = vec![(Some("sess_1"), &tab1), (Some("sess_3"), &tab2)];
        let next = merge_pending_queues(&persisted, &live).expect("queue changed");
        assert_eq!(
            next.get("sess_1").map(Vec::as_slice),
            Some(&[draft("new head"), draft("second")][..])
        );
        assert_eq!(
            next.get("sess_2").map(Vec::as_slice),
            Some(&[draft("waits for re-attach")][..]),
            "a session no tab holds keeps its drafts"
        );
        assert!(next.get("sess_3").is_none());
    }

    #[test]
    fn attached_session_with_no_queue_drops_its_entry() {
        // The flush/discard/edit drain path: attached + empty queue must
        // remove the persisted entry, or a restart would resurrect and
        // re-send drafts that already went out.
        let persisted = queues_of(&[("sess_1", &[draft("already sent")])]);
        let queues = queues_of(&[("sess_9", &[draft("unrelated")])]);
        let live = vec![(Some("sess_1"), &queues)];
        let next = merge_pending_queues(&persisted, &live).expect("entry must drop");
        assert!(next.get("sess_1").is_none());
        assert_eq!(
            next.get("sess_9").map(Vec::as_slice),
            Some(&[draft("unrelated")][..])
        );
    }

    #[test]
    fn unchanged_state_merges_to_none() {
        let persisted = queues_of(&[("sess_2", &[draft("parked")])]);
        // All idle: no live queues, nothing attached that has an entry.
        let empty = HashMap::new();
        let live = vec![(Some("sess_1"), &empty)];
        assert!(merge_pending_queues(&persisted, &live).is_none());
        // Same content re-reported: still None.
        let queues = queues_of(&[("sess_2", &[draft("parked")])]);
        let live = vec![(None, &queues)];
        assert!(merge_pending_queues(&persisted, &live).is_none());
    }
}
