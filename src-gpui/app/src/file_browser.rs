//! Session Files (batch F, CONTEXT.md "Session Files" / "File Viewer"):
//! the read-only, hub-proxied view of the ACTIVE session's files.
//!
//! Port of `src/components/FileBrowser.tsx` + the TextViewer half of
//! `src/components/FileViewer.tsx`, at workbench density:
//! - breadcrumb navigation over single-level listings (`fs/list`), dotfiles
//!   hidden by default with a client-side toggle;
//! - "Show in Finder" for the current directory — meaningful only next to a
//!   loopback hub, where the bridge's paths name real local files;
//! - a simplified read-only TEXT viewer: one whole-file load (`fs/file`,
//!   single line window at the mobile MAX_LINES budget), max-h scroll.
//!   Line-window paging, image preview, and the download fallback are
//!   later milestones (milestone 2), so those kinds explain themselves.
//!
//! All wire I/O lives in `runtime` (the driver's FilesPanel); this module
//! only holds the pure classification/formatting helpers and the painters.

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, App, Entity, div, px, SharedString};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::{ActiveTheme as _, Icon, Sizable as _, Theme};
// The kit glob also brings gpui's self-alias `gpui` into scope — the
// established import shape (workbench.rs, system_status.rs).
use gpui_kit::{assets::IconName as KitIconName, *};
// Plain divs register for headless tests through `.test_support()` (kit
// Buttons self-register); see gpui-kit-notes.md's test section.
use gpui_kit::TestSupportExt as _;

use crate::runtime::{FileViewerView, FilesPanelView, ProtocolRuntime, UiCommand};
use crate::workbench::Workbench;
use protocol::hub::{FsEntry, FsEntryKind};

/// The whole-viewer line budget (the TS viewer's MAX_LINES cap). The load
/// stitches server windows until this many lines or the file ends.
pub(crate) const VIEWER_LINE_BUDGET: u64 = 10_000;
/// The bridge silently clamps a `/fs/file` line window to its own
/// MAX_LINE_LIMIT (zcode-acp-server src/remote/file-endpoint.ts) — asking
/// for more returns exactly this many lines, never an error, so the budget
/// above is filled by consecutive windows of at most this size.
pub(crate) const SERVER_LINE_WINDOW: u64 = 5_000;
/// A display row longer than this is chopped (the TS TextViewer's
/// MAX_LINE_CHARS): a single mega-line must not lay out unbounded.
const MAX_LINE_CHARS: usize = 2_000;

/// The window to request for a load starting at `start` (1-based): the
/// server cap, or whatever is left of the whole-viewer budget.
pub(crate) fn viewer_window(start: u64) -> u64 {
    let budget_left = VIEWER_LINE_BUDGET.saturating_sub(start - 1);
    budget_left.min(SERVER_LINE_WINDOW)
}

/// After a window starting at `first_line` returned `received` lines, the
/// start of the next window — `None` when the file ended or the budget is
/// full. A full window cannot tell "file ends here" from "window cut it",
/// so an exactly-window-sized file costs one extra probe that comes back
/// empty (the TS viewer's windows probe the same way).
pub(crate) fn viewer_next_window(first_line: u64, received: u64) -> Option<u64> {
    if received == 0 || received < viewer_window(first_line) {
        return None;
    }
    let next = first_line.checked_add(received)?;
    (next <= VIEWER_LINE_BUDGET).then_some(next)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    Image,
    Text,
    Binary,
}

