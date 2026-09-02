[[ $- == *i* ]] || return 0
ble/is-function ble/complete/auto-complete/source:foresight && return 0

[[ ${FORESIGHT_SESSION:-} ]] || {
  FORESIGHT_SESSION="$$-$RANDOM"
  export FORESIGHT_SESSION
}

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
  if [[ -n $accepted ]]; then
    if [[ $1 == "$accepted" || $1 == "$accepted"* || "$accepted" == "$1"* ]]; then
      "$_ble_contrib_foresight_bin" accept "$1" >/dev/null 2>&1
    fi
  fi
}
blehook PREEXEC+='ble/contrib/foresight/preexec'

_ble_contrib_foresight_pending=
_ble_contrib_foresight_accepted=

ble/contrib/foresight/insert-observer() {
  local pending=${_ble_contrib_foresight_pending:-}
  _ble_contrib_foresight_pending=
  [[ -n $pending && $_ble_edit_str == "$pending" ]] || return 0
  _ble_contrib_foresight_accepted=$pending
}
blehook complete_insert+='ble/contrib/foresight/insert-observer'

_ble_contrib_foresight_cache_prefix=
_ble_contrib_foresight_cache_suggestion=
_ble_contrib_foresight_cache_t_ms=0

ble/complete/auto-complete/source:foresight() {
  ((_ble_edit_ind == ${#_ble_edit_str})) || return 1
  ((_ble_contrib_foresight_resolved)) || ble/contrib/foresight/resolve-bin || return 1

  local now_ms t_diff
  if [[ -n $_ble_contrib_foresight_cache_prefix ]]; then
    now_ms=$(( $(date +%s%N) / 1000000 ))
    t_diff=$(( now_ms - _ble_contrib_foresight_cache_t_ms ))
    if (( t_diff >= 0 && t_diff < 300 )) && [[ $_ble_edit_str == "$_ble_contrib_foresight_cache_prefix" ]]; then
      local cached=${_ble_contrib_foresight_cache_suggestion}
      if [[ -n $cached ]]; then
        local full=$_ble_edit_str$cached
        _ble_contrib_foresight_pending=$full
        ble/complete/auto-complete/enter h 0 "$cached" '' "$full" "$full"
        return 0
      fi
      return 1
    fi
  fi

  local suggestion
  suggestion=$(_ble_contrib_foresight_predict "$_ble_edit_str") || return 1
  [[ -n $suggestion ]] || return 1
  _ble_contrib_foresight_cache_prefix=$_ble_edit_str
  _ble_contrib_foresight_cache_suggestion=$suggestion
  _ble_contrib_foresight_cache_t_ms=$(( $(date +%s%N) / 1000000 ))
  local full=$_ble_edit_str$suggestion
  _ble_contrib_foresight_pending=$full
  ble/complete/auto-complete/enter h 0 "$suggestion" '' "$full" "$full"
}

_ble_contrib_foresight_coproc_alive=0
_ble_contrib_foresight_coproc_pid=
_ble_contrib_foresight_coproc_in=
_ble_contrib_foresight_coproc_out=
_ble_contrib_foresight_predict_spawn() {
  "$_ble_contrib_foresight_bin" predict "$1" 2>/dev/null
}

_ble_contrib_foresight_predict() {
  if (( _ble_contrib_foresight_coproc_alive )); then
    if {
      printf '%s\t%s\n' "$PWD" "$1" >&${_ble_contrib_foresight_coproc_in:-1}
    } 2>/dev/null && {
      IFS= read -r -t 1 reply <&${_ble_contrib_foresight_coproc_out:-0}
    } 2>/dev/null; then
      printf '%s' "$reply"
      return 0
    fi
    _ble_contrib_foresight_coproc_kill
  fi

  if coproc _ble_contrib_foresight_coproc {
    FORESIGHT_CWD="${PWD:-}" exec "$_ble_contrib_foresight_bin" stream
  } 2>/dev/null; then
    _ble_contrib_foresight_coproc_pid=$COPROC_PID
    if [[ -n ${COPROC_PID:-} ]]; then
      _ble_contrib_foresight_coproc_in=${COPROC[1]:-}
      _ble_contrib_foresight_coproc_out=${COPROC[0]:-}
      _ble_contrib_foresight_coproc_alive=1
      if {
        printf '%s\t%s\n' "$PWD" "$1" >&${_ble_contrib_foresight_coproc_in}
      } 2>/dev/null && {
        IFS= read -r -t 1 reply <&${_ble_contrib_foresight_coproc_out}
      } 2>/dev/null; then
        printf '%s' "$reply"
        return 0
      fi
      _ble_contrib_foresight_coproc_kill
    fi
  fi

  _ble_contrib_foresight_predict_spawn "$1"
}

_ble_contrib_foresight_coproc_kill() {
  _ble_contrib_foresight_coproc_alive=0
  if [[ -n ${_ble_contrib_foresight_coproc_pid:-} ]]; then
    kill "$_ble_contrib_foresight_coproc_pid" 2>/dev/null
    _ble_contrib_foresight_coproc_pid=
  fi
  _ble_contrib_foresight_coproc_in=
  _ble_contrib_foresight_coproc_out=
}

trap '_ble_contrib_foresight_coproc_kill' EXIT

ble/util/import/eval-after-load core-complete '
  ble/array#insert-before _ble_complete_auto_source history foresight'
