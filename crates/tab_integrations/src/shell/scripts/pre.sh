#!/usr/bin/env bash

mkdir -p "${HOME}/.local/bin" > /dev/null 2>&1

# add ~/.local/bin to PATH
if [[ -d "${HOME}/.local/bin" ]] && [[ ":$PATH:" != *":${HOME}/.local/bin:"* ]]; then
  PATH="${PATH:+"$PATH:"}${HOME}/.local/bin"
fi

if [[ -n "${TAB_NEW_SESSION:-}" ]]; then
  unset TABTERM_SESSION_ID
  unset TAB_TERM
  unset TAB_NEW_SESSION
fi

if [[ -z "${TAB_SET_PARENT_CHECK:-}" ]]; then
  # Load parent from env variables
  if [[ -z "${TAB_PARENT:-}" && -n "${TAB_SET_PARENT:-}" ]]; then
    export TAB_PARENT="$TAB_SET_PARENT"
    unset -v TAB_SET_PARENT
  fi
  export TAB_SET_PARENT_CHECK=1
fi

# 0 = Yes, 1 = No, 2 = Fallback to TAB_TERM
if [ -z "${SHOULD_TABTERM_LAUNCH:-}" ]; then
  {{CLI_BINARY_NAME}} _ should-figterm-launch 1>/dev/null 2>&1
  SHOULD_TABTERM_LAUNCH=$?
fi

# zsh talks to the desktop app directly, with no PTY in between. TAB_SHELL_LINK=0
# goes back to tabterm.
_tab_link_on=""
if [[ -n "${ZSH_NAME:-}" ]] \
  && [[ "${TAB_SHELL_LINK:-1}" != 0 ]] \
  && [[ -t 1 ]] && [[ $- == *i* ]] \
  && [[ -z "${TAB_TERM:-}" && -z "${PROCESS_LAUNCHED_BY_TAB:-}" ]] \
  && [[ ("${SHOULD_TABTERM_LAUNCH}" -eq 0) || ("${SHOULD_TABTERM_LAUNCH}" -eq 2) ]] \
  && zmodload zsh/net/socket 2>/dev/null
then
  _tab_link_on=1
fi

# Only launch figterm if current session is not already inside PTY and command exists.
# PWSH var is set when launched by `pwsh -Login`, in which case we don't want to init.
# It is not necessary in Fish.
if   [[ -t 1 ]] \
  && [[ -z "${_tab_link_on:-}" ]] \
  && [[ -z "${PROCESS_LAUNCHED_BY_TAB:-}" ]] \
  && command -v {{PTY_BINARY_NAME}} 1>/dev/null 2>&1 \
  && [[ ("${SHOULD_TABTERM_LAUNCH}" -eq 0) || (("${SHOULD_TABTERM_LAUNCH}" -eq 2) && (-z "${TAB_TERM:-}" || (-z "${TAB_TERM_TMUX:-}" && -n "${TMUX:-}"))) ]]
then
  # Pty module sets TAB_TERM or TAB_TERM_TMUX to avoid running twice.
  if [ -z "${TAB_SHELL:-}" ]; then
    TAB_SHELL=$({{CLI_BINARY_NAME}} _ get-shell)
  fi
  TAB_IS_LOGIN_SHELL="${TAB_IS_LOGIN_SHELL:='0'}"

  # shellcheck disable=SC2030
  if ([[ -n "${BASH:-}" ]] && shopt -q login_shell) \
    || [[ -n "${ZSH_NAME:-}" && -o login ]]; then
    TAB_IS_LOGIN_SHELL=1
  fi

  # Do not launch figterm in non-interactive shells (like VSCode Tasks)
  if [[ $- == *i* ]]; then
    TAB_TERM_NAME="$(basename "${TAB_SHELL}") ({{PTY_BINARY_NAME}})"
    if [[ -z "${TAB_TERM_PATH:-}" ]]; then
      if [[ -x "${HOME}/.local/bin/${TAB_TERM_NAME}" ]]; then
        TAB_TERM_PATH="${HOME}/.local/bin/${TAB_TERM_NAME}"
      else
        TAB_TERM_PATH="$(command -v {{PTY_BINARY_NAME}} || echo "${HOME}/.local/bin/{{PTY_BINARY_NAME}}")"
      fi
    fi

    TAB_EXECUTION_STRING="${BASH_EXECUTION_STRING:=$ZSH_EXECUTION_STRING}"

    # Get initial text.
    INITIAL_TEXT=""
    # shellcheck disable=SC2031
    if [[ -z "${BASH:-}" || "${BASH_VERSINFO[0]}" -gt "3" ]]; then
      while read -rt 0; do
        if [[ -n "${BASH:-}" ]]; then
          read -r
        fi
        INITIAL_TEXT="${INITIAL_TEXT}${REPLY}\n"
      done
    fi
    TAB_EXECUTION_STRING="${TAB_EXECUTION_STRING}" TAB_START_TEXT="$(printf "%b" "${INITIAL_TEXT}")" TAB_SHELL="${TAB_SHELL}" TAB_IS_LOGIN_SHELL="${TAB_IS_LOGIN_SHELL}" exec -a "${TAB_TERM_NAME}" "${TAB_TERM_PATH}"
  fi
fi
