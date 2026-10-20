use clap::Parser;

#[derive(Debug, Parser, PartialEq, Eq)]
#[command(version, about)]
pub struct Cli {
    #[arg(last = true)]
    pub command: Option<Vec<String>>,
    /// Run without a PTY: print a session id, then serve the shell from the background
    #[arg(long, requires = "shell_pid", conflicts_with = "command")]
    pub no_pty: bool,
    /// The shell to serve in no-PTY mode; the session ends when it exits
    #[arg(long, requires = "no_pty")]
    pub shell_pid: Option<i32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verify_app() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }

    #[test]
    fn test_command() {
        let cli = Cli::parse_from(["figterm"]);
        assert_eq!(cli, Cli {
            command: None,
            no_pty: false,
            shell_pid: None
        });

        let cli = Cli::parse_from(["figterm", "--", "exe", "arg1", "arg2"]);
        assert_eq!(cli, Cli {
            command: Some(vec!["exe".into(), "arg1".into(), "arg2".into()]),
            no_pty: false,
            shell_pid: None
        });
    }

    #[test]
    fn test_no_pty() {
        let cli = Cli::parse_from(["figterm", "--no-pty", "--shell-pid", "42"]);
        assert_eq!(cli, Cli {
            command: None,
            no_pty: true,
            shell_pid: Some(42)
        });

        assert!(Cli::try_parse_from(["figterm", "--no-pty"]).is_err());
        assert!(Cli::try_parse_from(["figterm", "--shell-pid", "42"]).is_err());
    }
}
