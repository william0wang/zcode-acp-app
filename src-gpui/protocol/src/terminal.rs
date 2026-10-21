//! Terminal preferences for the file panel's "open a terminal here" action.
//!
//! The ORDER is the user's own `remote.terminal` block in
//! `~/.config/zcode-acp/config.json` — the same list the hub walks when it
//! incubates a visible session terminal (server ADR-0016, `remote/config.ts`
//! `remoteTerminalPrefs` + `hub-server.ts` `resolveTerminalLaunches`). Reusing
//! it here keeps one answer to "which terminal is mine": the manual button and
//! the automatic incubation open the same app, and a broken first choice falls
//! down the same list.
//!
//! Pure logic only — the app layer hands each candidate to LaunchServices.

use serde_json::{Map, Value};

/// The `remote.terminal` block (`user-config.ts` `TerminalPrefs`). Every field
/// is optional: a missing block means "Terminal.app", the hub's own default.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TerminalPrefs {
    /// `false` = no visible terminal windows at all (headless setups). The
    /// manual affordance follows it too: the user turned the app off, not
    /// just the automatic incubation.
    pub enabled: Option<bool>,
    /// One app name (`remote.terminal.app`), used when `terminals` is empty.
    pub app: Option<String>,
    /// The ORDERED fallback list (`remote.terminal.terminals`).
    pub terminals: Vec<String>,
    /// The `{script}` command template. Carried so the block parses as one
    /// shape; the directory action ignores it (there is no script to hand
    /// over — that template only makes sense for the hub's incubation).
    pub command: Option<String>,
}

impl TerminalPrefs {
    /// The candidate bundle names to try, in the user's order. Empty = the
    /// affordance stays hidden (`enabled: false`).
    pub fn app_candidates(&self) -> Vec<String> {
        if self.enabled == Some(false) {
            return Vec::new();
        }
        let names: Vec<&str> = if !self.terminals.is_empty() {
            self.terminals.iter().map(String::as_str).collect()
        } else if let Some(app) = self.app.as_deref() {
            vec![app]
        } else {
            // macOS has no default-terminal setting to detect; Terminal.app is
            // the hub's fallback too.
            vec!["Terminal"]
        };
        names.into_iter().map(normalize_terminal_app).collect()
    }
}

