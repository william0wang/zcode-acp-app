# Pixel-golden baselines

Committed by the first `bash scripts/acceptance.sh` run on each machine
(or `GOLDEN_UPDATE=1` to re-baseline deliberately). One PNG per fixture —
`chat`, `workflows`, `settings` — rendered by the pinned fixture window
(`src-gpui/app/src/golden_fixtures.rs`, 1200×800 at (120,120)) and captured
either by the app's self-capture or the `screencapture -R` fallback.

Two machine-specific inputs make baselines non-portable:

- the display's backing scale (retina captures are 2×),
- the OS version's text rasterization.

Re-baseline with `GOLDEN_UPDATE=1 bash scripts/acceptance.sh` after a macOS
upgrade or an intended visual change; review the diff images in the stage
output before trusting the new baselines.

If the stage reports `golden=skip`: the Metal surface refused self-capture
readback AND `screencapture` produced nothing — grant Screen Recording to
the terminal running the script (System Settings → Privacy & Security →
Screen Recording) and re-run; the fallback then turns the stage into a real
gate.
