// Generates a 1024x1024 source PNG for `npm run tauri icon`.
// Zero deps: hand-rolled PNG encoder on top of node:zlib.
import { deflateSync } from "node:zlib";
import { writeFileSync, mkdirSync } from "node:fs";
import { dirname } from "node:path";

const S = 1024;

const crcTable = (() => {
  const t = new Int32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c;
  }
  return t;
})();

function crc32(buf) {
  let c = -1;
  for (let i = 0; i < buf.length; i++) c = crcTable[(c ^ buf[i]) & 0xff] ^ (c >>> 8);
  return (c ^ -1) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([len, body, crc]);
}

function png(width, height, rgba) {
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // colour type RGBA
  const raw = Buffer.alloc((width * 4 + 1) * height);
  for (let y = 0; y < height; y++) {
    raw[y * (width * 4 + 1)] = 0; // filter: none
    rgba.copy(raw, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4);
  }
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk("IHDR", ihdr),
    chunk("IDAT", deflateSync(raw, { level: 9 })),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

// --- the mark: three converging arcs (a council) on a deep indigo squircle ---
const buf = Buffer.alloc(S * S * 4);
const cx = S / 2;
const cy = S / 2;

function squircle(x, y) {
  // superellipse |x|^n + |y|^n = r^n, n=4 gives the macOS-ish rounded square
  const dx = Math.abs(x - cx) / (S * 0.46);
  const dy = Math.abs(y - cy) / (S * 0.46);
  return Math.pow(dx, 4) + Math.pow(dy, 4) <= 1;
}

const nodes = [];
for (let i = 0; i < 3; i++) {
  const a = -Math.PI / 2 + (i * 2 * Math.PI) / 3;
  nodes.push([cx + Math.cos(a) * S * 0.2, cy + Math.sin(a) * S * 0.2]);
}

function distToSegment(px, py, [ax, ay], [bx, by]) {
  const vx = bx - ax;
  const vy = by - ay;
  const wx = px - ax;
  const wy = py - ay;
  const t = Math.max(0, Math.min(1, (wx * vx + wy * vy) / (vx * vx + vy * vy)));
  return Math.hypot(px - (ax + t * vx), py - (ay + t * vy));
}

for (let y = 0; y < S; y++) {
  for (let x = 0; x < S; x++) {
    const o = (y * S + x) * 4;
    if (!squircle(x, y)) continue;

    // background: vertical gradient, slate-indigo
    const g = y / S;
    let r = Math.round(30 + g * 18);
    let gg = Math.round(32 + g * 20);
    let b = Math.round(58 + g * 40);

    // edges of the triangle, drawn as glowing links
    let link = 0;
    for (let i = 0; i < 3; i++) {
      const d = distToSegment(x, y, nodes[i], nodes[(i + 1) % 3]);
      link = Math.max(link, Math.max(0, 1 - Math.max(0, d - 5) / 10));
    }
    if (link > 0) {
      r = Math.round(r + link * (110 - r));
      gg = Math.round(gg + link * (125 - gg));
      b = Math.round(b + link * (215 - b));
    }

    // the three nodes themselves
    for (let i = 0; i < 3; i++) {
      const d = Math.hypot(x - nodes[i][0], y - nodes[i][1]);
      const a = Math.max(0, 1 - Math.max(0, d - 52) / 8);
      if (a > 0) {
        const tint = i === 0 ? [186, 200, 255] : [126, 142, 232];
        r = Math.round(r + a * (tint[0] - r));
        gg = Math.round(gg + a * (tint[1] - gg));
        b = Math.round(b + a * (tint[2] - b));
      }
    }

    buf[o] = r;
    buf[o + 1] = gg;
    buf[o + 2] = b;
    buf[o + 3] = 255;
  }
}

const out = process.argv[2] ?? "app-icon.png";
mkdirSync(dirname(out), { recursive: true });
writeFileSync(out, png(S, S, buf));
console.log(`wrote ${out}`);
