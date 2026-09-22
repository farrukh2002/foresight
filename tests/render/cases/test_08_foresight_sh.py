"""foresight.sh integration tests (plain mode and ble.sh gate)."""
from __future__ import annotations

import os
import time
import unittest

import shlex
import subprocess
import tempfile

from mock_cli import environment, requests
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


    def test_pipeline_is_trained_once_before_execution(self):
        proc, daemon = self.spawn_shell(extra_env={"HISTIGNORE": "*", "HISTCONTROL": "ignorespace"})
        self.assertTrue(proc.wait_for("$", timeout=5))
        line = " printf 'pipeline\\n' | cat"
        proc.send(line + "\r", settle=0.8)
        self.assertEqual([r["payload"] for r in daemon.requests if r["kind"] == "T"], [line])

    def test_multiline_compound_input_preserves_newlines(self):
        proc, daemon = self.spawn_shell(extra_env={"HISTIGNORE": "*"})
        self.assertTrue(proc.wait_for("$", timeout=5))
        for line in ("for value in one two; do", 'printf "%s\\n" "$value"', "done"):
            proc.send(line + "\r", settle=0.25)
        proc.drain(timeout=1)
        self.assertEqual([r["payload"] for r in daemon.requests if r["kind"] == "T"],
                         ['for value in one two; do\nprintf "%s\\n" "$value"\ndone'])

    def test_training_precedes_cwd_change(self):
        os.mkdir(os.path.join(self.home, "child"))
        proc, daemon = self.spawn_shell()
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send("cd child\r", settle=0.7)
        trained = [r for r in daemon.requests if r["kind"] == "T"]
        self.assertEqual([(r["payload"], r["cwd"]) for r in trained], [("cd child", self.home)])

    def test_array_prompt_debug_exit_hooks_and_status_survive(self):
        status_log = os.path.join(self.tmp, "status")
        debug_log = os.path.join(self.tmp, "debug")
        exit_log = os.path.join(self.tmp, "exit")
        prelude = (
            f"PROMPT_COMMAND=('printf \"%s\\\\n\" \"$?\" >> {shlex.quote(status_log)}' ':')\n"
            f"trap 'printf debug >> {shlex.quote(debug_log)}' DEBUG\n"
            f"trap 'printf \"%s\" \"$?\" > {shlex.quote(exit_log)}' EXIT\n"
        )
        proc, daemon = self.spawn_shell(rc_prelude=prelude)
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send("false\r", settle=0.5)
        with open(status_log) as file:
            self.assertEqual(file.read().splitlines()[-1], "1")
        with open(debug_log) as file:
            before = len(file.read())
        proc.send("exit 7\r", settle=0.6)
        with open(debug_log) as file:
            self.assertGreater(len(file.read()), before)
        with open(exit_log) as file:
            self.assertEqual(file.read(), "7")
        self.assertEqual([r["payload"] for r in daemon.requests if r["kind"] == "T"], ["false", "exit 7"])

    def test_next_word_includes_leading_space_and_quoted_word(self):
        proc, daemon = self.spawn_shell(predict={"echo": '  "hello world" later'})
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send("echo\x18\x17", settle=0.5)
        proc.send("\r", settle=0.7)
        self.assertEqual([r["payload"] for r in daemon.requests if r["kind"] == "T"],
                         ['echo  "hello world" '])

    def test_vi_acceptance_and_double_source_train_once(self):
        proc, daemon = self.spawn_shell(rc_prelude="set -o vi")
        self.assertTrue(proc.wait_for("$", timeout=5))
        proc.send(f"source {shlex.quote(self.foresight_sh)}\r", settle=0.5)
        proc.send("printf native\r", settle=0.5)
        self.assertEqual([r["payload"] for r in daemon.requests if r["kind"] == "T"],
                         [f"source {shlex.quote(self.foresight_sh)}", "printf native"])

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
            if any(r["kind"] == "T" and "echo marker-one" in r["payload"] for r in daemon.requests):
                break
        self.assertTrue(
            any(r["kind"] == "T" and "echo marker-one" in r["payload"] for r in daemon.requests),
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
            any(r["kind"] == "A" and r["payload"] == "git status" for r in daemon.requests),
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
        accept_lines = [r for r in daemon.requests if r["kind"] == "A"]
        self.assertEqual(
            len(accept_lines), 1,
            f"expected exactly one accept request; "
            f"got accept_lines={accept_lines!r} all_requests={daemon.requests}",
        )
        payload = accept_lines[0]["payload"]
        self.assertEqual(
            payload, "opencode2",
            f"accept payload should be the EXECUTED line, not the "
            f"shorter accepted buffer; got {payload!r}",
        )
        for r in accept_lines:
            self.assertNotEqual(r["payload"], "opencode",
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
        accept_lines = [r for r in daemon.requests if r["kind"] == "A"]
        self.assertEqual(
            accept_lines, [],
            f"expected NO accept request when the user edited away; "
            f"got accept_lines={accept_lines!r} all_requests={daemon.requests}",
        )



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


class BashTransportTests(unittest.TestCase):
    def scenario(self, body, mode=""):
        with tempfile.TemporaryDirectory(prefix="fr-bash-transport-") as tmp:
            env = environment(tmp, {"a\tb\nc": " \tanswer\n\n", "open": "code"})
            env["MOCK_STREAM_MODE"] = mode
            source = shlex.quote(os.path.join(REPO_ROOT, "foresight.sh"))
            proc = subprocess.run(
                ["bash", "--noprofile", "--norc", "-i", "-c", f"source {source}\n{body}"],
                cwd=tmp, env=env, capture_output=True, text=True, timeout=15,
            )
            self.assertEqual(proc.returncode, 0, proc.stderr)
            return proc.stdout, requests(env)

    def test_one_owned_stream_handles_multiline_and_empty_replies(self):
        out, log = self.scenario(r'''
            __foresight_predict $'a\tb\nc'
            printf 'FIRST:%s:END\n' "$__foresight_reply"
            __foresight_predict absent
            printf 'EMPTY:%s:END\n' "$__foresight_reply"
            __foresight_predict open
            printf 'SECOND:%s:END\n' "$__foresight_reply"
        ''')
        self.assertIn("FIRST: \tanswer\n\n:END", out)
        self.assertIn("EMPTY::END", out)
        self.assertIn("SECOND:code:END", out)
        self.assertEqual(len([r for r in log if r["kind"] == "stream-start"]), 1)
        self.assertEqual([r["payload"] for r in log if r["kind"] == "stream"],
                         ["a\tb\nc", "absent", "open"])
        self.assertEqual([r for r in log if r["kind"] == "predict"], [])

    def test_eof_and_incomplete_frames_stop_helper_before_fallback(self):
        for mode in ("eof", "partial"):
            with self.subTest(mode=mode):
                out, log = self.scenario(r'''
                    __foresight_predict open
                    printf 'RESULT:%s\n' "$__foresight_reply"
                ''', mode=mode)
                self.assertIn("RESULT:code", out)
                self.assertEqual([r["payload"] for r in log if r["kind"] == "predict"], ["open"])
                for row in log:
                    if row["kind"] == "stream-start":
                        with self.assertRaises(ProcessLookupError):
                            os.kill(row["pid"], 0)

    def test_noninteractive_source_has_no_side_effects(self):
        with tempfile.TemporaryDirectory(prefix="fr-bash-noninteractive-") as tmp:
            env = environment(tmp)
            source = shlex.quote(os.path.join(REPO_ROOT, "foresight.sh"))
            proc = subprocess.run(["bash", "-c", f"source {source}; printf unchanged"],
                                  cwd=tmp, env=env, capture_output=True, text=True, timeout=5)
            self.assertEqual(proc.stdout, "unchanged")
            self.assertEqual(proc.stderr, "")
            self.assertEqual(requests(env), [])


class BashTransportPtyTests(RenderTestCase):
    def spawn_fake_shell(self, predictions):
        env = environment(self.home, predictions)
        rc = os.path.join(self.tmp, "bashrc")
        with open(rc, "w") as file:
            source = shlex.quote(os.path.join(REPO_ROOT, "foresight.sh"))
            file.write(f"PS1='$ '\nsource {source}\n")
        proc = PtyProcess(["bash", "--noprofile", "--rcfile", rc, "-i"], env=env)
        self._procs.append(proc)
        self.assertTrue(proc.wait_for("$", timeout=5), proc.screen.full_text())
        return proc, env

    def test_real_readline_widgets_reuse_the_owning_shell_helper(self):
        proc, env = self.spawn_fake_shell({"echo hel": "lo world"})
        proc.send("echo hel\x18\x10", settle=0.5)
        self.assertIn("echo hello world", proc.screen.full_text())
        proc.send_key("C-c", settle=0.2)
        proc.send("echo hel\x18\x10", settle=0.5)
        proc.send_key("Enter", settle=0.5)
        log = requests(env)
        stream = [r for r in log if r["kind"] == "stream"]
        self.assertEqual([r["payload"] for r in stream], ["echo hel", "echo hel"])
        self.assertEqual(stream[0]["pid"], stream[1]["pid"])
        self.assertEqual(len([r for r in log if r["kind"] == "stream-start"]), 1)
        self.assertEqual([r for r in log if r["kind"] == "predict"], [])
        self.assertEqual([r["payload"] for r in log if r["kind"] == "train"], ["echo hello world"])

    def test_multiline_paste_and_suffix_reach_the_cli_without_normalization(self):
        prefix = "printf 'a\nb'"
        suffix = " tail\n\n"
        proc, env = self.spawn_fake_shell({prefix: suffix})
        proc.paste(prefix, settle=0.3)
        proc.send("\x18\x10", settle=0.5)
        proc.send_key("Enter", settle=0.5)
        log = requests(env)
        self.assertEqual([r["payload"] for r in log if r["kind"] == "stream"], [prefix])
        self.assertEqual([r["payload"] for r in log if r["kind"] == "train"], [prefix + suffix])
        self.assertEqual([r for r in log if r["kind"] == "predict"], [])

    def test_multiline_pasted_commands_are_captured_as_one_input(self):
        proc, env = self.spawn_fake_shell({})
        line = "printf first\nprintf second"
        proc.paste(line, settle=0.3)
        proc.send_key("Enter", settle=0.5)
        self.assertEqual([r["payload"] for r in requests(env) if r["kind"] == "train"], [line])


if __name__ == "__main__":
    unittest.main()