/// The TS `fileKindOf` ladder (FileViewer.tsx): extension decides; the
/// extension-less fallback list covers the known-text names.
pub fn file_kind_of(name: &str) -> FileKind {
    const IMAGE_EXT: &[&str] = &[
        "png", "jpg", "jpeg", "gif", "webp", "svg", "bmp", "ico", "avif",
    ];
    const TEXT_EXT: &[&str] = &[
        "txt", "md", "mdx", "markdown", "json", "js", "mjs", "cjs", "ts", "tsx", "jsx", "css",
        "scss", "sass", "less", "html", "htm", "xml", "yml", "yaml", "toml", "ini", "cfg", "conf",
        "properties", "env", "log", "sh", "bash", "zsh", "fish", "py", "rb", "go", "rs", "java",
        "kt", "kts", "swift", "c", "h", "cpp", "hpp", "cc", "cs", "php", "sql", "r", "pl", "lua",
        "dart", "scala", "proto", "graphql", "gql", "lock", "diff", "patch", "gitignore",
        "gitattributes", "npmrc", "editorconfig",
    ];
    const TEXT_NAMES: &[&str] = &[
        "dockerfile", "makefile", "rakefile", "license", "readme", "changelog", "notice",
    ];
    let lower = name.to_lowercase();
    let ext = lower.rsplit('.').next().filter(|_| lower.contains('.'));
    if let Some(ext) = ext {
        if IMAGE_EXT.contains(&ext) {
            return FileKind::Image;
        }
        if TEXT_EXT.contains(&ext) {
            return FileKind::Text;
        }
    } else if TEXT_NAMES.contains(&lower.as_str()) {
        return FileKind::Text;
    }
    FileKind::Binary
}

/// The TS `fmtSize` (FileViewer.tsx): `128 B`, `1.5 KB`, `2.0 MB`.
pub fn fmt_size(bytes: u64) -> String {
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    if bytes < 1024 * 1024 {
        return format!("{:.1} KB", bytes as f64 / 1024.0);
    }
    format!("{:.1} MB", bytes as f64 / 1024.0 / 1024.0)
}

/// The TS `joinPath` (FileBrowser.tsx): a relative path segment joins with
/// `/`; the root's empty path stays bare (`"" + "/" + name` trimmed).
pub fn join_dir(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_string()
    } else {
        format!("{dir}/{name}")
    }
}

/// The TS `isLoopbackHub` (lib/localOpen.ts): a loopback hub runs on THIS
/// machine, so bridge-reported paths name real local files. Manual host
/// parse — the URL crate is not a dependency here, and the hub URLs are
/// always `http://host:port`.
pub fn is_loopback_hub(hub_url: &str) -> bool {
    let after_scheme = hub_url
        .split_once("://")
        .map(|(_, rest)| rest)
        .unwrap_or(hub_url);
    let host = after_scheme
        .split(['/', '?'])
        .next()
        .unwrap_or_default()
        .rsplit_once(':') // strip the port; IPv6 hosts keep their brackets
        .map(|(host, _)| host)
        .unwrap_or(after_scheme.split(['/', '?']).next().unwrap_or_default())
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_lowercase();
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// F2: reveal one of the hub machine's directories in the platform file
/// manager (`open` on macOS, `xdg-open` elsewhere) — the desktop equivalent
/// of the TS `openLocalPath(abs, false)` invoke. Spawned detached; a
/// non-zero exit is not waited for.
pub fn open_in_file_manager(abs: &str) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    std::process::Command::new(program)
        .arg(abs)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{program}: {e}"))
}

/// Open one local FILE with the OS default application (the mobile
/// viewer-header affordance, `open_local_path` reveal=false), or reveal it
/// in the file manager (`open -R` on macOS). argv only, never a shell.
pub fn open_local_path(abs: &str, reveal: bool) -> Result<(), String> {
    let program = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
    let mut cmd = std::process::Command::new(program);
    if reveal && cfg!(target_os = "macos") {
        cmd.arg("-R");
    }
    cmd.arg(abs)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("{program}: {e}"))
}

/// The absolute machine path of one browsed directory — the listing's root
/// plus the relative path (TS FileBrowser `openCurrentDir`).
pub fn absolute_dir(root: &str, path: &str) -> String {
    let root = root.trim_end_matches('/');
    if path.is_empty() {
        root.to_string()
    } else {
        format!("{root}/{path}")
    }
}

/// The session root's display name for the breadcrumb's first crumb (TS
/// `rootName`): the root's last path segment, `"/"` for the filesystem root.
pub fn root_name(root: &str) -> String {
    root.trim_end_matches('/')
        .rsplit('/')
        .next()
        .filter(|seg| !seg.is_empty())
        .unwrap_or("/")
        .to_string()
}

