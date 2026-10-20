# zsh-only code, which shellcheck reads as bash: expansion flags, assoc array literals
# shellcheck disable=SC2034,SC2066,SC2190,SC2296,SC2298
if [[ -n "${ZSH_NAME:-}" ]]; then

# No-PTY mode: tabterm runs in the background and zsh reports to it over a
# socket, so nothing sits between the terminal and the shell. pre.sh starts it
# and sets TABTERM_MODE=no-pty and TABTERM_LINK_SOCKET. The protocol is in
# crates/tabterm/src/no_pty/protocol.rs.

typeset -g _tab_link_fd="" _tab_link_push_fd="" _tab_link_sent=""
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
    zle -F -w "$_tab_link_push_fd" tab-link-on-push 2>/dev/null
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

_tab_link_prompt() {
  _tab_link_connect || return
  _tab_link_escape "$PWD"
  _tab_link_send "prompt"$'\t'"$1"$'\t'"$REPLY" || return
  _tab_link_sent=""
  # Ask the terminal to report focus changes while at the prompt, so switching
  # tabs or windows can hide the popup
  print -n $'\e[?1004h'
  _tab_link_focus_on=1
  local reply
  read -r -u "$_tab_link_fd" -t 0.2 reply 2>/dev/null || return
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

# Reports the buffer on every redraw that changed it
_tab_link_report() {
  [[ -n "$_tab_link_fd" ]] || return
  local state="$CURSOR"$'\t'"$BUFFER"
  [[ "$state" == "$_tab_link_sent" ]] && return
  _tab_link_sent=$state
  _tab_link_escape "$BUFFER"
  _tab_link_send "buffer"$'\t'"$CURSOR"$'\t'"$REPLY"
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
      # Apply the insert here: only a widget can run the line
      local line
      if [[ -n "$_tab_link_push_fd" ]] && read -r -u "$_tab_link_push_fd" -t 0.2 line 2>/dev/null; then
        _tab_link_apply "$line" && zle accept-line
      fi
      return ;;
    $'key\t1\t0') return ;;
  esac

  local fallback=${_tab_link_fallback[$KEYMAP:$KEYS]:-${_tab_link_fallback[main:$KEYS]}}
  [[ -n "$fallback" && "$fallback" != undefined-key ]] && zle "$fallback"
}

# Parses a `set` line: the text into REPLY, its cursor and execute flag into
# _tab_link_set_cursor and _tab_link_set_execute. Fails for any other line.
typeset -g _tab_link_set_cursor="" _tab_link_set_execute=""
_tab_link_parse_set() {
  local -a fields
  fields=("${(@ps:\t:)1}")
  [[ "${fields[1]}" == set ]] || return 1
  _tab_link_set_cursor=${fields[2]} _tab_link_set_execute=${fields[3]}
  local text=${(pj:\t:)fields[4,-1]}
  # Undo the escapes, holding `\\` aside as NUL so `\\n` stays a backslash and an n
  text=${text//\\\\/$'\0'}
  text=${text//\\t/$'\t'}
  text=${text//\\n/$'\n'}
  REPLY=${text//$'\0'/\\}
}

# Sets the line from a `set` line; succeeds only if the line should run
_tab_link_apply() {
  _tab_link_parse_set "$1" || return 1
  BUFFER=$REPLY
  CURSOR=$_tab_link_set_cursor
  [[ "$_tab_link_set_execute" == 1 ]]
}

# Applies a buffer tabterm pushes while zsh is idle. zle does not finish a line
# from a -F handler, so a pushed `execute` waits for the next key.
_tab_link_on_push() {
  local line
  if ! read -r -u "$1" line 2>/dev/null; then
    _tab_link_disconnect
    return
  fi
  _tab_link_apply "$line"
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
  # Widget names without a leading underscore, so the inline suggestions plugin
  # wraps them and refreshes its ghost text when they change the line
  zle -N tab-link-key _tab_link_key
  zle -N tab-link-on-push _tab_link_on_push
  zle -N _tab_link_focus_out
  zle -N _tab_link_focus_in
  autoload -Uz add-zle-hook-widget
  add-zle-hook-widget line-pre-redraw _tab_link_report
  add-zle-hook-widget line-init _tab_link_line_init
  # Its own hook, not fig_preexec: fig_precmd also calls that after ^C, with no command
  preexec_functions+=(_tab_link_preexec)
  zshexit_functions+=(_tab_link_focus_off)
  _tab_link_bind_keys
fi

fi
