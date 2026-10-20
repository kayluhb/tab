//! Names of the environment variables that Tab reads or sets.
//!
//! [`ALL`] lists the names that diagnostics show. Names whose values can hold user commands
//! are not in [`ALL`].

macro_rules! define_env_vars {
    ($($(#[$meta:meta])* $ident:ident = $name:expr),*) => {
        $(
            $(#[$meta])*
            pub const $ident: &str = $name;
        )*

        pub const ALL: &[&str] = &[$($ident),*];
    }
}

define_env_vars! {
    /// The UUID of the current parent qterm instance
    TABTERM_SESSION_ID = "TABTERM_SESSION_ID",

    /// The current parent socket to connect to
    TAB_PARENT = "TAB_PARENT",

    /// Set the [`TAB_PARENT`] parent socket to connect to
    TAB_SET_PARENT = "TAB_SET_PARENT",

    /// Guard for the [`TAB_SET_PARENT`] check
    TAB_SET_PARENT_CHECK = "TAB_SET_PARENT_CHECK",

    /// Set if qterm is running, contains the version
    TAB_TERM = "TAB_TERM",

    /// Sets the current log level
    TAB_LOG_LEVEL = "TAB_LOG_LEVEL",

    /// Overrides the ZDOTDIR environment variable
    TAB_ZDOTDIR = "TAB_ZDOTDIR",

    /// Indicates a process was launched by Tab
    PROCESS_LAUNCHED_BY_TAB = "PROCESS_LAUNCHED_BY_TAB",

    /// The shell to use in qterm
    TAB_SHELL = "TAB_SHELL",

    /// Indicates the user is debugging the shell
    TAB_DEBUG_SHELL = "TAB_DEBUG_SHELL",

    /// Indicates the user is using zsh autosuggestions which disables Inline
    TAB_USING_ZSH_AUTOSUGGESTIONS = "TAB_USING_ZSH_AUTOSUGGESTIONS",

    /// Overrides the path to the bundle metadata released with certain desktop builds.
    TAB_BUNDLE_METADATA_PATH = "TAB_BUNDLE_METADATA_PATH",

    /// Set if qterm is running inside tmux, contains the version
    TAB_TERM_TMUX = "TAB_TERM_TMUX",

    /// Set to `no-pty` when tabterm serves the shell from the background instead of a PTY
    TABTERM_MODE = "TABTERM_MODE",

    /// The socket a no-PTY shell reports to tabterm on
    TABTERM_LINK_SOCKET = "TABTERM_LINK_SOCKET",

    /// Disables autocomplete in qterm when set to a non-empty value
    TAB_DISABLE_AUTOCOMPLETE = "TAB_DISABLE_AUTOCOMPLETE",

    /// Set to `1` when qterm should start the shell as a login shell
    TAB_IS_LOGIN_SHELL = "TAB_IS_LOGIN_SHELL",

    /// Forces codespaces detection
    TAB_CODESPACES = "TAB_CODESPACES",

    /// Forces CI detection
    TAB_CI = "TAB_CI",

    /// Forces the current environment to report as remote
    TAB_FAKE_IS_REMOTE = "TAB_FAKE_IS_REMOTE",

    /// Writes logs to stdout
    TAB_LOG_STDOUT = "TAB_LOG_STDOUT",

    /// Set during init snapshot tests
    TAB_INIT_SNAPSHOT_TEST = "TAB_INIT_SNAPSHOT_TEST",

    /// Overrides the backend
    TAB_BACKEND = "TAB_BACKEND",

    /// Overrides the prompt offset workaround
    TAB_PROMPT_OFFSET_WORKAROUND = "TAB_PROMPT_OFFSET_WORKAROUND"
}

// The values of the names below can hold user commands and arguments. They are not in [`ALL`],
// so diagnostics do not show them.

/// A command string that qterm passes to the shell with `-c`
pub const TAB_EXECUTION_STRING: &str = "TAB_EXECUTION_STRING";

/// Extra arguments that qterm passes to the shell
pub const TAB_SHELL_EXTRA_ARGS: &str = "TAB_SHELL_EXTRA_ARGS";

/// Initial text that qterm writes to the shell
pub const TAB_START_TEXT: &str = "TAB_START_TEXT";

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;

    #[test]
    fn all_has_no_duplicates() {
        let unique: HashSet<_> = ALL.iter().collect();
        assert_eq!(unique.len(), ALL.len());
    }

    #[test]
    fn all_contains_tabterm_vars() {
        for name in [TAB_DISABLE_AUTOCOMPLETE, TAB_IS_LOGIN_SHELL, TAB_TERM_TMUX] {
            assert!(ALL.contains(&name), "{name} is missing from ALL");
        }
    }

    #[test]
    fn all_excludes_command_vars() {
        for name in [TAB_EXECUTION_STRING, TAB_SHELL_EXTRA_ARGS, TAB_START_TEXT] {
            assert!(!ALL.contains(&name), "{name} must not be in ALL");
        }
    }
}
