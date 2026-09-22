"""Fake foresight daemon: binds the real unix-socket protocol so fr-ac
(and the real `foresight` CLI, unmodified) can be tested without the
actual Rust daemon or model. Canned predict replies only.

Wire protocol: one JSON object per line with kind/cwd/session/payload fields.
Replies are Result objects: {"Ok": string} or {"Ok": [tagged suggestions]}.
"""
from __future__ import annotations

import json
import os
import socket
import threading


class MockDaemon:
    def __init__(self, sock_path: str, predict: dict[str, str] | None = None):
        self.sock_path = sock_path
        self.predict = predict or {}
        self.requests: list[dict[str, str]] = []
        self._srv: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()

    def __enter__(self) -> "MockDaemon":
        self.start()
        return self

    def __exit__(self, *exc) -> None:
        self.stop()

    def start(self) -> None:
        os.makedirs(os.path.dirname(self.sock_path), exist_ok=True)
        if os.path.exists(self.sock_path):
            os.unlink(self.sock_path)
        self._srv = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self._srv.bind(self.sock_path)
        self._srv.listen(8)
        self._srv.settimeout(0.2)
        self._thread = threading.Thread(target=self._loop, daemon=True)
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        if self._thread:
            self._thread.join(timeout=2)
        if self._srv:
            self._srv.close()
        if os.path.exists(self.sock_path):
            os.unlink(self.sock_path)

    def _loop(self) -> None:
        while not self._stop.is_set():
            try:
                conn, _ = self._srv.accept()
            except (socket.timeout, OSError):
                continue
            threading.Thread(target=self._handle, args=(conn,), daemon=True).start()

    def _handle(self, conn: socket.socket) -> None:
        conn.settimeout(1.0)
        buf = b""
        try:
            while not self._stop.is_set():
                try:
                    chunk = conn.recv(4096)
                except socket.timeout:
                    continue
                if not chunk:
                    return
                buf += chunk
                while b"\n" in buf:
                    line, buf = buf.split(b"\n", 1)
                    reply = self._reply(json.loads(line))
                    conn.sendall((json.dumps({"Ok": reply}) + "\n").encode("utf-8"))
        except OSError:
            pass
        finally:
            conn.close()

    def _reply(self, request: dict[str, str]) -> str | list[str]:
        self.requests.append(request)
        kind = request["kind"]
        payload = request["payload"]
        if kind == "PING":
            return "PONG"
        if kind in ("T", "A"):
            return "OK"
        if kind == "P":
            suggestion = self.predict.get(payload, "")
            return f"P:{suggestion}" if suggestion else ""
        if kind == "PL":
            suggestion = self.predict.get(payload, "")
            return [f"P:{suggestion}"] if suggestion else []
        return ""
