//! What tabterm knows about the shell in no-PTY mode. The shell reports its own
//! state, so this replaces what PTY mode reads off the screen.

use std::path::PathBuf;
use std::str::Chars;
use std::time::SystemTime;

use alacritty_terminal::term::CommandInfo;
use tab_proto::figterm::InsertTextRequest;
use tab_proto::local::ShellContext;
use tab_util::Terminal as FigTerminal;

use crate::interceptor::{
    KeyInterceptor,
    key_from_text,
};
use crate::{
    HOSTNAME,
    SHELL_ALIAS,
    SHELL_ENVIRONMENT_VARIABLES,
};

pub struct Session {
    pub session_id: String,
    pub pid: i32,
    pub tty: Option<String>,
    pub shell_path: Option<String>,
    pub cwd: Option<PathBuf>,
    pub preexec: bool,
    pub buffer: String,
    /// Cursor position in characters
    pub cursor: usize,
    pub interceptor: KeyInterceptor,
    /// The running command and when it started, for history
    running: Option<(String, SystemTime)>,
}

impl Session {
    pub fn new(session_id: String, pid: i32, interceptor: KeyInterceptor) -> Self {
        Self {
            session_id,
            pid,
            tty: None,
            shell_path: None,
            cwd: None,
            preexec: false,
            buffer: String::new(),
            cursor: 0,
            interceptor,
            running: None,
        }
    }

    fn shell_name(&self) -> Option<String> {
        let path = self.shell_path.as_deref()?;
        Some(path.rsplit('/').next().unwrap_or(path).to_owned())
    }

    pub fn context(&self) -> ShellContext {
        ShellContext {
            pid: Some(self.pid),
            ttys: self.tty.clone(),
            process_name: self.shell_name(),
            current_working_directory: self.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            session_id: Some(self.session_id.clone()),
            terminal: FigTerminal::parent_terminal(&tab_os_shim::Context::new()).map(|t| t.to_string()),
            hostname: whoami_hostname(),
            shell_path: self.shell_path.clone(),
            environment_variables: SHELL_ENVIRONMENT_VARIABLES.lock().unwrap().clone(),
            qterm_version: Some(env!("CARGO_PKG_VERSION").into()),
            preexec: Some(self.preexec),
            osc_lock: Some(false),
            alias: SHELL_ALIAS.lock().unwrap().clone(),
            ..Default::default()
        }
    }

    /// The cursor as a byte index into the buffer, which is what the desktop expects
    pub fn cursor_bytes(&self) -> usize {
        char_to_byte(&self.buffer, self.cursor)
    }

    pub fn set_buffer(&mut self, text: String, cursor: usize) {
        self.cursor = cursor.min(text.chars().count());
        self.buffer = text;
    }

    /// Applies an insert from the desktop, in the order PTY mode types it: catch up
    /// with `insertion_buffer`, move by `offset`, delete, insert. Returns whether
    /// to run the line: `immediate`, or a trailing newline, which a PTY would run.
    pub fn apply_insert(&mut self, request: &InsertTextRequest) -> bool {
        if let Some(expected) = &request.insertion_buffer {
            // The popup computed the insert against `expected`; edit from there
            if self.buffer != *expected
                && (self.buffer.starts_with(expected.as_str()) || expected.starts_with(self.buffer.as_str()))
            {
                self.buffer.clone_from(expected);
                self.cursor = self.buffer.chars().count();
            }
        }

        if let Some(offset) = request.offset {
            let len = self.buffer.chars().count() as i64;
            self.cursor = (self.cursor as i64 + offset).clamp(0, len) as usize;
        }

        for _ in 0..request.deletion.unwrap_or(0).min(self.cursor as u64) {
            self.delete_before_cursor();
        }

        // A PTY runs the line on a newline, which is how the popup passes Enter through.
        // Only keys, such as a `{cursor}` move, can follow it: they would not edit the
        // line that ran, so they are dropped. A newline before more text stays in the line.
        let mut execute = request.immediate == Some(true);
        if let Some(insertion) = &request.insertion {
            let text = match insertion.rfind(['\r', '\n']) {
                Some(newline) if only_keys(&insertion[newline + 1..]) => {
                    execute = true;
                    insertion[..newline].trim_end_matches(['\r', '\n'])
                },
                _ => insertion.as_str(),
            };
            self.type_text(text);
        }

        execute
    }

