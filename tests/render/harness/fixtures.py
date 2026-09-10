"""Shared test fixtures: sandboxed HOME, generated rcfile, pty spawn helpers."""
from __future__ import annotations

import os
import shutil
import sys
import tempfile
import unittest

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from pty_driver import PtyProcess, Sleep, Resize
from mock_daemon import MockDaemon

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
RENDER_LIB = os.path.join(REPO_ROOT, "src", "render")


class RenderTestCase(unittest.TestCase):
    """Base class for pty-driven render tests.

    Subclasses call `self.spawn(sources=[...], prelude="...")` to get a
    live PtyProcess running bash with a generated --rcfile that sources
    the named fr-* modules (by bare name, e.g. "fr-bind") from
    src/render/, then drive it with `.send_key()` / `.run_script()` and
    assert against `.screen`.
    """

    rows = 24
    cols = 80

    def setUp(self) -> None:
        self.tmp = tempfile.mkdtemp(prefix="fr-test-")
        self.addCleanup(shutil.rmtree, self.tmp, ignore_errors=True)
        self.home = os.path.join(self.tmp, "home")
        os.makedirs(self.home)
        open(os.path.join(self.home, ".sudo_as_admin_successful"), "w").close()
        self._procs: list[PtyProcess] = []
        self._daemon: MockDaemon | None = None
        self.addCleanup(self._cleanup)

    def _cleanup(self) -> None:
        for p in self._procs:
            p.close()
        if self._daemon is not None:
            self._daemon.stop()

    def with_mock_daemon(self, predict: dict[str, str] | None = None) -> MockDaemon:
        sock = os.path.join(self.home, ".local", "share", "foresight", "foresight.sock")
        self._daemon = MockDaemon(sock, predict=predict)
        self._daemon.start()
        return self._daemon

    def write_rcfile(self, sources: list[str] = (), prelude: str = "", epilogue: str = "") -> str:
        path = os.path.join(self.tmp, "rcfile.bash")
        lines = [
            "PS1='$ '",
            "unset PROMPT_COMMAND",
            f"FR_LIB={RENDER_LIB}",
            "export FR_LIB",
            prelude,
        ]
        for name in sources:
            lines.append(f'. "$FR_LIB/{name}"')
        lines.append(epilogue)
        with open(path, "w") as f:
            f.write("\n".join(lines) + "\n")
        return path

    def spawn(self, sources: list[str] = (), prelude: str = "", epilogue: str = "",
              env: dict[str, str] | None = None) -> PtyProcess:
        rcfile = self.write_rcfile(sources, prelude, epilogue)
        full_env = {
            "HOME": self.home,
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "TERM": "xterm-256color",
            "FORESIGHT_BIN": os.path.join(REPO_ROOT, "target", "debug", "foresight"),
            "LC_ALL": "C.UTF-8",
        }
        full_env.update(env or {})
        proc = PtyProcess(
            ["bash", "--noprofile", "--rcfile", rcfile, "-i"],
            env=full_env, rows=self.rows, cols=self.cols,
        )
        self._procs.append(proc)
        proc.drain(timeout=0.3)
        return proc
