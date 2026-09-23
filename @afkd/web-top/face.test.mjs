// The cell face's own suite: `node --test @afkd/web-top/face.test.mjs`.
//
// afkd top draws its symbols in the terminal's own monospace face. A browser draws them in
// whatever face the visitor happens to have them in, and a fallback face sizes and places a
// shape for text rather than for a cell: one visitor's `▷` was a full em wide, another's `▯` a
// sliver. So the page carries its own face for them, cut out of DejaVu Sans Mono and embedded
// in `dashboard.css` by `tools/@afkd/web-top/cell-face.py`. This suite holds that embedded
// face to the layout it serves, by reading the font itself rather than the script's word.

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";
import { inflateSync } from "node:zlib";

import { textWidth } from "./layout.mjs";

const HERE = import.meta.dirname;
const CSS = readFileSync(join(HERE, "dashboard.css"), "utf8");
const FAMILY = "afkd cells";

/// The symbols the layout draws that no DejaVu face has, each on a visitor's fallback face and
/// centred on its cell by the painter instead.
const UNCOVERED = new Map([[0x23f1, "⏱ STOPWATCH: in none of DejaVu's faces"]]);

// --- reading the stylesheet ----------------------------------------------------------

/// `U+2190-2193, U+21BB` as the code points it names.
function rangeOf(spec) {
  const points = new Set();
  for (const part of spec.split(",")) {
    const [from, to = from] = part.trim().replace(/^U\+/i, "").split("-");
    for (let cp = parseInt(from, 16); cp <= parseInt(to, 16); cp += 1) points.add(cp);
  }
  return points;
}

/// Every `@font-face` the stylesheet declares for the cell face: its weight, its font file and
/// the code points its `unicode-range` claims. None at all fails here rather than letting a
/// test's loop pass over nothing.
function faces() {
  const found = [...CSS.matchAll(/@font-face \{([^}]*)\}/g)]
    .map(([, body]) => body)
    .filter((body) => body.includes(`font-family: "${FAMILY}"`))
    .map((body) => ({
      weight: body.match(/font-weight: (\d+);/)?.[1],
      file: Buffer.from(body.match(/src: url\(data:font\/woff;base64,([A-Za-z0-9+/=]+)\) format\("woff"\);/)?.[1] ?? "", "base64"),
      range: rangeOf(body.match(/unicode-range: ([^;]+);/)?.[1] ?? ""),
    }));
  assert.ok(found.length > 0, `dashboard.css embeds no "${FAMILY}" face; run tools/@afkd/web-top/cell-face.py`);
  return found;
}

// --- reading the font -----------------------------------------------------------------

/// A WOFF's tables by tag, each inflated back to its sfnt bytes.
function tablesOf(woff) {
  assert.equal(woff.toString("latin1", 0, 4), "wOFF", "the embedded file is a WOFF");
  const tables = new Map();
  for (let at = 44, n = 0; n < woff.readUInt16BE(12); n += 1, at += 20) {
    const [offset, compressed, length] = [woff.readUInt32BE(at + 4), woff.readUInt32BE(at + 8), woff.readUInt32BE(at + 12)];
    const data = woff.subarray(offset, offset + compressed);
    tables.set(woff.toString("latin1", at, at + 4), compressed < length ? inflateSync(data) : data);
  }
  return tables;
}

/// The code points a `cmap` maps to a real glyph, off its Windows Unicode subtable — format 4,
/// the only one a face this size needs.
function mappedBy(cmap) {
  const points = new Set();
  for (let at = 4, n = 0; n < cmap.readUInt16BE(2); n += 1, at += 8) {
    if (cmap.readUInt16BE(at) !== 3 || cmap.readUInt16BE(at + 2) !== 1) continue;
    const sub = cmap.readUInt32BE(at + 4);
    assert.equal(cmap.readUInt16BE(sub), 4, "the Unicode subtable is format 4");
    const segs = cmap.readUInt16BE(sub + 6) / 2;
    const ends = sub + 14;
    const [starts, deltas, offsets] = [ends + 2 * segs + 2, ends + 4 * segs + 2, ends + 6 * segs + 2];
    for (let s = 0; s < segs; s += 1) {
      const [start, end] = [cmap.readUInt16BE(starts + 2 * s), cmap.readUInt16BE(ends + 2 * s)];
      const [delta, rangeOffset] = [cmap.readInt16BE(deltas + 2 * s), cmap.readUInt16BE(offsets + 2 * s)];
      for (let cp = start; cp <= end && cp !== 0xffff; cp += 1) {
        const glyph = rangeOffset === 0 ? cp + delta : cmap.readUInt16BE(offsets + 2 * s + rangeOffset + 2 * (cp - start));
        if ((glyph & 0xffff) !== 0) points.add(cp);
      }
    }
  }
  return points;
}

