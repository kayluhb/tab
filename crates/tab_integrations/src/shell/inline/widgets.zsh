
#--------------------------------------------------------------------#
# Autosuggest Widget Implementations                                 #
#--------------------------------------------------------------------#

# Disable suggestions
_tab_autosuggest_disable() {
	typeset -g _TAB_AUTOSUGGEST_DISABLED
	_tab_autosuggest_clear
}

# Enable suggestions
_tab_autosuggest_enable() {
	unset _TAB_AUTOSUGGEST_DISABLED

	if (( $#BUFFER )); then
		_tab_autosuggest_fetch
	fi
}

# Toggle suggestions (enable/disable)
_tab_autosuggest_toggle() {
	if (( ${+_TAB_AUTOSUGGEST_DISABLED} )); then
		_tab_autosuggest_enable
	else
		_tab_autosuggest_disable
	fi
}

# Clear the suggestion
_tab_autosuggest_clear() {
	local orig_buffer="$BUFFER"

	# Remove the suggestion
	unset POSTDISPLAY

	_tab_autosuggest_invoke_original_widget $@

	# Remember a line a history widget recalled, so no fetch runs for it. An
	# async fetch ends in a widget, which changes $LASTWIDGET and breaks widgets
	# that check it to continue, e.g. up-line-or-beginning-search.
	typeset -g _TAB_AUTOSUGGEST_CLEARED_BUFFER
	if (( $# )) && [[ "$BUFFER" != "$orig_buffer" ]]; then
		_TAB_AUTOSUGGEST_CLEARED_BUFFER="$BUFFER"
	fi
}

# Modify the buffer and get a new suggestion
_tab_autosuggest_modify() {
	local -i retval

	# Only available in zsh >= 5.4
	local -i KEYS_QUEUED_COUNT

	# Save the contents of the buffer/postdisplay
	local orig_buffer="$BUFFER"
	local orig_postdisplay="$POSTDISPLAY"

	# Clear suggestion while waiting for next one
	unset POSTDISPLAY

	# Original widget may modify the buffer
	_tab_autosuggest_invoke_original_widget $@
	retval=$?

	emulate -L zsh

	# Don't fetch a new suggestion if there's more input to be read immediately
	if (( $PENDING > 0 || $KEYS_QUEUED_COUNT > 0 )); then
		POSTDISPLAY="$orig_postdisplay"
		return $retval
	fi

	# Optimize if manually typing in the suggestion or if buffer hasn't changed
	if [[ "$BUFFER" = "$orig_buffer"* && "$orig_postdisplay" = "${BUFFER:$#orig_buffer}"* ]]; then
		POSTDISPLAY="${orig_postdisplay:$(($#BUFFER - $#orig_buffer))}"
		return $retval
	fi

	# Bail out if suggestions are disabled
	if (( ${+_TAB_AUTOSUGGEST_DISABLED} )); then
		return $?
	fi

	# Get a new suggestion if the buffer is not empty after modification
	if (( $#BUFFER > 0 )); then
		if [[ -z "$TAB_AUTOSUGGEST_BUFFER_MAX_SIZE" ]] || (( $#BUFFER <= $TAB_AUTOSUGGEST_BUFFER_MAX_SIZE )); then
			_tab_autosuggest_fetch
		fi
	fi

	return $retval
}

# Fetch a new suggestion based on what's currently in the buffer
_tab_autosuggest_fetch() {
	# Not for a line a history widget recalled (see _tab_autosuggest_clear)
	[[ -n "$_TAB_AUTOSUGGEST_CLEARED_BUFFER" && "$BUFFER" == "$_TAB_AUTOSUGGEST_CLEARED_BUFFER" ]] && return

	if (( ${+TAB_AUTOSUGGEST_USE_ASYNC} )); then
		_tab_autosuggest_async_request "$BUFFER"
	else
		local suggestion
		_tab_autosuggest_fetch_suggestion "$BUFFER"
		_tab_autosuggest_suggest "$suggestion"
	fi
}

# Offer a suggestion
_tab_autosuggest_suggest() {
	emulate -L zsh

	local suggestion="$1"

	if [[ -n "$suggestion" ]] && (( $#BUFFER )); then
		POSTDISPLAY="${suggestion#$BUFFER}"
	else
		unset POSTDISPLAY
	fi
}

# Accept the entire suggestion
_tab_autosuggest_accept() {
	local -i retval max_cursor_pos=$#BUFFER

	# When vicmd keymap is active, the cursor can't move all the way
	# to the end of the buffer
	if [[ "$KEYMAP" = "vicmd" ]]; then
		max_cursor_pos=$((max_cursor_pos - 1))
	fi

	# If we're not in a valid state to accept a suggestion, just run the
	# original widget and bail out
	if (( $CURSOR != $max_cursor_pos || !$#POSTDISPLAY )); then
		_tab_autosuggest_invoke_original_widget $@
		return
	fi

	# Only accept if the cursor is at the end of the buffer
	# Add the suggestion to the buffer
	BUFFER="$BUFFER$POSTDISPLAY"

	# Remove the suggestion
	unset POSTDISPLAY

	# Run the original widget before manually moving the cursor so that the
	# cursor movement doesn't make the widget do something unexpected
	_tab_autosuggest_invoke_original_widget $@
	retval=$?

	# Move the cursor to the end of the buffer
	if [[ "$KEYMAP" = "vicmd" ]]; then
		CURSOR=$(($#BUFFER - 1))
	else
		CURSOR=$#BUFFER
	fi

	return $retval
}

# Accept the entire suggestion and execute it
_tab_autosuggest_execute() {

	# Add the suggestion to the buffer
	BUFFER="$BUFFER$POSTDISPLAY"

	# Remove the suggestion
	unset POSTDISPLAY

	# Call the original `accept-line` to handle syntax highlighting or
	# other potential custom behavior
	_tab_autosuggest_invoke_original_widget "accept-line"
}

# Partially accept the suggestion
_tab_autosuggest_partial_accept() {
	local -i retval cursor_loc

	# Save the contents of the buffer so we can restore later if needed
	local original_buffer="$BUFFER"

	# Temporarily accept the suggestion.
	BUFFER="$BUFFER$POSTDISPLAY"

	# Original widget moves the cursor
	_tab_autosuggest_invoke_original_widget $@
	retval=$?

	# Normalize cursor location across vi/emacs modes
	cursor_loc=$CURSOR
	if [[ "$KEYMAP" = "vicmd" ]]; then
		cursor_loc=$((cursor_loc + 1))
	fi

	# If we've moved past the end of the original buffer
	if (( $cursor_loc > $#original_buffer )); then
		# Set POSTDISPLAY to text right of the cursor
		POSTDISPLAY="${BUFFER[$(($cursor_loc + 1)),$#BUFFER]}"

		# Clip the buffer at the cursor
		BUFFER="${BUFFER[1,$cursor_loc]}"
	else
		# Restore the original buffer
		BUFFER="$original_buffer"
	fi

	return $retval
}

() {
	typeset -ga _TAB_AUTOSUGGEST_BUILTIN_ACTIONS

	_TAB_AUTOSUGGEST_BUILTIN_ACTIONS=(
		clear
		fetch
		suggest
		accept
		execute
		enable
		disable
		toggle
	)

	local action
	for action in $_TAB_AUTOSUGGEST_BUILTIN_ACTIONS modify partial_accept; do
		eval "_tab_autosuggest_widget_$action() {
			local -i retval

			_tab_autosuggest_highlight_reset

			_tab_autosuggest_$action \$@
			retval=\$?

			_tab_autosuggest_highlight_apply

			zle -R

			return \$retval
		}"
	done

	for action in $_TAB_AUTOSUGGEST_BUILTIN_ACTIONS; do
		zle -N autosuggest-$action _tab_autosuggest_widget_$action
	done
}
