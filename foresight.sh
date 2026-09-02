FORESIGHT_BIN="${FORESIGHT_BIN:-$HOME/.local/bin/foresight}"
if [ ! -x "$FORESIGHT_BIN" ]; then
    _cand=$(command -v foresight 2>/dev/null)
    [ -n "$_cand" ] && [ -x "$_cand" ] && FORESIGHT_BIN="$_cand"
    unset _cand
fi

FORESIGHT_SESSION="$$-${RANDOM}"
export FORESIGHT_SESSION

if [[ -n "${_ble_base:-}" ]]; then
    _foresight_layer_dir="${XDG_DATA_HOME:-$HOME/.local/share}/blesh/local/integration"
    if [ ! -f "$_foresight_layer_dir/foresight.bash" ] && [ -x "$FORESIGHT_BIN" ]; then
        "$FORESIGHT_BIN" init --quiet >/dev/null 2>&1
    fi
    unset _foresight_layer_dir
    declare -F ble-import >/dev/null 2>&1 && ble-import integration/foresight 2>/dev/null
    return 0 2>/dev/null || exit 0
fi

__foresight_in_prompt_command=0

__foresight_debug() {
    [[ ${BASH_COMMAND:0:12} == __foresight_ ]] && return
    [[ $BASH_COMMAND == "$FORESIGHT_BIN"* ]] && return
    (( __foresight_in_prompt_command )) && return
    (( ${#FUNCNAME[@]} > 1 )) && return
    "$FORESIGHT_BIN" train "$BASH_COMMAND" >/dev/null 2>&1
    local accepted=${__foresight_accepted:-}
    __foresight_accepted=
    if [[ -n $accepted ]]; then
        if [[ $BASH_COMMAND == "$accepted" || $BASH_COMMAND == "$accepted"* || "$accepted" == "$BASH_COMMAND"* ]]; then
            "$FORESIGHT_BIN" accept "$BASH_COMMAND" >/dev/null 2>&1
        fi
    fi
}

trap '__foresight_debug' DEBUG

__foresight_pc_enter() { __foresight_in_prompt_command=1; }
__foresight_pc_exit() { __foresight_in_prompt_command=0; }
PROMPT_COMMAND="__foresight_pc_enter${PROMPT_COMMAND:+; $PROMPT_COMMAND}; __foresight_pc_exit"

__foresight_accepted=

__foresight_accept_suggestion() {
    local suggestion
    suggestion=$(__foresight_predict "$READLINE_LINE")
    if [ -n "$suggestion" ]; then
        READLINE_LINE="${READLINE_LINE}${suggestion}"
        READLINE_POINT=${#READLINE_LINE}
        __foresight_accepted=$READLINE_LINE
    fi
}

__foresight_accept_word() {
    local suggestion word
    suggestion=$(__foresight_predict "$READLINE_LINE")
    [ -z "$suggestion" ] && return
    word=${suggestion%%[[:space:]]*}
    [ -z "$word" ] && return
    READLINE_LINE="${READLINE_LINE}${word}"
    READLINE_POINT=${#READLINE_LINE}
    if [[ "$suggestion" == *[[:space:]]* ]]; then
        READLINE_LINE="${READLINE_LINE} "
        READLINE_POINT=${#READLINE_LINE}
    fi
}

__foresight_fzf_pick() {
    command -v fzf >/dev/null 2>&1 || return
    local pick
    pick=$($FORESIGHT_BIN list "$READLINE_LINE" 2>/dev/null | fzf --height 40% --reverse --no-sort --prompt '> ')
    [ -n "$pick" ] || return
    READLINE_LINE="${READLINE_LINE}${pick}"
    READLINE_POINT=${#READLINE_LINE}
    __foresight_accepted=$READLINE_LINE
}

$FORESIGHT_BIN ensure-daemon >/dev/null 2>&1

__foresight_coproc_alive=0
__foresight_coproc_pid=
__foresight_coproc_in=
__foresight_coproc_out=

__foresight_predict_spawn() { $FORESIGHT_BIN predict "$1" 2>/dev/null; }

__foresight_coproc_kill() {
    __foresight_coproc_alive=0
    if [ -n "$__foresight_coproc_pid" ]; then
        kill "$__foresight_coproc_pid" 2>/dev/null
        __foresight_coproc_pid=
    fi
    __foresight_coproc_in=
    __foresight_coproc_out=
}

__foresight_predict() {
    if [ "$__foresight_coproc_alive" = "1" ]; then
        if {
            printf '%s\t%s\n' "$PWD" "$1" >&${__foresight_coproc_in:-1}
        } 2>/dev/null && {
            IFS= read -r -t 1 reply <&${__foresight_coproc_out:-0}
        } 2>/dev/null; then
            printf '%s' "$reply"
            return 0
        fi
        __foresight_coproc_kill
    fi
    if coproc __foresight_coproc {
        FORESIGHT_CWD="${PWD:-}" exec "$FORESIGHT_BIN" stream
    } 2>/dev/null; then
        if [ -n "${COPROC_PID:-}" ]; then
            __foresight_coproc_pid=$COPROC_PID
            __foresight_coproc_in=${COPROC[1]:-}
            __foresight_coproc_out=${COPROC[0]:-}
            __foresight_coproc_alive=1
            if {
                printf '%s\t%s\n' "$PWD" "$1" >&${__foresight_coproc_in}
            } 2>/dev/null && {
                IFS= read -r -t 1 reply <&${__foresight_coproc_out}
            } 2>/dev/null; then
                printf '%s' "$reply"
                return 0
            fi
            __foresight_coproc_kill
        fi
    fi
    __foresight_predict_spawn "$1"
}

trap '__foresight_coproc_kill' EXIT

bind -x '"\C-x\C-p": __foresight_accept_suggestion'
bind -x '"\C-x\C-w": __foresight_accept_word'
bind -x '"\C-x\C-f": __foresight_fzf_pick'
