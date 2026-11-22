//! The settings search index + matcher (ADR-0014 §4): the fixed settings
//! rows and section entries — never the user's dynamic resources (models,
//! agents, MCP servers), matching upstream's rule and keeping the index
//! cheap and offline.
//!
//! Matching follows the upstream semantics: normalized substring first,
//! then sequential characters, and for the Chinese labels FULL pinyin and
//! pinyin initials; both locales' labels are always indexed (the other
//! language doubles as the alias source). Normalization is lowercase +
//! whitespace-stripped (our fixed labels carry no NFKC edge cases).

use pinyin::ToPinyin;
use rust_i18n::t;

use crate::settings_overlay::SettingsSection;
use crate::runtime::ConfigPageKind;

/// One index row. `key` is the shared i18n key; both catalogs' labels are
/// indexed regardless of the active locale (upstream: current locale +
/// English aliases).
pub(crate) struct SearchEntry {
    pub section: SettingsSection,
    /// The mounted row this entry highlights after navigating (the General
    /// pane's rows and the rail's section rows honor it today; section
    /// pages adopt their own anchors as they grow fixed rows).
    pub anchor: &'static str,
    pub key: &'static str,
}

/// The index (upstream ships ~46 rows; ours is the 12 section entries, the
/// General pane's six rows, and the section pages' fixed rows — dynamic
/// resources stay out by rule).
pub(crate) fn entries() -> Vec<SearchEntry> {
    use ConfigPageKind as P;
    use SettingsSection as S;
    let section = |s: SettingsSection, anchor: &'static str, key: &'static str| SearchEntry {
        section: s,
        anchor,
        key,
    };
    vec![
        section(S::General, "settings-nav-general", "settings.general"),
        section(S::Models, "settings-nav-models", "zconfig.models"),
        section(S::Page(P::Skills), "settings-nav-skills", "zconfig.skills"),
        section(S::Page(P::Mcp), "settings-nav-mcp", "zconfig.mcp"),
        section(S::Page(P::Hooks), "settings-nav-hooks", "zconfig.hooks"),
        section(S::Page(P::Agents), "settings-nav-agents", "zconfig.agents"),
        section(S::Workflows, "settings-nav-workflows", "zconfig.entryWorkflows"),
        section(S::Usage, "settings-nav-usage", "zconfig.usage"),
        section(S::Quota, "settings-nav-quota", "zconfig.entryQuota"),
        section(S::System, "settings-nav-system", "system.title"),
        section(S::Page(P::Backups), "settings-nav-backups", "zconfig.backups"),
        section(S::Page(P::AppUpdate), "settings-nav-app-update", "zconfig.appUpdate"),
        // The General pane's own rows.
        section(S::General, "settings-row-servers", "connect.savedServers"),
        section(S::General, "settings-row-language", "panel.language"),
        section(S::General, "settings-row-font", "panel.fontSize"),
        section(S::General, "settings-row-osc52", "panel.clipboardRead"),
        section(S::General, "settings-row-upgrade", "panel.upgrade"),
        section(S::General, "settings-row-gate", "zconfig.workflowGate"),
        // The section pages' fixed rows (dynamic resources stay out).
        section(S::Quota, "settings-row-reset-cards", "zconfig.resetCards"),
        section(S::Page(P::AppUpdate), "settings-row-channel", "zconfig.updateChannel"),
        section(S::Page(P::Backups), "settings-row-restore", "zconfig.restore"),
        section(S::Usage, "settings-row-by-model", "zconfig.usageByModel"),
        section(S::Usage, "settings-row-by-day", "zconfig.usageByDay"),
        section(S::Page(P::Mcp), "settings-row-mcp-add", "zconfig.mcpAdd"),
    ]
}

/// Lowercase + drop all whitespace (label-side and query-side alike).
fn normalize(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).flat_map(|c| c.to_lowercase()).collect()
}

/// (full pinyin, initials) of a NORMALIZED label: hanzi contribute their
/// toneless pinyin / first letter, every other character itself — so a
/// mixed label ("OSC 52") still matches through both surfaces.
fn pinyin_forms(label: &str) -> (String, String) {
    let mut full = String::new();
    let mut initials = String::new();
    for c in label.chars() {
        match c.to_pinyin() {
            Some(py) => {
                full.push_str(py.plain());
                initials.push_str(py.first_letter());
            }
            None => {
                let c = c.to_lowercase().next().unwrap_or(c);
                full.push(c);
                initials.push(c);
            }
        }
    }
    (full, initials)
}

