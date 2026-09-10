"""Pure-bash subprocess tests for integrations/blesh/foresight.bash.

Each test writes a small bash script that:
- defines ble.sh stubs,
- drops a mock `foresight` binary on PATH,
- sources the real layer file,
- runs one scenario chosen by $1,
- emits one PASS/FAIL line plus a structured dump of the observed state
  to a results file the python side parses.

Stdlib only, no pty — `bash -i` is enough to satisfy the layer's
`[[ $- == *i* ]] || return 0` guard.
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
LAYER = os.path.join(REPO_ROOT, "integrations", "blesh", "foresight.bash")


MOCK_BIN = textwrap.dedent(r"""\
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
            printf '%s|%s\n' "$sub" "$*" >> "$FORESIGHT_LOG"
            ;;
    esac
    exit 0
""").strip() + "\n"


SCENARIO_TEMPLATE = textwrap.dedent("""\
    set +e
    # Sandbox HOME
    export HOME="$TMPDIR/home"
    mkdir -p "$HOME"
    touch "$HOME/.sudo_as_admin_successful"
    # Ensure a clean blesh integration dir so the self-heal branch in
    # foresight.sh doesn't trip (we're not testing foresight.sh here).
    export XDG_DATA_HOME="$TMPDIR/xdg"
    mkdir -p "$XDG_DATA_HOME"

    # Stubs for the ble.sh surface the layer uses. Defined BEFORE
    # sourcing the layer.
    ble/is-function() { declare -F "$1" >/dev/null 2>&1; }
    ble/util/import/eval-after-load() { eval "$2"; }
    # insert <new> before first occurrence of <before> in the bash array
    # whose name is <arr>; leave the array alone if <before> isn't there.
    # Real ble.sh iterates values via the array variable; namerefs let us
    # do the same in a plain bash function.
    ble/array#insert-before() {
        local __before=$2 __new=$3
        declare -n __arr=$1
        local -a __out=()
        local __e __found=0
        for __e in "${__arr[@]}"; do
            if (( ! __found )) && [[ $__e == "$__before" ]]; then
                __out+=( "$__new" )
                __found=1
            fi
            __out+=( "$__e" )
        done
        __arr=( "${__out[@]}" )
    }
    ble/complete/auto-complete/enter() { _TEST_ENTER_ARGS=( "$@" ); return 0; }
    blehook() { _TEST_HOOKS+=( "$1" ); }
    ble-import() { :; }

    # Pretend core-complete has loaded and pre-populated the source list.
    _ble_complete_auto_source=( history syntax )

    # Drop a mock `foresight` on PATH and tell the layer about it.
    mkdir -p "$TMPDIR/bin"
    export FORESIGHT_BIN="$TMPDIR/bin/foresight"
    cp "$MOCK_BIN_PATH" "$FORESIGHT_BIN"
    chmod +x "$FORESIGHT_BIN"
    export FORESIGHT_LOG="$TMPDIR/foresight.log"
    : > "$FORESIGHT_LOG"
    export PATH="$TMPDIR/bin:$PATH"

    # Source the real layer.
    . "$LAYER"

    # Run the chosen scenario.
    ${SCENARIO}

    # Dump observed state for the python side to parse.
    dump_state() {
        echo "==_ble_complete_auto_source=="
        printf '['
        __first=1
        for __e in "${_ble_complete_auto_source[@]}"; do
            (( __first )) || printf ','
            printf '%s' "$__e"
            __first=0
        done
        echo ']'
        echo "==_TEST_HOOKS=="
        printf '['
        __first=1
        for __e in "${_TEST_HOOKS[@]:-}"; do
            (( __first )) || printf ','
            printf '%s' "$__e"
            __first=0
        done
        echo ']'
        echo "==_TEST_ENTER_ARGS=="
        printf '['
        __first=1
        for __e in "${_TEST_ENTER_ARGS[@]:-}"; do
            (( __first )) || printf ','
            printf '%s' "$__e"
            __first=0
        done
        echo ']'
        echo "==pending=="
        printf '%s' "${_ble_contrib_foresight_pending:-}"
        echo
        echo "==accepted=="
        printf '%s' "${_ble_contrib_foresight_accepted:-}"
        echo
        echo "==log=="
        cat "$FORESIGHT_LOG"
        echo "==end=="
    }
    dump_state > "$RESULTS"
