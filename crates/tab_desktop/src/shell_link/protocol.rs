//! The line protocol between a shell and the desktop app.
//!
//! One message per line, fields separated by tabs. Values escape `\`, newline
//! and tab as `\\`, `\n` and `\t`, so zsh can send any buffer with plain
//! parameter expansion. Cursors count characters, as zsh's `$CURSOR` does.

use anyhow::{
    Result,
    anyhow,
    bail,
};

/// A message from the shell
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShellMessage {
    /// First line on a connection: which shell this is
    Hello {
        shell_path: Option<String>,
        tty: Option<String>,
    },
    /// The exported environment, as key/value pairs
    Env(Vec<(String, String)>),
    /// The shell's aliases, in the format `alias` prints them
    Alias(String),
    /// A prompt is about to show; the previous command (if any) exited with this code
    Prompt { exit_code: Option<i32>, cwd: String },
    /// A command is about to run
    PreExec { command: String },
    /// The edit buffer changed; `seq` counts changes so stale edits can be dropped
    Buffer { seq: u64, cursor: usize, text: String },
    /// The shell took a key for the popup
    Action { key: String, action: String },
}

/// A message to the shell
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DesktopMessage {
    /// Every key the popup may ever take; the shell binds these once and checks
    /// [`DesktopMessage::Keys`] on each press
    Bind(Vec<String>),
    /// The keys the popup takes right now, with their actions; empty means none
    Keys(Vec<(String, String)>),
    /// Replace the edit buffer if it is still at `seq` (any buffer if `None`),
    /// and run it if `execute`
    SetBuffer {
        seq: Option<u64>,
        cursor: usize,
        text: String,
        execute: bool,
    },
    /// Ghost text to show after `buffer`
    Ghost { buffer: String, suffix: String },
}

/// The longest line the desktop reads from a shell
pub const MAX_LINE_BYTES: usize = 1024 * 1024;

pub fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out
}

pub fn unescape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| unescape(value))
}

impl ShellMessage {
    pub fn parse(line: &str) -> Result<Self> {
        let line = line.strip_suffix('\n').unwrap_or(line);
        let mut fields = line.split('\t');
        let kind = fields.next().unwrap_or_default();
        let mut next = |name: &str| fields.next().ok_or_else(|| anyhow!("{kind}: missing {name}"));

        Ok(match kind {
            "hello" => ShellMessage::Hello {
                shell_path: non_empty(next("shell_path")?),
                tty: non_empty(next("tty")?),
            },
            "env" => {
                let mut pairs = Vec::new();
                while let Some(key) = fields.next() {
                    let value = fields.next().ok_or_else(|| anyhow!("env: {key} has no value"))?;
                    pairs.push((unescape(key), unescape(value)));
                }
                ShellMessage::Env(pairs)
            },
            "alias" => ShellMessage::Alias(unescape(next("text")?)),
            "prompt" => ShellMessage::Prompt {
                exit_code: next("exit_code")?.parse().ok(),
                cwd: unescape(next("cwd")?),
            },
            "preexec" => ShellMessage::PreExec {
                command: unescape(next("command")?),
            },
            "buffer" => ShellMessage::Buffer {
                seq: next("seq")?.parse()?,
                cursor: next("cursor")?.parse()?,
                text: unescape(next("text")?),
            },
            "action" => ShellMessage::Action {
                key: next("key")?.to_owned(),
                action: next("action")?.to_owned(),
            },
            other => bail!("unknown message {other:?}"),
        })
    }
}

