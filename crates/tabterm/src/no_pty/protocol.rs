//! The line protocol between the shell and tabterm in no-PTY mode.
//!
//! One message per line, fields separated by tabs. Values escape `\`, newline
//! and tab as `\\`, `\n` and `\t`, so the shell can send any buffer with plain
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
        pid: i32,
        tty: Option<String>,
        shell_path: Option<String>,
    },
    /// First line on the connection tabterm pushes edits down
    Push,
    /// A prompt is about to show; the previous command (if any) exited with this code.
    /// tabterm answers with a buffer to start the line with, or [`LinkMessage::Nothing`].
    Prompt { exit_code: Option<i32>, cwd: String },
    /// A command is about to run
    PreExec { command: String },
    /// The edit buffer changed
    Buffer { cursor: usize, text: String },
    /// A bound key was pressed; tabterm answers with [`LinkMessage::Key`]
    Key { name: String },
    /// The terminal tab or window gained or lost focus, from focus reporting (`CSI ? 1004 h`)
    Focus { focused: bool },
}

/// A message to the shell
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkMessage {
    /// Answer to [`ShellMessage::Key`]: whether tabterm took the key, and whether a
    /// buffer will follow on the push connection. zle can only run a line from inside
    /// a widget, so the key widget waits for that buffer instead of the push handler.
    Key { taken: bool, expect_buffer: bool },
    /// Replace the edit buffer, and run it if `execute`
    SetBuffer { cursor: usize, text: String, execute: bool },
    /// Answer to [`ShellMessage::Prompt`] when there is nothing to start the line with
    Nothing,
}

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
                pid: next("pid")?.parse()?,
                tty: non_empty(next("tty")?),
                shell_path: non_empty(next("shell_path")?),
            },
            "push" => ShellMessage::Push,
            "prompt" => ShellMessage::Prompt {
                exit_code: next("exit_code")?.parse().ok(),
                cwd: unescape(next("cwd")?),
            },
            "preexec" => ShellMessage::PreExec {
                command: unescape(next("command")?),
            },
            "buffer" => ShellMessage::Buffer {
                cursor: next("cursor")?.parse()?,
                text: unescape(next("text")?),
            },
            "key" => ShellMessage::Key {
                name: next("name")?.to_owned(),
            },
            "focus" => ShellMessage::Focus {
                focused: match next("focused")? {
                    "1" => true,
                    "0" => false,
                    other => bail!("focus: bad value {other:?}"),
                },
            },
            other => bail!("unknown message {other:?}"),
        })
    }
}

impl LinkMessage {
    /// The line to send, with its newline
    pub fn to_line(&self) -> String {
        match self {
            LinkMessage::Key { taken, expect_buffer } => {
                format!("key\t{}\t{}\n", u8::from(*taken), u8::from(*expect_buffer))
            },
            LinkMessage::Nothing => "none\n".into(),
            LinkMessage::SetBuffer { cursor, text, execute } => {
                format!("set\t{cursor}\t{}\t{}\n", u8::from(*execute), escape(text))
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_round_trips() {
        for value in [
            "",
            "git ch",
            "a\\b",
            "line one\nline two",
            "tab\there",
            "\\n literal",
            "trailing\\",
        ] {
            assert_eq!(unescape(&escape(value)), value, "{value:?}");
        }
        assert_eq!(escape("a\tb\nc\\d"), "a\\tb\\nc\\\\d");
    }

    #[test]
    fn parses_shell_messages() {
        assert_eq!(
            ShellMessage::parse("hello\t123\t/dev/ttys006\t/bin/zsh\n").unwrap(),
            ShellMessage::Hello {
                pid: 123,
                tty: Some("/dev/ttys006".into()),
                shell_path: Some("/bin/zsh".into()),
            }
        );
        assert_eq!(ShellMessage::parse("hello\t123\t\t").unwrap(), ShellMessage::Hello {
            pid: 123,
            tty: None,
            shell_path: None,
        });
        assert_eq!(ShellMessage::parse("push").unwrap(), ShellMessage::Push);
        assert_eq!(
            ShellMessage::parse("prompt\t1\t/tmp/a\\tb").unwrap(),
            ShellMessage::Prompt {
                exit_code: Some(1),
                cwd: "/tmp/a\tb".into(),
            }
        );
        assert_eq!(ShellMessage::parse("prompt\t\t/").unwrap(), ShellMessage::Prompt {
            exit_code: None,
            cwd: "/".into(),
        });
        assert_eq!(
            ShellMessage::parse("preexec\techo 'a\\nb'").unwrap(),
            ShellMessage::PreExec {
                command: "echo 'a\nb'".into()
            }
        );
        assert_eq!(
            ShellMessage::parse("buffer\t6\tgit ch").unwrap(),
            ShellMessage::Buffer {
                cursor: 6,
                text: "git ch".into(),
            }
        );
        assert_eq!(ShellMessage::parse("buffer\t0\t").unwrap(), ShellMessage::Buffer {
            cursor: 0,
            text: String::new(),
        });
        assert_eq!(ShellMessage::parse("key\tshift+tab").unwrap(), ShellMessage::Key {
            name: "shift+tab".into()
        });
        assert_eq!(ShellMessage::parse("focus\t0").unwrap(), ShellMessage::Focus {
            focused: false
        });
        assert_eq!(ShellMessage::parse("focus\t1").unwrap(), ShellMessage::Focus {
            focused: true
        });
    }

    #[test]
    fn rejects_bad_shell_messages() {
        for line in [
            "",
            "nope",
            "buffer\tx\ttext",
            "buffer\t3",
            "hello\tpid\t\t",
            "focus\t2",
            "focus",
        ] {
            assert!(ShellMessage::parse(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn formats_link_messages() {
        assert_eq!(
            LinkMessage::Key {
                taken: true,
                expect_buffer: true
            }
            .to_line(),
            "key\t1\t1\n"
        );
        assert_eq!(
            LinkMessage::Key {
                taken: false,
                expect_buffer: false
            }
            .to_line(),
            "key\t0\t0\n"
        );
        assert_eq!(LinkMessage::Nothing.to_line(), "none\n");
        assert_eq!(
            LinkMessage::SetBuffer {
                cursor: 12,
                text: "git checkout\n".into(),
                execute: true,
            }
            .to_line(),
            "set\t12\t1\tgit checkout\\n\n"
        );
    }
}
