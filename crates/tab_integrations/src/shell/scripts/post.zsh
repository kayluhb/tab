if [[ -n "${ZSH_NAME:-}" ]]; then

# add ~/.local/bin to PATH
if [[ -d "${HOME}/.local/bin" ]] && [[ ":$PATH:" != *":${HOME}/.local/bin:"* ]]; then
  PATH="${PATH:+"$PATH:"}${HOME}/.local/bin"
fi

if [[ -z "${TTY:-}" ]]; then
  TTY=$(tty)
fi
export TTY

export SHELL_PID="$$"

if [[ -z "${TAB_SHELL:-}" ]]; then
  TAB_SHELL=$({{CLI_BINARY_NAME}} _ get-shell)
fi

# shellcheck disable=SC2059
function fig_osc { printf "\033]697;$1\007" "${@:2}"; }

TAB_HAS_SET_PROMPT=0

fig_preexec() {
  # No-PTY mode never wraps the prompt, and no PTY reads the markers
  _tab_link_active && return

  # Restore user defined prompt before executing.
  if [ -n "${PS1+x}" ]; then
    PS1="$TAB_USER_PS1"
  fi
  if [ -n "${PROMPT+x}" ]; then
    PROMPT="$TAB_USER_PROMPT"
  fi
  if [ -n "${prompt+x}" ]; then
    prompt="$TAB_USER_prompt"
  fi

  if [ -n "${PS2+x}" ]; then
    PS2="$TAB_USER_PS2"
  fi
  if [ -n "${PROMPT2+x}" ]; then
    PROMPT2="$TAB_USER_PROMPT2"
  fi

  if [ -n "${PS3+x}" ]; then
    PS3="$TAB_USER_PS3"
  fi
  if [ -n "${PROMPT3+x}" ]; then
    PROMPT3="$TAB_USER_PROMPT3"
  fi

  if [ -n "${PS4+x}" ]; then
    PS4="$TAB_USER_PS4"
  fi
  if [ -n "${PROMPT4+x}" ]; then
    PROMPT4="$TAB_USER_PROMPT4"
  fi

  if [ -n "${RPS1+x}" ]; then
    RPS1="$TAB_USER_RPS1"
  fi
  if [ -n "${RPROMPT+x}" ]; then
    RPROMPT="$TAB_USER_RPROMPT"
  fi

  if [ -n "${RPS2+x}" ]; then
    RPS2="$TAB_USER_RPS2"
  fi
  if [ -n "${RPROMPT2+x}" ]; then
    RPROMPT2="$TAB_USER_RPROMPT2"
  fi

  TAB_HAS_SET_PROMPT=0

  fig_osc "OSCLock=%s" "${TABTERM_SESSION_ID}"
  fig_osc PreExec
}