/// One display row of the viewer body: `first_line + index` numbered,
/// chopped at MAX_LINE_CHARS for layout only (the TS truncatedLines rule —
/// line numbers still count the original lines).
fn display_line(line: &str) -> String {
    if line.chars().count() > MAX_LINE_CHARS {
        let mut head: String = line.chars().take(MAX_LINE_CHARS).collect();
        head.push_str(" …");
        head
    } else if line.is_empty() {
        // Keep empty rows one line tall.
        " ".to_string()
    } else {
        line.to_string()
    }
}

// ---------------------------------------------------------------------------
// Panel + viewer painters (called from workbench.rs)
// ---------------------------------------------------------------------------

/// Context the panel painter needs beyond the driver's view data.
pub(crate) struct FilesPanelCtx<'a> {
    pub runtime: ProtocolRuntime,
    /// The workbench entity — the hidden-files toggle is view-local state
    /// (the TS FileBrowser's `showHidden` useState).
    pub wb: Entity<Workbench>,
    pub show_hidden: bool,
    /// The dial address; the Finder affordance exists only on a loopback hub.
    pub hub_addr: Option<&'a str>,
    /// The active tab's bridge advertises the fs capability.
    pub fs_capable: bool,
}

/// The docked session-files panel: header (hidden toggle / refresh / Finder),
/// breadcrumb, and one directory level of rows.
pub(crate) fn render_files_panel(view: &FilesPanelView, ctx: &FilesPanelCtx, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let runtime = ctx.runtime.clone();

    let mut column = div().flex().flex_col().gap_2().p_2();

    // Header: title + the three affordances.
    let wb_toggle = ctx.wb.clone();
    let show_hidden = ctx.show_hidden;
    let current_path = view.path.clone();
    let refresh_runtime = runtime.clone();
    column = column.child(
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_size(px(11.))
                    .font_family(theme.mono_font_family.clone())
                    .text_color(theme.muted_foreground)
                    .child("SESSION FILES"),
            )
            .child(div().flex_1())
            .child(
                Button::new("files-hidden-toggle")
                    .ghost()
                    .xsmall()
                    .icon(if show_hidden {
                        KitIconName::Eye
                    } else {
                        KitIconName::EyeOff
                    })
                    .tooltip(if show_hidden {
                        "Hide dotfiles"
                    } else {
                        "Show dotfiles"
                    })
                    .on_click(move |_, _, cx| {
                        wb_toggle.update(cx, |wb, cx| wb.toggle_files_hidden(cx));
                    }),
            )
            .child(
                Button::new("files-refresh")
                    .ghost()
                    .xsmall()
                    .icon(KitIconName::RefreshCw)
                    .tooltip("Re-read this directory")
                    .on_click(move |_, _, _| {
                        refresh_runtime.send(UiCommand::FilesNavigate {
                            path: current_path.clone(),
                        })
                    }),
            ),
    );

    // The Finder affordance (F2): only where the hub is loopback AND a
    // listing names a real local directory (the TS `canLocal` pair).
    let can_local = ctx
        .hub_addr
        .is_some_and(|addr| is_loopback_hub(addr))
        && view.listing.is_some();
    if can_local {
        let wb_open = ctx.wb.clone();
        let root = view
            .listing
            .as_ref()
            .map(|l| l.root.clone())
            .unwrap_or_default();
        let path = view.path.clone();
        column = column.child(
            div().child(
                Button::new("files-open-finder")
                    .ghost()
                    .xsmall()
                    .icon(KitIconName::FolderOpen)
                    .label("Show in Finder")
                    .tooltip("Open the current directory in the platform file manager")
                    .on_click(move |_, _, cx| {
                        let abs = absolute_dir(&root, &path);
                        if let Err(e) = open_in_file_manager(&abs) {
                            wb_open.update(cx, |wb, cx| wb.files_notice(&e, cx));
                        }
                    }),
            ),
        );
    }

    // Breadcrumb over the relative path (the TS nav): the root crumb resets;
    // each segment crumb navigates to its prefix.
    let root_label = view
        .listing
        .as_ref()
        .map(|l| root_name(&l.root))
        .unwrap_or_else(|| "root".to_string());
    let segments: Vec<&str> = view.path.split('/').filter(|s| !s.is_empty()).collect();
    let mut crumbs = div().flex().items_center().gap(px(2.)).min_w_0();
    {
        let runtime = runtime.clone();
        crumbs = crumbs.child(
            div()
                .id("files-crumb-root")
                .test_support()
                .flex_none()
                .text_size(px(11.))
                .text_color(theme.foreground)
                .cursor_pointer()
                .child(root_label)
                .on_click(move |_, _, _| {
                    runtime.send(UiCommand::FilesNavigate {
                        path: String::new(),
                    })
                }),
        );
    }
    for (i, seg) in segments.iter().enumerate() {
        let runtime = runtime.clone();
        let target = segments[..=i].join("/");
        crumbs = crumbs
            .child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("/"),
            )
            .child(
                div()
                    .id(SharedString::from(format!("files-crumb-{i}")))
                    .test_support()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(if i + 1 == segments.len() {
                        theme.foreground
                    } else {
                        theme.muted_foreground
                    })
                    .cursor_pointer()
                    .child(seg.to_string())
                    .on_click(move |_, _, _| {
                        runtime.send(UiCommand::FilesNavigate { path: target.clone() })
                    }),
            );
    }
    column = column.child(
        div()
            .min_w_0()
            .overflow_x_hidden()
            .flex()
            .items_center()
            .child(crumbs),
    );

    // Body ladder (the TS render ladder): capability note / loading /
    // error+retry / rows / empty.
    let mut body = div().flex().flex_col().gap_1();
    if !ctx.fs_capable {
        body = body.child(
            div()
                .id("files-unsupported")
                .test_support()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(
                    "this bridge does not advertise session files — bridge 0.7.0+ required",
                ),
        );
    } else if view.loading {
        body = body.child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child("reading…"),
        );
    } else if let Some(err) = &view.error {
        let runtime = runtime.clone();
        let path = view.path.clone();
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.danger)
                        .child(err.clone()),
                )
                .child(
                    Button::new("files-retry")
                        .secondary()
                        .xsmall()
                        .label("Retry")
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::FilesNavigate { path: path.clone() })
                        }),
                ),
        );
    } else if let Some(listing) = &view.listing {
        let entries: Vec<&FsEntry> = listing
            .entries
            .iter()
            .filter(|e| ctx.show_hidden || !e.name.starts_with('.'))
            .collect();
        for entry in &entries {
            body = body.child(render_entry_row(&view.path, entry, &runtime, &theme));
        }
        if listing.truncated {
            body = body.child(
                div()
                    .id("files-truncated")
                    .test_support()
                    .text_size(px(10.))
                    .text_color(theme.warning_foreground)
                    .child("list truncated — enter a subdirectory to keep walking"),
            );
        }
        if entries.is_empty() {
            body = body.child(
                div()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("empty directory"),
            );
        }
    }

    div()
        .id("files-panel")
        .test_support()
        .h_full()
        .w_full()
        .min_w_0()
        .flex_none()
        .flex()
        .flex_col()
        .border_r_1()
        .border_color(theme.border)
        .bg(theme.title_bar)
        .child(column)
        .child(
            div()
                .id("files-body")
                .flex()
                .flex_col()
                .flex_1()
                .min_h_0()
                // Rows truncate INSIDE: x must clip here or wide names push
                // the panel past the resizable split into the chat area.
                .overflow_y_scroll()
                .overflow_x_hidden()
                .child(body),
        )
        .into_any_element()
}

