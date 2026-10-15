#!/usr/bin/env python3
"""Turn a terminal capture with ANSI colors into an SVG image.

Usage: ansi2svg.py CAPTURE.ans OUTPUT.svg [--title TEXT]
       ansi2svg.py --frames DIR OUTPUT.svg [--title TEXT] [--max-hold SECS] [--poster-time T]

CAPTURE.ans is what `tmux capture-pane -e -p` prints: one line per terminal row, with SGR
escape sequences. The SVG is a grid of monospace cells on a dark background, as wide as the
longest row and as tall as the number of rows. It understands the 16 standard colors, the
256-color palette and 24-bit color, plus bold, dim, italic, underline and reverse. Box-drawing
characters are drawn as lines, so borders join up whatever font the viewer has, and the lower
block elements (U+2581 to U+2588, the TUI's working icon) as rectangles that fill their cell.

With --frames, DIR holds a recording: NNNNN.ans captures, each with an NNNNN.t file holding
the Unix time it was taken. The SVG plays them in a loop with CSS animation, each frame for as
long as it was on screen, at most --max-hold seconds (default 3). A frame that differs from the
one before only in the working icon is dropped. Every element is drawn once and shown in the
frames that have it, so a frame costs only what changed. Viewers that ask for reduced motion
see one still frame: the one on screen at --poster-time, else the last.

Python standard library only.
"""

import html
import re
import sys
import unicodedata

CELL_W = 8.4  # px; 0.6em of a 14px monospace font
CELL_H = 18
FONT_SIZE = 14
PAD = 14
FONT = (
    "ui-monospace, SFMono-Regular, 'SF Mono', Menlo, Consolas, 'DejaVu Sans Mono', "
    "'Liberation Mono', monospace"
)

BG = "#0d1117"
FG = "#c9d1d9"
# The 16 ANSI colors (black, red, green, yellow, blue, magenta, cyan, white, then bright).
ANSI16 = [
    "#1b1f24", "#ff7b72", "#3fb950", "#d29922", "#58a6ff", "#bc8cff", "#39c5cf", "#b1bac4",
    "#6e7681", "#ffa198", "#56d364", "#e3b341", "#79c0ff", "#d2a8ff", "#56d4dd", "#f0f6fc",
]

SGR = re.compile(r"\x1b\[([0-9;:]*)m")
OTHER_ESC = re.compile(r"\x1b(\[[0-9;?]*[A-Za-ln-z]|\][^\x07]*\x07|[()][A-Z0-9])")


