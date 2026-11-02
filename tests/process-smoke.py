#!/usr/bin/env python3
"""Exercise the actual guard process and TUI PTY without touching Hyprland."""
import copy
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


BINARY = Path(__file__).resolve().parents[1] / "target/debug/hypr-monitor-tui"
MOCK = r'''#!/usr/bin/env python3
import json, os, re, sys
from pathlib import Path
root=Path(os.environ['FAKE_HYPR_ROOT'])
state=root/'state.json'
args=sys.argv[1:]
if args==['version']:
    print('Hyprland 0.57.0 (test backend)')
elif args==['-j','monitors','all']:
    print(state.read_text())
elif args[0]=='eval':
    if (root/'fail-next').exists():
        (root/'fail-next').unlink()
        print('simulated partial apply failure')
        sys.exit(1)
    rows=json.loads(state.read_text())
    script=args[1]
    def string(field):
        encoded=re.search(field+r'="([^"]*)"',script).group(1)
        return bytes(int(n) for n in re.findall(r'\\(\d{3})',encoded)).decode()
    row=next(row for row in rows if row['name']==string('output'))
    row['disabled']='disabled=true' in script
    if not row['disabled']:
        width,height,hz=re.match(r'(\d+)x(\d+)@([\d.]+)Hz',string('mode')).groups()
        row['width'],row['height'],row['refreshRate']=int(width),int(height),float(hz)
        row['x'],row['y']=map(int,string('position').split('x'))
        row['scale']=float(re.search(r'scale=([\d.]+)',script).group(1))
        row['transform']=int(re.search(r'transform=(\d+)',script).group(1))
        row['mirrorOf']=string('mirror') or 'none'
    state.write_text(json.dumps(rows))
    print('ok')
else:
    raise SystemExit('unexpected test command')
'''


def line(process, timeout=6):
    assert select.select([process.stdout], [], [], timeout)[0], "guard response timeout"
    response = process.stdout.readline().strip()
    assert response, f"guard exited: {process.poll()}"
    return response


