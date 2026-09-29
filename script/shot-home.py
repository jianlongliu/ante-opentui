#!/usr/bin/env python3
"""Render the antex home screen to a PNG, for the README.

The client is a TUI, so a "screenshot" needs a terminal emulator: this runs the
command in a PTY, replays its output into pyte, then draws the resulting screen
— colours included — through ImageMagick's Pango renderer.

    uv run --with pyte script/shot-home.py --out docs/home.png

Nothing here is Ante-specific; it draws whatever the command puts on screen.
"""
from __future__ import annotations

import argparse
import fcntl
import os
import pty
import select
import struct
import subprocess
import sys
import termios
import time
from collections import Counter

import pyte

# Ghostty's own font, so the picture matches what the user actually sees.
FONT = "GoogleSansCode Nerd Font Mono"
FALLBACK_FG = "#d8dee9"
FALLBACK_BG = "#1e1e2e"


def spawn(cmd: str, cols: int, rows: int):
    """A PTY running `cmd`, sized like a real terminal."""
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"] = str(cols)
        os.environ["LINES"] = str(rows)
        os.execvp("sh", ["sh", "-c", cmd])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
    return pid, fd


def capture(cmd: str, cols: int, rows: int, wait: float) -> pyte.Screen:
    pid, fd = spawn(cmd, cols, rows)
    screen = pyte.Screen(cols, rows)
    # 256-colour and truecolour are what the TUI emits; `history` off because a
    # screenshot is the visible screen only.
    stream = pyte.ByteStream(screen)
    deadline = time.time() + wait
    try:
        while time.time() < deadline:
            ready, _, _ = select.select([fd], [], [], 0.1)
            if not ready:
                continue
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            stream.feed(chunk)
    finally:
        os.kill(pid, 15)
        os.waitpid(pid, 0)
    return screen


def to_hex(colour: str | None, default: str) -> str:
    """pyte gives `default`, a 16-colour name, or bare 6-hex for 24-bit colour."""
    if not colour or colour == "default":
        return default
    if colour.startswith("#"):
        return colour
    if len(colour) == 6 and all(c in "0123456789abcdefABCDEF" for c in colour):
        return "#" + colour
    named = {
        "black": "#191114", "red": "#f38ba8", "green": "#a6e3a1", "brown": "#f9e2af",
        "yellow": "#f9e2af", "blue": "#89b4fa", "magenta": "#f5c2e7", "cyan": "#94e2d5",
        "white": "#eedfe2",
    }
    if colour.startswith("bright"):
        return named.get(colour[6:], default)
    return named.get(colour, default)


def terminal_colours(fallback: tuple[str, str]) -> tuple[str, str]:
    """The terminal's own background and foreground.

    The TUI paints its surfaces but leaves the page background to the terminal,
    so a faithful picture needs the terminal's colours. Only ghostty is queried
    (with a short timeout); anywhere else this falls back to the arguments.
    """
    try:
        out = subprocess.run(
            ["ghostty", "+show-config"], capture_output=True, text=True, timeout=3
        ).stdout
    except (OSError, subprocess.SubprocessError):
        return fallback
    found = {}
    for line in out.splitlines():
        key, _, value = line.partition("=")
        key = key.strip()
        if key in ("background", "foreground"):
            found[key] = value.strip()
    return found.get("foreground", fallback[0]), found.get("background", fallback[1])


def pick_colours(screen: pyte.Screen) -> tuple[str, str]:
    """Text colour used on the screen, paired with the terminal's own colours.

    The foreground comes from the capture (`default` cells are drawn in the
    terminal's foreground, but a run that says so explicitly is a better clue
    than a hard-coded constant); the background has to come from the terminal.
    """
    fgs: Counter[str] = Counter()
    for row in screen.buffer.values():
        for cell in row.values():
            if cell.fg != "default":
                fgs[cell.fg] += 1
    fg = to_hex(fgs.most_common(1)[0][0], FALLBACK_FG) if fgs else FALLBACK_FG
    term_fg, term_bg = terminal_colours((FALLBACK_FG, FALLBACK_BG))
    # Trust the terminal for the page background, but keep the capture's own
    # foreground when it disagrees — it is what the UI is actually drawing.
    return (fg or term_fg), term_bg


def escape(text: str) -> str:
    return text.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")


def markup(screen: pyte.Screen, fg: str, bg: str) -> str:
    """One `<span>` per run of identically-styled cells."""
    lines: list[str] = []
    for y in range(screen.lines):
        row = screen.buffer[y]
        parts: list[str] = []
        run_text, run_fg, run_bg, run_bold = "", None, None, None

        def flush() -> None:
            if not run_text:
                return
            style = f'foreground="{run_fg}" background="{run_bg}"'
            if run_bold:
                style += ' weight="bold"'
            parts.append(f"<span {style}>{escape(run_text)}</span>")

        for x in range(screen.columns):
            cell = row[x]
            cell_fg = to_hex(cell.bg if cell.reverse else cell.fg, fg)
            cell_bg = to_hex(cell.fg if cell.reverse else cell.bg, bg)
            if (cell_fg, cell_bg, cell.bold) != (run_fg, run_bg, run_bold):
                flush()
                run_text, run_fg, run_bg, run_bold = "", cell_fg, cell_bg, cell.bold
            # A marked cell can still hold a space (coloured blocks are made of
            # them), so the character goes in either way.
            run_text += cell.data or " "
        flush()
        lines.append("".join(parts).rstrip() or f'<span background="{bg}"> </span>')
    # Trailing blank rows are padding, not content.
    while lines and f'<span background="{bg}"> </span>' == lines[-1]:
        lines.pop()
    return "\n".join(lines)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cmd", default="./target/release/antex")
    ap.add_argument("--out", required=True)
    ap.add_argument("--cols", type=int, default=120)
    ap.add_argument("--rows", type=int, default=34)
    ap.add_argument("--wait", type=float, default=9.0, help="seconds to let the UI settle")
    ap.add_argument("--pointsize", type=float, default=11.5)
    ap.add_argument("--padding", type=int, default=16)
    ap.add_argument(
        "--line-gap",
        type=float,
        default=-4.0,
        help="extra leading in points; negative closes the gaps between block glyphs",
    )
    args = ap.parse_args()

    screen = capture(args.cmd, args.cols, args.rows, args.wait)
    fg, bg = pick_colours(screen)
    text = markup(screen, fg, bg)
    print(f"screen {args.cols}x{args.rows}; fg {fg} bg {bg}", file=sys.stderr)

    cmd = [
        "magick",
        "-background", bg,
        "-fill", fg,
        "-font", FONT,
        "-pointsize", str(args.pointsize),
        "-interline-spacing", str(args.line_gap),
        # Pango draws the runs; the cell grid comes from the font's own
        # advances, so block characters keep the logo aligned.
        f"pango:{text}",
        "-bordercolor", bg,
        "-border", str(args.padding),
        args.out,
    ]
    subprocess.run(cmd, check=True)
    print(f"写好：{args.out}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
