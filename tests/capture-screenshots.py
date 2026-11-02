#!/usr/bin/env python3
"""Render real PTY output to PNG with synthetic monitors and isolated profiles.

Requires Pillow and fontconfig. Does not connect to a real compositor.
The small renderer supports the cursor/color sequences emitted by this TUI.
"""
import ast
import fcntl
import json
import os
from pathlib import Path
import pty
import re
import select
import struct
import subprocess
import tempfile
import termios
import time

from PIL import Image, ImageDraw, ImageFont


ROOT = Path(__file__).resolve().parents[1]
BINARY = ROOT / "target/release/hypr-monitor-tui"
WIDTH, HEIGHT = 110, 34
PALETTE = [
    "212121", "c30771", "10a778", "a89c14", "008ec4", "523c79", "20a5ba", "e0e0e0",
    "818181", "fb007a", "5fd7af", "f3e430", "20bbfc", "6855de", "4fb8cc", "f1f1f1",
]
FOREGROUND, BACKGROUND = "d6d6d6", "000000"


def cells(data):
    grid = [[(" ", FOREGROUND, BACKGROUND, False) for _ in range(WIDTH)] for _ in range(HEIGHT)]
    x = y = 0
    fg, bg, bold = FOREGROUND, BACKGROUND, False
    text = re.sub(r"\x1b\][^\x07]*\x07", "", data.decode("utf-8", "replace"))
    for token in re.findall(r"\x1b\[[0-?]*[ -/]*[@-~]|\x1b[()][A-Z0-9]|[^\x1b]", text):
        if token.startswith("\x1b["):
            parameters, command = token[2:-1], token[-1]
            if parameters.startswith(("?", ">")):
                continue
            numbers = [int(n or "0") for n in parameters.split(";")]
            if command in ("H", "f"):
                y, x = (numbers[0] or 1) - 1, ((numbers[1] or 1) if len(numbers) > 1 else 1) - 1
            elif command == "J" and numbers[0] == 2:
                grid = [[(" ", fg, bg, bold) for _ in range(WIDTH)] for _ in range(HEIGHT)]
            elif command == "m":
                i = 0
                while i < len(numbers):
                    n = numbers[i]
                    if n == 0:
                        fg, bg, bold = FOREGROUND, BACKGROUND, False
                    elif n in (1, 22):
                        bold = n == 1
                    elif n in (39, 49):
                        if n == 39:
                            fg = FOREGROUND
                        else:
                            bg = BACKGROUND
                    elif 30 <= n <= 37 or 90 <= n <= 97:
                        fg = PALETTE[n - 30 if n < 90 else n - 90 + 8]
                    elif n in (38, 48):
                        mode = numbers[i + 1]
                        if mode == 5:
                            value = numbers[i + 2]
                            assert value < 16, "Unexpected indexed color"
                            color = PALETTE[value]
                            i += 2
                        else:
                            assert mode == 2
                            color = "".join(f"{v:02x}" for v in numbers[i + 2:i + 5])
                            i += 4
                        if n == 38:
                            fg = color
                        else:
                            bg = color
                    i += 1
        elif token == "\r":
            x = 0
        elif token == "\n":
            y += 1
        elif not token.startswith("\x1b") and 0 <= x < WIDTH and 0 <= y < HEIGHT:
            grid[y][x] = (token, fg, bg, bold)
            x += 1
    return grid