    /// Types `text` at the cursor the way zsh reads it from a PTY. The popup replaces
    /// what the user typed with backspaces, and moves the cursor with `ESC [ D`.
    fn type_text(&mut self, text: &str) {
        let mut chars = text.chars();
        while let Some(c) = chars.next() {
            match c {
                '\x08' | '\x7f' => self.delete_before_cursor(),
                '\x1b' => self.arrow_key(read_escape(&mut chars)),
                _ => {
                    let at = char_to_byte(&self.buffer, self.cursor);
                    self.buffer.insert(at, c);
                    self.cursor += 1;
                },
            }
        }
    }

    fn arrow_key(&mut self, key: Option<char>) {
        match key {
            Some('D') => self.cursor = self.cursor.saturating_sub(1),
            Some('C') => self.cursor = (self.cursor + 1).min(self.buffer.chars().count()),
            _ => {},
        }
    }

    fn delete_before_cursor(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            let at = char_to_byte(&self.buffer, self.cursor);
            self.buffer.remove(at);
        }
    }

    /// The action bound to a key the shell reports, if tabterm should take it now
    pub fn intercept(&mut self, key_name: &str) -> Option<String> {
        if self.preexec {
            return None;
        }
        let event = key_from_text(key_name)?;
        let action = self.interceptor.intercept_key(&event)?;
        if key_name == "esc" {
            self.interceptor.reset();
        }
        Some(action)
    }

    pub fn start_command(&mut self, command: String) {
        self.preexec = true;
        self.interceptor.reset();
        self.running = Some((command, SystemTime::now()));
        self.set_buffer(String::new(), 0);
    }

    /// Forgets the line and stops taking the popup's keys, e.g. when the popup should hide
    pub fn clear_line(&mut self) {
        self.set_buffer(String::new(), 0);
        self.interceptor.reset();
    }

    /// Marks the prompt as shown and returns the finished command, if any, for history.
    /// A new prompt starts an empty line, even after ^C, which runs no command.
    pub fn prompt(&mut self, exit_code: Option<i32>, cwd: PathBuf) -> Option<CommandInfo> {
        self.preexec = false;
        self.clear_line();
        let finished = self.running.take().map(|(command, start_time)| CommandInfo {
            command: Some(command),
            shell: self.shell_name(),
            pid: Some(self.pid),
            session_id: Some(self.session_id.clone()),
            cwd: self.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            start_time: Some(start_time),
            end_time: Some(SystemTime::now()),
            username: std::env::var("USER").ok(),
            exit_code,
        });
        self.cwd = Some(cwd);
        finished
    }
}

/// Whether the desktop answers an intercepted action by changing the buffer
pub fn action_inserts(action: &str) -> bool {
    action.starts_with("insert") || action == "execute"
}

fn whoami_hostname() -> Option<String> {
    let username = std::env::var("USER").ok()?;
    HOSTNAME.as_deref().map(|hostname| format!("{username}@{hostname}"))
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
}

/// Reads the escape sequence after an ESC. Returns its arrow key, if it is one.
fn read_escape(chars: &mut Chars<'_>) -> Option<char> {
    match chars.next() {
        // CSI: parameter and intermediate bytes, then one final byte
        Some('[') => {
            let seq = chars.as_str();
            let end = seq.find(|c: char| ('\x40'..='\x7e').contains(&c));
            *chars = seq[end.map_or(seq.len(), |end| end + 1)..].chars();
            if end == Some(0) { seq.chars().next() } else { None }
        },
        // SS3, which a terminal in application cursor mode sends for arrows
        Some('O') => chars.next(),
        // A meta key or a lone ESC runs a zsh widget; neither inserts text
        _ => None,
    }
}