/// Every query character appears in target, in order (the sequential
/// fallback — "mxe" finds "models example").
fn sequential(query: &str, target: &str) -> bool {
    let mut rest = target;
    for qc in query.chars() {
        match rest.find(qc) {
            Some(at) => rest = &rest[at + qc.len_utf8()..],
            None => return false,
        }
    }
    true
}

/// One entry's derived match surfaces, built per search (two dozen short
/// labels — re-deriving beats a cache).
struct Surfaces {
    zh: String,
    en: String,
    pinyin: String,
    initials: String,
}

fn surfaces(key: &str) -> Surfaces {
    let zh = t!(key, locale = "zh-CN").to_string();
    let en = t!(key, locale = "en").to_string();
    let zh = normalize(&zh);
    let (pinyin, initials) = pinyin_forms(&zh);
    Surfaces {
        zh,
        en: normalize(&en),
        pinyin,
        initials,
    }
}

/// The entries matching a query, index order (stable, section-first).
pub(crate) fn search(query: &str) -> Vec<SearchEntry> {
    let q = normalize(query);
    if q.is_empty() {
        return entries();
    }
    entries()
        .into_iter()
        .filter(|e| {
            let s = surfaces(e.key);
            // Substring on either locale's label, or either pinyin surface.
            s.zh.contains(&q)
                || s.en.contains(&q)
                || s.pinyin.contains(&q)
                || s.initials.contains(&q)
                // Sequential characters on either label (not on pinyin —
                // "moxng" is a typo, not a strategy).
                || sequential(&q, &s.zh)
                || sequential(&q, &s.en)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    // Explicit imports, NOT `use super::*`: the module-level kit glob
    // re-exports GPUI's `test` macro, which would shadow Rust's built-in
    // #[test] (the kit's own doc note).
    use super::{normalize, pinyin_forms, search, sequential};
    use crate::settings_overlay::SettingsSection;

    #[test]
    fn normalization_drops_case_and_whitespace() {
        assert_eq!(normalize("  Mo dels "), "models");
        assert_eq!(normalize("套餐 配额"), "套餐配额");
    }

    #[test]
    fn pinyin_forms_cover_full_and_initials() {
        let (full, initials) = pinyin_forms("模型");
        assert_eq!(full, "moxing");
        assert_eq!(initials, "mx");
        // Non-hanzi characters ride along on both surfaces.
        let (full, initials) = pinyin_forms("osc52");
        assert_eq!(full, "osc52");
        assert_eq!(initials, "osc52");
    }

    #[test]
    fn sequential_matches_in_order_substrings() {
        assert!(sequential("mdl", "models"));
        assert!(!sequential("mdx", "models"));
        assert!(!sequential("lmd", "models"));
    }

    #[test]
    fn search_matches_english_substring_pinyin_and_initials() {
        // English substring.
        assert!(search("model").iter().any(|e| e.key == "zconfig.models"));
        // Chinese label substring.
        assert!(search("模型").iter().any(|e| e.key == "zconfig.models"));
        // Full pinyin (acceptance scenario 3: "moxing").
        assert!(search("moxing").iter().any(|e| e.key == "zconfig.models"));
        // Pinyin initials ("mx").
        assert!(search("mx").iter().any(|e| e.key == "zconfig.models"));
        // The zh label also matches pinyin of its own (the alias direction).
        assert!(search("quota").iter().any(|e| e.key == "zconfig.entryQuota"));
        assert!(search("taocan").iter().any(|e| e.key == "zconfig.entryQuota"));
    }

    #[test]
    fn empty_query_lists_every_entry() {
        assert_eq!(search("").len(), super::entries().len());
    }

    #[test]
    fn every_entry_key_resolves_in_both_catalogs() {
        for e in super::entries() {
            let zh = rust_i18n::t!(e.key, locale = "zh-CN").to_string();
            let en = rust_i18n::t!(e.key, locale = "en").to_string();
            assert_ne!(zh, e.key, "zh-CN resolves for {}", e.key);
            assert_ne!(en, e.key, "en resolves for {}", e.key);
            let _ = e.section; // sections stay valid
            assert!(matches!(e.section, SettingsSection::Models | SettingsSection::General | SettingsSection::Quota | SettingsSection::Usage | SettingsSection::System | SettingsSection::Workflows | SettingsSection::Page(_)));
        }
    }
}
