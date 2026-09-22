[[ -o interactive ]] || return 0
(( ${+_FORESIGHT_LOADED} )) && return 0
typeset -g _FORESIGHT_LOADED=1
zmodload zsh/datetime
zmodload zsh/system
if [[ ${_FORESIGHT_SESSION_OWNER:-} != $$ ]]; then
  typeset -g FORESIGHT_SESSION="$$-$RANDOM"
  typeset -g _FORESIGHT_SESSION_OWNER=$$
fi
export FORESIGHT_SESSION

typeset -g _FORESIGHT_BIN=""
typeset -g _foresight_accepted=""

_foresight_resolve() {
  if [[ -z $_FORESIGHT_BIN ]]; then
    local cand=${FORESIGHT_BIN:-}
    [[ -n $cand && -x $cand ]] || cand=${commands[foresight]:-}
    [[ -n $cand && -x $cand ]] || cand=$HOME/.local/bin/foresight
    [[ -n $cand && -x $cand ]] && _FORESIGHT_BIN=$cand
  fi
  [[ -n $_FORESIGHT_BIN ]]
}

_foresight_preexec() {
  _foresight_resolve || return 0
  # $3 is the entire unabridged command; $1 may be history-shortened.
  local line=${3:-$1} accepted=$_foresight_accepted
  _foresight_accepted=
  "$_FORESIGHT_BIN" train "$line" >/dev/null 2>&1
  if [[ -n $accepted && ( $line == "$accepted"* || $accepted == "$line"* ) ]]; then
    "$_FORESIGHT_BIN" accept "$line" >/dev/null 2>&1
  fi
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _foresight_preexec

typeset -g _FORESIGHT_CACHE_PREFIX="" _FORESIGHT_CACHE_SUFFIX="" _FORESIGHT_CACHE_CWD=""
typeset -g _FORESIGHT_CACHE_T_MS=0

_zsh_autosuggest_strategy_foresight() {
  suggestion=
  _foresight_resolve || return 0
  local now_ms=$(( EPOCHREALTIME * 1000 ))
  local t_diff=$(( now_ms - _FORESIGHT_CACHE_T_MS ))
  if (( t_diff >= 0 && t_diff < 300 )) &&
      [[ $PWD == $_FORESIGHT_CACHE_CWD && $1 == $_FORESIGHT_CACHE_PREFIX ]]; then
    [[ -z $_FORESIGHT_CACHE_SUFFIX ]] || suggestion="$1$_FORESIGHT_CACHE_SUFFIX"
    return 0
  fi
  _foresight_predict "$1" || return 0
  _FORESIGHT_CACHE_PREFIX=$1
  _FORESIGHT_CACHE_SUFFIX=$_FORESIGHT_REPLY
  _FORESIGHT_CACHE_CWD=$PWD
  _FORESIGHT_CACHE_T_MS=$now_ms
  [[ -z $_FORESIGHT_REPLY ]] || suggestion="$1$_FORESIGHT_REPLY"
  return 0
}

typeset -g _FORESIGHT_STREAM_PID="" _FORESIGHT_STREAM_IN="" _FORESIGHT_STREAM_OUT=""
typeset -g _FORESIGHT_STREAM_OWNER=$sysparams[pid] _FORESIGHT_REPLY=""

_foresight_stream_close() {
  [[ -z $_FORESIGHT_STREAM_IN ]] || exec {_FORESIGHT_STREAM_IN}>&-
  [[ -z $_FORESIGHT_STREAM_OUT ]] || exec {_FORESIGHT_STREAM_OUT}<&-
  if [[ $_FORESIGHT_STREAM_OWNER == $sysparams[pid] && -n $_FORESIGHT_STREAM_PID ]]; then
    kill $_FORESIGHT_STREAM_PID 2>/dev/null
    wait $_FORESIGHT_STREAM_PID 2>/dev/null
  fi
  _FORESIGHT_STREAM_PID= _FORESIGHT_STREAM_IN= _FORESIGHT_STREAM_OUT=
}

_foresight_predict_spawn() {
  local reply
  reply=$("$_FORESIGHT_BIN" predict "$1" 2>/dev/null; printf '\001')
  _FORESIGHT_REPLY=${reply%$'\001'}
}

_foresight_predict() {
  _FORESIGHT_REPLY=
  # Async strategies run in short-lived forked workers. They must neither use
  # nor kill the owning editor's pipes, nor leave a stream helper behind.
  if [[ $_FORESIGHT_STREAM_OWNER != $sysparams[pid] ]]; then
    _foresight_stream_close
    _foresight_predict_spawn "$1"
    return
  fi
  if [[ -z $_FORESIGHT_STREAM_PID ]]; then
    coproc exec "$_FORESIGHT_BIN" stream 2>/dev/null
    _FORESIGHT_STREAM_PID=$!
    if ! { exec {_FORESIGHT_STREAM_IN}>&p {_FORESIGHT_STREAM_OUT}<&p }; then
      _foresight_stream_close
      _foresight_predict_spawn "$1"
      return
    fi
  fi
  if { printf '%s\0%s\0' "$PWD" "$1" >&$_FORESIGHT_STREAM_IN } 2>/dev/null; then
    local chunk
    local -F deadline=$(( EPOCHREALTIME + 1.0 )) remaining
    while (( (remaining = deadline - EPOCHREALTIME) > 0 )); do
      # Unlike read -t, sysread bounds each read, not just the first byte.
      sysread -i $_FORESIGHT_STREAM_OUT -s 4096 -t $remaining chunk || break
      if [[ $chunk == *$'\0'* ]]; then
        _FORESIGHT_REPLY+=${chunk%%$'\0'*}
        return 0
      fi
      _FORESIGHT_REPLY+=$chunk
    done
  fi
  _foresight_stream_close
  _foresight_predict_spawn "$1"
}

add-zsh-hook zshexit _foresight_stream_close

_foresight_register_strategy() {
  (( $+functions[_zsh_autosuggest_start] )) || return 1
  if [[ ${ZSH_AUTOSUGGEST_STRATEGY[(r)foresight]:-} != foresight ]]; then
    ZSH_AUTOSUGGEST_STRATEGY=(foresight ${ZSH_AUTOSUGGEST_STRATEGY[@]})
  fi
  # Wrap the plugin actions rather than replacing normal key bindings. Both
  # accept and execute then award deferred credit using the executed line.
  local action
  for action in accept execute; do
    if (( $+functions[_zsh_autosuggest_$action] && ! $+functions[_foresight_original_$action] )); then
      functions[_foresight_original_$action]=$functions[_zsh_autosuggest_$action]
      functions[_zsh_autosuggest_$action]='
        local full=$BUFFER$POSTDISPLAY ghost=$POSTDISPLAY
        _foresight_original_'$action' "$@"
        local ret=$?
        if [[ -n $ghost && $BUFFER == "$full" && -z $POSTDISPLAY ]]; then
          _foresight_accepted=$full
        fi
        return $ret'
    fi
  done
  return 0
}

if ! _foresight_register_strategy; then
  _foresight_strategy_retry() {
    if _foresight_register_strategy; then
      add-zsh-hook -d precmd _foresight_strategy_retry
    fi
  }
  add-zsh-hook precmd _foresight_strategy_retry
fi

foresight-accept-suggestion() {
  if _foresight_resolve; then
    _foresight_predict "$BUFFER"
    if [[ -n $_FORESIGHT_REPLY ]]; then
      BUFFER+=$_FORESIGHT_REPLY
      CURSOR=$#BUFFER
      _foresight_accepted=$BUFFER
    fi
  fi
}

foresight-accept-word() {
  _foresight_resolve || return 0
  _foresight_predict "$BUFFER"
  local suffix=$_FORESIGHT_REPLY text=$BUFFER$_FORESIGHT_REPLY
  local quote= ch
  local -i escaped=0 seen=0 i start=$#BUFFER end=$#text
  if [[ $text != *'$('* && $text != *'`'* ]]; then
    for ((i=1; i<=$#text; i++)); do
      ch=$text[i]
      if (( escaped )); then escaped=0
      elif [[ $quote == "'" ]]; then
        [[ $ch == "'" ]] && quote=
      elif [[ $ch == '\' ]]; then escaped=1
      elif [[ -n $quote ]]; then
        [[ $ch == "$quote" ]] && quote=
      elif [[ $ch == '"' || $ch == "'" ]]; then quote=$ch
      elif [[ $ch == [[:space:]] ]]; then
        if (( i > start && seen )); then end=$i; break; fi
      fi
      if (( i > start )) && [[ $ch != [[:space:]] || -n $quote ]]; then seen=1; fi
    done
  fi
  BUFFER+=${suffix:0:$((end-start))}
  CURSOR=$#BUFFER
}

foresight-fzf-pick() {
  (( $+commands[fzf] )) || return 0
  _foresight_resolve || return 0
  local pick
  IFS= read -r -d $'\0' pick < <("$_FORESIGHT_BIN" list --null "$BUFFER" 2>/dev/null |
    command fzf --read0 --print0 --height 40% --reverse --no-sort --prompt '> ') || return
  [[ -n $pick ]] || return 0
  BUFFER+=$pick
  CURSOR=$#BUFFER
  _foresight_accepted=$BUFFER
}

zle -N foresight-accept-suggestion
zle -N foresight-accept-word
zle -N foresight-fzf-pick
bindkey '^X^P' foresight-accept-suggestion
bindkey '^X^W' foresight-accept-word
bindkey '^X^F' foresight-fzf-pick

if _foresight_resolve; then
  ("$_FORESIGHT_BIN" ensure-daemon >/dev/null 2>&1 &)
fi
