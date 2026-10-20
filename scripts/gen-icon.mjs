// Renders the app icon (SVG -> 1024px PNG) for `tauri icon` to expand into
// the full set. Uses sharp (ADR 0004) — no hand-rolled image encoding.
// Design: dark rounded tile, white terminal chevron, amber cursor block.
//
// macOS extra: `icon-macos.icns` is generated here too, from a PADDED source
// (the tile at Apple's icon grid inset, ~824/1024 with transparent margins).
// `tauri icon` produces one full-bleed set for every platform — correct for
// iOS/Android (the system masks them) but wrong for macOS, where the icns
// must carry its own shape AND margins or the icon renders a ring larger
// than every neighbor in the Dock.
import { execFile } from "node:child_process";
import { mkdir, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import path from "node:path";
import { promisify } from "node:util";
import sharp from "sharp";

const exec = promisify(execFile);

// The full-bleed design (iOS/Android + the tauri icon set).
const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <rect width="1024" height="1024" rx="228" fill="#0d0d0f"/>
  <rect x="56" y="56" width="912" height="912" rx="180" fill="#1e1e22"/>
  <path d="M 330 318 L 566 512 L 330 706"
        stroke="#ececf1" stroke-width="118"
        stroke-linecap="round" stroke-linejoin="round" fill="none"/>
  <rect x="628" y="596" width="98" height="114" rx="22" fill="#fbbf24"/>
</svg>`;

// The same artwork scaled onto Apple's macOS icon grid: an 824px tile
// centered on a transparent 1024px canvas (~10% margin per side — measured
// against system icons, whose opaque art occupies ~83% of the canvas).
const TILE = 824;
const OFF = (1024 - TILE) / 2;
const S = TILE / 1024;
const at = (v) => Math.round(v * S + OFF);
const sz = (v) => Math.round(v * S);
const svgMacos = `<svg xmlns="http://www.w3.org/2000/svg" width="1024" height="1024" viewBox="0 0 1024 1024">
  <rect x="${OFF}" y="${OFF}" width="${TILE}" height="${TILE}" rx="${sz(228)}" fill="#0d0d0f"/>
  <rect x="${at(56)}" y="${at(56)}" width="${sz(912)}" height="${sz(912)}" rx="${sz(180)}" fill="#1e1e22"/>
  <path d="M ${at(330)} ${at(318)} L ${at(566)} ${at(512)} L ${at(330)} ${at(706)}"
        stroke="#ececf1" stroke-width="${sz(118)}"
        stroke-linecap="round" stroke-linejoin="round" fill="none"/>
  <rect x="${at(628)}" y="${at(596)}" width="${sz(98)}" height="${sz(114)}" rx="${sz(22)}" fill="#fbbf24"/>
</svg>`;

await sharp(Buffer.from(svg)).resize(1024, 1024).png().toFile("src-tauri/icons/app-icon.png");
console.log("wrote src-tauri/icons/app-icon.png");

await sharp(Buffer.from(svgMacos)).resize(1024, 1024).png().toFile("src-tauri/icons/app-icon-macos.png");
console.log("wrote src-tauri/icons/app-icon-macos.png");

// icns via iconutil (macOS only): resize the padded source into a proper
// iconset, then convert. Non-macOS environments keep the committed icns.
if (process.platform === "darwin") {
  // iconutil requires the directory name to end in `.iconset`.
  const dir = path.join(tmpdir(), `zc-iconset-${process.pid}.iconset`);
  await rm(dir, { recursive: true, force: true });
  try {
    await mkdir(dir, { recursive: true });
    const src = path.resolve("src-tauri/icons/app-icon-macos.png");
    const sizes = [16, 32, 128, 256, 512];
    for (const s of sizes) {
      await exec("sips", ["-z", String(s), String(s), src, "--out", path.join(dir, `icon_${s}x${s}.png`)]);
      await exec("sips", [
        "-z", String(s * 2), String(s * 2), src, "--out", path.join(dir, `icon_${s}x${s}@2x.png`),
      ]);
    }
    await exec("iconutil", [
      "-c", "icns", dir, "-o", "src-tauri/icons/icon-macos.icns",
    ]);
    console.log("wrote src-tauri/icons/icon-macos.icns");
  } finally {
    await rm(dir, { recursive: true, force: true });
  }
} else {
  console.log("skipped icon-macos.icns (iconutil is macOS-only)");
}
