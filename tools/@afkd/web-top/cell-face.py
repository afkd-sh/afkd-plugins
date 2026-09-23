#!/usr/bin/env python3
"""Cut @afkd/web-top's cell face and write it into the plugin's stylesheet.

afkd top draws its symbols -- the status shapes, the slot bars, the box lines, the rail's
marks -- in the terminal's own monospace face. A browser draws them in whatever face the
visitor happens to have them in, and a fallback face sizes and places a shape for text rather
than for a cell: one visitor's `▷` came out a full em wide, another's `▯` a sliver. So the
page carries a face of its own for them: every symbol the layout draws, plus the whole of the
box-drawing, block and geometric-shape blocks a terminal draws from, cut out of DejaVu Sans
Mono. The one symbol it lacks, `∥`, is borrowed from DejaVu Sans and centred on the mono cell.

The face is renamed, as Bitstream's license asks of a modified copy, and embedded in
`dashboard.css` as a WOFF data URL between the `@cell-face` markers, so the relay's
three-extension allowlist still serves everything the page needs. `face.test.mjs` reads the
embedded font back and holds it to the layout.

Run it from the repository root whenever the layout draws a new symbol:

    python3 tools/@afkd/web-top/cell-face.py [DEJAVU_DIR]

It needs fontTools and DejaVu's TTFs (`/usr/share/fonts/truetype/dejavu` by default). The page
and its relay need neither.
"""

import base64
import io
import pathlib
import re
import sys
import unicodedata

from fontTools import subset
from fontTools.pens.boundsPen import BoundsPen
from fontTools.pens.transformPen import TransformPen
from fontTools.pens.ttGlyphPen import TTGlyphPen
from fontTools.ttLib import TTFont

ROOT = pathlib.Path(__file__).resolve().parents[3]
PLUGIN = ROOT / "@afkd" / "web-top"
STYLESHEET = PLUGIN / "dashboard.css"
FAMILY = "afkd cells"
POSTSCRIPT = "afkdCells"

# The modules that spell the screen, whose every one-cell symbol the face must draw.
SOURCES = ("layout.mjs", "keymap.mjs")
# Where a symbol starts: past Latin-1 and the general punctuation every monospace face has.
FIRST = 0x2190
# Where the wide scripts start; a two-cell character is never drawn from a one-cell face.
LAST = 0x3000
# The blocks a terminal draws its boxes, bars and shapes from, taken whole.
BLOCKS = (range(0x2500, 0x2580), range(0x2580, 0x25A0), range(0x25A0, 0x2600))

BEGIN = "/* @cell-face begin */\n"
END = "/* @cell-face end */\n"


def vocabulary():
    """Every one-cell symbol the layout's modules spell, plus the terminal's drawing blocks."""
    points = set()
    for name in SOURCES:
        for ch in (PLUGIN / name).read_text(encoding="utf-8"):
            cp = ord(ch)
            if FIRST <= cp < LAST and unicodedata.east_asian_width(ch) not in ("W", "F"):
                points.add(cp)
    for block in BLOCKS:
        points.update(block)
    return points


def borrow(font, donor, cp):
    """Draw `donor`'s glyph for `cp` into `font`, its ink centred on the mono cell."""
    source = donor.getGlyphSet()[donor.getBestCmap()[cp]]
    bounds = BoundsPen(donor.getGlyphSet())
    source.draw(bounds)
    x_min, _, x_max, _ = bounds.bounds
    cell = font["hmtx"]["zero"][0]
    scale = min(1.0, cell / (x_max - x_min))
    shift = (cell - (x_max - x_min) * scale) / 2 - x_min * scale
    pen = TTGlyphPen(None)
    source.draw(TransformPen(pen, (scale, 0, 0, scale, shift, 0)))
    glyph = pen.glyph()
    name = f"uni{cp:04X}"
    font.setGlyphOrder([*font.getGlyphOrder(), name])
    font["glyf"].glyphs[name] = glyph
    glyph.recalcBounds(font["glyf"])
    font["hmtx"].metrics[name] = (cell, glyph.xMin)
    for table in font["cmap"].tables:
        if table.isUnicode():
            table.cmap[cp] = name


