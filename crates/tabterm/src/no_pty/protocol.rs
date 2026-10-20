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
    /// The edit buffer changed; `seq` counts changes, so tabterm can tag its edits
    /// with the buffer they were computed from
    Buffer { seq: u64, cursor: usize, text: String },
    /// The exported environment, as key/value pairs. Sent when it changes.
    Env(Vec<(String, String)>),
    /// The shell's aliases, in the format `alias` prints them. Sent when they change.
    Alias(String),
    /// A bound key was pressed; tabterm answers with [`LinkMessage::Key`]
    Key { name: String },
    /// The terminal tab or window gained or lost focus, from focus reporting (`CSI ? 1004 h`)
    Focus { focused: bool },
}

/// A message to the shell
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkMessage {
    /// Answer to [`ShellMessage::Key`]: whether tabterm took the key, and whether the
    /// key widget should wait for the buffer that follows on the push connection. zle
    /// can only run a line from inside a widget, so for actions that can run it the
    /// widget reads that buffer instead of the push handler.
    Key { taken: bool, expect_buffer: bool },
    /// Replace the edit buffer if it is still at `seq` (any buffer if `None`), and
    /// run it if `execute`
    SetBuffer {
        seq: Option<u64>,
        cursor: usize,
        text: String,
        execute: bool,
    },
    /// The popup's top suggestion: ghost text to show after `buffer`
    Ghost { buffer: String, suffix: String },
    /// The keys the popup may take, by name: the shell binds these and gives every
    /// other key back its own widget
    Bind { keys: Vec<String> },
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
                seq: next("seq")?.parse()?,
                cursor: next("cursor")?.parse()?,
                text: unescape(next("text")?),
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
            LinkMessage::SetBuffer {
                seq,
                cursor,
                text,
                execute,
            } => {
                let seq = seq.map(|seq| seq.to_string()).unwrap_or_default();
                format!("set\t{seq}\t{cursor}\t{}\t{}\n", u8::from(*execute), escape(text))
            },
            LinkMessage::Ghost { buffer, suffix } => {
                format!("ghost\t{}\t{}\n", escape(buffer), escape(suffix))
            },
            LinkMessage::Bind { keys } => {
                let mut line = String::from("bind");
                for key in keys {
                    line.push('\t');
                    line.push_str(&escape(key));
                }
                line.push('\n');
                line
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
            ShellMessage::parse("buffer\t4\t6\tgit ch").unwrap(),
            ShellMessage::Buffer {
                seq: 4,
                cursor: 6,
                text: "git ch".into(),
            }
        );
        assert_eq!(ShellMessage::parse("buffer\t0\t0\t").unwrap(), ShellMessage::Buffer {
            seq: 0,
            cursor: 0,
            text: String::new(),
        });
        assert_eq!(
            ShellMessage::parse("env\tPATH\t/bin:/usr/bin\tMULTI\ta\\nb\tEMPTY\t").unwrap(),
            ShellMessage::Env(vec![
                ("PATH".into(), "/bin:/usr/bin".into()),
                ("MULTI".into(), "a\nb".into()),
                ("EMPTY".into(), String::new()),
            ])
        );
        assert_eq!(ShellMessage::parse("env").unwrap(), ShellMessage::Env(vec![]));
        assert_eq!(
            ShellMessage::parse("alias\tg=git\\nll='ls -l'").unwrap(),
            ShellMessage::Alias("g=git\nll='ls -l'".into())
        );
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
            "buffer\tx\t3\ttext",
            "buffer\t1\tx\ttext",
            "buffer\t1\t3",
            "env\tKEY_WITHOUT_VALUE",
            "alias",
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
                seq: Some(7),
                cursor: 12,
                text: "git checkout\n".into(),
                execute: true,
            }
            .to_line(),
            "set\t7\t12\t1\tgit checkout\\n\n"
        );
        assert_eq!(
            LinkMessage::SetBuffer {
                seq: None,
                cursor: 2,
                text: "ls".into(),
                execute: false,
            }
            .to_line(),
            "set\t\t2\t0\tls\n"
        );
        assert_eq!(
            LinkMessage::Ghost {
                buffer: "git ch".into(),
                suffix: "eckout\t".into(),
            }
            .to_line(),
            "ghost\tgit ch\teckout\\t\n"
        );
        assert_eq!(
            LinkMessage::Bind {
                keys: vec!["enter".into(), "shift+tab".into()]
            }
            .to_line(),
            "bind\tenter\tshift+tab\n"
        );
        assert_eq!(LinkMessage::Bind { keys: vec![] }.to_line(), "bind\n");
    }
}
