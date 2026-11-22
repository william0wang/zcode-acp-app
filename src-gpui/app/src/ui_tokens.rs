//! App-owned design tokens (requirements §2, batch 2): the semantic
//! vocabulary the kit's `ThemeColor` does not carry — status text colors,
//! the diff pair, the categorical chart palette, the faint text tier — plus
//! the spacing ladder and the text-size roles.
//!
//! Mirrors the TS side (src/index.css `@theme`): both ends name the SAME
//! palette so a screenshot reads as one product. Colors derive from the
//! theme where a value must track it (faint follows `muted_foreground`);
//! the rest are the fixed dark-only values, single-theme by product
//! decision (ADR-0013 — not a theming seam).

use gpui_kit::gpui::{Hsla, rgb};
use gpui_kit::component::Theme;

// -- color ----------------------------------------------------------------

/// TS text-faint — one step dimmer than text-dim (`muted_foreground`).
/// The former per-file `faint()` helpers (quota_page / system_status).
pub(crate) fn faint(theme: &Theme) -> Hsla {
    theme.muted_foreground.opacity(0.75)
}

/// Status TEXT colors: the value half of the TS status pairs, on the dark
/// canvas. `*_wash` backgrounds stay `Hsla::opacity` derivations at the
/// call sites that need a tinted fill.
pub(crate) fn success_text() -> Hsla {
    rgb(0x34d399).into() // emerald-400
}
pub(crate) fn warning_text() -> Hsla {
    rgb(0xfbbf24).into() // amber-400
}
pub(crate) fn danger_text() -> Hsla {
    rgb(0xf87171).into() // red-400
}
pub(crate) fn info_text() -> Hsla {
    rgb(0x38bdf8).into() // sky-400
}

/// Status wash: the 12% tint the TS chips/banners paint (bg-*-wash).
pub(crate) fn wash(color: Hsla) -> Hsla {
    color.opacity(0.12)
}

// -- interactive overlays ----------------------------------------------------
//
// The ghost-button idiom both ends share (TS --color-hover/active/selected):
// white overlays on any surface, NOT a second surface color. The former
// per-file idioms (`theme.secondary.opacity(0.5/0.6)`, `white().opacity(
// 0.05/0.06/0.12)`) unify onto these three — one hover looks like every
// other hover, on canvas, surface or raised alike.

/// Hover wash: 6% white (TS bg-hover).
pub(crate) fn hover_bg() -> Hsla {
    let c: Hsla = rgb(0xffffff).into();
    c.opacity(0.06)
}
/// Pressed wash: 11% white (TS bg-active).
pub(crate) fn active_bg() -> Hsla {
    let c: Hsla = rgb(0xffffff).into();
    c.opacity(0.11)
}
/// Selected row fill: a steady 8% white (TS bg-selected).
pub(crate) fn selected_bg() -> Hsla {
    let c: Hsla = rgb(0xffffff).into();
    c.opacity(0.08)
}

/// Diff line colors (workbench diff cards): text + row background. The
/// former literals at workbench.rs:680-681, named once.
pub(crate) struct DiffColors {
    pub add_text: Hsla,
    pub add_bg: Hsla,
    pub del_text: Hsla,
    pub del_bg: Hsla,
    pub trunc_bg: Hsla,
}

pub(crate) fn diff_colors() -> DiffColors {
    DiffColors {
        add_text: rgb(0x6ee7b7).into(), // emerald-300
        add_bg: rgb(0x0f2a1e).into(),
        del_text: rgb(0xfca5a5).into(), // red-300
        del_bg: rgb(0x33141a).into(),
        trunc_bg: rgb(0x2a1d08).into(),
    }
}

/// The categorical chart palette (TS --color-chart-1..6): workflow artifact
/// series and usage graphs. Order-stable — series N always gets color N on
/// both ends.
pub(crate) fn chart_color(index: usize) -> Hsla {
    const CHART: [u32; 6] = [
        0x818cf8, // indigo-400  (chart-1)
        0x34d399, // emerald-400 (chart-2)
        0xfbbf24, // amber-400   (chart-3)
        0xf472b6, // pink-400    (chart-4)
        0x38bdf8, // sky-400     (chart-5)
        0xa78bfa, // violet-400  (chart-6)
    ];
    rgb(CHART[index % CHART.len()]).into()
}

/// Run-status dot colors (workflows_page `status_dot`): success emerald
/// (one step above the accent so it stays visible against it), failure red,
/// live sky, pending/fallback neutral grays.
pub(crate) fn run_status_color(status: &str) -> Hsla {
    match status {
        "completed" | "ok" | "settled" => rgb(0x10b981).into(),
        "errored" | "failed" => rgb(0xf87171).into(),
        "running" | "dispatched" | "executing" => rgb(0x38bdf8).into(),
        "pending" | "queued" | "waiting" => rgb(0x6b7280).into(),
        _ => rgb(0x9ca3af).into(),
    }
}