def rename(font, bold):
    """Give the cut face a name of its own, keeping DejaVu's copyright and license records."""
    style = "Bold" if bold else "Book"
    version = font["name"].getDebugName(5)
    names = {
        1: FAMILY,
        2: style,
        3: f"{FAMILY} {style} {version}",
        4: f"{FAMILY} {style}",
        6: f"{POSTSCRIPT}-{style}",
    }
    table = font["name"]
    table.names = [record for record in table.names if record.nameID not in (*names, 16, 17)]
    for name_id, text in names.items():
        table.setName(text, name_id, 3, 1, 0x409)


def cut(dejavu, points, bold):
    """The cell face at one weight, as WOFF bytes, and the code points it maps."""
    suffix = "-Bold" if bold else ""
    font = TTFont(dejavu / f"DejaVuSansMono{suffix}.ttf", recalcTimestamp=False)
    donor = TTFont(dejavu / f"DejaVuSans{suffix}.ttf")
    have = font.getBestCmap()
    for cp in sorted(points - set(have)):
        if cp in donor.getBestCmap():
            borrow(font, donor, cp)
    options = subset.Options()
    options.name_IDs = [0, 1, 2, 3, 4, 5, 6, 13, 14]
    options.name_languages = [0x409]
    options.layout_features = []
    options.notdef_outline = True
    cutter = subset.Subsetter(options)
    cutter.populate(unicodes=points)
    cutter.subset(font)
    rename(font, bold)
    mapped = set(font.getBestCmap())
    # On the font, not the subsetter's options: those reach only the subsetter's own writer.
    font.flavor = "woff"
    out = io.BytesIO()
    font.save(out)
    return out.getvalue(), mapped


def ranges(points):
    """`points` as a `unicode-range` value, runs folded."""
    spans = []
    for cp in sorted(points):
        if spans and cp == spans[-1][1] + 1:
            spans[-1][1] = cp
        else:
            spans.append([cp, cp])
    return ", ".join(f"U+{a:04X}" if a == b else f"U+{a:04X}-{b:04X}" for a, b in spans)


def face_rule(woff, mapped, weight):
    return (
        "@font-face {\n"
        f'  font-family: "{FAMILY}";\n'
        f"  font-weight: {weight};\n"
        "  font-display: block;\n"
        f'  src: url(data:font/woff;base64,{base64.b64encode(woff).decode()}) format("woff");\n'
        f"  unicode-range: {ranges(mapped)};\n"
        "}\n"
    )


def main():
    dejavu = pathlib.Path(sys.argv[1] if len(sys.argv) > 1 else "/usr/share/fonts/truetype/dejavu")
    points = vocabulary()
    rules = []
    for weight, bold in ((400, False), (700, True)):
        woff, mapped = cut(dejavu, points, bold)
        rules.append(face_rule(woff, mapped, weight))
        missing = sorted(points - mapped)
        lost = ", ".join(f"U+{cp:04X} {chr(cp)}" for cp in missing if not any(cp in b for b in BLOCKS))
        print(f"{weight}: {len(woff)} bytes, {len(mapped)} code points; in no DejaVu face: {lost or 'none'}")
    css = STYLESHEET.read_text(encoding="utf-8")
    pattern = re.compile(re.escape(BEGIN) + r".*?" + re.escape(END), re.S)
    if len(pattern.findall(css)) != 1:
        sys.exit(f"{STYLESHEET} has no single {BEGIN.strip()} … {END.strip()} block to write into")
    STYLESHEET.write_text(pattern.sub(lambda _: BEGIN + "\n".join(rules) + END, css), encoding="utf-8")


if __name__ == "__main__":
    main()