fn render_entry_row(
    dir: &str,
    entry: &FsEntry,
    runtime: &ProtocolRuntime,
    theme: &Theme,
) -> AnyElement {
    let is_dir = entry.kind == FsEntryKind::Dir;
    let icon = match entry.kind {
        FsEntryKind::Dir => Icon::new(KitIconName::Folder).text_color(theme.accent),
        FsEntryKind::Symlink => Icon::new(KitIconName::Link2).text_color(theme.muted_foreground),
        FsEntryKind::File => Icon::new(KitIconName::FileText).text_color(theme.muted_foreground),
    }
    .size_4()
    .flex_none();
    let target = join_dir(dir, &entry.name);
    let runtime = runtime.clone();
    div()
        .id(SharedString::from(format!("files-entry-{}", entry.name)))
        .test_support()
        .flex()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .rounded_md()
        .hover(|h| h.bg(theme.secondary.opacity(0.6)))
        .cursor_pointer()
        .child(icon)
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(12.))
                .text_color(theme.foreground)
                .child(entry.name.clone()),
        )
        .when(!is_dir, |this| {
            this.child(
                div()
                    .flex_none()
                    .text_size(px(10.))
                    .text_color(theme.muted_foreground)
                    .child(fmt_size(entry.size)),
            )
        })
        .when(is_dir, |this| {
            this.child(
                div()
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme.muted_foreground)
                    .child("▸"),
            )
        })
        .on_click(move |_, _, _| {
            // The TS row split: directories navigate, everything else opens
            // the viewer (symlinks included).
            if is_dir {
                runtime.send(UiCommand::FilesNavigate { path: target.clone() });
            } else {
                runtime.send(UiCommand::FilesView { path: target.clone() });
            }
        })
        .into_any_element()
}