with tempfile.TemporaryDirectory(prefix="hypr-monitor-test-") as directory:
    root = Path(directory)
    executable = root / "hyprctl"
    executable.write_text(MOCK)
    executable.chmod(0o755)
    environment = dict(os.environ, PATH=str(root) + os.pathsep + os.environ["PATH"],
                       XDG_RUNTIME_DIR=str(root), FAKE_HYPR_ROOT=str(root))
    baseline = [{"id": 0, "name": "TEST-1", "model": "Test", "disabled": False,
                 "width": 1920, "height": 1080, "refreshRate": 60, "x": 0, "y": 0,
                 "scale": 1, "transform": 0, "mirrorOf": "none",
                 "availableModes": ["1920x1080@60Hz"]}]

    def reset():
        (root / "state.json").write_text(json.dumps(baseline))

    def request():
        before = json.loads(subprocess.check_output([BINARY, "--list"], env=environment))
        draft = copy.deepcopy(before)
        draft[0]["x"] = 100
        return dict(before=before, draft=draft)

    def start(data):
        process = subprocess.Popen([BINARY, "--guard"], stdin=subprocess.PIPE,
                                   stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                   text=True, env=environment)
        process.stdin.write(json.dumps(data) + "\n")
        process.stdin.flush()
        return process

    def position():
        return json.loads((root / "state.json").read_text())[0]["x"]

    for action in ("CONFIRM", "CANCEL", "EOF", "TIMEOUT"):
        reset()
        process = start(request())
        try:
            assert line(process) == "READY"
            assert position() == 100
            if action == "EOF":
                process.stdin.close()
            elif action != "TIMEOUT":
                process.stdin.write(action + "\n")
                process.stdin.flush()
            expected = "CONFIRMED" if action == "CONFIRM" else "REVERTED"
            assert line(process, 20) == expected
            assert process.wait(timeout=6) == 0
            assert position() == (100 if action == "CONFIRM" else 0)
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
        print(f"PASS guard {action}")

    reset()
    data = request()
    (root / "fail-next").touch()
    process = start(data)
    assert "previous layout restored" in line(process)
    assert process.wait(timeout=6) != 0 and position() == 0
    print("PASS guard failed apply restores")

    reset()
    first = start(request())
    try:
        assert line(first) == "READY"
        second = start(request())
        assert "Another apply transaction" in line(second)
        assert second.wait(timeout=6) != 0
    finally:
        first.stdin.close()
    assert line(first) == "REVERTED"
    assert first.wait(timeout=6) == 0
    print("PASS guard concurrent apply rejected")

    # PTY startup, keyboard edits, draft confirmation, and terminal restoration.
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 110, 0, 0))
    original = termios.tcgetattr(slave)
    process = subprocess.Popen([BINARY, "--demo"], stdin=slave, stdout=slave, stderr=slave,
                               env=dict(environment, TERM="xterm-256color"))
    captured = bytearray()

    def wait_for(expected, timeout=5):
        deadline = time.monotonic() + timeout
        while expected not in captured:
            assert time.monotonic() < deadline, f"PTY missing {expected!r}"
            if select.select([master], [], [], 0.1)[0]:
                captured.extend(os.read(master, 65536))

    try:
        wait_for(b"Displays")
        os.write(master, b"m\x1b[C")
        wait_for(b"reference:")
        os.write(master, b"a")
        # Ratatui may position the cursor between words already on screen.
        wait_for(b"temporarily?")
        os.write(master, b"nqq")  # Cancel review, leave movement, then quit.
        wait_for(b"Discard")
        os.write(master, b"y")
        assert process.wait(timeout=5) == 0
        assert termios.tcgetattr(slave) == original
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)
    print("PASS TUI PTY edits, review, quit, terminal restoration")

    # Real TUI profile workflow against an isolated config and fake compositor.
    reset()
    master, slave = pty.openpty()
    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 34, 110, 0, 0))
    original = termios.tcgetattr(slave)
    process = subprocess.Popen([BINARY], stdin=slave, stdout=slave, stderr=slave,
                               env=dict(environment, TERM="xterm-256color",
                                        XDG_CONFIG_HOME=str(root / "config")))
    captured = bytearray()
    profiles = root / "config/hypr-monitor-tui/profiles"

    def send(data):
        os.write(master, data)

    def screen():
        text = re.sub(r"\x1b\][^\x07]*\x07", "", captured.decode("utf-8", "replace"))
        rows = [[" "] * 110 for _ in range(34)]
        x = y = 0
        for token in re.findall(r"\x1b\[[0-?]*[ -/]*[@-~]|[^\x1b]", text):
            if token.startswith("\x1b["):
                params, command = token[2:-1], token[-1]
                if command in ("H", "f"):
                    parts = [int(n or "1") for n in params.split(";")]
                    y, x = parts[0] - 1, (parts[1] if len(parts) > 1 else 1) - 1
                elif command == "J" and params == "2":
                    rows = [[" "] * 110 for _ in range(34)]
            elif token == "\r":
                x = 0
            elif token == "\n":
                y += 1
            elif 0 <= x < 110 and 0 <= y < 34:
                rows[y][x] = token
                x += 1
        return "\n".join("".join(row) for row in rows)

    def wait_for(expected, timeout=5):
        deadline = time.monotonic() + timeout
        while expected.decode() not in screen():
            assert time.monotonic() < deadline, f"PTY missing {expected!r}\n{screen()}"
            if select.select([master], [], [], 0.05)[0]:
                captured.extend(os.read(master, 65536))

    def click(column, row):
        send(f"\x1b[<0;{column};{row}M\x1b[<0;{column};{row}m".encode())

    def click_action(label):
        wait_for(label.encode())
        for row, text in reversed(list(enumerate(screen().splitlines()))):
            if label in text:
                click(text.index(label) + 1, row + 1)
                return
        raise AssertionError(f"Missing footer action {label}")

    def await_file(path):
        deadline = time.monotonic() + 5
        while not path.exists():
            assert time.monotonic() < deadline, f"Missing profile file: {path.name}"
            if select.select([master], [], [], 0.05)[0]:
                captured.extend(os.read(master, 65536))

    try:
        wait_for(b"Displays")
        send(b"p")
        wait_for(b"Create")
        click_action("Create profile")
        wait_for(b"Name")
        send(b"Casa")
        click_action("Save profile")
        await_file(profiles / "Casa.json")
        wait_for(b"Open/edit")
        click_action("Rename")
        wait_for(b"New name")
        send(b"Oficina\r")
        await_file(profiles / "Oficina.json")
        wait_for(b"Open/edit")
        assert not (profiles / "Casa.json").exists()
        click_action("Open/edit")
        wait_for(b"Editing")
        send(b"\t" + b"\x1b[B" * 6 + b"123\r")
        wait_for(b"updated")
        send(b"p")
        wait_for(b"Create")
        click_action("Save changes")
        await_file(profiles / "Oficina.json.bak")
        assert json.loads((profiles / "Oficina.json").read_text())["outputs"][0]["x"] == 123
        click_action("Activate")
        wait_for(b"temporarily?")
        send(b"nq")
        wait_for(b"Discard")
        send(b"y")
        assert process.wait(timeout=5) == 0
        assert termios.tcgetattr(slave) == original
        assert json.loads((root / "state.json").read_text()) == baseline
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()
        os.close(master)
        os.close(slave)
    print("PASS profile buttons: create, rename, edit, save, activate review; isolated persistence")
