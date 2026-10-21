# Desktop Workbench (gpui client)

Cargo workspace for the ZCode ACP desktop client, the desktop product line on
gpui-kit — not a port of the mobile UI (see
`docs/adr/0009-desktop-client-as-gpui-product-line.md`). It is split in two
layers with a one-way dependency edge: `protocol` is the pure-logic library
(hub WebSocket client, Settings API client, ACP message types, replay and
earlier-page pagination, reconnect state machine, session/pending-queue/cache
semantics) and is forbidden from taking any UI dependency — its deps stop at
serde / serde_json / tokio / tokio-tungstenite / thiserror / anyhow plus the
pure-Rust attachment codecs (`image` for the ADR 0007 downsampling port of
`src/lib/image.ts`, `base64` for the ACP image-block payload); the TS vitest
suites are ported there as Rust unit tests to serve as the equivalence
anchor; `app` is the thin gpui window layer on `gpui-kit` that
mirrors appStore slices as entities and binds views to protocol events. The
gpui distribution itself is pinned by `gpui-kit`, never added directly; the
app's only further dependencies are the workspace pins `tokio` and
`serde_json` — it owns the real `HttpTransport` the protocol crate
deliberately leaves to its caller (protocol's `hub_client` module doc) plus
the tokio driver thread that runs the protocol futures, and builds the few
request envelopes protocol does not yet expose helpers for — plus the i18n
pair `rust-i18n` + `sys-locale` (both already in the tree via gpui-kit's own
dependencies): the app compiles the shared mobile/web catalogs
(`src/i18n/*.json`, requirements §2 — same JSON, no fork) in at build time
and picks `en` / `zh-CN` from the system locale.
Bundle the macOS app with `scripts/build-macos-gpui.sh` (unsigned, into
`dist/ZCode-acp.app` — the desktop product line's bundle name, no desktop
suffix).
Authoritative requirements: `.zcode/docs/desktop-client-requirements.md`
(scope) and `.zcode/docs/desktop-parity-requirements.md` (feature parity).
