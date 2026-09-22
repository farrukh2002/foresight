"""Owned zsh transports and acceptance, including real autosuggestions widgets."""
from __future__ import annotations

import os
import shlex
import shutil
import subprocess
import tempfile
import unittest

from fixtures import REPO_ROOT, RenderTestCase
from mock_cli import environment, requests
from pty_driver import PtyProcess

FORESIGHT_ZSH = os.path.join(REPO_ROOT, "foresight.zsh")
PLUGIN = os.environ.get("ZSH_AUTOSUGGESTIONS_FILE", "")


@unittest.skipUnless(shutil.which("zsh"), "zsh not installed")
class ForesightZshTests(unittest.TestCase):
    def scenario(self, body, predictions=None, extra=None):
        with tempfile.TemporaryDirectory(prefix="fr-zsh-") as tmp:
            env = environment(tmp, predictions)
            env.update(extra or {})
            script = f"source {shlex.quote(FORESIGHT_ZSH)}\n" + body
            proc = subprocess.run(["zsh", "-f", "-i", "-c", script], cwd=tmp, env=env,
                                  capture_output=True, text=True, timeout=15)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            return proc.stdout, requests(env)

    def test_strategy_returns_full_line(self):
        out, _ = self.scenario('_zsh_autosuggest_strategy_foresight hello; print -r -- "RESULT:$suggestion"')
        self.assertIn("RESULT:hello world", out)

    def test_accept_extend_credits_only_executed_line(self):
        _, log = self.scenario('BUFFER=open; foresight-accept-suggestion; _foresight_preexec opencode2; _foresight_preexec other')
        self.assertEqual([r["payload"] for r in log if r["kind"] == "accept"], ["opencode2"])

    def test_accept_unchanged_credits_executed_line(self):
        _, log = self.scenario('BUFFER=open; foresight-accept-suggestion; _foresight_preexec opencode')
        self.assertEqual([r["payload"] for r in log if r["kind"] == "accept"], ["opencode"])

    def test_edit_away_is_not_credited(self):
        _, log = self.scenario('BUFFER=open; foresight-accept-suggestion; _foresight_preexec ls')
        self.assertEqual([r for r in log if r["kind"] == "accept"], [])
        self.assertEqual([r["payload"] for r in log if r["kind"] == "train"], ["ls"])

    def test_word_accepts_leading_space_and_preserves_quotes(self):
        out, _ = self.scenario(r'''
            BUFFER=echo; foresight-accept-word
            print -r -- "RESULT:$BUFFER:END"
        ''', {"echo": '  "hello world" later'})
        self.assertIn('RESULT:echo  "hello world" :END', out)

    def test_transport_reuse_preserves_multiline_and_trailing_newlines(self):
        out, log = self.scenario(r'''
            _foresight_resolve
            _foresight_predict $'a\tb\nc'; print -r -- "FIRST:$_FORESIGHT_REPLY:END"
            _foresight_predict open; print -r -- "SECOND:$_FORESIGHT_REPLY:END"
        ''', {"a\tb\nc": " \tanswer\n\n", "open": "code"})
        self.assertIn("FIRST: \tanswer\n\n:END", out)
        self.assertIn("SECOND:code:END", out)
        self.assertEqual(len([r for r in log if r["kind"] == "stream-start"]), 1)
        self.assertEqual([r["payload"] for r in log if r["kind"] == "stream"], ["a\tb\nc", "open"])
        self.assertEqual([r for r in log if r["kind"] == "predict"], [])

    def test_empty_reply_is_not_eof(self):
        _, log = self.scenario('_foresight_resolve; _foresight_predict absent; _foresight_predict open')
        self.assertEqual(len([r for r in log if r["kind"] == "stream-start"]), 1)
        self.assertEqual([r for r in log if r["kind"] == "predict"], [])

    def test_eof_and_partial_reply_fall_back_without_live_helper(self):
        for mode in ("eof", "partial"):
            with self.subTest(mode=mode):
                out, log = self.scenario(r'''
                    _foresight_resolve; _foresight_predict open
                    print -r -- "RESULT:$_FORESIGHT_REPLY"
                ''', extra={"MOCK_STREAM_MODE": mode})
                self.assertIn("RESULT:code", out)
                self.assertEqual([r["payload"] for r in log if r["kind"] == "predict"], ["open"])
                for row in log:
                    if row["kind"] == "stream-start":
                        with self.assertRaises(ProcessLookupError):
                            os.kill(row["pid"], 0)

    def test_forked_strategy_does_not_share_or_kill_parent_stream(self):
        out, log = self.scenario(r'''
            _foresight_resolve; _foresight_predict open
            ( _foresight_predict hello; print -r -- "CHILD:$_FORESIGHT_REPLY" )
            _foresight_predict open; print -r -- "PARENT:$_FORESIGHT_REPLY"
        ''')
        self.assertIn("CHILD: world", out)
        self.assertIn("PARENT:code", out)
        streams = [r for r in log if r["kind"] == "stream"]
        self.assertEqual([r["payload"] for r in streams], ["open", "open"])
        self.assertEqual(streams[0]["pid"], streams[1]["pid"])
        self.assertEqual([r["payload"] for r in log if r["kind"] == "predict"], ["hello"])

    def test_cache_invalidates_when_directory_changes(self):
        out, log = self.scenario(r'''
            mkdir child
            _zsh_autosuggest_strategy_foresight hello
            cd child
            _zsh_autosuggest_strategy_foresight hello
            print -r -- "RESULT:$suggestion"
        ''', {"hello": "@cwd"})
        stream = [r for r in log if r["kind"] == "stream"]
        self.assertEqual(len(stream), 2)
        self.assertEqual(stream[1]["cwd"], stream[0]["cwd"] + "/child")
        self.assertIn("RESULT:hello" + stream[1]["cwd"], out)

    def test_preexec_uses_full_unabridged_multiline_command(self):
        _, log = self.scenario(r'''_foresight_preexec short expanded $'printf a |\ncat' ''')
        self.assertEqual([r["payload"] for r in log if r["kind"] == "train"], ["printf a |\ncat"])