/// The simplified file viewer (F3): a full-workbench overlay — read-only,
/// whole-file load, scrolling body. Text loads whole (F3); the image kind
/// loads its raster via the hub's token-query URL (batch K) and enlarges
/// into the lightbox on click; binary explains its deferral instead of
/// fetching.
pub(crate) fn render_file_viewer(
    view: &FileViewerView,
    runtime: &ProtocolRuntime,
    wb: &gpui::WeakEntity<crate::workbench::Workbench>,
    local_abs: Option<String>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let mut column = div().flex().flex_col().h_full();

    // Header: close, name, the size meta (TS viewer header), then the two
    // local affordances the mobile viewer carries (FileViewer.tsx:679-696):
    // open with the OS default application + reveal in the file manager —
    // loopback hubs only (the file must live on this machine).
    let mut header = div()
        .flex()
        .items_center()
        .gap_2()
        .p_2()
        .border_b_1()
        .border_color(theme.border)
        .child(
            Button::new("file-viewer-close")
                .ghost()
                .xsmall()
                .icon(KitIconName::X)
                .tooltip("Back to the file list")
                .on_click({
                    let runtime = runtime.clone();
                    move |_, _, _| runtime.send(UiCommand::FilesCloseView)
                }),
        )
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_size(px(13.))
                .text_color(theme.foreground)
                .child(view.name.clone()),
        )
        .child(
            div()
                .id("file-viewer-meta")
                .test_support()
                .flex_none()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child(format!("{} · {}", fmt_size(view.size), kind_label(view.kind))),
        );
    if let Some(abs) = local_abs {
        for (id, icon, tooltip, reveal) in [
            (
                "file-viewer-open",
                KitIconName::ExternalLink,
                "Open with the default application",
                false,
            ),
            (
                "file-viewer-reveal",
                KitIconName::FolderOpen,
                "Show in Finder",
                true,
            ),
        ] {
            let wb_open = wb.clone();
            let abs = abs.clone();
            header = header.child(
                Button::new(id)
                    .ghost()
                    .xsmall()
                    .icon(icon)
                    .tooltip(tooltip)
                    .on_click(move |_, _, cx| {
                        if let Err(e) = open_local_path(&abs, reveal) {
                            wb_open.update(cx, |wb, cx| wb.files_notice(&e, cx));
                        }
                    }),
            );
        }
    }
    column = column.child(header);

    // Body ladder: loading / error+retry / deferred kinds / text rows.
    let mut body = div().flex().flex_col().gap(px(2.)).p_2();
    if view.loading {
        body = body.child(
            div()
                .text_size(px(11.))
                .text_color(theme.muted_foreground)
                .child("reading…"),
        );
    } else if let Some(err) = &view.error {
        let runtime = runtime.clone();
        let path = view.path.clone();
        body = body.child(
            div()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.danger)
                        .child(err.clone()),
                )
                .child(
                    Button::new("file-viewer-retry")
                        .secondary()
                        .xsmall()
                        .label("Retry")
                        .on_click(move |_, _, _| {
                            runtime.send(UiCommand::FilesView { path: path.clone() })
                        }),
                ),
        );
    } else {
        match view.kind {
            FileKind::Image => {
                // Batch K: the whole-file fetch through the token-query URL.
                // A transport failure (or an unrasterizable payload) already
                // landed in `view.error` — the shared error ladder above
                // paints it; this arm only owns the success and the brief
                // loading window.
                if view.loading {
                    body = body.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("loading preview…"),
                    );
                } else if let Some(src) = &view.image {
                    let open_src = src.clone();
                    let caption = view.path.clone();
                    let wb = wb.clone();
                    body = body.child(
                        div()
                            .id("file-viewer-image")
                            .test_support()
                            .flex()
                            .justify_center()
                            .p_2()
                            .on_click(move |_, _, cx| {
                                let _ = wb.update(cx, |this, cx| {
                                    this.open_lightbox(open_src.clone(), Some(caption.clone()), cx)
                                });
                            })
                            .child(
                                gpui_kit::img(src.clone())
                                    .max_w(px(640.))
                                    .max_h(px(420.))
                                    .object_fit(gpui::ObjectFit::Contain),
                            ),
                    );
                } else {
                    body = body.child(
                        div()
                            .text_size(px(11.))
                            .text_color(theme.muted_foreground)
                            .child("no preview available"),
                    );
                }
            }
            FileKind::Binary => {
                body = body.child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme.muted_foreground)
                        .child("binary file — the download fallback lands in a later milestone"),
                );
            }
            FileKind::Text => {
                if let Some(text) = &view.text {
                    let first = view.first_line.unwrap_or(1);
                    for (i, line) in text.split('\n').enumerate() {
                        body = body.child(
                            div()
                                .id(SharedString::from(format!(
                                    "file-viewer-line-{}",
                                    first.saturating_add(i as u64)
                                )))
                                .test_support()
                                .flex()
                                .items_start()
                                .gap_2()
                                .child(
                                    div()
                                        .flex_none()
                                        .w(px(44.))
                                        .text_right()
                                        .text_size(px(10.))
                                        .text_color(theme.muted_foreground)
                                        .child(format!("{}", first.saturating_add(i as u64))),
                                )
                                .child(
                                    div()
                                        .min_w_0()
                                        .text_size(px(11.))
                                        .font_family(theme.mono_font_family.clone())
                                        .text_color(theme.foreground)
                                        .child(display_line(line)),
                                ),
                        );
                    }
                }
            }
        }
    }

    // Footer (the TS viewer's footer line): line count, or the capped note
    // when the whole-file budget cut the tail off.
    let mut footer_text: Option<String> = None;
    if view.kind == FileKind::Text && !view.loading && view.error.is_none() {
        if let Some(text) = &view.text {
            let lines = text.split('\n').count() as u64;
            footer_text = Some(if lines >= VIEWER_LINE_BUDGET {
                format!("first {VIEWER_LINE_BUDGET} lines — file truncated")
            } else {
                format!("{lines} lines")
            });
        }
    }
    column = column.child(
        div()
            .id("file-viewer-body")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            // Long display rows wrap (no horizontal pan): the number gutter
            // stays put and wide content can never push the overlay wide.
            .overflow_y_scroll()
            .overflow_x_hidden()
            .child(body),
    );
    if let Some(footer) = footer_text {
        column = column.child(
            div()
                .id("file-viewer-foot")
                .test_support()
                .flex_none()
                .px_2()
                .py_1()
                .border_t_1()
                .border_color(theme.border)
                .text_size(px(10.))
                .text_color(theme.muted_foreground)
                .child(footer),
        );
    }

    div()
        .id("file-viewer")
        .test_support()
        .absolute()
        .inset_0()
        .flex()
        .flex_col()
        .bg(theme.background)
        .child(column)
        .into_any_element()
}