/// A `name` table's Windows records as `nameID → text`.
function namesOf(name) {
  const names = new Map();
  const strings = name.readUInt16BE(4);
  for (let at = 6, n = 0; n < name.readUInt16BE(2); n += 1, at += 12) {
    if (name.readUInt16BE(at) !== 3) continue;
    const [id, length, offset] = [name.readUInt16BE(at + 6), name.readUInt16BE(at + 8), name.readUInt16BE(at + 10)];
    // Copied before the swap, which is in place: two records may share one string's bytes.
    names.set(id, Buffer.from(name.subarray(strings + offset, strings + offset + length)).swap16().toString("utf16le"));
  }
  return names;
}

/// Every symbol the layout can draw: each one-cell code point past the Latin punctuation in
/// the two modules that spell the screen, which is what a terminal would draw in its own face.
function vocabulary() {
  const points = new Set();
  for (const file of ["layout.mjs", "keymap.mjs"]) {
    for (const ch of readFileSync(join(HERE, file), "utf8")) {
      const cp = ch.codePointAt(0);
      if (cp >= 0x2190 && cp < 0x3000 && textWidth(ch) === 1) points.add(cp);
    }
  }
  return points;
}

const hex = (points) => [...points].map((cp) => `U+${cp.toString(16).toUpperCase().padStart(4, "0")}`).join(" ");

// --- the contract ---------------------------------------------------------------------

test("the cell face is embedded at both of the grid's weights", () => {
  assert.deepEqual(
    faces().map((face) => face.weight),
    ["400", "700"],
    "a regular and a bold face, so a bold row's symbols are drawn bold rather than smeared",
  );
});

test("the cell face draws every symbol the layout draws", () => {
  const wanted = vocabulary();
  assert.ok(wanted.size >= 50, `the layout's symbols were read (${wanted.size})`);
  for (const face of faces()) {
    const mapped = mappedBy(tablesOf(face.file).get("cmap"));
    const missing = new Set([...wanted].filter((cp) => !mapped.has(cp) || !face.range.has(cp)));
    assert.deepEqual(
      hex(missing),
      hex(UNCOVERED.keys()),
      `the ${face.weight} face covers and claims every symbol but the ones no DejaVu face has; rerun tools/@afkd/web-top/cell-face.py`,
    );
    const unmapped = new Set([...face.range].filter((cp) => !mapped.has(cp)));
    assert.equal(hex(unmapped), "", `the ${face.weight} face claims only what it draws`);
  }
});

test("the cell face claims no text, so the grid's own face keeps the rows' metrics", () => {
  // A face is the element's *first available* one only if it covers the space, and the first
  // available face sets the strut every row's height and baseline come from. A cell face that
  // claimed ASCII, the space or the no-break space would quietly re-measure the whole grid.
  for (const face of faces()) {
    const text = [...face.range].filter((cp) => cp < 0x2000 || cp === 0x00a0);
    assert.deepEqual(text, [], `the ${face.weight} face's range claims no text`);
  }
});

test("the cell face heads the page's font stack", () => {
  const stack = CSS.match(/body \{[^}]*font-family: ([^;]+);/)?.[1] ?? "";
  assert.ok(stack.startsWith(`"${FAMILY}", `), `the symbols reach the cell face before any face of the visitor's: ${stack}`);
});

test("the cell face is renamed and carries DejaVu's notices, as its license asks", () => {
  // Bitstream's license lets its fonts be cut down only under a name without "Bitstream" or
  // "Vera", and wants its notices in every copy — in the face itself, and beside it in the
  // stylesheet a browser actually fetches.
  for (const face of faces()) {
    const names = namesOf(tablesOf(face.file).get("name"));
    for (const id of [1, 3, 4, 6]) {
      assert.ok(names.has(id), `the ${face.weight} face has name ${id}`);
      assert.doesNotMatch(names.get(id), /Bitstream|Vera|DejaVu/, `the ${face.weight} face's name ${id} is its own`);
    }
    assert.match(names.get(1), new RegExp(`^${FAMILY}$`), "its family is the one the stylesheet names");
    assert.match(names.get(0), /Copyright \(c\) 2003 by Bitstream, Inc\./, "it keeps Bitstream's copyright");
    assert.match(names.get(13), /Permission is hereby granted/, "it keeps the license");
  }
  assert.match(CSS, /Bitstream Vera is a trademark of\s+Bitstream, Inc\./, "the stylesheet carries the trademark notice");
  assert.match(CSS, /Permission is hereby granted, free of charge/, "…and the permission notice");
});