@unittest.skipUnless(shutil.which("zsh") and os.path.isfile(PLUGIN),
                     "set ZSH_AUTOSUGGESTIONS_FILE for real-plugin PTY tests")
class ZshPluginTests(RenderTestCase):
    def spawn_zsh(self):
        env = environment(self.home, {"echo hel": "lo world", "echo": '  "hello world" later'})
        env["ZDOTDIR"] = self.home
        with open(os.path.join(self.home, ".zshrc"), "w") as file:
            file.write("PROMPT='$ '\n")
            file.write(f"source {shlex.quote(PLUGIN)}\n")
            file.write(f"source {shlex.quote(FORESIGHT_ZSH)}\n")
        proc = PtyProcess(["zsh", "-d", "-i"], env=env)
        self._procs.append(proc)
        self.assertTrue(proc.wait_for("$", timeout=8), proc.screen.full_text())
        return proc, env

    def test_custom_widgets_reuse_stream_while_async_workers_remain_independent(self):
        proc, env = self.spawn_zsh()
        proc.send("echo", settle=0.3)
        proc.send("\x18\x10", settle=0.4)
        proc.send_key("C-c", settle=0.2)
        proc.send("echo", settle=0.3)
        proc.send("\x18\x10", settle=0.4)
        proc.send_key("Enter", settle=0.6)
        streams = [r for r in requests(env) if r["kind"] == "stream"]
        self.assertEqual([r["payload"] for r in streams], ["echo", "echo"])
        self.assertEqual(streams[0]["pid"], streams[1]["pid"])

    def test_normal_right_arrow_accept_then_extend_credits_execution(self):
        proc, env = self.spawn_zsh()
        proc.send("echo hel", settle=0.8)
        self.assertTrue(proc.wait_for("world", timeout=5), proc.screen.full_text())
        proc.send_key("Right", settle=0.3)
        proc.send("!", settle=0.2)
        proc.send_key("Enter", settle=0.8)
        self.assertEqual([r["payload"] for r in requests(env) if r["kind"] == "accept"],
                         ["echo hello world!"])

    def test_normal_accept_then_edit_away_does_not_credit(self):
        proc, env = self.spawn_zsh()
        proc.send("echo hel", settle=0.8)
        self.assertTrue(proc.wait_for("world", timeout=5), proc.screen.full_text())
        proc.send_key("Right", settle=0.2)
        proc.send("\x01\x0bprintf changed\r", settle=0.8)
        self.assertEqual([r for r in requests(env) if r["kind"] == "accept"], [])

    def test_custom_word_widget_preserves_shell_word(self):
        proc, env = self.spawn_zsh()
        proc.send("echo", settle=0.2)
        proc.send("\x18\x17", settle=0.3)
        proc.send_key("Enter", settle=0.8)
        self.assertIn(["echo", "hello world"], [shlex.split(r["payload"]) for r in requests(env) if r["kind"] == "train"])