def color256(n):
    if n < 16:
        return ANSI16[n]
    if n < 232:
        n -= 16
        steps = [0, 95, 135, 175, 215, 255]
        r, g, b = steps[n // 36], steps[(n // 6) % 6], steps[n % 6]
        return f"#{r:02x}{g:02x}{b:02x}"
    v = 8 + (n - 232) * 10
    return f"#{v:02x}{v:02x}{v:02x}"


class Style:
    __slots__ = ("fg", "bg", "bold", "dim", "italic", "underline", "reverse")

    def __init__(self):
        self.reset()

    def reset(self):
        self.fg = None
        self.bg = None
        self.bold = self.dim = self.italic = self.underline = self.reverse = False

    def copy(self):
        s = Style()
        for k in self.__slots__:
            setattr(s, k, getattr(self, k))
        return s

    def apply(self, params):
        codes = [int(p) if p else 0 for p in re.split(r"[;:]", params)] if params else [0]
        i = 0
        while i < len(codes):
            c = codes[i]
            if c == 0:
                self.reset()
            elif c == 1:
                self.bold = True
            elif c == 2:
                self.dim = True
            elif c == 3:
                self.italic = True
            elif c == 4:
                self.underline = True
            elif c == 7:
                self.reverse = True
            elif c == 22:
                self.bold = self.dim = False
            elif c == 23:
                self.italic = False
            elif c == 24:
                self.underline = False
            elif c == 27:
                self.reverse = False
            elif 30 <= c <= 37:
                self.fg = ANSI16[c - 30]
            elif 90 <= c <= 97:
                self.fg = ANSI16[c - 90 + 8]
            elif 40 <= c <= 47:
                self.bg = ANSI16[c - 40]
            elif 100 <= c <= 107:
                self.bg = ANSI16[c - 100 + 8]
            elif c == 39:
                self.fg = None
            elif c == 49:
                self.bg = None
            elif c in (38, 48) and i + 1 < len(codes):
                if codes[i + 1] == 5 and i + 2 < len(codes):
                    value = color256(codes[i + 2])
                    i += 2
                elif codes[i + 1] == 2 and i + 4 < len(codes):
                    value = "#{:02x}{:02x}{:02x}".format(*codes[i + 2 : i + 5])
                    i += 4
                else:
                    value = None
                if c == 38:
                    self.fg = value
                else:
                    self.bg = value
            i += 1

    def colors(self):
        """Resolved (fg, bg); bg is None for the default background."""
        fg, bg = self.fg or FG, self.bg
        if self.reverse:
            fg, bg = (bg or BG), fg
        return fg, bg

    def key(self):
        fg, _ = self.colors()
        return (fg, self.bold, self.dim, self.italic, self.underline)


def width(ch):
    if unicodedata.combining(ch):
        return 0
    return 2 if unicodedata.east_asian_width(ch) in ("W", "F") else 1


def parse(text):
    """Rows of cells: (char, Style) per column; a wide char's second column is None."""
    rows = []
    style = Style()
    for line in text.rstrip("\n").split("\n"):
        cells = []
        pos = 0
        while pos < len(line):
            m = SGR.match(line, pos)
            if m:
                style.apply(m.group(1))
                pos = m.end()
                continue
            m = OTHER_ESC.match(line, pos)
            if m:
                pos = m.end()
                continue
            ch = line[pos]
            pos += 1
            w = width(ch)
            if w == 0:
                continue
            cells.append((ch, style.copy()))
            if w == 2:
                cells.append(None)
        rows.append(cells)
    return rows


def box_segments(ch):
    """For a box-drawing character: {direction: weight} (weight 1 light, 2 heavy), else None."""
    if not "─" <= ch <= "╿":
        return None
    name = unicodedata.name(ch, "")
    words = name.replace("BOX DRAWINGS ", "").split()
    if "DASH" in words or "DIAGONAL" in words:
        return None
    dirs = {}
    weight = 1
    pending = []

    def flush(w):
        for d in pending:
            dirs[d] = w
        pending.clear()

    for word in words:
        if word in ("LIGHT", "HEAVY", "DOUBLE"):
            w = 1 if word == "LIGHT" else 2
            if pending:
                flush(w)  # "DOWN LIGHT AND RIGHT HEAVY": the weight follows its directions
            weight = w
        elif word in ("UP", "DOWN", "LEFT", "RIGHT"):
            pending.append(word)
        elif word == "VERTICAL":
            pending += ["UP", "DOWN"]
        elif word == "HORIZONTAL":
            pending += ["LEFT", "RIGHT"]
    flush(weight)
    return dirs or None


def block_eighths(ch):
    """For a lower block element (U+2581 LOWER ONE EIGHTH BLOCK to U+2588 FULL BLOCK): its
    height in eighths of the cell, else None."""
    if "\u2581" <= ch <= "\u2588":
        return ord(ch) - 0x2580
    return None


def fmt(v):
    return f"{v:.1f}".rstrip("0").rstrip(".")


def header(ncols, nrows, title, extra_css=""):
    w = ncols * CELL_W + 2 * PAD
    h = nrows * CELL_H + 2 * PAD
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{fmt(w)}" height="{fmt(h)}" '
        f'viewBox="0 0 {fmt(w)} {fmt(h)}" role="img"'
        + (f' aria-label="{html.escape(title)}"' if title else "")
        + ">"
    ]
    if title:
        out.append(f"<title>{html.escape(title)}</title>")
    out.append(
        "<style>text{font-family:" + FONT + f";font-size:{FONT_SIZE}px;white-space:pre}}"
        ".b{font-weight:bold}.d{opacity:.55}.i{font-style:italic}.u{text-decoration:underline}"
        + extra_css
        + "</style>"
    )
    out.append(f'<rect width="100%" height="100%" rx="8" fill="{BG}"/>')
    return out


def render(rows, title=None):
    ncols = max((len(r) for r in rows), default=0)
    out = header(ncols, len(rows), title)
    for layer in layers(rows):
        out += layer
    out.append("</svg>")
    return "\n".join(out) + "\n"


