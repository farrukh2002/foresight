[[ -o interactive ]] || return 0

FORESIGHT_SESSION="${FORESIGHT_SESSION:-$$-$RANDOM}"
export FORESIGHT_SESSION

typeset -g _FORESIGHT_BIN=""
typeset -g _foresight_accepted=""

_foresight_resolve() {
  if [[ -z $_FORESIGHT_BIN ]]; then
    local cand=${FORESIGHT_BIN:-}
    [[ -n $cand && -x $cand ]] || cand=$(command -v foresight 2>/dev/null)
    [[ -n $cand && -x $cand ]] && _FORESIGHT_BIN=$cand
  fi
  [[ -n $_FORESIGHT_BIN ]]
}

_foresight_preexec() {
  _foresight_resolve || return 0
  "$_FORESIGHT_BIN" train "$1" >/dev/null 2>&1
  local accepted=${_foresight_accepted:-}
  typeset -g _foresight_accepted=
  if [[ -n $accepted ]]; then
    if [[ $1 == "$accepted" || $1 == "$accepted"* || "$accepted" == "$1"* ]]; then
      "$_FORESIGHT_BIN" accept "$1" >/dev/null 2>&1
    fi
  fi
}

autoload -Uz add-zsh-hook
add-zsh-hook preexec _foresight_preexec

typeset -g _FORESIGHT_CACHE_PREFIX=""
typeset -g _FORESIGHT_CACHE_SUFFIX=""
typeset -g _FORESIGHT_CACHE_T_MS=0

_zsh_autosuggest_strategy_foresight() {
  typeset -g suggestion=
  _foresight_resolve || return 0

  local now_ms=$(( EPOCHREALTIME * 1000 ))
  if [[ -n $_FORESIGHT_CACHE_PREFIX ]]; then
    local t_diff=$(( now_ms - _FORESIGHT_CACHE_T_MS ))
    if (( t_diff >= 0 && t_diff < 300 )) && [[ $1 == "$_FORESIGHT_CACHE_PREFIX" ]]; then
      if [[ -n $_FORESIGHT_CACHE_SUFFIX ]]; then
        suggestion="$1$_FORESIGHT_CACHE_SUFFIX"
        return 0
      fi
      return 0
    fi
  fi

  local suffix
  suffix=$(_foresight_predict "$1") || return 0
  [[ -n $suffix ]] || return 0
  _FORESIGHT_CACHE_PREFIX=$1
  _FORESIGHT_CACHE_SUFFIX=$suffix
  _FORESIGHT_CACHE_T_MS=$now_ms
  suggestion="$1$suffix"
  return 0
}

typeset -g _FORESIGHT_PTY=""
typeset -g _FORESIGHT_PTY_ALIVE=0

_foresight_predict_spawn() {
  "$_FORESIGHT_BIN" predict "$1" 2>/dev/null
}

_foresight_pty_kill() {
  if [[ -n $_FORESIGHT_PTY ]]; then
    zpty -d $_FORESIGHT_PTY 2>/dev/null
    _FORESIGHT_PTY=""
  fi
  _FORESIGHT_PTY_ALIVE=0
}

_foresight_predict() {
  if (( _FORESIGHT_PTY_ALIVE )); then
    if zpty -w -t 500 -- $_FORESIGHT_PTY "$PWD"$'\t'"$1"$'\n' 2>/dev/null \
       && zpty -r -t 500 -- $_FORESIGHT_PTY reply $'\n' 2>/dev/null; then
      print -r -- "$reply"
      return 0
    fi
    _foresight_pty_kill
  fi

  zmodload zsh/zpty 2>/dev/null
  if zpty -b foresight-stream FORESIGHT_CWD="${PWD:-}" "$_FORESIGHT_BIN" stream 2>/dev/null; then
    _FORESIGHT_PTY=foresight-stream
    _FORESIGHT_PTY_ALIVE=1
    if zpty -w -t 500 -- $_FORESIGHT_PTY "$PWD"$'\t'"$1"$'\n' 2>/dev/null \
       && zpty -r -t 500 -- $_FORESIGHT_PTY reply $'\n' 2>/dev/null; then
      print -r -- "$reply"
      return 0
    fi
    _foresight_pty_kill
  fi

  _foresight_predict_spawn "$1"
}

zshexit_functions+=(_foresight_pty_kill)

_foresight_register_strategy() {
  (( $+functions[_zsh_autosuggest_start] )) || return 1
  if [[ ${ZSH_AUTOSUGGEST_STRATEGY[(r)foresight]:-} != foresight ]]; then
    ZSH_AUTOSUGGEST_STRATEGY=(foresight ${ZSH_AUTOSUGGEST_STRATEGY[@]})
  fi
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
    local s; s=$(_foresight_predict "$BUFFER")
    if [[ -n $s ]]; then
      BUFFER="$BUFFER$s"
      CURSOR=$#BUFFER
      typeset -g _foresight_accepted=$BUFFER
    fi
  fi
}

foresight-accept-word() {
  if _foresight_resolve; then
    local s; s=$(_foresight_predict "$BUFFER")
    local w=${s%%[[:space:]]*}
    if [[ -n $w ]]; then
      BUFFER="$BUFFER$w"
      [[ $s == *[[:space:]]* ]] && BUFFER="$BUFFER "
      CURSOR=$#BUFFER
    fi
  fi
}

foresight-fzf-pick() {
  (( $+commands[fzf] )) || return 0
  _foresight_resolve || return 0
  local pick
  pick=$("$_FORESIGHT_BIN" list "$BUFFER" 2>/dev/null | command fzf --height 40% --reverse --no-sort --prompt '> ')
  [[ -n $pick ]] || return 0
  BUFFER="$BUFFER$pick"
  CURSOR=$#BUFFER
  typeset -g _foresight_accepted=$BUFFER
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
