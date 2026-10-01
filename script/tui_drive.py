#!/usr/bin/env python3
r"""Drive the antex TUI in a PTY and dump what ends up on screen.

`script/shot-home.py` renders a PNG of whatever a command draws; this one goes
one step further and *types into it*, so a feature that only shows up after
some interaction (a form prompt, a badge, an ordering fix) can be checked
without a human watching. Same trick underneath: run the client in a PTY and
replay its output into pyte.

    uv run --with pyte script/tui_drive.py \
        --send '8:请调用一次 AskUser 问我：午饭吃什么？两个选项：吃面、吃饭。\r' \
        --send '40:选第一项\r'

Each `--send` is `<seconds from start>:<text>`; `\r`, `\n`, `\t`, `\e` are
decoded, and `\r` is what submits in the client's input widget (a bare newline
only inserts a line). The screen when everything has settled is printed at the
end, minus blank lines.
"""
from __future__ import annotations

import argparse
import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

import pyte


def unescape(text: str) -> str:
    """Decode the handful of escapes a `--send` may carry, and nothing else.

    `codecs.decode(..., "unicode_escape")` would also reinterpret the UTF-8
    bytes of any non-ASCII text, which turns a Chinese prompt into mojibake.
    """
    mapped = {"r": "\r", "n": "\n", "t": "\t", "e": "\x1b", "\\": "\\"}
    out: list[str] = []
    index = 0
    while index < len(text):
        char = text[index]
        if char == "\\" and index + 1 < len(text) and text[index + 1] in mapped:
            out.append(mapped[text[index + 1]])
            index += 2
            continue
        out.append(char)
        index += 1
    return "".join(out)


def parse_send(spec: str) -> tuple[float, str]:
    at, colon, text = spec.partition(":")
    if not colon:
        raise argparse.ArgumentTypeError(f"expected <seconds>:<text>, got {spec!r}")
    return float(at), unescape(text)


def run(cmd: str, cols: int, rows: int, sends: list[tuple[float, str]], after: float) -> pyte.Screen:
    pid, fd = pty.fork()
    if pid == 0:
        os.environ["TERM"] = "xterm-256color"
        os.environ["COLUMNS"] = str(cols)
        os.environ["LINES"] = str(rows)
        os.execvp("sh", ["sh", "-c", cmd])
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    screen = pyte.Screen(cols, rows)
    stream = pyte.ByteStream(screen)
    start = time.time()
    # What is left to type, and when to give up waiting for the screen.
    pending = sorted(sends, key=lambda send: send[0])
    deadline = (pending[-1][0] + after) if pending else after
    try:
        while time.time() - start < deadline:
            while pending and time.time() - start >= pending[0][0]:
                _, text = pending.pop(0)
                os.write(fd, text.encode())
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


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--cmd", default="./target/debug/antex", help="the client to drive")
    ap.add_argument(
        "--send",
        action="append",
        default=[],
        metavar="SECONDS:TEXT",
        help="type TEXT at SECONDS from start (\\r submits); repeatable",
    )
    ap.add_argument("--after", type=float, default=20.0, help="seconds to wait after the last send")
    ap.add_argument("--cols", type=int, default=140)
    ap.add_argument("--rows", type=int, default=44)
    ap.add_argument("--dump", help="also write the screen text here")
    args = ap.parse_args()

    sends = [parse_send(spec) for spec in args.send]
    screen = run(args.cmd, args.cols, args.rows, sends, args.after)

    lines = [line.rstrip() for line in screen.display]
    # Blank rows are padding; the tail is also usually an empty prompt area.
    text = "\n".join(line for line in lines if line.strip())
    print(text or "(空屏)")
    if args.dump:
        with open(args.dump, "w") as file:
            file.write(text + "\n")
        print(f"写好：{args.dump}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
