"""foresight.sh integration tests (plain mode and ble.sh gate)."""
from __future__ import annotations

import os
import time
import unittest

from pty_driver import PtyProcess, Sleep
from fixtures import REPO_ROOT, RenderTestCase


class ForesightShTests(RenderTestCase):
    def setUp(self) -> None:
        super().setUp()
        self.foresight_sh = os.path.join(REPO_ROOT, "foresight.sh")
        self.bin = os.path.join(REPO_ROOT, "target", "debug", "foresight")

    def last_row(self, proc) -> tuple[int, str]:
        last = 0
        for i in range(proc.screen.rows):
            if proc.screen.row_text(i).rstrip():
                last = i
        return last, proc.screen.row_text(last)

    def spawn_shell(self, extra_env: dict[str, str] | None = None,
                    rc_prelude: str = "",
                    predict: dict[str, str] | None = None):
        if predict is None:
            predict = {"gi": "t status"}
        daemon = self.with_mock_daemon(predict=predict)
        rc = os.path.join(self.tmp, "bashrc")
        with open(rc, "w") as f:
            f.write("PS1='$ '\n" + rc_prelude + f"\n. {self.foresight_sh}\n")
        env = {
            "HOME": self.home,
            "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
            "TERM": "xterm-256color",
            "LC_ALL": "C.UTF-8",
            "FORESIGHT_BIN": self.bin,
            "FORESIGHT_SOCK": daemon.sock_path,
        }
        env.update(extra_env or {})
        proc = PtyProcess(
            ["bash", "--noprofile", "--rcfile", rc, "-i"],
            env=env, rows=self.rows, cols=self.cols,
        )
        self._procs.append(proc)
        return proc, daemon


    def test_histignored_command_still_trains(self):
        proc, daemon = self.spawn_shell(
            extra_env={
                "FORESIGHT_SHELL_ACTIVE": "1",
                "HISTIGNORE": "echo*",
                "HISTCONTROL": "ignorespace",
            },
        )
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send(" echo marker-one\r", settle=1.0)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            proc.drain(timeout=0.2)
            if any(r.startswith("T\t") and "echo marker-one" in r for r in daemon.requests):
                break
        self.assertTrue(
            any(r.startswith("T\t") and "echo marker-one" in r for r in daemon.requests),
            f"suppressed command never trained; requests: {daemon.requests}",
        )

    def test_ctrl_x_ctrl_p_appends_suggestion(self):
        proc, daemon = self.spawn_shell(
            extra_env={"FORESIGHT_SHELL_ACTIVE": "1"},
        )
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send("gi", settle=0.3)
        proc.send("\x18\x10", settle=0.8)
        _, text = self.last_row(proc)
        self.assertTrue(
            text.rstrip().endswith("$ git status"),
            f"plain-mode accept failed, got {text!r}",
        )
        proc.send("\r", settle=1.0)
        self.assertTrue(
            any(r.startswith("A\t") and "git status" in r for r in daemon.requests),
            f"accept was not reported to the daemon; requests: {daemon.requests}",
        )

    def test_ctrl_x_ctrl_p_accept_then_extend_credits_executed_line(self):
        proc, daemon = self.spawn_shell(
            extra_env={"FORESIGHT_SHELL_ACTIVE": "1"},
            predict={"open": "code"},
        )
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send("open", settle=0.3)
        proc.send("\x18\x10", settle=0.8)
        _, text = self.last_row(proc)
        self.assertTrue(
            text.rstrip().endswith("$ opencode"),
            f"plain-mode accept failed, got {text!r}",
        )
        proc.send("2", settle=0.3)
        proc.send("\r", settle=1.0)
        accept_lines = [r for r in daemon.requests if r.startswith("A\t")]
        self.assertEqual(
            len(accept_lines), 1,
            f"expected exactly one accept request; "
            f"got accept_lines={accept_lines!r} all_requests={daemon.requests}",
        )
        payload = accept_lines[0].rsplit("\t", 1)[-1]
        self.assertEqual(
            payload, "opencode2",
            f"accept payload should be the EXECUTED line, not the "
            f"shorter accepted buffer; got {payload!r}",
        )
        for r in accept_lines:
            self.assertFalse(r.endswith("\topencode"),
                             f"dead prefix line was credited; requests: {daemon.requests}")

    def test_ctrl_x_ctrl_p_accept_then_edit_away_sends_no_accept(self):
        proc, daemon = self.spawn_shell(
            extra_env={"FORESIGHT_SHELL_ACTIVE": "1"},
            predict={"open": "code"},
        )
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send("open", settle=0.3)
        proc.send("\x18\x10", settle=0.8)
        _, text = self.last_row(proc)
        self.assertTrue(
            text.rstrip().endswith("$ opencode"),
            f"plain-mode accept failed, got {text!r}",
        )
        proc.send("\x01", settle=0.1)
        proc.send("\x0b", settle=0.1)
        proc.send("ls", settle=0.2)
        proc.send("\r", settle=1.0)
        accept_lines = [r for r in daemon.requests if r.startswith("A\t")]
        self.assertEqual(
            accept_lines, [],
            f"expected NO accept request when the user edited away; "
            f"got accept_lines={accept_lines!r} all_requests={daemon.requests}",
        )


    def test_blesh_active_stands_down(self):
        record = os.path.join(self.tmp, "ble_import.log")
        probe = os.path.join(self.tmp, "probe.txt")
        prelude = (
            f'_ble_base="/tmp/fake-ble"\n'
            f'ble-import() {{ echo "$1" >> "{record}"; }}\n'
        )
        proc, _daemon = self.spawn_shell(
            extra_env={"FORESIGHT_SHELL_ACTIVE": "1"},
            rc_prelude=prelude,
        )
        self.assertTrue(proc.wait_for("$", timeout=5))
        cmd = (
            '{ '
            f'bind -Xp > "{probe}"; '
            f'trap -p DEBUG >> "{probe}"; '
            f'printf "PROMPT_COMMAND=%s\\n" "$PROMPT_COMMAND" >> "{probe}"; '
            '} >/dev/null 2>&1; '
            f'echo done > "{probe}.done"'
        )
        proc.send(cmd + "\r", settle=1.0)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            proc.drain(timeout=0.2)
            if os.path.exists(probe + ".done"):
                break
        self.assertTrue(os.path.exists(probe + ".done"),
                        f"probe never finished; screen={proc.screen.full_text()!r}")
        with open(probe) as f:
            text = f.read()
        for token in ("__foresight_accept_suggestion",
                      "__foresight_accept_word",
                      "__foresight_fzf_pick",
                      "__foresight_debug",
                      "__foresight_pc_enter",
                      "__foresight_pc_exit"):
            self.assertNotIn(token, text,
                             f"{token!r} should not be present under ble.sh; probe: {text!r}")
        for line in text.splitlines():
            if line.startswith("PROMPT_COMMAND="):
                self.assertNotIn("__foresight_pc_enter", line,
                                 f"PROMPT_COMMAND wrapper installed under ble.sh: {line!r}")
        with open(record) as f:
            calls = [line.strip() for line in f if line.strip()]
        self.assertIn("integration/foresight", calls,
                      f"ble-import was not called with integration/foresight; got: {calls!r}")

    def test_blesh_gate_selfheals_layer(self):
        proc, _daemon = self.spawn_shell(
            extra_env={"FORESIGHT_SHELL_ACTIVE": "1"},
            rc_prelude='_ble_base="/tmp/fake-ble"\nble-import() { :; }\n',
        )
        self.assertTrue(proc.wait_for("$", timeout=10))
        time.sleep(0.5)
        proc.drain(timeout=0.3)
        layer = os.path.join(
            self.home, ".local", "share", "blesh", "local", "integration",
            "foresight.bash",
        )
        self.assertTrue(
            os.path.isfile(layer),
            f"foresight init --quiet did not materialize layer file at {layer}",
        )
        with open(layer) as f:
            materialized = f.read()
        with open(os.path.join(REPO_ROOT, "integrations", "blesh", "foresight.bash")) as f:
            original = f.read()
        self.assertEqual(materialized, original,
                         "materialized layer differs from the in-repo source")


if __name__ == "__main__":
    unittest.main()
