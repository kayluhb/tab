# zsh-only code, which shellcheck reads as bash: expansion flags, assoc array literals
# shellcheck disable=SC2034,SC2066,SC2190,SC2296,SC2298
if [[ -n "${ZSH_NAME:-}" ]]; then

# No-PTY mode: tabterm runs in the background and zsh reports to it over a
# socket, so nothing sits between the terminal and the shell. pre.sh starts it
# and sets TABTERM_MODE=no-pty and TABTERM_LINK_SOCKET. The protocol is in
# crates/tabterm/src/no_pty/protocol.rs. Lines with text are read with `IFS=`,
# or read would strip spaces at the ends of the text.

typeset -g _tab_link_fd="" _tab_link_push_fd="" _tab_link_sent=""
# Counts buffer reports; tabterm tags each insert with the one it was computed from
typeset -gi _tab_link_seq=0
typeset -gA _tab_link_fallback _tab_link_key_name

_tab_link_active() {
  [[ "${TABTERM_MODE:-}" == no-pty && -n "${TABTERM_LINK_SOCKET:-}" ]]
}

# Escapes a value for the protocol into REPLY
_tab_link_escape() {
  REPLY=${1//\\/\\\\}
  REPLY=${REPLY//$'\n'/\\n}
  REPLY=${REPLY//$'\t'/\\t}
}

_tab_link_disconnect() {
  if [[ -n "$_tab_link_push_fd" ]]; then
    zle -F "$_tab_link_push_fd" 2>/dev/null
    exec {_tab_link_push_fd}>&-
  fi
  [[ -n "$_tab_link_fd" ]] && exec {_tab_link_fd}>&-
  _tab_link_fd="" _tab_link_push_fd="" _tab_link_sent=""
  _tab_link_env_sent="" _tab_link_alias_sent=""
}

_tab_link_connect() {
  [[ -n "$_tab_link_fd" ]] && return 0
  _tab_link_active || return 1
  [[ -S "$TABTERM_LINK_SOCKET" ]] || return 1
  zmodload zsh/net/socket 2>/dev/null || return 1

  zsocket "$TABTERM_LINK_SOCKET" 2>/dev/null || return 1
  _tab_link_fd=$REPLY
  if zsocket "$TABTERM_LINK_SOCKET" 2>/dev/null; then
    _tab_link_push_fd=$REPLY
    print -r -u "$_tab_link_push_fd" -- push
    zle -F -w "$_tab_link_push_fd" _tab_link_on_push 2>/dev/null
  fi

  _tab_link_escape "${TTY:-}"
  local tty=$REPLY
  _tab_link_escape "${TAB_SHELL:-$SHELL}"
  _tab_link_send "hello"$'\t'"$$"$'\t'"$tty"$'\t'"$REPLY"
}

# Sends one line; drops the connection if tabterm is gone
_tab_link_send() {
  [[ -n "$_tab_link_fd" ]] || return 1
  if ! print -r -u "$_tab_link_fd" -- "$1" 2>/dev/null; then
    _tab_link_disconnect
    return 1
  fi
}

# Text the next line should start with (e.g. a shell started with initial text).
# precmd cannot edit the line, so zle-line-init applies it.
typeset -g _tab_link_start_text="" _tab_link_start_run=""

typeset -g _tab_link_env_sent="" _tab_link_alias_sent=""

# Sends the exported environment and the aliases, each only when it changed
# since it was last sent. Built in the shell, with no process or subshell.
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

  # The same `name=value` lines `alias` prints
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

_tab_link_prompt() {
  _tab_link_connect || return
  _tab_link_send_context
  _tab_link_escape "$PWD"
  _tab_link_send "prompt"$'\t'"$1"$'\t'"$REPLY" || return
  _tab_link_sent=""
  # Ask the terminal to report focus changes while at the prompt, so switching
  # tabs or windows can hide the popup
  print -n $'\e[?1004h'
  _tab_link_focus_on=1
  local reply
  IFS= read -r -u "$_tab_link_fd" -t 0.2 reply 2>/dev/null || return
  _tab_link_parse_set "$reply" || return
  _tab_link_start_text=$REPLY _tab_link_start_run=$_tab_link_set_execute
}

_tab_link_line_init() {
  [[ -n "$_tab_link_start_text" ]] || return
  BUFFER=$_tab_link_start_text
  CURSOR=${#BUFFER}
  _tab_link_start_text=""
  [[ "$_tab_link_start_run" == 1 ]] && zle accept-line
}

typeset -g _tab_link_focus_on=""

# Stops focus reporting, so commands never see the focus sequences
_tab_link_focus_off() {
  [[ -n "$_tab_link_focus_on" ]] || return
  print -n $'\e[?1004l'
  _tab_link_focus_on=""
}

# Hides the popup. The line stays reported as is, so the redraw after this
# widget does not show it again; the next edit does.
_tab_link_focus_out() {
  _tab_link_send "focus"$'\t'"0"
}

_tab_link_focus_in() {
  _tab_link_send "focus"$'\t'"1"
}

_tab_link_preexec() {
  _tab_link_focus_off
  [[ -n "$_tab_link_fd" ]] || return
  _tab_link_escape "$1"
  _tab_link_send "preexec"$'\t'"$REPLY"
}

# Reports the buffer on every redraw that changed it, while editing a command
# line: not in vared or other zle contexts. ^R search runs no redraw hook, so
# the line is reported once the search ends.
_tab_link_report() {
  [[ -n "$_tab_link_fd" ]] && [[ "$CONTEXT" == start || "$CONTEXT" == cont ]] || return
  local state="$CURSOR"$'\t'"$BUFFER"
  [[ "$state" == "$_tab_link_sent" ]] && return
  _tab_link_sent=$state
  (( ++_tab_link_seq ))
  _tab_link_escape "$BUFFER"
  _tab_link_send "buffer"$'\t'"$_tab_link_seq"$'\t'"$CURSOR"$'\t'"$REPLY"
}

# Bound to the keys the popup uses. Asks tabterm whether the popup takes the
# key; if not, runs whatever the key did before.
_tab_link_key() {
  local name=${_tab_link_key_name[$KEYS]} reply=""
  # Drop an answer that arrived after an earlier query gave up waiting
  [[ -n "$_tab_link_fd" ]] && while read -r -u "$_tab_link_fd" -t 0 reply 2>/dev/null; do :; done
  reply=""
  if [[ -n "$name" && -n "$_tab_link_fd" ]] && _tab_link_send "key"$'\t'"$name"; then
    read -r -u "$_tab_link_fd" -t 0.1 reply 2>/dev/null
  fi
  case "$reply" in
    $'key\t1\t1')
      # Apply the insert here: only a widget can run the line. Ghost text may
      # arrive before it on the same connection.
      local line
      while [[ -n "$_tab_link_push_fd" ]] && IFS= read -r -u "$_tab_link_push_fd" -t 0.2 line 2>/dev/null; do
        if [[ "$line" == ghost$'\t'* ]]; then
          _tab_link_set_ghost "$line"
          continue
        fi
        _tab_link_apply "$line" && zle accept-line
        break
      done
      return ;;
    $'key\t1\t0') return ;;
  esac

  local fallback=${_tab_link_fallback[$KEYMAP:$KEYS]:-${_tab_link_fallback[main:$KEYS]}}
  [[ -n "$fallback" && "$fallback" != undefined-key ]] && zle "$fallback"
}

# Parses a `set` line: the text into REPLY, its seq, cursor and execute flag into
# _tab_link_set_seq, _tab_link_set_cursor and _tab_link_set_execute. Fails for
# any other line.
typeset -g _tab_link_set_seq="" _tab_link_set_cursor="" _tab_link_set_execute=""
_tab_link_parse_set() {
  local -a fields
  fields=("${(@ps:\t:)1}")
  [[ "${fields[1]}" == set ]] || return 1
  _tab_link_set_seq=${fields[2]} _tab_link_set_cursor=${fields[3]} _tab_link_set_execute=${fields[4]}
  _tab_link_unescape "${(pj:\t:)fields[5,-1]}"
}

# Undoes _tab_link_escape into REPLY, holding `\\` aside as NUL so `\\n` stays
# a backslash and an n
_tab_link_unescape() {
  REPLY=${1//\\\\/$'\0'}
  REPLY=${REPLY//\\t/$'\t'}
  REPLY=${REPLY//\\n/$'\n'}
  REPLY=${REPLY//$'\0'/\\}
}

# Sets the line from a `set` line; succeeds only if the line should run. Drops
# an insert computed for an older buffer, i.e. the user typed since.
_tab_link_apply() {
  _tab_link_parse_set "$1" || return 1
  [[ -z "$_tab_link_set_seq" || "$_tab_link_set_seq" == "$_tab_link_seq" ]] || return 1
  BUFFER=$REPLY
  CURSOR=$_tab_link_set_cursor
  # Report the new line now, at the next seq, so a following insert lines up
  _tab_link_report
  [[ "$_tab_link_set_execute" == 1 ]]
}

# The popup's top suggestion, which the `specs` inline strategy reads
typeset -g _tab_link_ghost_buffer="" _tab_link_ghost_suffix=""

# Stores the suggestion from a `ghost` line, and redraws the ghost text if the
# line still matches
_tab_link_set_ghost() {
  local -a fields
  fields=("${(@ps:\t:)1}")
  _tab_link_unescape "${fields[2]}"
  _tab_link_ghost_buffer=$REPLY
  _tab_link_unescape "${fields[3]}"
  _tab_link_ghost_suffix=$REPLY
  [[ "$_tab_link_ghost_buffer" == "$BUFFER" && " ${TAB_AUTOSUGGEST_STRATEGY[*]} " == *" specs "* ]] \
    && _tab_link_refresh_ghost
}

# Redraws the inline suggestions plugin's ghost text for the line. The plugin
# skips the push widget: its wrapper puts the old ghost text back whenever the
# line did not change, which would hide a pushed suggestion.
_tab_link_refresh_ghost() {
  # shellcheck disable=SC2154
  (( ${+widgets[autosuggest-fetch]} )) || return
  zle autosuggest-clear
  (( ${#BUFFER} && ! ${+_TAB_AUTOSUGGEST_DISABLED} )) && zle autosuggest-fetch
}

# Handles a line tabterm pushes while zsh is idle: a buffer or ghost text. zle
# does not finish a line from a -F handler, so a pushed `execute` waits for the
# next key.
_tab_link_on_push() {
  local line
  if ! IFS= read -r -u "$1" line 2>/dev/null; then
    _tab_link_disconnect
    return
  fi
  if [[ "$line" == ghost$'\t'* ]]; then
    _tab_link_set_ghost "$line"
  else
    _tab_link_apply "$line"
    _tab_link_refresh_ghost
  fi
  zle -R
}

_tab_link_bind_keys() {
  local -A names=(
    $'\t' tab
    $'\r' enter
    $'\e' esc
    $'\e[A' up
    $'\eOA' up
    $'\e[B' down
    $'\eOB' down
    $'\e[Z' shift+tab
    $'\C-p' control+p
    $'\C-n' control+n
    $'\C-k' control+k
    $'\C-r' control+r
  )
  local keymap seq binding
  for keymap in main emacs viins; do
    for seq in "${(@k)names}"; do
      binding=${${(z)$(bindkey -M "$keymap" -- "$seq" 2>/dev/null)}[2]}
      [[ "$binding" == tab-link-key ]] && continue
      _tab_link_fallback[$keymap:$seq]=$binding
      bindkey -M "$keymap" -- "$seq" tab-link-key
    done
  done
  _tab_link_key_name=("${(@kv)names}")

  # Focus reports, bound in every keymap so they never insert text
  for keymap in main emacs viins vicmd; do
    bindkey -M "$keymap" -- $'\e[O' _tab_link_focus_out
    bindkey -M "$keymap" -- $'\e[I' _tab_link_focus_in
  done
}

if _tab_link_active; then
  # No leading underscore, so the inline suggestions plugin wraps the key widget
  # and refreshes its ghost text when it changes the line. The push widget has
  # one, so the plugin skips it; it refreshes the ghost text itself.
  zle -N tab-link-key _tab_link_key
  zle -N _tab_link_on_push
  zle -N _tab_link_focus_out
  zle -N _tab_link_focus_in
  autoload -Uz add-zle-hook-widget
  add-zle-hook-widget line-pre-redraw _tab_link_report
  add-zle-hook-widget line-init _tab_link_line_init
  # Its own hook: fig_preexec does nothing in no-PTY mode
  preexec_functions+=(_tab_link_preexec)
  zshexit_functions+=(_tab_link_focus_off)
  _tab_link_bind_keys
fi

fi
