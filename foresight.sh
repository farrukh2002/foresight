[[ $- == *i* ]] || return 0
[[ ${__foresight_loaded:-} == "$BASHPID" ]] && return 0
__foresight_loaded=$BASHPID

FORESIGHT_BIN="${FORESIGHT_BIN:-$HOME/.local/bin/foresight}"
if [[ ! -x $FORESIGHT_BIN ]]; then
    _cand=$(command -v foresight 2>/dev/null)
    [[ -n $_cand && -x $_cand ]] && FORESIGHT_BIN=$_cand
    unset _cand
fi
if [[ ${_FORESIGHT_SESSION_OWNER:-} != "$BASHPID" ]]; then
    FORESIGHT_SESSION="$BASHPID-$RANDOM"
    _FORESIGHT_SESSION_OWNER=$BASHPID
fi
export FORESIGHT_SESSION

if [[ -n ${_ble_base:-} ]]; then
    _foresight_layer_dir="${XDG_DATA_HOME:-$HOME/.local/share}/blesh/local/integration"
    if [[ ! -f $_foresight_layer_dir/foresight.bash && -x $FORESIGHT_BIN ]]; then
        "$FORESIGHT_BIN" init --quiet >/dev/null 2>&1
    fi
    unset _foresight_layer_dir
    declare -F ble-import >/dev/null 2>&1 && ble-import integration/foresight 2>/dev/null
    return 0
fi

__foresight_accepted=
__foresight_input=
__foresight_input_pending=0

# Readline supplies the actual input, before history filtering. Keep native
# accept-line in the macro so Readline still handles multiline/vi input itself.
__foresight_capture() {
    if (( __foresight_input_pending )); then
        __foresight_input+=$'\n'
    else
        __foresight_input=
    fi
    __foresight_input+=$READLINE_LINE
    __foresight_input_pending=1
}

__foresight_preexec() {
    (( __foresight_input_pending )) || return 0
    local line=$__foresight_input accepted=$__foresight_accepted
    if [[ -n $line ]]; then
        "$FORESIGHT_BIN" train "$line" >/dev/null 2>&1
        if [[ -n $accepted && ( $line == "$accepted"* || $accepted == "$line"* ) ]]; then
            "$FORESIGHT_BIN" accept "$line" >/dev/null 2>&1
        fi
    fi
    return 0
}

# PS0 runs after a complete command is parsed, before execution. Prediction
# transport state is never used in this substitution. The zero-length
# substring resets the pending flag in the owning shell without printing a
# byte, so additional commands in one multiline Readline buffer train once.
PS0='$(__foresight_preexec)${__foresight_input_pending:0:__foresight_input_pending=0}'"${PS0-}"
__foresight_flush() {
    local status=$?
    __foresight_input=
    __foresight_input_pending=0
    __foresight_accepted=
    return "$status"
}

# An array retains each existing prompt command as a separate evaluation and
# returning the entry status keeps $? available to the user's first hook.
PROMPT_COMMAND=(__foresight_flush "${PROMPT_COMMAND[@]}")

__foresight_coproc_pid=
__foresight_coproc_in=
__foresight_coproc_out=
__foresight_coproc_owner=$BASHPID
__foresight_reply=

__foresight_coproc_kill() {
    [[ -z $__foresight_coproc_in ]] || exec {__foresight_coproc_in}>&-
    [[ -z $__foresight_coproc_out ]] || exec {__foresight_coproc_out}<&-
    if [[ $__foresight_coproc_owner == "$BASHPID" && -n $__foresight_coproc_pid ]]; then
        kill "$__foresight_coproc_pid" 2>/dev/null
        wait "$__foresight_coproc_pid" 2>/dev/null
    fi
    __foresight_coproc_pid= __foresight_coproc_in= __foresight_coproc_out=
}

__foresight_predict() {
    __foresight_reply=
    if [[ $__foresight_coproc_owner != "$BASHPID" ]]; then
        __foresight_coproc_kill
        __foresight_coproc_owner=$BASHPID
    fi
    if [[ -z $__foresight_coproc_pid ]]; then
        # The group's stderr redirect silences the job-control announcement
        # ("[1] PID") interactive bash prints for a freshly spawned coproc;
        # disown suppresses the matching Done/Exit notice if the helper dies.
        { coproc __foresight_stream { exec "$FORESIGHT_BIN" stream 2>/dev/null; }
          disown "$__foresight_stream_PID" 2>/dev/null || : ;} 2>/dev/null
        __foresight_coproc_pid=$__foresight_stream_PID
        if ! { exec {__foresight_coproc_in}>&${__foresight_stream[1]} {__foresight_coproc_out}<&${__foresight_stream[0]}; } 2>/dev/null; then
            __foresight_coproc_kill
        fi
    fi
    if [[ -n $__foresight_coproc_pid ]] &&
        { printf '%s\0%s\0' "$PWD" "$1" >&$__foresight_coproc_in; } 2>/dev/null &&
        IFS= read -r -d '' -t 1 -u "$__foresight_coproc_out" __foresight_reply; then
        return 0
    fi
    __foresight_coproc_kill
    # Sentinel prevents command substitution from removing trailing newlines.
    local reply
    reply=$("$FORESIGHT_BIN" predict "$1" 2>/dev/null; printf '\001')
    __foresight_reply=${reply%$'\001'}
}

