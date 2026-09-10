"""Pty-based process driver for render tests.

Spawns a command under a real pty, replays a keystroke script with
realistic timing, and hands raw output bytes to a VTScreen. Stdlib only.
"""
from __future__ import annotations

import fcntl
import os
import pty
import select
import signal
import struct
import termios
import time
from dataclasses import dataclass

from vt_screen import VTScreen


@dataclass
class Sleep:
    seconds: float


@dataclass
class Resize:
    rows: int
    cols: int


ESC = "\x1b"
CSI = ESC + "["
DSR_CURSOR = b"\x1b[6n"

KEYS = {
    "Enter": "\r",
    "Tab": "\t",
    "Backspace": "\x7f",
    "Esc": ESC,
    "Up": CSI + "A",
    "Down": CSI + "B",
    "Right": CSI + "C",
    "Left": CSI + "D",
    "Home": CSI + "H",
    "End": CSI + "F",
    "C-a": "\x01",
    "C-b": "\x02",
    "C-c": "\x03",
    "C-d": "\x04",
    "C-e": "\x05",
    "C-f": "\x06",
    "C-g": "\x07",
    "C-k": "\x0b",
    "C-l": "\x0c",
    "C-u": "\x15",
    "C-w": "\x17",
    "C-Right": CSI + "1;5C",
    "C-Left": CSI + "1;5D",
    "M-f": ESC + "f",
    "M-b": ESC + "b",
    "PasteStart": CSI + "200~",
    "PasteEnd": CSI + "201~",
}


class PtyProcess:
    def __init__(self, argv: list[str], env: dict[str, str] | None = None, rows: int = 24, cols: int = 80):
        self.screen = VTScreen(rows, cols)
        self._rows, self._cols = rows, cols
        pid, fd = pty.fork()
        if pid == 0:
            os.environ.clear()
            os.environ.update(env or {})
            try:
                os.execvp(argv[0], argv)
            except Exception:
                os._exit(127)
        self.pid = pid
        self.fd = fd
        self._set_winsize(rows, cols)

    def _set_winsize(self, rows: int, cols: int) -> None:
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

    def resize(self, rows: int, cols: int) -> None:
        self._rows, self._cols = rows, cols
        self.screen.resize(rows, cols)
        self._set_winsize(rows, cols)
        os.kill(self.pid, signal.SIGWINCH)

    def drain(self, timeout: float = 0.2, idle: float = 0.15) -> bytes:
        """Read whatever the child emits, up to `timeout` total. Once at
        least one chunk has arrived, stop early after `idle` seconds of
        silence — but never bail out early just because the *first*
        chunk hasn't shown up yet (data can legitimately take a while:
        fr_attach's ~500 bind -x calls, a daemon round-trip, ...).

        The 0.15s default idle matters for asynchronous bash responses:
        a SIGINT trap can take ~30-100ms between the first chunk (e.g.
        a stray "\r\n" before the trap handler runs) and the actual
        redraw bytes that come after, especially when the trap also
        calls `fr_render`. A 30ms idle (the old default) bailed out
        before those later chunks ever arrived, masking redraw bugs."""
        chunks = []
        deadline = time.monotonic() + timeout
        last_data = None
        while time.monotonic() < deadline:
            remaining = deadline - time.monotonic()
            r, _, _ = select.select([self.fd], [], [], min(idle, max(remaining, 0)))
            if self.fd in r:
                try:
                    data = os.read(self.fd, 65536)
                except OSError:
                    break
                if not data:
                    break
                chunks.append(data)
                self.screen.feed(data)
                if b"\x1b[?2004h" in data:
                    self.bracketed_paste = True
                if b"\x1b[?2004l" in data:
                    self.bracketed_paste = False
                n = data.count(DSR_CURSOR)
                if n:
                    row, col = self.screen.cursor()
                    os.write(self.fd, f"\x1b[{row + 1};{col + 1}R".encode() * n)
                last_data = time.monotonic()
            elif last_data is not None and time.monotonic() - last_data >= idle:
                break
        return b"".join(chunks)

    def send(self, data: str, settle: float = 0.05) -> None:
        os.write(self.fd, data.encode("utf-8"))
        self.drain(timeout=settle)

    def paste(self, text: str, settle: float = 0.05) -> None:
        r"""Paste like a real terminal emulator does: bracket the text in
        \e[200~/\e[201~ ONLY while the application has bracketed-paste mode
        armed (\e[?2004h, tracked from its output); raw otherwise."""
        if getattr(self, "bracketed_paste", False):
            data = "\x1b[200~" + text + "\x1b[201~"
        else:
            data = text
        os.write(self.fd, data.encode("utf-8"))
        self.drain(timeout=settle)

    def send_key(self, name: str, settle: float = 0.05) -> None:
        self.send(KEYS[name], settle=settle)

    def wait_for(self, marker: str, timeout: float = 3.0) -> bool:
        """Poll drain() until `marker` shows up in the rendered screen, or
        timeout. Use this instead of a fixed sleep after anything whose
        wall-clock cost varies (e.g. fr_attach's ~500 `bind -x` calls)."""
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            self.drain(timeout=0.1, idle=0.02)
            if marker in self.screen.full_text():
                return True
        return False

    def run_script(self, steps: list) -> None:
        """steps: list of str (raw bytes to send), key names, Sleep, or Resize."""
        for step in steps:
            if isinstance(step, Sleep):
                time.sleep(step.seconds)
                self.drain(timeout=step.seconds * 0.2 + 0.05)
            elif isinstance(step, Resize):
                self.resize(step.rows, step.cols)
                self.drain(timeout=0.1)
            elif step in KEYS:
                self.send_key(step)
            else:
                self.send(step)

    def close(self) -> None:
        try:
            os.kill(self.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        try:
            os.waitpid(self.pid, 0)
        except ChildProcessError:
            pass
        try:
            os.close(self.fd)
        except OSError:
            pass
