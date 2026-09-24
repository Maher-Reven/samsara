#!/usr/bin/env python3
"""Render captured terminal output as an SVG.

A README needs to show what the tool does without asking anyone to run it,
and a GIF is a bad fit for a terminal: it is heavy, it autoplays, it cannot
be searched, and it renders text as mush on a high-DPI screen. An SVG of the
real output stays crisp, weighs a few kilobytes, and — because it is
generated from a live run by `make demo-svg` — cannot quietly drift away
from what the tool actually prints.

Reads ANSI on stdin, writes SVG on stdout.
"""

import html
import re
import sys

# Only the codes Samsara itself emits. Anything else is dropped rather than
# guessed at.
COLOURS = {
    "31": "#e06c75",  # red    — a violation
    "32": "#98c379",  # green  — a passing claim
    "33": "#e5c07b",  # yellow — an injected fault
    "36": "#56b6c2",  # cyan
}
BG, FG, DIM = "#16151a", "#d7d5e0", "#7d7a8c"
CHAR_W, LINE_H, PAD = 8.4, 20, 26
TITLE_H = 34

ANSI = re.compile(r"\x1b\[([0-9;]*)m")


def spans(line):
    """Split a line into (text, fill, bold) runs."""
    out, pos = [], 0
    fill, bold = FG, False
    for match in ANSI.finditer(line):
        if match.start() > pos:
            out.append((line[pos : match.start()], fill, bold))
        for code in (match.group(1) or "0").split(";"):
            if code in ("", "0"):
                fill, bold = FG, False
            elif code == "1":
                bold = True
            elif code == "2":
                fill = DIM
            elif code in COLOURS:
                fill = COLOURS[code]
        pos = match.end()
    if pos < len(line):
        out.append((line[pos:], fill, bold))
    return out


def main():
    lines = [l.rstrip("\n") for l in sys.stdin.read().split("\n")]
    while lines and not lines[-1].strip():
        lines.pop()

    width = max((len(ANSI.sub("", l)) for l in lines), default=80)
    w = int(width * CHAR_W + PAD * 2)
    h = int(len(lines) * LINE_H + PAD * 2 + TITLE_H)

    parts = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" '
        f'viewBox="0 0 {w} {h}" font-family="ui-monospace,SFMono-Regular,'
        f'Menlo,Consolas,monospace" font-size="13">',
        f'<rect width="{w}" height="{h}" rx="10" fill="{BG}"/>',
        # Window chrome, so it reads as a terminal at a glance.
        f'<circle cx="22" cy="19" r="6" fill="#e06c75"/>',
        f'<circle cx="42" cy="19" r="6" fill="#e5c07b"/>',
        f'<circle cx="62" cy="19" r="6" fill="#98c379"/>',
        f'<text x="{w // 2}" y="24" fill="{DIM}" text-anchor="middle" '
        f'font-size="12">samsara</text>',
    ]

    for row, line in enumerate(lines):
        y = PAD + TITLE_H + row * LINE_H
        x = PAD
        for text, fill, bold in spans(line):
            if not text:
                continue
            weight = ' font-weight="600"' if bold else ""
            parts.append(
                f'<text x="{x:.1f}" y="{y}" fill="{fill}"{weight} '
                f'xml:space="preserve">{html.escape(text)}</text>'
            )
            x += len(text) * CHAR_W
    parts.append("</svg>")
    sys.stdout.write("\n".join(parts) + "\n")


if __name__ == "__main__":
    main()