fig_precmd() {
  local LAST_STATUS=$?

  fig_reset_hooks
  _tab_link_prompt "$LAST_STATUS"
  # No-PTY mode: the link socket already has the prompt, cwd, exit code, env
  # and aliases. The OSC markers and prompt wrapping below are only read by the
  # PTY tabterm.
  _tab_link_active && return

  fig_osc "OSCUnlock=%s" "${TABTERM_SESSION_ID}"
  fig_osc "Dir=%s" "$PWD"
  fig_osc "Shell=zsh"
  fig_osc "ShellPath=%s" "${TAB_SHELL:-$SHELL}"
  if [[ -n "${WSL_DISTRO_NAME:-}" ]]; then
    fig_osc "WSLDistro=%s" "${WSL_DISTRO_NAME}"
  fi
  fig_osc "PID=%d" "$$"
  fig_osc "ExitCode=%s" "${LAST_STATUS}"
  fig_osc "TTY=%s" "${TTY}"
  fig_osc "Log=%s" "${TAB_LOG_LEVEL}"
  fig_osc "ZshAutosuggestionColor=%s" "${ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE}"
  fig_osc "FigAutosuggestionColor=%s" "${TAB_AUTOSUGGEST_HIGHLIGHT_STYLE}"
  fig_osc "User=%s" "${USER:-root}"

  if [ "$TAB_HAS_SET_PROMPT" -eq 1 ]; then
    # ^C pressed while entering command, call preexec manually to clear fig prompts.
    fig_preexec
  fi

  START_PROMPT=$'\033]697;StartPrompt\007'
  END_PROMPT=$'\033]697;EndPrompt\007'
  NEW_CMD=$'\033]697;NewCmd='"${TABTERM_SESSION_ID}"$'\007'

  # Save user defined prompts.
  TAB_USER_PS1="$PS1"
  TAB_USER_PROMPT="$PROMPT"
  TAB_USER_prompt="$prompt"

  TAB_USER_PS2="$PS2"
  TAB_USER_PROMPT2="$PROMPT2"

  TAB_USER_PS3="$PS3"
  TAB_USER_PROMPT3="$PROMPT3"

  TAB_USER_PS4="$PS4"
  TAB_USER_PROMPT4="$PROMPT4"

  TAB_USER_RPS1="$RPS1"
  TAB_USER_RPROMPT="$RPROMPT"

  TAB_USER_RPS2="$RPS2"
  TAB_USER_RPROMPT2="$RPROMPT2"

  if [ -n "${PROMPT+x}" ]; then
    PROMPT="%{$START_PROMPT%}$PROMPT%{$END_PROMPT$NEW_CMD%}"
  elif [ -n "${prompt+x}" ]; then
    prompt="%{$START_PROMPT%}$prompt%{$END_PROMPT$NEW_CMD%}"
  else
    PS1="%{$START_PROMPT%}$PS1%{$END_PROMPT$NEW_CMD%}"
  fi

  if [ -n "${PROMPT2+x}" ]; then
    PROMPT2="%{$START_PROMPT%}$PROMPT2%{$END_PROMPT%}"
  else
    PS2="%{$START_PROMPT%}$PS2%{$END_PROMPT%}"
  fi

  if [ -n "${PROMPT3+x}" ]; then
    PROMPT3="%{$START_PROMPT%}$PROMPT3%{$END_PROMPT$NEW_CMD%}"
  else
    PS3="%{$START_PROMPT%}$PS3%{$END_PROMPT$NEW_CMD%}"
  fi

  if [ -n "${PROMPT4+x}" ]; then
    PROMPT4="%{$START_PROMPT%}$PROMPT4%{$END_PROMPT%}"
  else
    PS4="%{$START_PROMPT%}$PS4%{$END_PROMPT%}"
  fi

  # Previously, the af-magic theme added a final % to expand. We need to paste without the %
  # to avoid doubling up and mangling the prompt. I've removed this workaround for now.
  if [ -n "${RPROMPT+x}" ]; then
    RPROMPT="%{$START_PROMPT%}$RPROMPT%{$END_PROMPT%}"
  else
    RPS1="%{$START_PROMPT%}$RPS1%{$END_PROMPT%}"
  fi

  if [ -n "${RPROMPT2+x}" ]; then
    RPROMPT2="%{$START_PROMPT%}$RPROMPT2%{$END_PROMPT%}"
  else
    RPS2="%{$START_PROMPT%}$RPS2%{$END_PROMPT%}"
  fi

  TAB_HAS_SET_PROMPT=1

  if command -v {{CLI_BINARY_NAME}} >/dev/null 2>&1; then
    (command {{CLI_BINARY_NAME}} _ pre-cmd --alias "$(\alias)" > /dev/null 2>&1 &) >/dev/null 2>&1
  fi
}

fig_reset_hooks() {
  # shellcheck disable=SC1087,SC2193
  if [[ "$precmd_functions[-1]" != fig_precmd ]]; then
    # shellcheck disable=SC2206,SC2296
    precmd_functions=(${(@)precmd_functions:#fig_precmd} fig_precmd)
  fi
  # shellcheck disable=SC1087,SC2193
  if [[ "$preexec_functions[1]" != fig_preexec ]]; then
    # shellcheck disable=SC2206,SC2296
    preexec_functions=(fig_preexec ${(@)preexec_functions:#fig_preexec})
  fi
}

fig_reset_hooks
if [[ -n "${PROCESS_LAUNCHED_BY_TAB:-}" ]]; then
  fig_osc DoneSourcing
fi

fi

# no_pty.zsh, which defines _tab_link_active, is sourced after this file
if [[ "${TABTERM_MODE:-}" != no-pty ]]; then
  (command {{CLI_BINARY_NAME}} _ pre-cmd --alias "$(\alias)" > /dev/null 2>&1 &) >/dev/null 2>&1
fi
