if [[ -n "${ZSH_NAME:-}" && -n "${_tab_link_on:-}" ]]; then

# Shell link: zsh talks straight to the desktop app over one socket, so nothing
# sits between the terminal and the shell. pre.sh sets _tab_link_on. The
# protocol is in crates/tab_desktop/src/shell_link/protocol.rs. pre.sh has
# already loaded zsh/net/socket.
#
# Keys: the desktop says once which keys the popup may ever use (`bind`). The
# shell binds those to _tab_link_key and remembers what each did before. Then
# the desktop pushes which of them the popup takes right now (`keys`), so each
# press is decided here with no round trip. Switching keymaps instead does not
# work: a keymap selected from a `zle -F` handler only applies from the second
# key on, and the first key after the popup opens is usually Tab or Enter.

typeset -g _tab_link_fd="" _tab_link_sent="" _tab_link_env_sent="" _tab_link_alias_sent=""
typeset -g _tab_link_ghost_buffer="" _tab_link_ghost_suffix=""
typeset -gi _tab_link_seq=0
typeset -gA _tab_link_actions _tab_link_key_names _tab_link_fallback

_tab_link_socket() {
  local dir=${TMPDIR:-}
  [[ -n "$dir" ]] || dir=$(getconf DARWIN_USER_TEMP_DIR 2>/dev/null)
  REPLY="${dir%/}/tabrun/shell.sock"
}