def layers(rows, granular=False):
    """The image's elements in four layers drawn in order: backgrounds, blocks, box lines and
    text. With granular, box lines are one path per row or column and color instead of one per
    color, so a frame that changes one border doesn't redraw them all."""
    backgrounds, blocks, paths, texts = [], [], [], []

    def x(col):
        return PAD + col * CELL_W

    def y(row):
        return PAD + row * CELL_H

    # Backgrounds: runs of the same non-default color.
    for r, cells in enumerate(rows):
        c = 0
        while c < len(cells):
            cell = cells[c]
            bg = cell[1].colors()[1] if cell else None
            if not bg:
                c += 1
                continue
            start = c
            while c < len(cells) and (cells[c] is None or cells[c][1].colors()[1] == bg):
                c += 1
            backgrounds.append(
                f'<rect x="{fmt(x(start))}" y="{fmt(y(r))}" width="{fmt((c - start) * CELL_W)}" '
                f'height="{CELL_H}" fill="{bg}"/>'
            )

    # Lower blocks: the bottom of their cell, filled.
    for r, cells in enumerate(rows):
        for c, cell in enumerate(cells):
            n = block_eighths(cell[0]) if cell else None
            if n is None:
                continue
            fg = cell[1].colors()[0]
            h = CELL_H * n / 8
            extra = ' opacity=".55"' if cell[1].dim else ""
            blocks.append(
                f'<rect x="{fmt(x(c))}" y="{fmt(y(r) + CELL_H - h)}" width="{fmt(CELL_W)}" '
                f'height="{fmt(h)}" fill="{fg}"{extra}/>'
            )

    # Box drawing: half-cell segments from the cell center, merged per row and column.
    lines = {}  # (color, weight, dim) -> list of (x1, y1, x2, y2)
    half_w, half_h = CELL_W / 2, CELL_H / 2
    horiz = {}  # (row, key) -> list of [x1, x2]
    vert = {}  # (col, key) -> list of [y1, y2]
    for r, cells in enumerate(rows):
        for c, cell in enumerate(cells):
            if not cell:
                continue
            seg = box_segments(cell[0])
            if not seg:
                continue
            cx, cy = x(c) + half_w, y(r) + half_h
            for d, wt in seg.items():
                key = (cell[1].colors()[0], wt, cell[1].dim)
                if d == "LEFT":
                    horiz.setdefault((r, key), []).append([cx - half_w, cx])
                elif d == "RIGHT":
                    horiz.setdefault((r, key), []).append([cx, cx + half_w])
                elif d == "UP":
                    vert.setdefault((c, key), []).append([cy - half_h, cy])
                else:
                    vert.setdefault((c, key), []).append([cy, cy + half_h])

    def merge(spans):
        spans.sort()
        merged = [spans[0][:]]
        for a, b in spans[1:]:
            if a <= merged[-1][1] + 0.01:
                merged[-1][1] = max(merged[-1][1], b)
            else:
                merged.append([a, b])
        return merged

    for (r, key), spans in horiz.items():
        cy = y(r) + half_h
        for a, b in merge(spans):
            lines.setdefault((key, ("h", r) if granular else None), []).append(
                f"M{fmt(a)} {fmt(cy)}H{fmt(b)}")
    for (c, key), spans in vert.items():
        cx = x(c) + half_w
        for a, b in merge(spans):
            lines.setdefault((key, ("v", c) if granular else None), []).append(
                f"M{fmt(cx)} {fmt(a)}V{fmt(b)}")
    for ((color, wt, dim), _), parts in sorted(lines.items(), key=lambda kv: (kv[0][0], str(kv[0][1]))):
        extra = ' opacity=".55"' if dim else ""
        paths.append(
            f'<path d="{"".join(parts)}" stroke="{color}" stroke-width="{1.2 if wt == 1 else 2.4}" '
            f'fill="none" stroke-linecap="square"{extra}/>'
        )

    # Text: runs of one style, split at blanks; each run pinned to its cells.
    for r, cells in enumerate(rows):
        baseline = y(r) + CELL_H * 0.75
        c = 0
        while c < len(cells):
            cell = cells[c]
            if cell is None or cell[0] == " " or box_segments(cell[0]) or block_eighths(cell[0]):
                c += 1
                continue
            key = cell[1].key()
            start = c
            text = []
            span = 0
            while c < len(cells):
                cur = cells[c]
                if cur is None:
                    c += 1
                    span += 1
                    continue
                if box_segments(cur[0]) or block_eighths(cur[0]) or cur[1].key() != key:
                    break
                # Two blanks end a run (columns of a table, padding).
                if cur[0] == " " and (c + 1 >= len(cells) or not cells[c + 1] or cells[c + 1][0] == " "):
                    break
                text.append(cur[0])
                span += 1
                c += 1
            s = "".join(text).rstrip(" ")
            if not s:
                continue
            fg, bold, dim, italic, underline = key
            classes = " ".join(
                n for n, on in (("b", bold), ("d", dim), ("i", italic), ("u", underline)) if on
            )
            attrs = f' class="{classes}"' if classes else ""
            cols = sum(width(ch) for ch in s)
            length = f' textLength="{fmt(cols * CELL_W)}"' if cols > 1 else ""
            texts.append(
                f'<text x="{fmt(x(start))}" y="{fmt(baseline)}" fill="{fg}"{attrs}{length}>'
                f"{html.escape(s, quote=False)}</text>"
            )
    return backgrounds, blocks, paths, texts


