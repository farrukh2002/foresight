"""ble.sh editor-state regressions plus actual-plugin PTY acceptance tests."""
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

LAYER = os.path.join(REPO_ROOT, "integrations", "blesh", "foresight.bash")
BLE_SH = os.environ.get("BLE_SH", os.path.expanduser("~/.local/share/blesh/ble.sh"))

# The seam models the upstream core-complete.sh contract: enter inserts the
# ghost in _ble_edit_str WITHOUT advancing _ble_edit_ind. Partial acceptance
# advances only that cursor and fires complete_insert with the ghost intact.
EDITOR = r'''
ble/is-function() { declare -F "$1" >/dev/null; }
ble/util/import/eval-after-load() { eval "$2"; }
ble/array#insert-before() { :; }
blehook() {
    local name=${1%%+=*} value=${1#*+=}
    declare -ga "hooks_$name"
    local -n hook="hooks_$name"
    hook+=("$value")
}
fire() {
    local -n hook="hooks_$1"
    shift
    local fn
    for fn in "${hook[@]}"; do "$fn" "$@"; done
}
ble/complete/auto-complete/enter() {
    _ble_edit_str=${_ble_edit_str:0:_ble_edit_ind}$3${_ble_edit_str:_ble_edit_ind}
    _ble_edit_mark=$((_ble_edit_ind+${#3}))
    _ble_edit_mark_active=auto_complete
}
offer() {
    _ble_edit_str=$1
    _ble_edit_ind=${#1}
    ble/complete/auto-complete/source:foresight
}
commit() {
    local comp_text=${_ble_edit_str:0:_ble_edit_ind} insert=$_ble_edit_str
    _ble_edit_ind=${#_ble_edit_str}
    fire complete_insert
    _ble_edit_mark_active=
}
'''


class BleshLayerTests(unittest.TestCase):
    def scenario(self, body, predictions=None):
        with tempfile.TemporaryDirectory(prefix="fr-blesh-") as tmp:
            env = environment(tmp, predictions)
            script = EDITOR + f"\nsource {shlex.quote(LAYER)}\n" + body + "\nfire EXIT\n"
            proc = subprocess.run(["bash", "--noprofile", "--norc", "-i", "-c", script],
                                  env=env, cwd=tmp, capture_output=True, text=True, timeout=15)
            self.assertEqual(proc.returncode, 0, proc.stderr)
            return proc.stdout, requests(env)

    def test_full_accept_then_extend_credits_executed_line(self):
        _, log = self.scenario('offer open; commit; fire PREEXEC opencode2; fire PREEXEC other')
        self.assertEqual([r["payload"] for r in log if r["kind"] == "accept"], ["opencode2"])
        self.assertEqual([r["payload"] for r in log if r["kind"] == "train"], ["opencode2", "other"])

    def test_full_accept_then_run_unchanged(self):
        _, log = self.scenario('offer open; commit; fire PREEXEC opencode')
        self.assertEqual([r["payload"] for r in log if r["kind"] == "accept"], ["opencode"])

    def test_edit_away_does_not_receive_credit(self):
        _, log = self.scenario('offer open; commit; fire PREEXEC ls')
        self.assertEqual([r for r in log if r["kind"] == "accept"], [])
        self.assertEqual([r["payload"] for r in log if r["kind"] == "train"], ["ls"])

    def test_partial_cursor_with_full_ghost_is_not_full_acceptance(self):
        out, log = self.scenario(r'''
            offer 'echo hel'
            _ble_edit_ind=11
            fire complete_insert
            printf 'VISIBLE:%s\n' "$_ble_edit_str"
            fire PREEXEC 'echo hello '
        ''')
        self.assertIn("VISIBLE:echo hello world", out)
        self.assertEqual([r for r in log if r["kind"] == "accept"], [])

    def test_partial_then_full_commit_gets_credit(self):
        _, log = self.scenario(r'''
            offer 'echo hel'; _ble_edit_ind=11; fire complete_insert
            commit; fire PREEXEC 'echo hello world'
        ''')
        self.assertEqual([r["payload"] for r in log if r["kind"] == "accept"], ["echo hello world"])

    def test_manually_typing_the_last_ghost_character_is_not_acceptance(self):
        _, log = self.scenario(r'''
            offer 'echo hel'
            _ble_edit_ind=${#_ble_edit_str}
            comp_text= insert=d fire complete_insert
            fire PREEXEC 'echo hello world'
        ''')
        self.assertEqual([r for r in log if r["kind"] == "accept"], [])

    def test_multiline_transport_reuses_one_helper_without_spawn_fallback(self):
        out, log = self.scenario(r'''
            _ble_contrib_foresight_predict $'a\tb\nc'
            printf 'FIRST:%s:END\n' "$_ble_contrib_foresight_reply"
            _ble_contrib_foresight_predict open
            printf 'SECOND:%s:END\n' "$_ble_contrib_foresight_reply"
        ''', {"a\tb\nc": " \tvalue\n\n", "open": "code"})
        self.assertIn("FIRST: \tvalue\n\n:END", out)
        self.assertIn("SECOND:code:END", out)
        self.assertEqual(len([r for r in log if r["kind"] == "stream-start"]), 1)
        self.assertEqual([r["payload"] for r in log if r["kind"] == "stream"], ["a\tb\nc", "open"])
        self.assertEqual([r for r in log if r["kind"] == "predict"], [])

    def test_cache_is_scoped_to_cwd(self):
        out, log = self.scenario(r'''
            mkdir child
            offer hello
            cd child
            offer hello
            printf 'VISIBLE:%s\n' "$_ble_edit_str"
        ''', {"hello": "@cwd"})
        stream = [r for r in log if r["kind"] == "stream"]
        self.assertEqual(len(stream), 2)
        self.assertEqual(stream[1]["cwd"], stream[0]["cwd"] + "/child")
        self.assertIn("VISIBLE:hello" + stream[1]["cwd"], out)

    def test_entry_and_layer_share_session_but_new_shell_does_not(self):
        out, log = self.scenario(f'''
            first=$FORESIGHT_SESSION
            source {shlex.quote(LAYER)}
            printf 'SAME:%s\\n' "$([[ $first == "$FORESIGHT_SESSION" ]] && echo yes)"
            fire PREEXEC first
            bash --noprofile --norc -i -c 'source {shlex.quote(os.path.join(REPO_ROOT, "foresight.sh"))}; "$FORESIGHT_BIN" train child'
        ''')
        self.assertIn("SAME:yes", out)
        lines = [r for r in log if r["kind"] == "train"]
        self.assertEqual([r["payload"] for r in lines], ["first", "child"])
        self.assertNotEqual(lines[0]["session"], lines[1]["session"])


