#!/usr/bin/env node
// Pixel-diff one golden-screenshot capture against its committed baseline
// (the acceptance script's stage 6). Inputs are plain PNGs from the app's
// self-capture (or the `screencapture` fallback) over the pinned fixture
// window; the diff is pixelmatch with a tolerant per-pixel threshold plus
// a global differing-pixel-ratio budget (AA noise stays far below it).
//
// Exit codes: 0 = ok / baseline generated; 1 = real pixel DIFF (or size
// drift); 3 = UNUSABLE capture — the acceptance script maps that to a
// SKIPPED stage with instructions (an environment precondition, not a
// regression): a fully transparent capture is the Metal-readback case, an
// overly bright one is the wallpaper (screencapture without Screen
// Recording permission strips the windows out).
//
// A missing baseline is GENERATED (with a loud note) — first run on a new
// machine bootstraps; every later run diffs. --update (GOLDEN_UPDATE=1 in
// acceptance.sh) rebaselines deliberately.
//
// Usage: node scripts/golden-diff.cjs --baseline scripts/golden/chat.png \
//   --capture /tmp/golden-chat.png [--diff /tmp/golden-chat-diff.png] \
//   [--update] [--max-ratio 0.005]

"use strict";

const fs = require("fs");
const path = require("path");
const { PNG } = require("pngjs");
// pixelmatch 8 ships as pure ESM: under require() the function sits behind
// `.default` (the plain export is the {__esModule} namespace).
const pixelmatchFn = require("pixelmatch");
const pixelmatch = typeof pixelmatchFn === "function" ? pixelmatchFn : pixelmatchFn.default;

function arg(name, fallback) {
  const i = process.argv.indexOf(`--${name}`);
  return i === -1 ? fallback : process.argv[i + 1];
}

const baselinePath = arg("baseline");
const capturePath = arg("capture");
const diffPath = arg("diff");
const update = process.argv.includes("--update");
const maxRatio = parseFloat(arg("max-ratio", "0.005"));

if (!baselinePath || !capturePath) {
  console.error("usage: golden-diff.cjs --baseline <png> --capture <png> [--update]");
  process.exit(2);
}

const capture = PNG.sync.read(fs.readFileSync(capturePath));

// Mean luminance + opaque fraction over a 1/16 sample — the sanity guards.
let sum = 0;
let opaque = 0;
let n = 0;
for (let i = 0; i < capture.data.length; i += 4 * 16) {
  const r = capture.data[i] / 255;
  const g = capture.data[i + 1] / 255;
  const b = capture.data[i + 2] / 255;
  sum += 0.2126 * r + 0.7152 * g + 0.0722 * b;
  if (capture.data[i + 3] !== 0) opaque += 1;
  n += 1;
}
const mean = n ? sum / n : 1;
const opaqueRatio = n ? opaque / n : 0;
if (opaqueRatio < 0.99) {
  console.error(
    `capture is ${(100 * (1 - opaqueRatio)).toFixed(1)}% transparent — the Metal surface ` +
      "refused readback (self-capture) and the fallback did not produce a window either",
  );
  process.exit(3);
}
if (mean > 0.25) {
  console.error(
    `capture mean luminance ${mean.toFixed(3)} is too bright for the quiet-dark canvas — ` +
      "this looks like the wallpaper: grant Screen Recording permission to the terminal " +
      "and re-run",
  );
  process.exit(3);
}

if (update || !fs.existsSync(baselinePath)) {
  if (!fs.existsSync(baselinePath)) {
    console.log(`note: no baseline at ${baselinePath} — generating (first run on this machine)`);
  }
  fs.mkdirSync(path.dirname(baselinePath), { recursive: true });
  fs.writeFileSync(baselinePath, PNG.sync.write(capture));
  console.log(`baseline written: ${baselinePath} (${capture.width}x${capture.height})`);
  process.exit(0);
}

const baseline = PNG.sync.read(fs.readFileSync(baselinePath));
if (baseline.width !== capture.width || baseline.height !== capture.height) {
  console.error(
    `size drift: baseline ${baseline.width}x${baseline.height} vs capture ` +
      `${capture.width}x${capture.height} — screen scale or window frame changed; ` +
      "re-baseline with GOLDEN_UPDATE=1",
  );
  process.exit(1);
}

const diff = diffPath ? new PNG({ width: capture.width, height: capture.height }) : null;
const differing = pixelmatch(
  baseline.data,
  capture.data,
  diff ? diff.data : null,
  capture.width,
  capture.height,
  // Default includeAA (false) on purpose: anti-aliased edges are the noise
  // floor between identical runs; only real pixel changes should count.
  { threshold: 0.1 },
);
const ratio = differing / (capture.width * capture.height);
if (diffPath) fs.writeFileSync(diffPath, PNG.sync.write(diff));

if (ratio > maxRatio) {
  console.error(
    `DIFF: ${(100 * ratio).toFixed(3)}% pixels differ (budget ${(100 * maxRatio).toFixed(1)}%)` +
      (diffPath ? ` — diff image: ${diffPath}` : ""),
  );
  process.exit(1);
}
console.log(`ok: ${(100 * ratio).toFixed(4)}% pixels differ (${differing} px)`);