fn kind_label(kind: FileKind) -> &'static str {
    match kind {
        FileKind::Image => "image",
        FileKind::Text => "text",
        FileKind::Binary => "binary",
    }
}

#[cfg(test)]
mod tests {
    // Explicit imports only — the file-root kit glob would shadow the
    // built-in #[test] with gpui's macro (see gpui-kit-notes.md).
    use super::{
        absolute_dir, display_line, file_kind_of, fmt_size, is_loopback_hub, join_dir, root_name,
        viewer_next_window, viewer_window, FileKind, SERVER_LINE_WINDOW, VIEWER_LINE_BUDGET,
    };

    #[test]
    fn kind_ladder_matches_the_ts_viewer() {
        assert_eq!(file_kind_of("logo.PNG"), FileKind::Image);
        assert_eq!(file_kind_of("photo.jpeg"), FileKind::Image);
        assert_eq!(file_kind_of("main.rs"), FileKind::Text);
        assert_eq!(file_kind_of(".gitignore"), FileKind::Text);
        assert_eq!(file_kind_of("Dockerfile"), FileKind::Text);
        assert_eq!(file_kind_of("Makefile"), FileKind::Text);
        assert_eq!(file_kind_of("archive.tar.gz"), FileKind::Binary);
        assert_eq!(file_kind_of("app.dmg"), FileKind::Binary);
        assert_eq!(file_kind_of("unknown"), FileKind::Binary);
    }

