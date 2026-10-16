//! The saved-server book — the desktop port of the TS Connection Profile
//! store (CONTEXT.md "Connection Profile": many saved hubs, exactly one
//! active; the local hub is the implicit entry with no book record).
//!
//! Storage is a single JSON file under the app's config dir
//! (`~/.config/zcode-acp-remote/servers.json`, XDG_CONFIG_HOME aware). The
//! TS app keeps the same shape in localStorage (`zcode-acp:servers`); the
//! desktop client owns its own file and does not migrate the WebView copy.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::hub::SavedServer;

/// The book: every saved remote hub plus which one is active. An absent or
/// empty `active_id` means the implicit local hub (Hub Bootstrap path).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerBook {
    pub servers: Vec<SavedServer>,
    pub active_id: Option<String>,
}

impl ServerBook {
    /// The active remote server, if one is set and still present.
    pub fn active(&self) -> Option<&SavedServer> {
        let id = self.active_id.as_ref()?;
        self.servers.iter().find(|s| &s.id == id)
    }

    /// Insert or replace by id (the TS `upsert` semantics) and make it
    /// active — saving a server you just edited means using it.
    pub fn upsert_active(&mut self, server: SavedServer) {
        self.active_id = Some(server.id.clone());
        match self.servers.iter().position(|s| s.id == server.id) {
            Some(i) => self.servers[i] = server,
            None => self.servers.push(server),
        }
    }

    /// Remove by id; a removed active id falls back to the local hub.
    pub fn remove(&mut self, id: &str) {
        self.servers.retain(|s| s.id != id);
        if self.active_id.as_deref() == Some(id) {
            self.active_id = None;
        }
    }

    /// Load from `path`; a missing file is an empty book, not an error.
    pub fn load(path: &std::path::Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str(&text).map_err(|e| e.to_string()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// Persist to `path` (pretty JSON, trailing newline), creating the
    /// parent directory when missing.
    pub fn save(&self, path: &std::path::Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let body = format!("{}\n", serde_json::to_string_pretty(self).map_err(|e| e.to_string())?);
        std::fs::write(path, body).map_err(|e| e.to_string())
    }
}

/// The app's config directory — the one directory every desktop-side
/// persisted file lives in (`$XDG_CONFIG_HOME/zcode-acp-remote` or
/// `~/.config/zcode-acp-remote`): the server book here, plus the app
/// layer's pending-queue and UI-preference files beside it.
pub fn config_dir() -> PathBuf {
    let base = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|v| !v.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })
        .expect("neither XDG_CONFIG_HOME nor HOME is set");
    base.join("zcode-acp-remote")
}

/// The book's file location: `servers.json` inside [`config_dir`].
pub fn servers_path() -> PathBuf {
    config_dir().join("servers.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server(id: &str) -> SavedServer {
        SavedServer {
            id: id.to_string(),
            name: format!("hub {id}"),
            hub_url: format!("https://{id}.example"),
            token: "t".to_string(),
        }
    }

    #[test]
    fn upsert_activates_and_replaces_by_id() {
        let mut book = ServerBook::default();
        book.upsert_active(server("a"));
        book.upsert_active(server("b"));
        assert_eq!(book.active().map(|s| s.id.as_str()), Some("b"));
        let mut edited = server("a");
        edited.name = "renamed".to_string();
        book.upsert_active(edited);
        assert_eq!(book.servers.len(), 2);
        assert_eq!(book.active().map(|s| s.id.as_str()), Some("a"));
        assert_eq!(book.servers[0].name, "renamed");
    }

    #[test]
    fn removing_the_active_falls_back_to_local() {
        let mut book = ServerBook::default();
        book.upsert_active(server("a"));
        book.remove("a");
        assert!(book.active().is_none());
        assert!(book.servers.is_empty());
    }

    #[test]
    fn missing_file_loads_empty_and_save_round_trips() {
        let dir = std::env::temp_dir().join(format!("zcode-book-{}", std::process::id()));
        let path = dir.join("nested").join("servers.json");
        let _ = std::fs::remove_file(&path);
        let book = ServerBook::load(&path).expect("missing file is empty");
        assert_eq!(book, ServerBook::default());

        let mut book = book;
        book.upsert_active(server("x"));
        book.save(&path).expect("save creates dirs");
        let back = ServerBook::load(&path).expect("round trip");
        assert_eq!(back, book);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
