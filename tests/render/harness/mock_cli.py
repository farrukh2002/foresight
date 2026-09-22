"""Sandbox CLI fake with the real NUL stream contract and observable process IDs."""
from __future__ import annotations

import json
import os
import pwd
import sys

MOCK_BIN = r'''#!/usr/bin/env python3
import json
import os
import sys
import time


def record(kind, payload="", cwd=None):
    data = {"kind": kind, "payload": payload, "cwd": cwd or os.getcwd(),
            "pid": os.getpid(), "session": os.environ.get("FORESIGHT_SESSION", "")}
    fd = os.open(os.environ["FORESIGHT_LOG"], os.O_WRONLY | os.O_CREAT | os.O_APPEND, 0o600)
    os.write(fd, (json.dumps(data) + "\n").encode())
    os.close(fd)


def prediction(payload, cwd):
    value = json.loads(os.environ["MOCK_PREDICTIONS"]).get(payload, "")
    return cwd if value == "@cwd" else value


def field():
    out = bytearray()
    while True:
        byte = sys.stdin.buffer.read(1)
        if not byte:
            return None
        if byte == b"\0":
            return out.decode()
        out.extend(byte)


kind = sys.argv[1]
if kind == "stream":
    record("stream-start")
    while True:
        cwd = field()
        if cwd is None:
            break
        payload = field()
        if payload is None:
            break
        record("stream", payload, cwd)
        mode = os.environ.get("MOCK_STREAM_MODE", "")
        if mode == "eof":
            break
        if mode == "partial":
            sys.stdout.buffer.write(b"incomplete")
            sys.stdout.buffer.flush()
            time.sleep(10)
            break
        sys.stdout.buffer.write(prediction(payload, cwd).encode() + b"\0")
        sys.stdout.buffer.flush()
    record("stream-end")
elif kind == "predict":
    record(kind, sys.argv[2])
    sys.stdout.write(prediction(sys.argv[2], os.getcwd()))
elif kind == "list":
    record(kind, sys.argv[-1])
    sys.stdout.buffer.write(prediction(sys.argv[-1], os.getcwd()).encode() + b"\0")
else:
    record(kind, sys.argv[2] if len(sys.argv) > 2 else "")
'''


def environment(tmp: str, predictions: dict[str, str] | None = None) -> dict[str, str]:
    path = os.path.join(tmp, "foresight")
    with open(path, "w") as file:
        file.write(MOCK_BIN.replace("#!/usr/bin/env python3", f"#!{sys.executable}", 1))
    os.chmod(path, 0o755)
    log = os.path.join(tmp, "foresight.jsonl")
    open(log, "w").close()
    user = pwd.getpwuid(os.getuid()).pw_name
    return {
        "HOME": tmp, "PATH": os.environ.get("PATH", "/usr/bin:/bin"), "LC_ALL": "C.UTF-8",
        "USER": user, "LOGNAME": user,
        "TERM": "xterm-256color", "FORESIGHT_BIN": path, "FORESIGHT_LOG": log,
        "MOCK_PREDICTIONS": json.dumps(predictions if predictions is not None else {
            "echo hel": "lo world", "hello": " world", "open": "code",
        }),
    }


def requests(env: dict[str, str]) -> list[dict]:
    with open(env["FORESIGHT_LOG"]) as file:
        return [json.loads(line) for line in file]