/// MCP server health dot colors (zconfig MCP rows, `mcp/list` mode:"status"
/// vocabulary): connected emerald, failed red, untrusted amber, connecting
/// sky, and the neutral grays for disabled/disconnected/unknown.
pub(crate) fn mcp_health_color(status: &str) -> Hsla {
    match status {
        "connected" => rgb(0x10b981).into(),
        "failed" => rgb(0xf87171).into(),
        "untrusted" => rgb(0xfbbf24).into(),
        "connecting" => rgb(0x38bdf8).into(),
        _ => rgb(0x6b7280).into(),
    }
}

/// The eight agent swatch colors the server's agent form offers
/// (zconfig_pages `swatch_fill`); the TS picker paints the same tailwind
/// values.
pub(crate) fn agent_swatch(name: &str) -> Hsla {
    match name {
        "red" => rgb(0xef4444).into(),
        "blue" => rgb(0x3b82f6).into(),
        "green" => rgb(0x22c55e).into(),
        "yellow" => rgb(0xfacc15).into(),
        "purple" => rgb(0xa855f7).into(),
        "orange" => rgb(0xf97316).into(),
        "pink" => rgb(0xec4899).into(),
        _ => rgb(0x06b6d4).into(), // cyan
    }
}

/// Update-stage text colors (zconfig app-update card): done emerald,
/// needs-user amber, failed red — the same families as the status trio.
pub(crate) fn update_stage_text(stage: &str) -> Hsla {
    match stage {
        "done" => success_text(),
        "needs-user-install" => warning_text(),
        "failed" => danger_text(),
        _ => info_text(),
    }
}

// -- spacing ladder ---------------------------------------------------------

/// The spacing ladder (requirements 2.3): the ONLY spacing values new
/// layout code may use — the TS/Tailwind scale restated. `layout_lint.rs`
/// fails on spacing calls (`p*`/`m*`/`gap*`) outside it. 1px stays legal
/// for hairline offsets; element SIZES (`size(px(N))`, bars, dots) and
/// canvas geometry are not spacing and are out of the lint's scope. Two
/// alignment-derived indents (`pl(38)` op-tile spine = tile 28 + gap 10,
/// `pl(46)` chart labels = y-axis col 40 + gap 6) carry a documented
/// allow-list in the lint.
pub(crate) const SPACING_LADDER_PX: [f32; 14] = [
    1., 2., 3., 4., 6., 8., 10., 12., 14., 16., 20., 24., 32., 48.,
];

// -- text roles -------------------------------------------------------------

/// The text-size roles (requirements 2.5): the whitelist `ui_text` calls
/// must name. Caption = dense metadata (10), body = labels and prose
/// (12), emphasized body (13), title = page/dialog headers (14).
/// The ladder's small/medium/large steps scale every role uniformly.
pub(crate) mod text_role {
    pub(crate) const CAPTION: f32 = 10.;
    pub(crate) const BODY: f32 = 12.;
    pub(crate) const BODY_STRONG: f32 = 13.;
    pub(crate) const TITLE: f32 = 14.;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chart_palette_is_stable_and_cycling() {
        assert_eq!(chart_color(0), chart_color(0));
        assert_ne!(chart_color(0), chart_color(1));
        assert_eq!(chart_color(6), chart_color(0)); // cycles past the end
    }

    #[test]
    fn run_status_families() {
        assert_eq!(run_status_color("completed"), run_status_color("ok"));
        assert_eq!(run_status_color("failed"), run_status_color("errored"));
        assert_eq!(run_status_color("unknown-token"), run_status_color("x"));
    }

    #[test]
    fn mcp_health_families() {
        assert_eq!(mcp_health_color("connected"), mcp_health_color("connected"));
        assert_ne!(mcp_health_color("connected"), mcp_health_color("failed"));
        // disabled / disconnected / an unknown spelling share the neutral dot.
        assert_eq!(mcp_health_color("disabled"), mcp_health_color("disconnected"));
        assert_eq!(mcp_health_color("disabled"), mcp_health_color("never-heard-of"));
    }

    #[test]
    fn agent_swatches_cover_the_form_names() {
        // "cyan" IS the fallback arm (the server's own form lists it last);
        // the other seven must each name a distinct color.
        for name in ["red", "blue", "green", "yellow", "purple", "orange", "pink"] {
            assert_ne!(agent_swatch(name), agent_swatch("__never__"));
        }
        assert_eq!(agent_swatch("cyan"), agent_swatch("__never__"));
    }

    #[test]
    fn spacing_ladder_matches_the_ts_scale() {
        for v in SPACING_LADDER_PX {
            assert!(v >= 1., "ladder value {v} below hairline");
        }
        assert!(SPACING_LADDER_PX.contains(&8.));
        assert!(SPACING_LADDER_PX.contains(&16.));
        assert!(!SPACING_LADDER_PX.contains(&5.), "5px is off-ladder: use 4");
    }

    #[test]
    fn text_roles_are_the_whitelist() {
        // The lint fails on any ui_text() outside these four roles.
        assert_eq!(
            [
                text_role::CAPTION,
                text_role::BODY,
                text_role::BODY_STRONG,
                text_role::TITLE
            ],
            [10., 12., 13., 14.]
        );
    }
}