impl DesktopMessage {
    /// The line to send, with its newline
    pub fn to_line(&self) -> String {
        match self {
            DesktopMessage::Bind(keys) => {
                let mut line = String::from("bind");
                for key in keys {
                    line.push('\t');
                    line.push_str(&escape(key));
                }
                line.push('\n');
                line
            },
            DesktopMessage::Keys(keys) => {
                let mut line = String::from("keys");
                for (key, action) in keys {
                    line.push('\t');
                    line.push_str(&escape(key));
                    line.push('\t');
                    line.push_str(&escape(action));
                }
                line.push('\n');
                line
            },
            DesktopMessage::SetBuffer {
                seq,
                cursor,
                text,
                execute,
            } => {
                let seq = seq.map(|seq| seq.to_string()).unwrap_or_default();
                format!("set\t{seq}\t{cursor}\t{}\t{}\n", u8::from(*execute), escape(text))
            },
            DesktopMessage::Ghost { buffer, suffix } => {
                format!("ghost\t{}\t{}\n", escape(buffer), escape(suffix))
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trips() {
        for value in ["", "plain", "a\tb", "line\nnext", "back\\slash", "\\n literal", "é ✓"] {
            assert_eq!(unescape(&escape(value)), value);
        }
    }

    #[test]
    fn parses_shell_messages() {
        assert_eq!(
            ShellMessage::parse("hello\t/bin/zsh\t/dev/ttys001\n").unwrap(),
            ShellMessage::Hello {
                shell_path: Some("/bin/zsh".into()),
                tty: Some("/dev/ttys001".into()),
            }
        );
        assert_eq!(ShellMessage::parse("hello\t\t").unwrap(), ShellMessage::Hello {
            shell_path: None,
            tty: None,
        });
        assert_eq!(
            ShellMessage::parse("env\tPATH\t/bin:/usr/bin\tMULTI\ta\\nb").unwrap(),
            ShellMessage::Env(vec![
                ("PATH".into(), "/bin:/usr/bin".into()),
                ("MULTI".into(), "a\nb".into())
            ])
        );
        assert_eq!(ShellMessage::parse("env").unwrap(), ShellMessage::Env(vec![]));
        assert_eq!(
            ShellMessage::parse("alias\tg=git\\nll='ls -l'").unwrap(),
            ShellMessage::Alias("g=git\nll='ls -l'".into())
        );
        assert_eq!(
            ShellMessage::parse("prompt\t1\t/tmp/a\\tb").unwrap(),
            ShellMessage::Prompt {
                exit_code: Some(1),
                cwd: "/tmp/a\tb".into()
            }
        );
        assert_eq!(ShellMessage::parse("prompt\t\t/").unwrap(), ShellMessage::Prompt {
            exit_code: None,
            cwd: "/".into()
        });
        assert_eq!(
            ShellMessage::parse("preexec\techo hi").unwrap(),
            ShellMessage::PreExec {
                command: "echo hi".into()
            }
        );
        assert_eq!(
            ShellMessage::parse("buffer\t7\t3\tgit\\tx").unwrap(),
            ShellMessage::Buffer {
                seq: 7,
                cursor: 3,
                text: "git\tx".into()
            }
        );
        assert_eq!(
            ShellMessage::parse("action\ttab\tinsertCommonPrefix").unwrap(),
            ShellMessage::Action {
                key: "tab".into(),
                action: "insertCommonPrefix".into()
            }
        );
    }

    #[test]
    fn rejects_bad_shell_messages() {
        assert!(ShellMessage::parse("nope").is_err());
        assert!(ShellMessage::parse("buffer\tx\t1\tgit").is_err());
        assert!(ShellMessage::parse("buffer\t1").is_err());
        assert!(ShellMessage::parse("env\tKEY").is_err());
        assert!(ShellMessage::parse("action\ttab").is_err());
    }

    #[test]
    fn formats_desktop_messages() {
        assert_eq!(
            DesktopMessage::Bind(vec!["tab".into(), "control+r".into()]).to_line(),
            "bind\ttab\tcontrol+r\n"
        );
        assert_eq!(DesktopMessage::Keys(vec![]).to_line(), "keys\n");
        assert_eq!(
            DesktopMessage::Keys(vec![
                ("tab".into(), "insertCommonPrefix".into()),
                ("esc".into(), "hide".into())
            ])
            .to_line(),
            "keys\ttab\tinsertCommonPrefix\tesc\thide\n"
        );
        assert_eq!(
            DesktopMessage::SetBuffer {
                seq: Some(4),
                cursor: 12,
                text: "git checkout".into(),
                execute: false
            }
            .to_line(),
            "set\t4\t12\t0\tgit checkout\n"
        );
        assert_eq!(
            DesktopMessage::SetBuffer {
                seq: None,
                cursor: 1,
                text: "a\nb".into(),
                execute: true
            }
            .to_line(),
            "set\t\t1\t1\ta\\nb\n"
        );
        assert_eq!(
            DesktopMessage::Ghost {
                buffer: "git ch".into(),
                suffix: "eckout".into()
            }
            .to_line(),
            "ghost\tgit ch\teckout\n"
        );
    }
}