def render(data, path):
    def font(style):
        filename = subprocess.check_output(
            ["fc-match", "-f", "%{file}", f"UbuntuMono Nerd Font:style={style}"], text=True)
        return ImageFont.truetype(filename, 20)
    regular, bold_font = font("Regular"), font("Bold")
    fallback_path = subprocess.check_output(["fc-match", "-f", "%{file}", "DejaVu Sans Mono"], text=True)
    fallback = ImageFont.truetype(fallback_path, 20)
    # Like terminal emulators, connect box-drawing strokes across cell boundaries
    # and use a fallback font for arrows absent from the selected font.
    box = {"━": "lr", "┃": "ud", "┏": "rd", "┓": "ld", "┗": "ru", "┛": "lu",
           "─": "lr", "│": "ud", "┌": "rd", "┐": "ld", "└": "ru", "┘": "lu"}
    cell_width, cell_height, padding = 12, 26, 20
    image = Image.new("RGB", (WIDTH * cell_width + padding * 2, HEIGHT * cell_height + padding * 2), "#" + BACKGROUND)
    draw = ImageDraw.Draw(image)
    for row, line in enumerate(cells(data)):
        for column, (char, fg, bg, bold) in enumerate(line):
            x, y = padding + column * cell_width, padding + row * cell_height
            draw.rectangle((x, y, x + cell_width - 1, y + cell_height - 1), fill="#" + bg)
            if char in box:
                center = (x + cell_width // 2, y + cell_height // 2)
                ends = {"l": (x, center[1], center[0] + 1, center[1] + 1),
                        "r": (center[0], center[1], x + cell_width - 1, center[1] + 1),
                        "u": (center[0], y, center[0] + 1, center[1] + 1),
                        "d": (center[0], center[1], center[0] + 1, y + cell_height - 1)}
                for direction in box[char]:
                    draw.rectangle(ends[direction], fill="#" + fg)
            else:
                face = fallback if "\u2190" <= char <= "\u21ff" else bold_font if bold else regular
                draw.text((x + cell_width / 2, y + cell_height / 2), char, anchor="mm",
                          font=face, fill="#" + fg)
    path.parent.mkdir(parents=True, exist_ok=True)
    image.save(path, optimize=True)
    print(path.relative_to(ROOT))


def capture(args, environment, actions, path):
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", HEIGHT, WIDTH, 0, 0))
    process = subprocess.Popen([BINARY, *args], stdin=slave, stdout=slave, stderr=slave,
                               env=dict(environment, TERM="xterm-256color"))
    data = bytearray()
    def wait_for(expected):
        deadline, quiet = time.monotonic() + 5, time.monotonic()
        previous = ""
        while time.monotonic() < deadline:
            if select.select([master], [], [], 0.05)[0]:
                data.extend(os.read(master, 65536))
            text = "\n".join("".join(c[0] for c in line) for line in cells(data))
            if text != previous:
                quiet, previous = time.monotonic(), text
            if expected in text and time.monotonic() - quiet > 0.15:
                return
        raise AssertionError(f"Missing screen content: {expected}")
    try:
        wait_for("Displays")
        for keys, expected in actions:
            os.write(master, keys)
            wait_for(expected)
        render(data, path)
    finally:
        process.terminate()
        process.wait(timeout=5)
        os.close(master)
        os.close(slave)


with tempfile.TemporaryDirectory(prefix="hypr-monitor-capture-") as directory:
    root = Path(directory)
    # Reuse the test compositor without executing the smoke test module.
    tree = ast.parse((ROOT / "tests/process-smoke.py").read_text())
    mock = next(ast.literal_eval(node.value) for node in tree.body
                if isinstance(node, ast.Assign) and any(isinstance(t, ast.Name) and t.id == "MOCK" for t in node.targets))
    (root / "hyprctl").write_text(mock)
    (root / "hyprctl").chmod(0o755)
    environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       XDG_RUNTIME_DIR=str(root), XDG_CONFIG_HOME=str(root / "config"),
                       FAKE_HYPR_ROOT=str(root))
    monitors = [dict(id=i, name=name, model="Demo display", disabled=False,
                     width=1920, height=1080, refreshRate=60, x=i * 1920, y=0,
                     scale=1, transform=0, mirrorOf="none",
                     availableModes=["1920x1080@60Hz", "1920x1080@120Hz", "1280x720@60Hz"])
                for i, name in enumerate(["eDP-1", "DP-1", "HDMI-A-1"])]
    (root / "state.json").write_text(json.dumps(monitors))
    layout = json.loads(subprocess.check_output([BINARY, "--list"], env=environment))
    profiles = root / "config/hypr-monitor-tui/profiles"
    profiles.mkdir(parents=True)
    for name in ["Home", "Office"]:
        (profiles / f"{name}.json").write_text(json.dumps(dict(
            version=2, name=name, created_at="2026-11-01T09:00:00Z",
            updated_at="2026-11-01T14:30:00Z", outputs=layout)))
    capture(["--demo"], environment, [], ROOT / "docs/screenshots/layout.png")
    capture([], environment, [(b"p", "Saved profiles")], ROOT / "docs/screenshots/profiles.png")