/// Whether `text` holds only keys (backspaces and escapes), and no text to insert
fn only_keys(text: &str) -> bool {
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match c {
            '\x08' | '\x7f' => {},
            '\x1b' => _ = read_escape(&mut chars),
            _ => return false,
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use tab_proto::figterm::Action;

    use super::*;

    fn session(buffer: &str, cursor: usize) -> Session {
        let mut session = Session::new("abc".into(), 1, KeyInterceptor::new());
        session.set_buffer(buffer.into(), cursor);
        session
    }

    fn insert(insertion: &str) -> InsertTextRequest {
        InsertTextRequest {
            insertion: Some(insertion.into()),
            ..Default::default()
        }
    }

    #[test]
    fn insert_appends_at_cursor() {
        let mut s = session("git che", 7);
        assert!(!s.apply_insert(&InsertTextRequest {
            insertion_buffer: Some("git che".into()),
            ..insert("ckout")
        }));
        assert_eq!((s.buffer.as_str(), s.cursor), ("git checkout", 12));
    }

    #[test]
    fn insert_deletes_then_inserts() {
        let mut s = session("git ch", 6);
        s.apply_insert(&InsertTextRequest {
            deletion: Some(2),
            ..insert("checkout ")
        });
        assert_eq!((s.buffer.as_str(), s.cursor), ("git checkout ", 13));
    }

    #[test]
    fn insert_moves_by_offset_first() {
        let mut s = session("ab", 2);
        s.apply_insert(&InsertTextRequest {
            offset: Some(-1),
            ..insert("X")
        });
        assert_eq!((s.buffer.as_str(), s.cursor), ("aXb", 2));
    }

    #[test]
    fn insert_catches_up_with_insertion_buffer() {
        // The user typed one more key before the insert arrived
        let mut s = session("git stat", 8);
        s.apply_insert(&InsertTextRequest {
            insertion_buffer: Some("git sta".into()),
            ..insert("tus")
        });
        assert_eq!(s.buffer, "git status");

        // The insert raced a key the shell has not reported yet
        let mut s = session("git st", 6);
        s.apply_insert(&InsertTextRequest {
            insertion_buffer: Some("git sta".into()),
            ..insert("tus")
        });
        assert_eq!(s.buffer, "git status");
    }

    #[test]
    fn insert_ignores_an_unrelated_insertion_buffer() {
        let mut s = session("ls", 2);
        s.apply_insert(&InsertTextRequest {
            insertion_buffer: Some("git".into()),
            ..insert(" -la")
        });
        assert_eq!(s.buffer, "ls -la");
    }

    #[test]
    fn insert_handles_multibyte_text() {
        let mut s = session("echo é", 6);
        assert_eq!(s.cursor_bytes(), 7);
        s.apply_insert(&InsertTextRequest {
            deletion: Some(1),
            ..insert("ü")
        });
        assert_eq!((s.buffer.as_str(), s.cursor, s.cursor_bytes()), ("echo ü", 6, 7));
    }

    #[test]
    fn insert_reports_immediate() {
        let mut s = session("ls", 2);
        assert!(s.apply_insert(&InsertTextRequest {
            immediate: Some(true),
            ..insert("")
        }));
    }

    #[test]
    fn trailing_newline_runs_the_line() {
        // The popup passes Enter through by inserting a newline, which a PTY would run
        for newline in ["\n", "\r", "\r\n"] {
            let mut s = session("git status", 10);
            assert!(s.apply_insert(&insert(newline)), "{newline:?}");
            assert_eq!((s.buffer.as_str(), s.cursor), ("git status", 10), "{newline:?}");
        }

        let mut s = session("git che", 7);
        assert!(s.apply_insert(&insert("ckout\n")));
        assert_eq!(s.buffer, "git checkout");
    }

    #[test]
    fn deletion_stops_at_start() {
        let mut s = session("ab", 1);
        s.apply_insert(&InsertTextRequest {
            deletion: Some(5),
            ..Default::default()
        });
        assert_eq!((s.buffer.as_str(), s.cursor), ("b", 0));
    }

    #[test]
    fn insertion_backspaces_delete_instead_of_inserting() {
        // The popup replaces what the user typed by sending backspaces before the suggestion
        let branch = "fix/variant-image-late-sync";
        let mut s = session(&format!("gco {branch}"), 31);
        s.apply_insert(&insert(&format!("{}{branch}", "\x08".repeat(branch.len()))));
        assert_eq!((s.buffer.as_str(), s.cursor), ("gco fix/variant-image-late-sync", 31));

        let mut s = session("ab", 1);
        s.apply_insert(&insert("\x08\x08\x7fX"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("Xb", 1));
    }

    #[test]
    fn insertion_arrow_keys_move_the_cursor() {
        // A suggestion with `{cursor}` moves back into the inserted text
        let mut s = session("git commit -m ", 14);
        s.apply_insert(&insert("\"\"\x1b[D"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("git commit -m \"\"", 15));

        let mut s = session("ab", 0);
        s.apply_insert(&insert("\x1b[C\x1b[C\x1b[CX"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("abX", 3));
    }

    #[test]
    fn insertion_backspace_deletes_a_whole_char() {
        let mut s = session("aé", 2);
        s.apply_insert(&insert("\x08"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("a", 1));
    }

    #[test]
    fn insertion_keys_stop_at_the_ends() {
        let mut s = session("ab", 1);
        s.apply_insert(&insert("\x08\x08\x1b[D\x1b[DX"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("Xb", 1));
    }

    #[test]
    fn insertion_arrow_keys_in_application_mode() {
        let mut s = session("ab", 2);
        s.apply_insert(&insert("\x1bODX\x1bOC"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("aXb", 3));
    }

    #[test]
    fn insertion_drops_other_escapes() {
        // Up arrow, alt+left and meta-b: zsh runs a widget for each
        for escape in ["\x1b[A", "\x1b[1;3D", "\x1bb"] {
            let mut s = session("", 0);
            s.apply_insert(&insert(&format!("a{escape}b")));
            assert_eq!((s.buffer.as_str(), s.cursor), ("ab", 2), "{escape:?}");
        }

        let mut s = session("", 0);
        s.apply_insert(&insert("a\x1b"));
        assert_eq!((s.buffer.as_str(), s.cursor), ("a", 1));
    }

    #[test]
    fn insertion_newline_runs_the_line_before_later_keys() {
        // A `{cursor}` move comes after the suggestion, so after its newline
        let mut s = session("", 0);
        assert!(s.apply_insert(&insert("ls\n\x1b[D")));
        assert_eq!((s.buffer.as_str(), s.cursor), ("ls", 2));
    }

    #[test]
    fn insertion_newline_before_text_stays_in_the_line() {
        // Nothing is dropped: a newline with text after it does not run the line
        let mut s = session("", 0);
        assert!(!s.apply_insert(&insert("a\nb")));
        assert_eq!((s.buffer.as_str(), s.cursor), ("a\nb", 3));

        let mut s = session("", 0);
        assert!(s.apply_insert(&insert("a\nb\r\n\x1b[D")));
        assert_eq!((s.buffer.as_str(), s.cursor), ("a\nb", 3));
    }

    #[test]
    fn huge_deletion_stops_at_start() {
        let mut s = session("ab", 1);
        s.apply_insert(&InsertTextRequest {
            deletion: Some(u64::MAX),
            ..Default::default()
        });
        assert_eq!((s.buffer.as_str(), s.cursor), ("b", 0));
    }

    #[test]
    fn intercepts_bound_keys_only_while_bound() {
        let mut s = session("git ch", 6);
        s.interceptor.set_actions(
            &[Action {
                identifier: "insertSelected".into(),
                bindings: vec!["enter".into()],
            }],
            true,
        );
        assert_eq!(s.intercept("enter"), None, "not intercepting yet");

        s.interceptor.set_intercept(true);
        s.interceptor.set_window_visible(true);
        assert_eq!(s.intercept("enter").as_deref(), Some("insertSelected"));
        assert_eq!(s.intercept("tab"), None, "unbound key");

        s.start_command("ls".into());
        assert_eq!(s.intercept("enter"), None, "a command is running");
    }

    #[test]
    fn knows_which_actions_insert() {
        for action in [
            "insertSelected",
            "insertCommonPrefix",
            "insertCommonPrefixOrNavigateDown",
            "insertSelectedAndExecute",
            "execute",
        ] {
            assert!(action_inserts(action), "{action}");
        }
        for action in [
            "navigateDown",
            "navigateUp",
            "hideAutocomplete",
            "toggleDescription",
            "toggleHistoryMode",
        ] {
            assert!(!action_inserts(action), "{action}");
        }
    }

    #[test]
    fn prompt_clears_the_line_and_releases_keys() {
        // ^C at the prompt: the line is gone but no command ran
        let mut s = session("git ch", 6);
        s.interceptor.set_actions(
            &[Action {
                identifier: "insertSelected".into(),
                bindings: vec!["enter".into()],
            }],
            true,
        );
        s.interceptor.set_intercept(true);
        s.interceptor.set_window_visible(true);
        assert!(s.intercept("enter").is_some());

        assert!(s.prompt(None, "/".into()).is_none());
        assert_eq!((s.buffer.as_str(), s.cursor), ("", 0));
        assert_eq!(s.intercept("enter"), None);
    }

    #[test]
    fn prompt_finishes_the_running_command() {
        let mut s = session("", 0);
        assert!(s.prompt(None, "/".into()).is_none());

        s.start_command("ls -la".into());
        assert!(s.preexec);
        let info = s.prompt(Some(2), "/tmp".into()).unwrap();
        assert_eq!(info.command.as_deref(), Some("ls -la"));
        assert_eq!(info.exit_code, Some(2));
        assert_eq!(info.cwd.as_deref(), Some("/"));
        assert!(!s.preexec);
        assert_eq!(s.cwd, Some("/tmp".into()));
    }
}
