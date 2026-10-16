# Desktop client as a separate gpui-kit product line

The WebView shell (Tauri 2 + React) has hit its performance ceiling on
desktop: even with memoization, identity caches, and virtualization in place,
streaming chat rendering stays CPU-heavy and the WKWebView process group
keeps memory high. We decided to build the desktop client as a **separate
product line** in `src-gpui/` on gpui-kit (native Rust UI), not a port of the
mobile UI — a dock-style workbench with its own desktop-grade information
density. The mobile and web clients stay on React/Tauri in low-intensity
maintenance; the app name is unchanged. This deliberately scopes ADR-0004's
off-the-shelf-UI-primitives-first rule to the mobile/web clients only.

## Considered Options

- Keep optimizing React in the WebView — rejected: the remaining cost is
  structural (WKWebView + React runtime), not fixable in app code.
- egui / Slint (the only frameworks covering desktop+Android+Web) — rejected:
  Android/Web stay on the existing React app anyway, so cross-platform reach
  buys nothing here and gpui-kit is the strongest desktop-only stack
  (production-proven by Longbridge Pro; TextView/MessageScroller cover the
  chat core).
- Dioxus — rejected: its desktop renderer is still a WebView, which does not
  address the motivation.
