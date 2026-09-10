"""Pure-subprocess tests for foresight.zsh.

Each test writes a small zsh script that:
- sets up a sandbox HOME / FORESIGHT_BIN / FORESIGHT_LOG,
- drops a mock `foresight` executable on PATH,
- optionally stubs the zsh-autosuggestions plugin,
- sources the real foresight.zsh,
- runs the scenario, emitting `MARK:<key>=<value>` lines the python side greps for.

`zsh -f -i -c '...'` keeps startup files off the path; the `zmodload
zsh/zle` is a defensive load in case `zle -N` complains in some builds.
Stdlib only.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import sys
import tempfile
import textwrap
import unittest

sys.path.insert(0, os.path.join(os.path.dirname(__file__), "..", "harness"))

REPO_ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", ".."))
FORESIGHT_ZSH = os.path.join(REPO_ROOT, "foresight.zsh")


MOCK_BIN = textwrap.dedent("""\
    #!/usr/bin/env bash
    : "${FORESIGHT_LOG:=/dev/null}"
    sub=$1
    shift || true
    case $sub in
        predict)
            case "$1" in
                "echo hel") printf %s "lo world" ;;
                "hello")    printf %s " world"  ;;
                "open")     printf %s "code"    ;;
                *) ;;
            esac
            ;;
        train|accept|ensure-daemon)
            printf '%s|%s\\n' "$sub" "$*" >> "$FORESIGHT_LOG"
            ;;
    esac
    exit 0
""").strip() + "\n"


def _run_zsh(zsh_body: str) -> tuple[int, str, str]:
    """Run `zsh -f -i -c <body>` with a sandbox env. Returns (rc, stdout, stderr)."""
    with tempfile.TemporaryDirectory(prefix="fr-zsh-") as tmp:
        mock_path = os.path.join(tmp, "foresight")
        with open(mock_path, "w") as f:
            f.write(MOCK_BIN)
        os.chmod(mock_path, 0o755)

        env = {
            "HOME": tmp,
            "PATH": "/usr/bin:/bin",
            "LC_ALL": "C.UTF-8",
            "TERM": "dumb",
            "FORESIGHT_BIN": mock_path,
            "FORESIGHT_LOG": os.path.join(tmp, "foresight.log"),
        }
        cmd = (
            "zmodload zsh/zle 2>/dev/null; "
            + zsh_body
        )
        proc = subprocess.run(
            ["zsh", "-f", "-i", "-c", cmd],
            env=env, capture_output=True, text=True, timeout=15,
        )
        return proc.returncode, proc.stdout, proc.stderr


def _first_value(stdout: str, key: str) -> str | None:
    """Find the first MARK:<key>=<value> line in stdout, return the value."""
    prefix = f"MARK:{key}="
    for line in stdout.splitlines():
        if line.startswith(prefix):
            return line[len(prefix):]
    return None



SCENARIO_A = textwrap.dedent("""\
    . "$ZSH_FILE"
    typeset -g suggestion=
    _zsh_autosuggest_strategy_foresight hello
    print -r -- "MARK:suggestion=$suggestion"
""")

SCENARIO_B = textwrap.dedent("""\
    # Pretend the zsh-autosuggestions plugin is already loaded: it
    # defines _zsh_autosuggest_start and ZSH_AUTOSUGGEST_STRATEGY.
    _zsh_autosuggest_start() { :; }
    ZSH_AUTOSUGGEST_STRATEGY=( history completion )
    . "$ZSH_FILE"
    print -r -- "MARK:strategy1=${ZSH_AUTOSUGGEST_STRATEGY[1]}"
    print -r -- "MARK:rest=${ZSH_AUTOSUGGEST_STRATEGY[@]:1}"
""")

SCENARIO_C = textwrap.dedent("""\
    . "$ZSH_FILE"
    if (( ${+functions[_foresight_preexec]} )); then
        print -r -- "MARK:preexec_defined=_foresight_preexec"
    else
        print -r -- "MARK:preexec_defined="
    fi
    _foresight_preexec "ls -la"
    # Ensure the log is flushed before we read it. Replace newlines with
    # the NUL byte in the MARK payload so the python side can recover
    # the original lines by splitting on NUL.
    print -r -- "MARK:log=$(tr '\\n' '\\0' < "$FORESIGHT_LOG")"
