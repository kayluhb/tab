//! What tabterm knows about the shell in no-PTY mode. The shell reports its own
//! state, so this replaces what PTY mode reads off the screen.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::SystemTime;

use alacritty_terminal::term::CommandInfo;
use tab_proto::figterm::InsertTextRequest;
use tab_proto::local::ShellContext;
use tab_util::Terminal as FigTerminal;

use crate::input::{
    KeyCode,
    KeyEvent,
    Modifiers,
};
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
    /// The shell's number for the last buffer it reported
    pub seq: u64,
    pub interceptor: KeyInterceptor,
    /// The running command and when it started, for history
    running: Option<(String, SystemTime)>,
    /// The keys the shell was last told to bind
    bound_keys: Option<BTreeSet<String>>,
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
            seq: 0,
            interceptor,
            running: None,
            bound_keys: None,
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
            self.cursor -= 1;
            let at = char_to_byte(&self.buffer, self.cursor);
            self.buffer.remove(at);
        }

        // A PTY runs the line on a newline, which is how the popup passes Enter through
        let mut execute = request.immediate == Some(true);
        if let Some(insertion) = &request.insertion {
            let text = insertion.trim_end_matches(['\r', '\n']);
            execute |= text.len() != insertion.len();
            let at = char_to_byte(&self.buffer, self.cursor);
            self.buffer.insert_str(at, text);
            self.cursor += text.chars().count();
        }

        execute
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

    /// The keys the shell should bind, by the names it reports them with. A lone Esc
    /// also starts other keys, so while it is bound zsh waits `KEYTIMEOUT` after every
    /// Esc; it is bound only while the popup would take it.
    pub fn shell_keys(&self) -> BTreeSet<String> {
        let mut keys: BTreeSet<String> = self
            .interceptor
            .bound_keys()
            .iter()
            .filter_map(shell_key_name)
            .collect();
        let esc = KeyEvent {
            key: KeyCode::Escape,
            modifiers: Modifiers::NONE,
        };
        if self.preexec || self.interceptor.intercept_key(&esc).is_none() {
            keys.remove("esc");
        }
        keys
    }

    /// The keys to bind, if they changed since the shell was last told
    pub fn take_bind_update(&mut self) -> Option<BTreeSet<String>> {
        let keys = self.shell_keys();
        if self.bound_keys.as_ref() == Some(&keys) {
            return None;
        }
        self.bound_keys = Some(keys.clone());
        Some(keys)
    }

    /// A new shell connection binds nothing yet
    pub fn forget_bound_keys(&mut self) {
        self.bound_keys = None;
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

/// Whether the key widget should wait for the desktop's insert for this action. Only
/// a widget can run the line, so it waits for the actions that always insert and can
/// run it. The common-prefix actions insert nothing when there is no common prefix
/// (the popup shakes, as in PTY mode); their inserts come through the push handler,
/// so a Tab with nothing to insert does not stall the shell.
pub fn action_awaits_insert(action: &str) -> bool {
    matches!(
        action,
        "insertSelected" | "insertSelectedAndExecute" | "insertCommonPrefixOrInsertSelected" | "execute"
    )
}

/// The name the shell reports a key with (see `_tab_link_init_keys` in no_pty.zsh), if
/// the shell can bind it. Keys a terminal sends no sequence for, like command+i or
/// control+1, have none.
fn shell_key_name(event: &KeyEvent) -> Option<String> {
    let name = match (event.key, event.modifiers) {
        (KeyCode::Char(letter), Modifiers::CTRL) if letter.is_ascii_alphabetic() => {
            return Some(format!("control+{}", letter.to_ascii_lowercase()));
        },
        (KeyCode::Char(letter), Modifiers::ALT) if letter.is_ascii_alphabetic() => {
            return Some(format!("option+{}", letter.to_ascii_lowercase()));
        },
        (KeyCode::Tab, Modifiers::SHIFT) => "shift+tab",
        (key, Modifiers::NONE) => match key {
            KeyCode::Tab => "tab",
            KeyCode::Enter => "enter",
            KeyCode::Escape => "esc",
            KeyCode::UpArrow | KeyCode::ApplicationUpArrow => "up",
            KeyCode::DownArrow | KeyCode::ApplicationDownArrow => "down",
            KeyCode::LeftArrow | KeyCode::ApplicationLeftArrow => "left",
            KeyCode::RightArrow | KeyCode::ApplicationRightArrow => "right",
            KeyCode::Home => "home",
            KeyCode::End => "end",
            KeyCode::PageUp => "pageup",
            KeyCode::PageDown => "pagedown",
            KeyCode::Backspace => "backspace",
            KeyCode::Delete => "delete",
            _ => return None,
        },
        _ => return None,
    };
    Some(name.into())
}

fn whoami_hostname() -> Option<String> {
    let username = std::env::var("USER").ok()?;
    HOSTNAME.as_deref().map(|hostname| format!("{username}@{hostname}"))
}

fn char_to_byte(text: &str, chars: usize) -> usize {
    text.char_indices().nth(chars).map_or(text.len(), |(i, _)| i)
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
    fn knows_which_actions_await_an_insert() {
        for action in [
            "insertSelected",
            "insertSelectedAndExecute",
            "insertCommonPrefixOrInsertSelected",
            "execute",
        ] {
            assert!(action_awaits_insert(action), "{action}");
        }
        for action in [
            "insertCommonPrefix",
            "insertCommonPrefixOrNavigateDown",
            "navigateDown",
            "navigateUp",
            "hideAutocomplete",
            "toggleDescription",
            "toggleHistoryMode",
        ] {
            assert!(!action_awaits_insert(action), "{action}");
        }
    }

    fn action(identifier: &str, bindings: &[&str]) -> Action {
        Action {
            identifier: identifier.into(),
            bindings: bindings.iter().map(|&binding| binding.into()).collect(),
        }
    }

    #[test]
    fn names_the_keys_the_shell_can_bind() {
        let mut s = session("", 0);
        s.interceptor.set_actions(
            &[
                action("insertSelected", &["enter"]),
                action("navigateUp", &["shift+tab", "up", "control+K", "ctrl+p"]),
                action("navigateDown", &["arrowdown", "option+j"]),
                action("selectSuggestion1", &["control+1"]),
                action("toggleDescription", &["command+i"]),
                action("ignore", &["control+n"]),
            ],
            true,
        );
        let keys: Vec<String> = s.shell_keys().into_iter().collect();
        assert_eq!(keys, [
            "control+k",
            "control+p",
            "down",
            "enter",
            "option+j",
            "shift+tab",
            "up"
        ]);
    }

    #[test]
    fn binds_esc_only_while_the_popup_takes_it() {
        let mut s = session("git ch", 6);
        s.interceptor.set_actions(
            &[
                action("hideAutocomplete", &["esc"]),
                action("insertSelected", &["enter"]),
            ],
            true,
        );
        assert!(!s.shell_keys().contains("esc"));
        assert!(s.take_bind_update().is_some());
        assert!(s.take_bind_update().is_none(), "unchanged");

        s.interceptor.set_intercept(true);
        s.interceptor.set_window_visible(true);
        assert!(s.take_bind_update().unwrap().contains("esc"));

        // Esc hides the popup and releases its keys
        assert_eq!(s.intercept("esc").as_deref(), Some("hideAutocomplete"));
        let keys = s.take_bind_update().unwrap();
        assert!(!keys.contains("esc"));
        assert!(keys.contains("enter"), "still bound, to ask when pressed");

        s.forget_bound_keys();
        assert_eq!(s.take_bind_update(), Some(keys), "a new connection gets the keys again");
    }

    #[test]
    fn releases_esc_while_a_command_runs() {
        let mut s = session("ls", 2);
        s.interceptor.set_actions(
            &[
                action("hideAutocomplete", &["esc"]),
                action("insertSelected", &["enter"]),
            ],
            true,
        );
        s.start_command("ls".into());
        s.interceptor.set_intercept(true);
        s.interceptor.set_window_visible(true);
        let keys = s.shell_keys();
        assert!(!keys.contains("esc"));
        assert!(keys.contains("enter"));
    }

    #[test]
    fn names_each_unmodified_key() {
        for (key, name) in [
            (KeyCode::Tab, Some("tab")),
            (KeyCode::Enter, Some("enter")),
            (KeyCode::Escape, Some("esc")),
            (KeyCode::UpArrow, Some("up")),
            (KeyCode::ApplicationUpArrow, Some("up")),
            (KeyCode::DownArrow, Some("down")),
            (KeyCode::ApplicationDownArrow, Some("down")),
            (KeyCode::LeftArrow, Some("left")),
            (KeyCode::ApplicationLeftArrow, Some("left")),
            (KeyCode::RightArrow, Some("right")),
            (KeyCode::ApplicationRightArrow, Some("right")),
            (KeyCode::Home, Some("home")),
            (KeyCode::End, Some("end")),
            (KeyCode::PageUp, Some("pageup")),
            (KeyCode::PageDown, Some("pagedown")),
            (KeyCode::Backspace, Some("backspace")),
            (KeyCode::Delete, Some("delete")),
            (KeyCode::Char('a'), None),
        ] {
            let event = KeyEvent {
                key,
                modifiers: Modifiers::NONE,
            };
            assert_eq!(shell_key_name(&event).as_deref(), name, "{key:?}");
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