/// Parse the `remote.terminal` block out of its parent object — the same
/// permissive doctrine as `parse_remote_section` (`user-config.ts`: junk
/// values read as absent, blank strings trim away, a non-object block is
/// ignored whole).
pub fn parse_terminal_prefs(remote: &Map<String, Value>) -> TerminalPrefs {
    let Some(block) = remote.get("terminal").and_then(Value::as_object) else {
        return TerminalPrefs::default();
    };
    let mut prefs = TerminalPrefs::default();
    if let Some(enabled) = block.get("enabled").and_then(Value::as_bool) {
        prefs.enabled = Some(enabled);
    }
    if let Some(names) = block.get("terminals").and_then(Value::as_array) {
        prefs.terminals = names
            .iter()
            .filter_map(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .map(str::to_owned)
            .collect();
    }
    if let Some(app) = block.get("app").and_then(Value::as_str) {
        let app = app.trim();
        if !app.is_empty() {
            prefs.app = Some(app.to_owned());
        }
    }
    if let Some(command) = block.get("command").and_then(Value::as_str) {
        let command = command.trim();
        if !command.is_empty() {
            prefs.command = Some(command.to_owned());
        }
    }
    prefs
}

/// Normalize one configured name onto the bundle name `open -a` resolves:
/// case- and `.app`-suffix-insensitive aliases for the well-known terminals
/// (the hub's `TERMINAL_APP_LAUNCHERS` keys), everything else passed through
/// as typed (a moved install, a niche terminal).
pub fn normalize_terminal_app(raw: &str) -> String {
    let trimmed = raw.trim();
    let lowered = trimmed.to_lowercase();
    let bare = lowered.strip_suffix(".app").unwrap_or(&lowered);
    let key = bare.replace(char::is_whitespace, "-");
    let canonical = match key.as_str() {
        "terminal" | "apple_terminal" => "Terminal",
        "iterm" | "iterm2" => "iTerm",
        "wezterm" => "WezTerm",
        "kitty" => "kitty",
        "alacritty" => "Alacritty",
        "ghostty" => "Ghostty",
        "warp" => "Warp",
        "warp-preview" | "warp_preview" => "Warp Preview",
        _ => return trimmed.to_owned(),
    };
    canonical.to_owned()
}

#[cfg(test)]
mod tests {
    use super::{normalize_terminal_app, parse_terminal_prefs, TerminalPrefs};
    use serde_json::json;

    fn block(value: serde_json::Value) -> TerminalPrefs {
        let map = value.as_object().expect("object").clone();
        parse_terminal_prefs(&map)
    }

    /// The user's list IS the order — no sorting, no dedupe, no dropping of
    /// names we do not recognize.
    #[test]
    fn the_configured_order_is_preserved_verbatim() {
        let prefs = block(json!({
            "terminal": {"app": "ghostty", "terminals": ["ghostty", "warp", "Terminal"]}
        }));
        assert_eq!(prefs.terminals, vec!["ghostty", "warp", "Terminal"]);
        assert_eq!(prefs.app.as_deref(), Some("ghostty"));
        assert_eq!(
            prefs.app_candidates(),
            vec![
                "Ghostty".to_string(),
                "Warp".to_string(),
                "Terminal".to_string()
            ]
        );
    }

    #[test]
    fn lists_beat_app_beat_the_terminal_default() {
        let prefs = TerminalPrefs {
            enabled: None,
            app: Some("wezterm".to_owned()),
            terminals: vec!["kitty".to_owned()],
            command: None,
        };
        assert_eq!(prefs.app_candidates(), vec!["kitty".to_string()]);
        let app_only = TerminalPrefs {
            terminals: Vec::new(),
            ..prefs.clone()
        };
        assert_eq!(app_only.app_candidates(), vec!["WezTerm".to_string()]);
        assert_eq!(
            TerminalPrefs::default().app_candidates(),
            vec!["Terminal".to_string()]
        );
    }

    /// `enabled: false` is the user saying "no visible terminal windows" — the
    /// button stays hidden, exactly like the hub skipping its incubation.
    #[test]
    fn the_disabled_gate_yields_no_candidates() {
        let prefs = TerminalPrefs {
            enabled: Some(false),
            app: Some("ghostty".to_owned()),
            terminals: vec!["ghostty".to_owned(), "Terminal".to_owned()],
            command: None,
        };
        assert!(prefs.app_candidates().is_empty());
        assert_eq!(
            TerminalPrefs {
                enabled: Some(true),
                ..prefs
            }
            .app_candidates(),
            vec!["Ghostty".to_string(), "Terminal".to_string()]
        );
    }

    #[test]
    fn aliases_normalize_to_bundle_names() {
        assert_eq!(normalize_terminal_app("Ghostty.app"), "Ghostty");
        assert_eq!(normalize_terminal_app("  iterm2  "), "iTerm");
        assert_eq!(normalize_terminal_app("Apple_Terminal"), "Terminal");
        assert_eq!(normalize_terminal_app("warp-preview"), "Warp Preview");
        assert_eq!(normalize_terminal_app("Warp Preview"), "Warp Preview");
        // Unknown names pass through as typed — `open -a` resolves them.
        assert_eq!(normalize_terminal_app("Alacritty Beta"), "Alacritty Beta");
        assert_eq!(normalize_terminal_app("my-term"), "my-term");
    }

    #[test]
    fn junk_reads_as_absent_like_every_other_config_field() {
        assert_eq!(block(json!({"terminal": 7})), TerminalPrefs::default());
        assert_eq!(block(json!({})), TerminalPrefs::default());
        let junk = block(json!({
            "terminal": {"enabled": "yes", "app": "  ", "command": "", "terminals": "ghostty"}
        }));
        assert_eq!(junk, TerminalPrefs::default());
        // A non-string entry drops out; the rest of the list survives.
        let mixed = block(json!({
            "terminal": {"terminals": ["ghostty", 3, "  ", "warp"]}
        }));
        assert_eq!(mixed.terminals, vec!["ghostty", "warp"]);
    }
}
