//! Tab's vendored fork of zsh-autosuggestions, which draws inline "ghost text" suggestions.

use tab_util::CLI_BINARY_NAME;

// The order here is very specific, do not edit without understanding the implications
const ZSH_SCRIPT_TEMPLATE: &str = concat!(
    include_str!("inline/guard_start.zsh"),
    include_str!("inline/LICENSE"),
    include_str!("inline/config.zsh"),
    include_str!("inline/util.zsh"),
    include_str!("inline/bind.zsh"),
    include_str!("inline/highlight.zsh"),
    include_str!("inline/widgets.zsh"),
    include_str!("inline/strategies/specs.zsh"),
    include_str!("inline/strategies/history.zsh"),
    include_str!("inline/strategies/match_prev_cmd.zsh"),
    include_str!("inline/fetch.zsh"),
    include_str!("inline/async.zsh"),
    include_str!("inline/start.zsh"),
    include_str!("inline/guard_end.zsh"),
);

/// The plugin as a single zsh script
pub fn zsh_script() -> String {
    ZSH_SCRIPT_TEMPLATE.replace("{{CLI_BINARY_NAME}}", CLI_BINARY_NAME)
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::process::{
        Command,
        Stdio,
    };

    use super::*;

    #[test]
    fn script_contents() {
        let script = zsh_script();
        assert!(script.contains("Copyright (c) 2013 Thiago de Arruda"));
        assert!(script.contains("_tab_autosuggest_strategy_specs()"));
        assert!(script.contains("add-zsh-hook precmd _tab_autosuggest_start"));
        assert!(script.contains("export TAB_USING_ZSH_AUTOSUGGESTIONS=1"));
        assert!(!script.contains("{{"));
        assert!(!script.contains("inline_shell_completion"));
        assert!(!script.contains("inline-shell-completion"));
    }

    #[test]
    fn script_parses() {
        if Command::new("zsh").arg("--version").output().is_err() {
            return;
        }

        let mut child = Command::new("zsh")
            .arg("-n")
            .stdin(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(zsh_script().as_bytes()).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    }

    #[test]
    fn skips_fetch_for_recalled_line() {
        if Command::new("zsh").arg("--version").output().is_err() {
            return;
        }

        // An async fetch ends in a widget, which becomes $LASTWIDGET and stops
        // up-line-or-beginning-search continuing. So no fetch runs for a line a
        // history widget recalled, until the line is edited.
        let mut script = tempfile::NamedTempFile::new().unwrap();
        script.write_all(zsh_script().as_bytes()).unwrap();
        let output = Command::new("zsh")
            .args([
                "-f",
                "-c",
                r#"source "$1"
                zle -N recall-line true
                zle() { BUFFER="echo two"; }
                _tab_autosuggest_async_request() { print -r -- "fetch [$1]"; }
                BUFFER=echo
                _tab_autosuggest_clear recall-line
                _tab_autosuggest_fetch
                BUFFER="echo twox"
                _tab_autosuggest_fetch"#,
                "zsh",
            ])
            .arg(script.path())
            .output()
            .unwrap();
        assert!(
            output.status.success() && output.stderr.is_empty(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "fetch [echo twox]");
    }
}
