FORESIGHT_BIN="$HOME/.local/bin/foresight"

FORESIGHT_SESSION="$$-${RANDOM}"
export FORESIGHT_SESSION

__foresight_train_last() {
    local last
    last=$(history 1 | sed 's/^[ ]*[0-9]*[ ]*//')
    [ -n "$last" ] && $FORESIGHT_BIN train "$last" >/dev/null 2>&1
}

__foresight_accept_suggestion() {
    local suggestion
    suggestion=$($FORESIGHT_BIN predict "$READLINE_LINE")
    if [ -n "$suggestion" ]; then
        READLINE_LINE="${READLINE_LINE}${suggestion}"
        READLINE_POINT=${#READLINE_LINE}
        $FORESIGHT_BIN accept "$READLINE_LINE" >/dev/null 2>&1
    fi
}

__foresight_accept_word() {
    local suggestion word
    suggestion=$($FORESIGHT_BIN predict "$READLINE_LINE")
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
    $FORESIGHT_BIN accept "$READLINE_LINE" >/dev/null 2>&1
}

$FORESIGHT_BIN ensure-daemon >/dev/null 2>&1

PROMPT_COMMAND="__foresight_train_last${PROMPT_COMMAND:+; $PROMPT_COMMAND}"
bind -x '"\C-x\C-p": __foresight_accept_suggestion'
bind -x '"\C-x\C-w": __foresight_accept_word'
bind -x '"\C-x\C-f": __foresight_fzf_pick'