    #[test]
    fn size_formatting_matches_the_ts_viewer() {
        assert_eq!(fmt_size(128), "128 B");
        assert_eq!(fmt_size(2_048), "2.0 KB");
        assert_eq!(fmt_size(1_500_000), "1.4 MB");
    }

    #[test]
    fn path_helpers_join_strip_and_name() {
        assert_eq!(join_dir("", "src"), "src");
        assert_eq!(join_dir("src", "main.rs"), "src/main.rs");
        assert_eq!(absolute_dir("/work/proj/", "src"), "/work/proj/src");
        assert_eq!(absolute_dir("/work/proj", ""), "/work/proj");
        assert_eq!(root_name("/work/proj"), "proj");
        assert_eq!(root_name("/"), "/");
    }

    #[test]
    fn loopback_ladder_takes_host_only() {
        assert!(is_loopback_hub("http://127.0.0.1:8377"));
        assert!(is_loopback_hub("http://localhost:8377"));
        assert!(is_loopback_hub("http://[::1]:8377"));
        assert!(!is_loopback_hub("http://192.168.1.4:8377"));
        assert!(!is_loopback_hub("https://hub.example.com:8377"));
        assert!(!is_loopback_hub("not a url"));
    }

    #[test]
    fn display_lines_pad_empty_and_chop_mega_lines() {
        assert_eq!(display_line(""), " ");
        let mega = "x".repeat(3_000);
        let shown = display_line(&mega);
        assert_eq!(shown.chars().count(), 2_002); // 2000 chars + " …"
    }

    #[test]
    fn viewer_windows_never_exceed_the_server_cap() {
        assert_eq!(viewer_window(1), SERVER_LINE_WINDOW);
        // The tail window shrinks to the budget remainder, never past it.
        assert_eq!(viewer_window(9_001), 1_000);
        assert_eq!(viewer_window(VIEWER_LINE_BUDGET), 1);
    }

    #[test]
    fn stitch_probes_only_while_a_full_window_left_budget() {
        // Full first window: the 10k budget needs the second half.
        assert_eq!(viewer_next_window(1, SERVER_LINE_WINDOW), Some(5_001));
        // Full second window: the budget is exhausted.
        assert_eq!(
            viewer_next_window(5_001, SERVER_LINE_WINDOW),
            None
        );
        // A partial window means the file ended mid-window.
        assert_eq!(viewer_next_window(1, SERVER_LINE_WINDOW - 1), None);
        // An exactly-cap-sized file costs one empty probe, which ends the
        // stitch (received == 0), not another request.
        assert_eq!(viewer_next_window(5_001, 0), None);
    }
}