WORKING_ICON = str.maketrans("", "", "\u2581\u2582\u2583\u2584\u2585\u2586\u2587\u2588_")


def read_rows(path):
    with open(path, encoding="utf-8") as f:
        return parse(f.read())


def trim(rows):
    """Drop blank rows at the bottom (tmux pads the capture to the pane height)."""
    while rows and not any(cell and cell[0] != " " for cell in rows[-1]):
        rows.pop()
    return rows


def load_frames(directory, max_hold):
    """The recording in DIR as [(rows, seconds, start_time)], working-icon-only changes dropped."""
    import glob
    import os

    shots = []
    for ans in sorted(glob.glob(os.path.join(directory, "*.ans"))):
        with open(ans[:-4] + ".t") as f:
            t = float(f.read())
        with open(ans, encoding="utf-8") as f:
            text = f.read()
        plain = SGR.sub("", text).translate(WORKING_ICON)
        if shots and plain == shots[-1][2]:
            continue
        shots.append((text, t, plain))
    frames = []
    for i, (text, t, _) in enumerate(shots):
        end = shots[i + 1][1] if i + 1 < len(shots) else t + max_hold
        frames.append((parse(text), min(max(end - t, 0.04), max_hold), t))
    return frames


def animate(frames, title=None, poster_time=None):
    """One SVG that plays FRAMES ([(rows, seconds, start_time)]) in a loop."""
    nrows = max(len(trim(rows)) for rows, _, _ in frames)
    ncols = max((len(r) for rows, _, _ in frames for r in rows), default=0)
    total = sum(d for _, d, _ in frames)
    starts = []
    at = 0.0
    for _, d, _ in frames:
        starts.append(at)
        at += d
    poster = len(frames) - 1
    if poster_time is not None:
        poster = max((i for i, f in enumerate(frames) if f[2] <= poster_time), default=0)

    # Each element, per layer, with the set of frames it is in.
    seen = [{}, {}, {}, {}]
    for i, (rows, _, _) in enumerate(frames):
        for layer, elements in zip(seen, layers(rows, granular=True)):
            for e in elements:
                layer.setdefault(e, []).append(i)

    groups = {}  # frame tuple -> class name
    css = []
    body = []
    for layer in seen:
        by_set = {}
        for e, idx in layer.items():
            by_set.setdefault(tuple(idx), []).append(e)
        for idx, elements in by_set.items():
            if len(idx) == len(frames):
                body += elements
                continue
            name = groups.get(idx)
            if name is None:
                name = groups[idx] = f"f{len(groups)}"
                on = set(idx)
                stops = []
                last = None
                for i in range(len(frames)):
                    v = 1 if i in on else 0
                    if v != last:
                        stops.append(f"{fmt(starts[i] / total * 100)}%{{opacity:{v}}}")
                        last = v
                css.append(
                    f"@keyframes {name}{{{''.join(stops)}}}"
                    f".{name}{{opacity:{1 if poster in on else 0};"
                    f"animation:{name} {fmt(total)}s step-end infinite}}"
                )
            body.append(f'<g class="{name}">' + "".join(elements) + "</g>")
    css.append("@media (prefers-reduced-motion:reduce){g{animation:none!important}}")
    out = header(ncols, nrows, title, "".join(css))
    out += body
    out.append("</svg>")
    return "\n".join(out) + "\n"


def option(args, name, default=None):
    if name not in args:
        return default
    i = args.index(name)
    value = args[i + 1]
    del args[i : i + 2]
    return value


def main(argv):
    args = argv[1:]
    title = option(args, "--title")
    frames_dir = option(args, "--frames")
    max_hold = float(option(args, "--max-hold", "3"))
    poster_time = option(args, "--poster-time")
    if len(args) != (1 if frames_dir else 2):
        sys.exit(__doc__.split("\n\n")[1])
    if frames_dir:
        frames = load_frames(frames_dir, max_hold)
        svg = animate(frames, title, float(poster_time) if poster_time else None)
    else:
        svg = render(trim(read_rows(args[0])), title)
    with open(args[-1], "w", encoding="utf-8") as f:
        f.write(svg)


if __name__ == "__main__":
    main(sys.argv)