__foresight_accept_suggestion() {
    __foresight_predict "$READLINE_LINE"
    if [[ -n $__foresight_reply ]]; then
        READLINE_LINE+=$__foresight_reply
        READLINE_POINT=${#READLINE_LINE}
        __foresight_accepted=$READLINE_LINE
    fi
}

__foresight_accept_word() {
    __foresight_predict "$READLINE_LINE"
    local suffix=$__foresight_reply text=$READLINE_LINE$__foresight_reply
    local quote= escaped=0 seen=0 ch i start=${#READLINE_LINE} end=${#text}
    # Nested substitutions can contain arbitrary shell grammar. Accept them
    # intact rather than cutting a syntactically incomplete substitution.
    if [[ $text != *'$('* && $text != *'`'* ]]; then
        for ((i=0; i<${#text}; i++)); do
            ch=${text:i:1}
            if (( escaped )); then escaped=0
            elif [[ $quote == "'" ]]; then
                [[ $ch == "'" ]] && quote=
            elif [[ $ch == '\' ]]; then escaped=1
            elif [[ -n $quote ]]; then
                [[ $ch == "$quote" ]] && quote=
            elif [[ $ch == '"' || $ch == "'" ]]; then quote=$ch
            elif [[ $ch == [[:space:]] ]]; then
                if (( i >= start && seen )); then end=$((i+1)); break; fi
            fi
            if (( i >= start )) && [[ $ch != [[:space:]] || -n $quote ]]; then seen=1; fi
        done
    fi
    READLINE_LINE+=${suffix:0:end-start}
    READLINE_POINT=${#READLINE_LINE}
}

__foresight_fzf_pick() {
    command -v fzf >/dev/null 2>&1 || return
    local pick
    IFS= read -r -d '' pick < <("$FORESIGHT_BIN" list --null "$READLINE_LINE" 2>/dev/null |
        fzf --read0 --print0 --height 40% --reverse --no-sort --prompt '> ') || return
    [[ -n $pick ]] || return
    READLINE_LINE+=$pick
    READLINE_POINT=${#READLINE_LINE}
    __foresight_accepted=$READLINE_LINE
}

# Do not touch DEBUG. Chain EXIT for transport cleanup, restoring the original
# status before running the user's hook.
__foresight_saved_exit=$(trap -p EXIT)
if [[ -n $__foresight_saved_exit ]]; then
    __foresight_saved_exit=${__foresight_saved_exit#trap -- }
    __foresight_saved_exit=${__foresight_saved_exit% EXIT}
    eval "__foresight_saved_exit=$__foresight_saved_exit"
fi
__foresight_restore_status() { return "$1"; }
__foresight_exit() {
    local status=$?
    __foresight_coproc_kill
    __foresight_restore_status "$status"
    eval "$__foresight_saved_exit"
    return "$status"
}
trap '__foresight_exit' EXIT

for __foresight_map in emacs-standard vi-insert vi-command; do
    while IFS= read -r __foresight_binding; do
        case $__foresight_binding in
            *': accept-line') __foresight_native='\C-x\C-^' ;;
            *': operate-and-get-next') __foresight_native='\C-x\C-_' ;;
            *) continue ;;
        esac
        __foresight_key=${__foresight_binding%: *}
        bind -m "$__foresight_map" "$__foresight_key: \"\\C-x\\C-]$__foresight_native\""
    done < <(bind -m "$__foresight_map" -p)
    bind -m "$__foresight_map" -x '"\C-x\C-]": __foresight_capture'
    bind -m "$__foresight_map" '"\C-x\C-^": accept-line'
    bind -m "$__foresight_map" '"\C-x\C-_": operate-and-get-next'
    bind -m "$__foresight_map" -x '"\C-x\C-p": __foresight_accept_suggestion'
    bind -m "$__foresight_map" -x '"\C-x\C-w": __foresight_accept_word'
    bind -m "$__foresight_map" -x '"\C-x\C-f": __foresight_fzf_pick'
done
unset __foresight_map __foresight_binding __foresight_key __foresight_native
"$FORESIGHT_BIN" ensure-daemon >/dev/null 2>&1