# Escapes a value for the protocol into REPLY
_tab_link_escape() {
  REPLY=${1//\\/\\\\}
  REPLY=${REPLY//$'\n'/\\n}
  REPLY=${REPLY//$'\t'/\\t}
}

# Undoes _tab_link_escape into REPLY
_tab_link_unescape() {
  REPLY=${1//\\\\/$'\0'}
  REPLY=${REPLY//\\n/$'\n'}
  REPLY=${REPLY//\\t/$'\t'}
  REPLY=${REPLY//$'\0'/\\}
}

_tab_link_disconnect() {
  if [[ -n "$_tab_link_fd" ]]; then
    zle -F "$_tab_link_fd" 2>/dev/null
    exec {_tab_link_fd}>&-
  fi
  _tab_link_fd="" _tab_link_sent="" _tab_link_env_sent="" _tab_link_alias_sent=""
  _tab_link_actions=()
}

_tab_link_connect() {
  [[ -n "$_tab_link_fd" ]] && return 0
  _tab_link_socket
  local socket=$REPLY
  # Only a socket this user owns; the desktop checks our uid in turn
  [[ -S "$socket" && -O "$socket" ]] || return 1
  zsocket "$socket" 2>/dev/null || return 1
  _tab_link_fd=$REPLY
  zle -F -w "$_tab_link_fd" _tab_link_on_push 2>/dev/null

  local shell=${TAB_SHELL:-$SHELL}
  _tab_link_escape "$shell"
  shell=$REPLY
  _tab_link_escape "${TTY:-}"
  _tab_link_send "hello"$'\t'"$shell"$'\t'"$REPLY"
}

# Sends one line; drops the connection if the desktop is gone
_tab_link_send() {
  [[ -n "$_tab_link_fd" ]] || return 1
  if ! print -r -u "$_tab_link_fd" -- "$1" 2>/dev/null; then
    _tab_link_disconnect
    return 1
  fi
}

# Sends the exported environment and aliases when they change
_tab_link_send_context() {
  local key line="env"
  for key in ${(ko)parameters[(R)*export*]}; do
    _tab_link_escape "$key"
    line+=$'\t'"$REPLY"
    _tab_link_escape "${(P)key}"
    line+=$'\t'"$REPLY"
  done
  if [[ "$line" != "$_tab_link_env_sent" ]] && _tab_link_send "$line"; then
    _tab_link_env_sent=$line
  fi

  # The same `name=value` lines `alias` prints, without a subshell
  local -a lines
  for key in ${(ko)aliases}; do
    lines+=("$key=${(q-)aliases[$key]}")
  done
  for key in ${(ko)galiases}; do
    lines+=("$key=${(q-)galiases[$key]}")
  done
  _tab_link_escape "${(pj:\n:)lines}"
  line="alias"$'\t'"$REPLY"
  if [[ "$line" != "$_tab_link_alias_sent" ]] && _tab_link_send "$line"; then
    _tab_link_alias_sent=$line
  fi
}

_tab_link_precmd() {
  local exit_code=$?
  # The inline plugin loads after this script and wraps every widget at its first
  # prompt; it must leave these alone or it resets the ghost text they set
  if (( ${+TAB_AUTOSUGGEST_IGNORE_WIDGETS} && ! ${TAB_AUTOSUGGEST_IGNORE_WIDGETS[(Ie)_tab_link_key]} )); then
    TAB_AUTOSUGGEST_IGNORE_WIDGETS+=(_tab_link_key _tab_link_on_push)
  fi
  _tab_link_connect || return
  _tab_link_send_context
  _tab_link_escape "$PWD"
  _tab_link_send "prompt"$'\t'"$exit_code"$'\t'"$REPLY"
}

_tab_link_preexec() {
  [[ -n "$_tab_link_fd" ]] || return
  _tab_link_escape "$1"
  _tab_link_send "preexec"$'\t'"$REPLY"
}

# Reports the buffer on every redraw that changed it
_tab_link_report() {
  [[ -n "$_tab_link_fd" ]] || return
  [[ "$CONTEXT" == start || "$CONTEXT" == cont ]] || return
  local state="$CURSOR:$BUFFER"
  [[ "$state" == "$_tab_link_sent" ]] && return
  _tab_link_sent=$state
  (( _tab_link_seq++ ))
  _tab_link_escape "$BUFFER"
  _tab_link_send "buffer"$'\t'"$_tab_link_seq"$'\t'"$CURSOR"$'\t'"$REPLY"
}

# Bound to every key the popup may use
_tab_link_key() {
  local name=${_tab_link_key_names[$KEYS]}
  local action=${_tab_link_actions[$name]}
  if [[ -n "$action" ]] && _tab_link_send "action"$'\t'"$name"$'\t'"$action"; then
    # These insert, and may ask to run the line. Wait for the insert here: only a
    # key widget can run the line, a `zle -F` handler can't until the next key.
    [[ "$action" == (insertSelected|insertSelectedAndExecute|insertCommonPrefixOrInsertSelected|execute) ]] \
      && _tab_link_await_set
    return
  fi
  local fallback=${_tab_link_fallback[$KEYMAP:$KEYS]:-${_tab_link_fallback[main:$KEYS]}}
  [[ -n "$fallback" && "$fallback" != undefined-key ]] && zle "$fallback"
}

# Handles pushes until the desktop's `set` arrives, for at most about 200 ms
_tab_link_await_set() {
  local line _tab_link_in_key=1
  local -i tries
  for (( tries = 0; tries < 20; tries++ )); do
    IFS= read -r -t 0.01 -u "$_tab_link_fd" line 2>/dev/null || continue
    _tab_link_handle "$line"
    [[ "$line" == set$'\t'* ]] && return
  done
}

# Binds the keys the popup may use, keeping what they did as the fallback
_tab_link_bind() {
  local keymap line seq name
  local -a words
  local -A previous
  for keymap in main emacs viins; do
    previous=()
    for line in "${(@f)$(bindkey -M "$keymap" 2>/dev/null)}"; do
      words=("${(@z)line}")
      previous[${(Q)words[1]}]=${words[-1]}
    done
    for name in "$@"; do
      for seq in "${(@k)_tab_link_key_names[(R)${(b)name}]}"; do
        [[ "${previous[${(V)seq}]}" == _tab_link_key ]] && continue
        _tab_link_fallback[$keymap:$seq]=${previous[${(V)seq}]}
        bindkey -M "$keymap" -- "$seq" _tab_link_key
      done
    done
  done
}

# Replaces the buffer if the user hasn't typed since the desktop saw it
_tab_link_set_buffer() {
  local seq=$1 cursor=$2 execute=$3
  [[ -z "$seq" || "$seq" == "$_tab_link_seq" ]] || return
  _tab_link_unescape "$4"
  BUFFER=$REPLY
  CURSOR=$cursor

  # Report the new buffer at the next seq, which the desktop expects
  (( _tab_link_seq++ ))
  _tab_link_sent="$CURSOR:$BUFFER"
  _tab_link_escape "$BUFFER"
  _tab_link_send "buffer"$'\t'"$_tab_link_seq"$'\t'"$CURSOR"$'\t'"$REPLY"

  # Only a key widget can run the line; from a push the user presses Enter
  if [[ "$execute" == 1 && -n "${_tab_link_in_key:-}" ]]; then
    zle accept-line
  else
    zle -R
  fi
}

# Shows the popup's top suggestion as ghost text, for the `specs` strategy
_tab_link_set_ghost() {
  _tab_link_unescape "$1"
  _tab_link_ghost_buffer=$REPLY
  _tab_link_unescape "$2"
  _tab_link_ghost_suffix=$REPLY
  if [[ "$_tab_link_ghost_buffer" == "$BUFFER" && -n "$_tab_link_ghost_suffix" ]] \
    && (( ${TAB_AUTOSUGGEST_STRATEGY[(Ie)specs]} && $+functions[_tab_autosuggest_suggest] )); then
    _tab_autosuggest_suggest "$BUFFER$_tab_link_ghost_suffix"
    zle -R
  fi
}

_tab_link_handle() {
  local -a fields
  fields=("${(@ps:\t:)1}")
  case "${fields[1]}" in
    bind|keys)
      local -a values
      local field
      for field in "${(@)fields[2,-1]}"; do
        _tab_link_unescape "$field"
        values+=("$REPLY")
      done
      if [[ "${fields[1]}" == bind ]]; then
        _tab_link_bind "${(@)values}"
      else
        _tab_link_actions=("${(@)values}")
      fi
      ;;
    set) _tab_link_set_buffer "${fields[2]}" "${fields[3]}" "${fields[4]}" "${fields[5]}" ;;
    ghost) _tab_link_set_ghost "${fields[2]}" "${fields[3]}" ;;
  esac
}

# Runs when the desktop pushes lines
_tab_link_on_push() {
  local line
  if ! IFS= read -r -u "$1" line 2>/dev/null; then
    _tab_link_disconnect
    return
  fi
  _tab_link_handle "$line"
  while IFS= read -r -t 0 -u "$1" line 2>/dev/null; do
    _tab_link_handle "$line"
  done
}

_tab_link_init_keys() {
  local -A names=(
    tab $'\t'
    shift+tab $'\e[Z'
    enter $'\r'
    esc $'\e'
    up $'\e[A'
    down $'\e[B'
    left $'\e[D'
    right $'\e[C'
    home $'\e[H'
    end $'\e[F'
    backspace $'\x7f'
    delete $'\e[3~'
    pageup $'\e[5~'
    pagedown $'\e[6~'
  )
  local name letter
  # control+letter first, so the named keys win where they share a byte
  for letter in {a..z}; do
    _tab_link_key_names[${(#)$(( ##$letter - 96 ))}]="control+$letter"
    _tab_link_key_names[$'\e'$letter]="option+$letter"
  done
  for name in "${(@k)names}"; do
    _tab_link_key_names[${names[$name]}]=$name
  done
  # Application-mode arrows and home/end
  _tab_link_key_names[$'\eOA']=up
  _tab_link_key_names[$'\eOB']=down
  _tab_link_key_names[$'\eOD']=left
  _tab_link_key_names[$'\eOC']=right
  _tab_link_key_names[$'\eOH']=home
  _tab_link_key_names[$'\eOF']=end
}

_tab_link_init_keys
zle -N _tab_link_key
zle -N _tab_link_on_push
autoload -Uz add-zle-hook-widget add-zsh-hook
add-zle-hook-widget line-pre-redraw _tab_link_report
add-zsh-hook precmd _tab_link_precmd
add-zsh-hook preexec _tab_link_preexec

fi
