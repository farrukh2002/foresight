[[ $- == *i* ]] || return 0
ble/is-function ble/complete/auto-complete/source:foresight && return 0

if [[ ${_FORESIGHT_SESSION_OWNER:-} != "$BASHPID" ]]; then
  FORESIGHT_SESSION="$BASHPID-$RANDOM"
  _FORESIGHT_SESSION_OWNER=$BASHPID
fi
export FORESIGHT_SESSION

_ble_contrib_foresight_bin=
_ble_contrib_foresight_resolved=0

ble/contrib/foresight/resolve-bin() {
  local cand=${FORESIGHT_BIN:-}
  [[ -n $cand && -x $cand ]] || cand=$(command -v foresight 2>/dev/null)
  [[ -n $cand && -x $cand ]] || return 1
  _ble_contrib_foresight_bin=$cand
  _ble_contrib_foresight_resolved=1
  return 0
}

if ble/contrib/foresight/resolve-bin; then
  ("$_ble_contrib_foresight_bin" ensure-daemon >/dev/null 2>&1 &)
fi

ble/contrib/foresight/preexec() {
  ((_ble_contrib_foresight_resolved)) || ble/contrib/foresight/resolve-bin || return 0
  "$_ble_contrib_foresight_bin" train "$1" >/dev/null 2>&1
  local accepted=${_ble_contrib_foresight_accepted:-}
  _ble_contrib_foresight_accepted=
  _ble_contrib_foresight_pending=
  if [[ -n $accepted && ( $1 == "$accepted"* || $accepted == "$1"* ) ]]; then
    "$_ble_contrib_foresight_bin" accept "$1" >/dev/null 2>&1
  fi
}
blehook PREEXEC+='ble/contrib/foresight/preexec'

_ble_contrib_foresight_pending=
_ble_contrib_foresight_accepted=

ble/contrib/foresight/insert-observer() {
  local pending=${_ble_contrib_foresight_pending:-}
  [[ -n $pending ]] || return 0
  # auto_complete keeps the unaccepted ghost in _ble_edit_str. A partial
  # insertion merely moves _ble_edit_ind through it, and invokes this hook
  # too. A real full insertion supplies comp_text and the whole insert;
  # self-insert also fires the hook, but with empty comp_text.
  if [[ $_ble_edit_str == "$pending" && -n ${comp_text:-} && ${insert:-} == "$pending" ]] &&
      (( _ble_edit_ind == ${#pending} )); then
    _ble_contrib_foresight_accepted=$pending
    _ble_contrib_foresight_pending=
  elif [[ $_ble_edit_str != "$pending" ]]; then
    _ble_contrib_foresight_pending=
  fi
}
blehook complete_insert+='ble/contrib/foresight/insert-observer'

_ble_contrib_foresight_cache_prefix=
_ble_contrib_foresight_cache_suggestion=
_ble_contrib_foresight_cache_cwd=
_ble_contrib_foresight_cache_t_ms=0

ble/complete/auto-complete/source:foresight() {
  ((_ble_edit_ind == ${#_ble_edit_str})) || return 1
  ((_ble_contrib_foresight_resolved)) || ble/contrib/foresight/resolve-bin || return 1

  local now_ms=$(( $(date +%s%N) / 1000000 ))
  local t_diff=$(( now_ms - _ble_contrib_foresight_cache_t_ms )) suggestion
  if (( t_diff >= 0 && t_diff < 300 )) &&
      [[ $PWD == "$_ble_contrib_foresight_cache_cwd" && $_ble_edit_str == "$_ble_contrib_foresight_cache_prefix" ]]; then
    suggestion=$_ble_contrib_foresight_cache_suggestion
  else
    _ble_contrib_foresight_predict "$_ble_edit_str" || return 1
    suggestion=$_ble_contrib_foresight_reply
    _ble_contrib_foresight_cache_prefix=$_ble_edit_str
    _ble_contrib_foresight_cache_suggestion=$suggestion
    _ble_contrib_foresight_cache_cwd=$PWD
    _ble_contrib_foresight_cache_t_ms=$now_ms
  fi
  [[ -n $suggestion ]] || return 1
  local full=$_ble_edit_str$suggestion
  _ble_contrib_foresight_pending=$full
  ble/complete/auto-complete/enter h 0 "$suggestion" '' "$full" "$full"
}

_ble_contrib_foresight_coproc_pid=
_ble_contrib_foresight_coproc_in=
_ble_contrib_foresight_coproc_out=
_ble_contrib_foresight_coproc_owner=$BASHPID
_ble_contrib_foresight_reply=

_ble_contrib_foresight_coproc_kill() {
  [[ -z $_ble_contrib_foresight_coproc_in ]] || exec {_ble_contrib_foresight_coproc_in}>&-
  [[ -z $_ble_contrib_foresight_coproc_out ]] || exec {_ble_contrib_foresight_coproc_out}<&-
  if [[ $_ble_contrib_foresight_coproc_owner == "$BASHPID" && -n $_ble_contrib_foresight_coproc_pid ]]; then
    kill "$_ble_contrib_foresight_coproc_pid" 2>/dev/null
    wait "$_ble_contrib_foresight_coproc_pid" 2>/dev/null
  fi
  _ble_contrib_foresight_coproc_pid= _ble_contrib_foresight_coproc_in= _ble_contrib_foresight_coproc_out=
}

_ble_contrib_foresight_predict() {
  _ble_contrib_foresight_reply=
  if [[ $_ble_contrib_foresight_coproc_owner != "$BASHPID" ]]; then
    _ble_contrib_foresight_coproc_kill
    _ble_contrib_foresight_coproc_owner=$BASHPID
  fi
  if [[ -z $_ble_contrib_foresight_coproc_pid ]]; then
    coproc _ble_contrib_foresight_stream { exec "$_ble_contrib_foresight_bin" stream 2>/dev/null; }
    _ble_contrib_foresight_coproc_pid=$_ble_contrib_foresight_stream_PID
    if ! { exec {_ble_contrib_foresight_coproc_in}>&${_ble_contrib_foresight_stream[1]} {_ble_contrib_foresight_coproc_out}<&${_ble_contrib_foresight_stream[0]}; } 2>/dev/null; then
      _ble_contrib_foresight_coproc_kill
    fi
  fi
  if [[ -n $_ble_contrib_foresight_coproc_pid ]] &&
      { printf '%s\0%s\0' "$PWD" "$1" >&$_ble_contrib_foresight_coproc_in; } 2>/dev/null &&
      IFS= read -r -d '' -t 1 -u "$_ble_contrib_foresight_coproc_out" _ble_contrib_foresight_reply; then
    return 0
  fi
  _ble_contrib_foresight_coproc_kill
  local reply
  reply=$("$_ble_contrib_foresight_bin" predict "$1" 2>/dev/null; printf '\001')
  _ble_contrib_foresight_reply=${reply%$'\001'}
}

# ble owns the process lifecycle and its EXIT trap; register rather than replace.
blehook EXIT+='_ble_contrib_foresight_coproc_kill'

ble/util/import/eval-after-load core-complete '
  ble/array#insert-before _ble_complete_auto_source history foresight'