""")

SCENARIO_D = textwrap.dedent("""\
    . "$ZSH_FILE"
    for w in foresight-accept-suggestion foresight-accept-word foresight-fzf-pick; do
        if (( ${+functions[$w]} )); then
            print -r -- "MARK:widget_$w=yes"
        else
            print -r -- "MARK:widget_$w=no"
        fi
    done
    # bindkey '^X^P' is the only keymap we can read back in -i without
    # relying on a keymap name; the layer binds it in the main keymap.
    bindkey_output=$(bindkey '^X^P')
    print -r -- "MARK:bindkey_main=$bindkey_output"
""")


SCENARIO_F = textwrap.dedent("""\
    # Accept a ghost, keep typing, run the extended command. The
    # accept request must be deferred to preexec and carry the
    # EXECUTED line, not the shorter accepted buffer.
    . "$ZSH_FILE"
    BUFFER="open"
    CURSOR=${#BUFFER}
    foresight-accept-suggestion
    print -r -- "MARK:buffer_after_accept=$BUFFER"
    print -r -- "MARK:accepted_after_accept=${_foresight_accepted:-}"
    # preexec fires for the extended line; accepted buffer "opencode"
    # is a prefix of the executed "opencode2", so the match should
    # succeed and the accept should land carrying "opencode2".
    _foresight_preexec "opencode2"
    print -r -- "MARK:accepted_after_preexec=${_foresight_accepted:-}"
    print -r -- "MARK:log=$(tr '\\n' '\\0' < "$FORESIGHT_LOG")"
""")

SCENARIO_G = textwrap.dedent("""\
    # Accept a ghost, run it unchanged. The accept request lands at
    # preexec carrying the EXECUTED line (= the accepted buffer).
    . "$ZSH_FILE"
    BUFFER="open"
    CURSOR=${#BUFFER}
    foresight-accept-suggestion
    print -r -- "MARK:buffer_after_accept=$BUFFER"
    _foresight_preexec "opencode"
    print -r -- "MARK:accepted_after_preexec=${_foresight_accepted:-}"
    print -r -- "MARK:log=$(tr '\\n' '\\0' < "$FORESIGHT_LOG")"
""")

SCENARIO_H = textwrap.dedent("""\
    # Accept a ghost, then edit it into an unrelated line before
    # running. The marker must be cleared at preexec and NO accept
    # request must be sent.
    . "$ZSH_FILE"
    BUFFER="open"
    CURSOR=${#BUFFER}
    foresight-accept-suggestion
    print -r -- "MARK:buffer_after_accept=$BUFFER"
    _foresight_preexec "ls"
    print -r -- "MARK:accepted_after_preexec=${_foresight_accepted:-}"
    print -r -- "MARK:log=$(tr '\\n' '\\0' < "$FORESIGHT_LOG")"
""")



class ForesightZshTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        if not shutil.which("zsh"):
            raise unittest.SkipTest("zsh not installed")

    def _run_with_source(self, body: str) -> tuple[int, str, str]:
        return _run_zsh(f'ZSH_FILE="{FORESIGHT_ZSH}"; {body}')

    def test_a_strategy_predicts_full_line(self):
        rc, out, err = self._run_with_source(SCENARIO_A)
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        self.assertEqual(_first_value(out, "suggestion"), "hello world",
                         f"unexpected suggestion; out={out!r}")

    def test_b_strategy_registered_first_when_plugin_present(self):
        rc, out, err = self._run_with_source(SCENARIO_B)
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        self.assertEqual(_first_value(out, "strategy1"), "foresight",
                         f"strategy not registered first; out={out!r}")
        rest = _first_value(out, "rest")
        self.assertIn("history", rest, f"history missing from {rest!r}")
        self.assertIn("completion", rest, f"completion missing from {rest!r}")
        self.assertNotIn("foresight", rest,
                         f"foresight should be index 1, not in tail; rest={rest!r}")

    def test_c_preexec_defined_and_trains(self):
        rc, out, err = self._run_with_source(
            f'ZSH_FILE="{FORESIGHT_ZSH}"; {SCENARIO_C}'
        )
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        self.assertEqual(_first_value(out, "preexec_defined"), "_foresight_preexec",
                         f"preexec fn not defined; out={out!r}")
        log_raw = _first_value(out, "log")
        self.assertIsNotNone(log_raw, f"log marker missing; out={out!r}")
        log_lines = (log_raw or "").split("\x00")
        self.assertIn("train|ls -la", log_lines,
                      f"train not logged; log={log_raw!r}")

    def test_d_widgets_and_bindings(self):
        rc, out, err = self._run_with_source(
            f'ZSH_FILE="{FORESIGHT_ZSH}"; {SCENARIO_D}'
        )
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        for w in ("foresight-accept-suggestion",
                  "foresight-accept-word",
                  "foresight-fzf-pick"):
            self.assertEqual(_first_value(out, f"widget_{w}"), "yes",
                             f"widget {w} not defined; out={out!r}")
        bk = _first_value(out, "bindkey_main")
        self.assertIsNotNone(bk, f"bindkey marker missing; out={out!r}")
        self.assertIn("foresight-accept-suggestion", bk,
                      f"^X^P not bound to accept-suggestion; got {bk!r}")

    def test_e_syntax_check(self):
        with tempfile.TemporaryDirectory() as tmp:
            env = {"HOME": tmp, "PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8"}
            proc = subprocess.run(
                ["zsh", "-n", FORESIGHT_ZSH],
                env=env, capture_output=True, text=True, timeout=5,
            )
        self.assertEqual(proc.returncode, 0,
                         f"zsh -n {FORESIGHT_ZSH} failed: "
                         f"stdout={proc.stdout!r} stderr={proc.stderr!r}")

    def test_f_widget_accept_then_extend_credits_executed_line(self):
        rc, out, err = self._run_with_source(
            f'ZSH_FILE="{FORESIGHT_ZSH}"; {SCENARIO_F}'
        )
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        self.assertEqual(_first_value(out, "buffer_after_accept"), "opencode",
                         f"widget did not merge suggestion; out={out!r}")
        self.assertEqual(_first_value(out, "accepted_after_accept"), "opencode",
                         f"widget should have recorded accepted buffer; out={out!r}")
        self.assertEqual(_first_value(out, "accepted_after_preexec"), "",
                         f"accepted marker should be cleared after preexec; out={out!r}")
        log_lines = (_first_value(out, "log") or "").split("\x00")
        accept_lines = [l for l in log_lines if l.startswith("accept|")]
        self.assertEqual(
            accept_lines, ["accept|opencode2"],
            f"expected exactly one accept of the EXECUTED line; "
            f"accept_lines={accept_lines!r} log_lines={log_lines!r}",
        )
        for l in accept_lines:
            self.assertFalse(l.endswith("opencode") and not l.endswith("opencode2"),
                             f"dead prefix line was credited; log={log_lines!r}")

    def test_g_widget_accept_then_run_unchanged_credits_executed_line(self):
        rc, out, err = self._run_with_source(
            f'ZSH_FILE="{FORESIGHT_ZSH}"; {SCENARIO_G}'
        )
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        self.assertEqual(_first_value(out, "buffer_after_accept"), "opencode",
                         f"widget did not merge suggestion; out={out!r}")
        self.assertEqual(_first_value(out, "accepted_after_preexec"), "",
                         f"accepted marker should be cleared; out={out!r}")
        log_lines = (_first_value(out, "log") or "").split("\x00")
        accept_lines = [l for l in log_lines if l.startswith("accept|")]
        self.assertEqual(
            accept_lines, ["accept|opencode"],
            f"expected exactly one accept of the EXECUTED line; "
            f"accept_lines={accept_lines!r} log_lines={log_lines!r}",
        )

    def test_h_widget_accept_then_edit_away_sends_no_accept(self):
        rc, out, err = self._run_with_source(
            f'ZSH_FILE="{FORESIGHT_ZSH}"; {SCENARIO_H}'
        )
        self.assertEqual(rc, 0, f"zsh exited {rc}; stderr: {err}")
        self.assertEqual(_first_value(out, "buffer_after_accept"), "opencode",
                         f"widget did not merge suggestion; out={out!r}")
        self.assertEqual(_first_value(out, "accepted_after_preexec"), "",
                         f"accepted marker should be cleared; out={out!r}")
        log_lines = (_first_value(out, "log") or "").split("\x00")
        accept_lines = [l for l in log_lines if l.startswith("accept|")]
        self.assertEqual(
            accept_lines, [],
            f"expected NO accept when the user edited away; "
            f"accept_lines={accept_lines!r} log_lines={log_lines!r}",
        )
        self.assertIn("train|ls", log_lines,
                      f"unrelated command should still be trained; log={log_lines!r}")


if __name__ == "__main__":
    unittest.main()
