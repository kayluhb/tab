
#--------------------------------------------------------------------#
# Specs Suggestion Strategy                                          #
#--------------------------------------------------------------------#
# Suggests the autocomplete popup's top suggestion, which tabterm
# (or, in shell link mode, the shell itself) caches for the current buffer. The buffer goes over stdin, not
# argv, so other users can't read it with ps.
#

_tab_autosuggest_strategy_specs() {
	# Shell link mode: the desktop pushes the suggestion to the shell
	if [[ -n "${_tab_link_on:-}" ]]; then
		[[ "$1" == "$_tab_link_ghost_buffer" && -n "$_tab_link_ghost_suffix" ]] \
			&& typeset -g suggestion="$1$_tab_link_ghost_suffix"
		return
	fi
	[[ -n "${TABTERM_SESSION_ID}" ]] || return
	typeset -g suggestion="$(command -v {{CLI_BINARY_NAME}} >/dev/null 2>&1 && print -rn -- "$1" | {{CLI_BINARY_NAME}} _ inline-suggestion 2>/dev/null)"
}
