
#--------------------------------------------------------------------#
# Specs Suggestion Strategy                                          #
#--------------------------------------------------------------------#
# Suggests the autocomplete popup's top suggestion, which tabterm
# caches for the current buffer. The buffer goes over stdin, not
# argv, so other users can't read it with ps. In no-PTY mode tabterm
# pushes the suggestion to the shell instead (see no_pty.zsh).
#

_tab_autosuggest_strategy_specs() {
	if (( $+functions[_tab_link_active] )) && _tab_link_active; then
		[[ "$1" == "$_tab_link_ghost_buffer" && -n "$_tab_link_ghost_suffix" ]] \
			&& typeset -g suggestion="$1$_tab_link_ghost_suffix"
		return
	fi
	[[ -n "${TABTERM_SESSION_ID}" ]] || return
	typeset -g suggestion="$(command -v {{CLI_BINARY_NAME}} >/dev/null 2>&1 && print -rn -- "$1" | {{CLI_BINARY_NAME}} _ inline-suggestion 2>/dev/null)"
}
