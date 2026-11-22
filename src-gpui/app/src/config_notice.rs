//! The config surfaces' notice model (ADR-0014 §2): typed, localized,
//! lifecycle-bound. Every settings outcome reaches the banner as a
//! (kind, i18n key, optional verbatim detail) triple — English failure
//! prose stops at the runtime boundary, and the kind decides how the
//! banner lives and dies: Saved auto-expires on the driver tick, Error
//! waits for the dismiss click, NeedsRestart stays while its flag does.

use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{IconName, Sizable as _, Theme};
use gpui_kit::*;
use rust_i18n::t;

use crate::runtime::{ProtocolRuntime, UiCommand};
use crate::ui_prefs::ui_text;

/// How a notice renders and expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    /// A write landed — confirmed with a short "Saved", then gone.
    Saved,
    /// A failure — stays until dismissed or replaced.
    Error,
    /// A needs-restart write is armed — stays while `pending_restart` is.
    NeedsRestart,
}

/// One config-surface notice.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigNotice {
    /// Monotonic id: the auto-expire deadline names its notice, so a late
    /// deadline never clears a newer banner (the terminal-bell text
    /// guard, id-shaped).
    pub id: u64,
    pub kind: NoticeKind,
    /// `notice.*` i18n key, resolved through the shared catalog.
    pub key: &'static str,
    /// Verbatim second line (the server's refusal body) — localized text
    /// first, raw detail under it, never mixed.
    pub detail: Option<String>,
}

/// A failure's classified (key, detail) pair. Network / auth / server
/// classes get their own keys; a refused write surfaces the server's own
/// message as the detail line under the localized text. `load` only picks
/// between the write and load phrasings of the generic refusal.
pub fn classify_failure(
    err: &protocol::settings_client::HubApiError,
    load: bool,
) -> (&'static str, Option<String>) {
    use protocol::settings_client::HubApiError;
    match err {
        HubApiError::Network(_) => ("notice.errorNetwork", Some(err.to_string())),
        HubApiError::Unauthorized => ("notice.errorAuth", None),
        HubApiError::Http { status, message } if *status >= 500 => {
            ("notice.errorServer", Some(message.clone()))
        }
        // A 4xx refusal is the route talking: its body is the reason the
        // user can act on, so it rides whole as the detail.
        HubApiError::Http { message, .. } => {
            let key = if load { "notice.loadFailed" } else { "notice.writeFailed" };
            (key, Some(message.clone()))
        }
        HubApiError::Unexpected(_) => {
            let key = if load { "notice.loadFailed" } else { "notice.writeFailed" };
            (key, Some(err.to_string()))
        }
    }
}

/// Resolve a `notice.*` key through the shared catalog; anything else
/// passes through (t! falls back to its input).
pub(crate) fn localized_text(key: &str) -> String {
    t!(key).into_owned()
}

/// The hub client's struct-shaped error (the backend-restart route is its
/// one config-relevant caller), mapped onto the same classification.
pub fn classify_hub_failure(
    err: &protocol::hub_client::HubApiError,
) -> (&'static str, Option<String>) {
    if err.network {
        ("notice.errorNetwork", Some(err.message.clone()))
    } else {
        match err.status {
            Some(401) => ("notice.errorAuth", None),
            Some(status) if status >= 500 => ("notice.errorServer", Some(err.message.clone())),
            _ => ("notice.writeFailed", Some(err.message.clone())),
        }
    }
}

/// The one banner every config surface paints (previously six per-page
/// copies). Kind picks the tint — Saved reads green, Error red, the
/// needs-restart pair stays neutral (the restart card below it carries
/// the action) — and the dismiss click covers every kind: even an
/// auto-expiring Saved may outstay a fast reader.
pub fn notice_banner(
    theme: &Theme,
    notice: &ConfigNotice,
    runtime: &ProtocolRuntime,
    id: impl Into<gpui::SharedString>,
) -> AnyElement {
    let runtime = runtime.clone();
    let id: gpui::SharedString = id.into();
    let (bg, fg) = match notice.kind {
        NoticeKind::Saved => (theme.success.opacity(0.10), theme.success_foreground),
        NoticeKind::Error => (theme.danger.opacity(0.10), theme.danger_foreground),
        NoticeKind::NeedsRestart => (theme.secondary, theme.foreground),
    };
    let mut text = div()
        .min_w_0()
        .flex_1()
        .text_size(ui_text(12.))
        .text_color(fg)
        .child(localized_text(notice.key));
    if let Some(detail) = &notice.detail {
        text = text.child(
            div()
                .pt_1()
                .text_size(ui_text(10.))
                .text_color(theme.muted_foreground)
                .child(detail.clone()),
        );
    }
    div()
        .flex()
        .items_center()
        .gap_2()
        .mx_3()
        .mt_2()
        .px_3()
        .py_2()
        .flex_none()
        .rounded_md()
        .bg(bg)
        .child(text)
        .child(
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(IconName::CircleX)
                .tooltip(t!("common.dismiss"))
                .on_click(move |_, _, _| runtime.send(UiCommand::DismissConfigNotice)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    // Explicit imports, NOT `use super::*`: the module-level kit glob
    // re-exports GPUI's `test` macro, which would shadow Rust's built-in
    // #[test] (the kit's own doc note).
    use super::{classify_failure, classify_hub_failure, localized_text};
    use protocol::settings_client::HubApiError;

    #[test]
    fn notice_keys_resolve_through_the_shared_catalog() {
        assert_eq!(localized_text("notice.saved"), "Saved");
        assert_eq!(localized_text("plain failure text"), "plain failure text");
    }

    #[test]
    fn hub_failure_classification_reads_the_struct_fields() {
        let network = protocol::hub_client::HubApiError {
            message: "dial failed".into(),
            status: None,
            network: true,
            instance_dead: false,
        };
        assert_eq!(classify_hub_failure(&network).0, "notice.errorNetwork");

        let unauthorized = protocol::hub_client::HubApiError {
            message: "bad token".into(),
            status: Some(401),
            network: false,
            instance_dead: false,
        };
        assert_eq!(classify_hub_failure(&unauthorized).0, "notice.errorAuth");
    }

    #[test]
    fn failure_classification_picks_the_key_by_kind() {
        let (key, detail) = classify_failure(&HubApiError::Network("boom".into()), false);
        assert_eq!(key, "notice.errorNetwork");
        assert!(detail.is_some());

        let (key, detail) = classify_failure(&HubApiError::Unauthorized, true);
        assert_eq!(key, "notice.errorAuth");
        assert_eq!(detail, None);

        let (key, detail) = classify_failure(
            &HubApiError::Http { status: 500, message: "upstream died".into() },
            false,
        );
        assert_eq!(key, "notice.errorServer");
        assert_eq!(detail.as_deref(), Some("upstream died"));

        // A 4xx refusal keeps the action phrasing and rides the body whole.
        let (key, detail) = classify_failure(
            &HubApiError::Http { status: 409, message: "nonce is stale".into() },
            false,
        );
        assert_eq!(key, "notice.writeFailed");
        assert_eq!(detail.as_deref(), Some("nonce is stale"));

        let (key, _) = classify_failure(
            &HubApiError::Http { status: 404, message: "gone".into() },
            true,
        );
        assert_eq!(key, "notice.loadFailed");
    }
}