""")


def _run_scenario(scenario: str) -> dict[str, object]:
    """Compile and run a scenario script; return the parsed state dict."""
    tmp = tempfile.mkdtemp(prefix="fr-blesh-")
    try:
        mock_path = os.path.join(tmp, "mock_foresight")
        with open(mock_path, "w") as f:
            f.write(MOCK_BIN)
        os.chmod(mock_path, 0o755)

        script_path = os.path.join(tmp, "run.sh")
        results_path = os.path.join(tmp, "results.txt")
        with open(script_path, "w") as f:
            f.write(SCENARIO_TEMPLATE.replace("${SCENARIO}", scenario)
                                       .replace("$LAYER", LAYER)
                                       .replace("$RESULTS", results_path)
                                       .replace("$MOCK_BIN_PATH", mock_path)
                                       .replace("$TMPDIR", tmp))

        env = {
            "HOME": tmp,
            "PATH": "/usr/bin:/bin",
            "LC_ALL": "C.UTF-8",
            "TERM": "dumb",
        }
        proc = subprocess.run(
            ["bash", "-i", script_path],
            env=env, capture_output=True, text=True, timeout=15,
        )
        if proc.returncode != 0:
            raise AssertionError(
                f"scenario {scenario!r} exited {proc.returncode}\n"
                f"stdout: {proc.stdout}\nstderr: {proc.stderr}\n"
                f"script: {open(script_path).read()!r}"
            )
        with open(results_path) as f:
            dump = f.read()
        if "==end==" not in dump:
            raise AssertionError(
                f"scenario {scenario!r} did not produce a complete dump; "
                f"got: {dump!r}\nstderr: {proc.stderr}\n"
                f"script: {open(script_path).read()!r}"
            )
        return _parse_dump(dump)
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _parse_dump(dump: str) -> dict[str, object]:
    """Parse the key==value== / [..] / raw-text blocks from the layer script."""
    out: dict[str, object] = {}
    sections = dump.split("==")
    i = 1
    while i < len(sections) - 1:
        name = sections[i].strip()
        body = sections[i + 1]
        body = body.rstrip("\n")
        if name == "_ble_complete_auto_source" or name == "_TEST_ENTER_ARGS" or name == "_TEST_HOOKS":
            inner = body.strip()
            assert inner.startswith("[") and inner.endswith("]"), f"bad {name}: {body!r}"
            inner = inner[1:-1]
            if inner == "":
                out[name] = []
            else:
                out[name] = inner.split(",")
        elif name == "log":
            out["log"] = body.lstrip("\n")
        elif name == "pending":
            out["pending"] = body.lstrip("\n")
        elif name == "accepted":
            out["accepted"] = body.lstrip("\n")
        i += 2
    return out



SCENARIO_A = textwrap.dedent("""\
    # The eval-after-load stub fires immediately on source, so by the time
    # we get here _ble_complete_auto_source should already have foresight
    # in front of history. Nothing else to do.
    :