@unittest.skipUnless(os.path.isfile(BLE_SH), "set BLE_SH to run actual ble.sh PTY tests")
class BleshPluginTests(RenderTestCase):
    def spawn_ble(self):
        env = environment(self.home, {"echo hel": "lo world"})
        layer_dir = os.path.join(self.home, ".local", "share", "blesh", "local", "integration")
        os.makedirs(layer_dir, exist_ok=True)
        shutil.copyfile(LAYER, os.path.join(layer_dir, "foresight.bash"))
        rc = os.path.join(self.tmp, "bashrc")
        with open(rc, "w") as file:
            file.write(f"PS1='$ '\nsource {shlex.quote(BLE_SH)} --noattach\n")
            file.write("bleopt complete_auto_delay=20\n")
            entry = shlex.quote(os.path.join(REPO_ROOT, "foresight.sh"))
            file.write(f"source {entry}\nsource {entry}\nble-attach\n")
        proc = PtyProcess(["bash", "--noprofile", "--rcfile", rc, "-i"], env=env)
        self._procs.append(proc)
        self.assertTrue(proc.wait_for("$", timeout=10), proc.screen.full_text())
        return proc, env

    def test_real_partial_ghost_is_not_credited_as_full(self):
        proc, env = self.spawn_ble()
        proc.send("echo hel", settle=1)
        self.assertTrue(proc.wait_for("world", timeout=5), proc.screen.full_text())
        proc.send_key("M-f", settle=0.3)
        proc.send_key("Enter", settle=0.8)
        log = requests(env)
        self.assertEqual([r for r in log if r["kind"] == "accept"], [])
        self.assertIn(["echo", "hello"], [shlex.split(r["payload"]) for r in log if r["kind"] == "train"])

    def test_real_full_accept_then_extend_is_credited(self):
        proc, env = self.spawn_ble()
        proc.send("echo hel", settle=1)
        self.assertTrue(proc.wait_for("world", timeout=5), proc.screen.full_text())
        proc.send_key("End", settle=0.3)
        proc.send("!", settle=0.2)
        proc.send_key("Enter", settle=0.8)
        self.assertEqual([r["payload"] for r in requests(env) if r["kind"] == "accept"],
                         ["echo hello world!"])
