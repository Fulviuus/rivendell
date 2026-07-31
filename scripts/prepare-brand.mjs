/**
 * Turns the chroma-keyed source art into the assets the app and README use.
 *
 *   node scripts/prepare-brand.mjs ~/Downloads/rivendell-icon.png \
 *                                  ~/Downloads/rivendell-logo.png \
 *                                  ~/Downloads/rivendell-logo-darkscreens.png
 *
 * The sources are rendered on a solid green screen. Keying that out needs two
 * steps, not one: cut the background to transparent, then *despill* — the green
 * that bled into the light edges of the artwork. Without the despill the marks
 * keep a green halo that is obvious against a white background.
 *
 * Outputs (committed, so a build never needs the sources):
 *   src-tauri/icons/…        app icon, via `tauri icon`
 *   assets/logo.png          for light backgrounds
 *   assets/logo-dark.png     for dark backgrounds
 *   assets/icon.png          square mark
 */
import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";

const [iconSrc, logoSrc, logoDarkSrc] = process.argv.slice(2);
if (!iconSrc || !logoSrc || !logoDarkSrc) {
  console.error("usage: prepare-brand.mjs <icon.png> <logo.png> <logo-dark.png>");
  process.exit(2);
}

const run = (cmd, args) => execFileSync(cmd, args, { stdio: ["ignore", "pipe", "inherit"] });

/**
 * Key out the green screen and remove the spill it left in the light edges.
 *
 * Keying on *distance to the chroma colour* does not work here. The background
 * has a gradient, so the fuzz has to be generous — and at any fuzz generous
 * enough to catch all of it, the teal speech bubble starts getting eaten. A
 * connected flood fill avoids that but cannot reach the enclosed counters
 * inside letters like R, e and d, which survive as green blobs.
 *
 * Greenness — how far green runs ahead of *both* red and blue — separates them
 * cleanly, and being a global test it keys the counters too:
 *
 *   chroma  (26,241,30)   g - max(r,b) = 211   -> background
 *   teal    (45,180,170)                  10   -> keep
 *   foliage (120,160,90)                  40   -> keep
 *   arch    (20,70,40)                    30   -> keep
 *
 * The ramp gives anti-aliased edges a partial alpha rather than a hard cut.
 */
function keySimple(src, out) {
  const GREENNESS_ALPHA = "1 - min(1, max(0, ((u.g - max(u.r,u.b)) - 0.25) / 0.20))";
  // Pull green back toward the brighter of red/blue wherever it overshoots,
  // which clears the fringe left along light edges.
  const DESPILL_GREEN = "min(u.g, max(u.r,u.b) + 0.10)";

  run("magick", [
    src,
    "-alpha", "set",
    "-channel", "A", "-fx", GREENNESS_ALPHA, "+channel",
    "-channel", "G", "-fx", DESPILL_GREEN, "+channel",
    "-trim", "+repage",
    out,
  ]);
}

mkdirSync("assets", { recursive: true });

console.log("keying icon…");
keySimple(iconSrc, "assets/icon.png");

console.log("keying logo (light backgrounds)…");
keySimple(logoSrc, "assets/logo.png");

console.log("keying logo (dark backgrounds)…");
keySimple(logoDarkSrc, "assets/logo-dark.png");

// Small copies for the sidebar: the full-size art is ~1900px wide and the
// app draws it 20px tall, so shipping the original would be pure weight.
console.log("sizing web copies…");
mkdirSync("src/assets", { recursive: true });
for (const [from, to] of [
  ["assets/logo.png", "src/assets/logo.png"],
  ["assets/logo-dark.png", "src/assets/logo-dark.png"],
]) {
  run("magick", [from, "-resize", "x64", "-strip", to]);
}

// A square, padded source is what `tauri icon` wants.
console.log("building app icon source…");
run("magick", [
  "assets/icon.png",
  "-resize", "1024x1024",
  "-background", "none",
  "-gravity", "center",
  "-extent", "1024x1024",
  "app-icon.png",
]);

console.log("generating platform icons…");
execFileSync("npx", ["tauri", "icon", "app-icon.png"], { stdio: "inherit" });

for (const f of ["assets/icon.png", "assets/logo.png", "assets/logo-dark.png"]) {
  const size = run("magick", ["identify", "-format", "%wx%h opaque=%[opaque]", f]).toString();
  console.log(`  ${f.padEnd(24)} ${size}`);
}
console.log("\ndone");