""")

SCENARIO_B = textwrap.dedent("""\
    _ble_edit_str="echo hel"
    _ble_edit_ind=${#_ble_edit_str}
    ble/complete/auto-complete/source:foresight
    printf 'source_rc=%d\\n' $? >/dev/null
""")

SCENARIO_C = textwrap.dedent("""\
    _ble_edit_str="echo hello world"
    _ble_edit_ind=5   # mid-line
    _TEST_ENTER_ARGS=()
    ble/complete/auto-complete/source:foresight
    printf 'source_rc=%d\\n' $? >/dev/null
""")

SCENARIO_D = textwrap.dedent("""\
    _ble_edit_str="zzz"
    _ble_edit_ind=${#_ble_edit_str}
    _TEST_ENTER_ARGS=()
    ble/complete/auto-complete/source:foresight
    printf 'source_rc=%d\\n' $? >/dev/null
""")

SCENARIO_E = textwrap.dedent("""\
    # PREEXEC hook was registered at source time; invoking the function
    # directly simulates blehook firing it with the preexec argument.
    ble/contrib/foresight/preexec "ls -la"
""")

SCENARIO_F = textwrap.dedent("""\
    # First do a source call so the pending flag is set to the full
    # predicted line; then a full accept records the accepted buffer
    # into the deferred-credit marker. The actual accept request is
    # only sent when the PREEXEC hook fires for the executed line.
    _ble_edit_str="echo hel"
    _ble_edit_ind=${#_ble_edit_str}
    ble/complete/auto-complete/source:foresight
    _ble_edit_str="echo hello world"
    ble/contrib/foresight/insert-observer
    # Now simulate the user running the same line (so it matches
    # exactly) — the accept request lands at preexec.
    ble/contrib/foresight/preexec "echo hello world"
""")

SCENARIO_G = textwrap.dedent("""\
    # Source call sets pending=full; we then mutate the buffer to a strict
    # prefix of pending before the observer fires — accept must NOT log.
    _ble_edit_str="echo hel"
    _ble_edit_ind=${#_ble_edit_str}
    ble/complete/auto-complete/source:foresight
    _ble_edit_str="echo hel"   # strict prefix of pending "echo hello world"
    ble/contrib/foresight/insert-observer
""")

SCENARIO_H = textwrap.dedent("""\
    # Source the layer file a second time. The
    # `ble/is-function ble/complete/auto-complete/source:foresight`
    # guard at the top should return early BEFORE calling
    # eval-after-load again, so _ble_complete_auto_source still has
    # foresight exactly once.
    . "$LAYER"
""")

SCENARIO_I = textwrap.dedent("""\
    # Accept a ghost, keep typing, run the extended command. The
    # accept request must be deferred to preexec and carry the
    # EXECUTED line (the final command), not the shorter accepted
    # buffer (the dead prefix line).
    _ble_edit_str="open"
    _ble_edit_ind=${#_ble_edit_str}
    ble/complete/auto-complete/source:foresight
    # user accepts the ghost -> buffer becomes "opencode"
    _ble_edit_str="opencode"
    ble/contrib/foresight/insert-observer
    # the marker is recorded but no accept has been sent yet
    printf 'mark_before_preexec=%s\\n' "${_ble_contrib_foresight_accepted:-}" >/dev/null
    # user types one more char and runs "opencode2"
    ble/contrib/foresight/preexec "opencode2"
""")

SCENARIO_J = textwrap.dedent("""\
    # Accept a ghost, run it unchanged. The accept request must be
    # deferred to preexec and carry the EXECUTED line (= the accepted
    # buffer, exact match).
    _ble_edit_str="open"
    _ble_edit_ind=${#_ble_edit_str}
    ble/complete/auto-complete/source:foresight
    _ble_edit_str="opencode"
    ble/contrib/foresight/insert-observer
    ble/contrib/foresight/preexec "opencode"
""")

SCENARIO_K = textwrap.dedent("""\
    # Accept a ghost, then edit it into an unrelated line before
    # running. The marker must be cleared at preexec and NO accept
    # request must be sent (since the executed line does not match
    # the accepted buffer).
    _ble_edit_str="open"
    _ble_edit_ind=${#_ble_edit_str}
    ble/complete/auto-complete/source:foresight
    _ble_edit_str="opencode"
    ble/contrib/foresight/insert-observer
    # the user deleted and retyped, executing something unrelated
    ble/contrib/foresight/preexec "ls"
""")


class BleshLayerTests(unittest.TestCase):
    def test_a_source_list_has_foresight_first(self):
        state = _run_scenario(SCENARIO_A)
        self.assertEqual(
            state["_ble_complete_auto_source"], ["foresight", "history", "syntax"],
            f"unexpected _ble_complete_auto_source: {state['_ble_complete_auto_source']!r}",
        )

    def test_b_source_returns_zero_with_full_enter_args(self):
        state = _run_scenario(SCENARIO_B)
        self.assertEqual(
            state["_TEST_ENTER_ARGS"],
            ["h", "0", "lo world", "", "echo hello world", "echo hello world"],
            f"unexpected _TEST_ENTER_ARGS: {state['_TEST_ENTER_ARGS']!r}",
        )
        self.assertEqual(state["pending"], "echo hello world",
                         f"unexpected pending: {state['pending']!r}")

    def test_c_cursor_midline_returns_one(self):
        state = _run_scenario(SCENARIO_C)
        self.assertEqual(state["_TEST_ENTER_ARGS"], [],
                         f"enter should not have been called; got {state['_TEST_ENTER_ARGS']!r}")
        self.assertEqual(state["pending"], "")

    def test_d_empty_prediction_returns_one(self):
        state = _run_scenario(SCENARIO_D)
        self.assertEqual(state["_TEST_ENTER_ARGS"], [],
                         f"enter should not have been called; got {state['_TEST_ENTER_ARGS']!r}")
        self.assertEqual(state["pending"], "")

    def test_e_preexec_hook_registered_and_trains(self):
        state = _run_scenario(SCENARIO_E)
        preexec_registered = any(h.startswith("PREEXEC+=") for h in state["_TEST_HOOKS"])
        self.assertTrue(preexec_registered,
                        f"PREEXEC hook not registered; got {state['_TEST_HOOKS']!r}")
        log = state["log"]
        self.assertIn("train|ls -la", log,
                      f"train call not in log: {log!r}")

    def test_f_complete_accept_fires_observer(self):
        state = _run_scenario(SCENARIO_F)
        ci_registered = any(h.startswith("complete_insert+=") for h in state["_TEST_HOOKS"])
        self.assertTrue(ci_registered,
                        f"complete_insert hook not registered; got {state['_TEST_HOOKS']!r}")
        self.assertIn("accept|echo hello world", state["log"],
                      f"accept not logged: {state['log']!r}")
        self.assertEqual(state["pending"], "",
                         f"pending should be cleared after observer; got {state['pending']!r}")
        self.assertEqual(state["accepted"], "",
                         f"accepted should be cleared after preexec; got {state['accepted']!r}")

    def test_g_partial_accept_does_not_fire_observer(self):
        state = _run_scenario(SCENARIO_G)
        self.assertNotIn("accept|", state["log"],
                         f"accept should not have been called; log: {state['log']!r}")
        self.assertEqual(state["pending"], "",
                         f"pending should be cleared even on partial; got {state['pending']!r}")

    def test_h_double_source_is_idempotent(self):
        state = _run_scenario(SCENARIO_H)
        count = state["_ble_complete_auto_source"].count("foresight")
        self.assertEqual(count, 1,
                         f"expected exactly one foresight entry, got "
                         f"{state['_ble_complete_auto_source']!r}")

    def test_i_accept_then_extend_credits_executed_line(self):
        state = _run_scenario(SCENARIO_I)
        log = state["log"]
        accept_lines = [l for l in log.splitlines() if l.startswith("accept|")]
        self.assertEqual(
            accept_lines, ["accept|opencode2"],
            f"expected exactly one accept carrying the EXECUTED line; "
            f"got accept_lines={accept_lines!r} full log={log!r}",
        )
        self.assertIn("train|opencode2", log,
                      f"train not logged for executed line; log={log!r}")
        self.assertEqual(state["accepted"], "",
                         f"accepted marker should be cleared; got {state['accepted']!r}")

    def test_j_accept_then_run_unchanged_credits_executed_line(self):
        state = _run_scenario(SCENARIO_J)
        log = state["log"]
        accept_lines = [l for l in log.splitlines() if l.startswith("accept|")]
        self.assertEqual(
            accept_lines, ["accept|opencode"],
            f"expected exactly one accept carrying 'opencode'; "
            f"got accept_lines={accept_lines!r} full log={log!r}",
        )
        self.assertEqual(state["accepted"], "",
                         f"accepted marker should be cleared; got {state['accepted']!r}")

    def test_k_accept_then_edit_away_sends_no_accept(self):
        state = _run_scenario(SCENARIO_K)
        log = state["log"]
        accept_lines = [l for l in log.splitlines() if l.startswith("accept|")]
        self.assertEqual(
            accept_lines, [],
            f"expected NO accept request when the user edited away; "
            f"got accept_lines={accept_lines!r} full log={log!r}",
        )
        self.assertIn("train|ls", log,
                      f"unrelated command should still be trained; log={log!r}")
        self.assertEqual(state["accepted"], "",
                         f"accepted marker should be cleared; got {state['accepted']!r}")


if __name__ == "__main__":
    unittest.main()
