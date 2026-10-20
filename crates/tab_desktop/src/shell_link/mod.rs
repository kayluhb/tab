//! Shells connect here directly, with nothing between the terminal and the shell.
//!
//! Each connection is one terminal session. The shell reports its buffer, prompt
//! and command events over the line protocol in [`protocol`]; the desktop pushes
//! back which keys to bind, which of them the popup takes now, new buffers, and
//! ghost text. The session is
//! registered in [`FigtermState`] like a tabterm session, so the popup and the
//! rest of the app work unchanged. Closing the connection ends the session.
//!
//! Only processes running as the same user may connect.

mod edit;
mod keys;
mod process;
mod protocol;

use std::collections::HashMap;
use std::path::{
    Path,
    PathBuf,
};
use std::sync::atomic::AtomicU64;
use std::sync::{
    Arc,
    LazyLock,
};
use std::time::SystemTime;

use anyhow::{
    Context as _,
    Result,
    bail,
};
use edit::{
    Insert,
    Line,
};
use keys::KeyState;
use process::ProcessRequest;
use protocol::{
    DesktopMessage,
    MAX_LINE_BYTES,
    ShellMessage,
};
use tab_os_shim::{
    Context,
    Env,
};
use tab_proto::fig::EnvironmentVariable;
use tab_proto::local::{
    EditBufferHook,
    InterceptedKeyHook,
    PostExecHook,
    PreExecHook,
    PromptHook,
    ShellContext,
};
use tab_remote_ipc::RemoteHookHandler;
use tab_remote_ipc::figterm::{
    EditBuffer,
    FigtermCommand,
    FigtermSession,
    FigtermState,
    InterceptMode,
};
use tab_settings::history::{
    CommandInfo,
    History,
};
use tab_util::terminal::Terminal;
use tokio::io::{
    AsyncBufRead,
    AsyncBufReadExt,
    AsyncReadExt,
    AsyncWriteExt,
    BufReader,
};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{
    debug,
    error,
    info,
    warn,
};
use uuid::Uuid;

use crate::remote_ipc::RemoteHook;

static HISTORY: LazyLock<History> = LazyLock::new(History::new);
static HOSTNAME: LazyLock<Option<String>> = LazyLock::new(sysinfo::System::host_name);

pub async fn start(socket_path: PathBuf, figterm_state: Arc<FigtermState>, hook: RemoteHook) -> Result<()> {
    // socket_bind makes the folder 0700, so only this user can reach the socket
    let listener = tab_ipc::socket_bind(&socket_path)
        .await
        .context("Failed binding shell link socket")?;
    info!(path = %socket_path.display(), "Listening for shells");

    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(serve(stream, figterm_state.clone(), hook.clone()));
            },
            Err(err) => error!(%err, "Failed accepting shell connection"),
        }
    }
}

/// Reads one line, failing if it is longer than [`MAX_LINE_BYTES`]. Returns
/// `None` at the end of the stream.
async fn read_line(reader: &mut (impl AsyncBufRead + Unpin), buf: &mut Vec<u8>) -> Result<Option<String>> {
    buf.clear();
    let read = reader.take(MAX_LINE_BYTES as u64 + 1).read_until(b'\n', buf).await?;
    if read == 0 {
        return Ok(None);
    }
    if buf.len() > MAX_LINE_BYTES {
        bail!("line longer than {MAX_LINE_BYTES} bytes");
    }
    Ok(Some(String::from_utf8_lossy(buf).into_owned()))
}

async fn write_lines(mut writer: OwnedWriteHalf, mut lines: mpsc::UnboundedReceiver<String>) {
    while let Some(line) = lines.recv().await {
        if writer.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn serve(stream: UnixStream, figterm_state: Arc<FigtermState>, hook: RemoteHook) {
    let pid = match stream.peer_cred() {
        Ok(cred) if cred.uid() == nix::unistd::getuid().as_raw() => cred.pid(),
        Ok(cred) => {
            warn!(uid = cred.uid(), "Refused shell connection from another user");
            return;
        },
        Err(err) => {
            warn!(%err, "Refused shell connection without peer credentials");
            return;
        },
    };

    let (reader, writer) = stream.into_split();
    let (out_tx, out_rx) = mpsc::unbounded_channel();
    let writer_task = tokio::spawn(write_lines(writer, out_rx));

    let (command_tx, command_rx) = flume::unbounded();
    let (on_close_tx, _) = tokio::sync::broadcast::channel(1);
    let mut link = Link::new(pid, out_tx, hook, figterm_state.clone());
    figterm_state.insert(FigtermSession {
        id: link.session_id,
        sender: command_tx,
        writer: None,
        dead_since: None,
        last_receive: Instant::now(),
        edit_buffer: EditBuffer::default(),
        context: None,
        terminal_cursor_coordinates: None,
        current_session_metrics: None,
        response_map: HashMap::new(),
        nonce_counter: Arc::new(AtomicU64::new(0)),
        on_close_tx,
        intercept: InterceptMode::Unlocked,
        intercept_global: InterceptMode::Unlocked,
    });
    debug!(session_id = %link.session_id, ?pid, "Shell connected");

    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        tokio::select! {
            line = read_line(&mut reader, &mut buf) => match line {
                Ok(Some(line)) => match ShellMessage::parse(&line) {
                    Ok(message) => link.on_shell_message(message).await,
                    // Never log the line itself: it can hold what the user typed
                    Err(err) => warn!(%err, "Bad message from shell"),
                },
                Ok(None) => break,
                Err(err) => {
                    warn!(%err, "Dropping shell connection");
                    break;
                },
            },
            command = command_rx.recv_async() => match command {
                Ok(command) => link.on_command(command),
                Err(_) => break,
            },
        }
    }

    figterm_state.remove_id(&link.session_id);
    let session_id = link.session_id;
    drop(link);
    writer_task.await.ok();
    debug!(%session_id, "Shell disconnected");
}

/// One shell's session
struct Link {
    session_id: Uuid,
    pid: Option<i32>,
    out: mpsc::UnboundedSender<String>,
    hook: RemoteHook,
    figterm_state: Arc<FigtermState>,

    shell_path: Option<String>,
    tty: Option<String>,
    env: Vec<(String, String)>,
    terminal: Option<String>,
    alias: Option<String>,
    cwd: Option<PathBuf>,

    line: Line,
    preexec: bool,
    running: Option<(String, SystemTime)>,
    keys: KeyState,
    sent_bind: Vec<String>,
    sent_keys: Vec<(String, String)>,
}

impl Link {
    fn new(
        pid: Option<i32>,
        out: mpsc::UnboundedSender<String>,
        hook: RemoteHook,
        figterm_state: Arc<FigtermState>,
    ) -> Self {
        Self {
            session_id: Uuid::new_v4(),
            pid,
            out,
            hook,
            figterm_state,
            shell_path: None,
            tty: None,
            env: vec![],
            terminal: None,
            alias: None,
            cwd: None,
            line: Line::default(),
            preexec: false,
            running: None,
            keys: KeyState::new(tab_settings::settings::get_bool_or("autocomplete.onlyShowOnTab", false)),
            sent_bind: vec![],
            sent_keys: vec![],
        }
    }

    fn send(&self, message: DesktopMessage) {
        self.out.send(message.to_line()).ok();
    }

    /// Tells the shell which keys to bind and which to hand over, if that changed
    fn push_keys(&mut self) {
        let bindable = self.keys.bindable();
        if bindable != self.sent_bind {
            self.sent_bind.clone_from(&bindable);
            self.send(DesktopMessage::Bind(bindable));
        }
        let active = self.keys.active();
        if active != self.sent_keys {
            self.sent_keys.clone_from(&active);
            self.send(DesktopMessage::Keys(active));
        }
    }

    fn shell_name(&self) -> Option<String> {
        let path = self.shell_path.as_deref()?;
        Path::new(path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
    }

    fn username(&self) -> Option<&str> {
        self.env
            .iter()
            .find_map(|(key, value)| (key == "USER").then_some(value.as_str()))
    }

    fn hostname(&self) -> Option<String> {
        let username = self.username()?;
        HOSTNAME.as_deref().map(|hostname| format!("{username}@{hostname}"))
    }

    fn context(&self) -> ShellContext {
        ShellContext {
            pid: self.pid,
            ttys: self.tty.clone(),
            process_name: self.shell_name(),
            current_working_directory: self.cwd.as_ref().map(|cwd| cwd.display().to_string()),
            session_id: Some(self.session_id.to_string()),
            terminal: self.terminal.clone(),
            hostname: self.hostname(),
            shell_path: self.shell_path.clone(),
            wsl_distro: None,
            environment_variables: self
                .env
                .iter()
                .map(|(key, value)| EnvironmentVariable {
                    key: key.clone(),
                    value: Some(value.clone()),
                })
                .collect(),
            qterm_version: Some(env!("CARGO_PKG_VERSION").into()),
            preexec: Some(self.preexec),
            osc_lock: Some(false),
            alias: self.alias.clone(),
        }
    }

    async fn on_shell_message(&mut self, message: ShellMessage) {
        let result = match message {
            ShellMessage::Hello { shell_path, tty } => {
                self.shell_path = shell_path;
                self.tty = tty;
                self.push_keys();
                Ok(None)
            },
            ShellMessage::Env(env) => {
                let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
                let ctx = Context::builder().with_env(Env::from_slice(&pairs)).build();
                self.terminal = Terminal::parent_terminal(&ctx).map(|terminal| terminal.to_string());
                self.env = env;
                Ok(None)
            },
            ShellMessage::Alias(alias) => {
                self.alias = Some(alias);
                Ok(None)
            },
            ShellMessage::Prompt { exit_code, cwd } => self.on_prompt(exit_code, cwd).await,
            ShellMessage::PreExec { command } => {
                self.preexec = true;
                self.keys.reset();
                self.push_keys();
                self.running = Some((command.clone(), SystemTime::now()));
                self.line = Line::new(self.line.seq, String::new(), 0);
                let hook = PreExecHook {
                    context: Some(self.context()),
                    command: Some(command),
                };
                self.hook.pre_exec(&hook, self.session_id, &self.figterm_state).await
            },
            ShellMessage::Buffer { seq, cursor, text } => {
                self.line = Line::new(seq, text, cursor);
                let hook = EditBufferHook {
                    context: Some(self.context()),
                    text: self.line.text.clone(),
                    cursor: self.line.cursor_bytes() as i64,
                    histno: 0,
                    terminal_cursor_coordinates: None,
                };
                self.hook.edit_buffer(&hook, self.session_id, &self.figterm_state).await
            },
            ShellMessage::Action { key, action } => {
                if key == "esc" {
                    self.keys.reset();
                    self.push_keys();
                }
                let hook = InterceptedKeyHook {
                    context: Some(self.context()),
                    key,
                    action,
                };
                self.hook.intercepted_key(hook, self.session_id).await
            },
        };

        if let Err(err) = result {
            error!(%err, "Failed handling shell message");
        }
    }

    async fn on_prompt(
        &mut self,
        exit_code: Option<i32>,
        cwd: String,
    ) -> Result<Option<tab_proto::remote::clientbound::response::Response>> {
        self.preexec = false;
        if !cwd.is_empty() {
            self.cwd = Some(PathBuf::from(cwd));
        }

        if let Some((command, start_time)) = self.running.take() {
            let post_exec = PostExecHook {
                context: Some(self.context()),
                command: Some(command.clone()),
                exit_code,
            };
            let info = CommandInfo {
                command: Some(command),
                shell: self.shell_name(),
                pid: self.pid,
                session_id: Some(self.session_id.to_string()),
                cwd: self.cwd.as_ref().map(|cwd| cwd.display().to_string()),
                start_time: Some(start_time),
                end_time: Some(SystemTime::now()),
                hostname: self.hostname(),
                exit_code,
            };
            tokio::task::spawn_blocking(move || {
                if let Err(err) = HISTORY.insert_command_history(&info, true) {
                    error!(%err, "Failed to insert command into history");
                }
            });
            self.hook
                .post_exec(&post_exec, self.session_id, &self.figterm_state)
                .await?;
        }

        let hook = PromptHook {
            context: Some(self.context()),
        };
        self.hook.prompt(&hook, self.session_id, &self.figterm_state).await
    }

    fn on_command(&mut self, command: FigtermCommand) {
        match command {
            FigtermCommand::InterceptFigJs {
                intercept_keystrokes,
                intercept_global_keystrokes,
                actions,
                override_actions,
            } => {
                self.keys.set_intercepts(
                    intercept_keystrokes,
                    intercept_global_keystrokes,
                    &actions,
                    override_actions,
                );
                self.push_keys();
            },
            FigtermCommand::InterceptFigJSVisible { visible } => {
                self.keys.set_visible(visible);
                self.push_keys();
            },
            FigtermCommand::InsertText {
                insertion,
                deletion,
                offset,
                immediate,
                insertion_buffer,
                insert_during_command,
            } => {
                if self.preexec && insert_during_command != Some(true) {
                    return;
                }
                let (line, execute) = self.line.apply(&Insert {
                    insertion,
                    deletion,
                    offset,
                    immediate,
                    insertion_buffer,
                });
                self.send(DesktopMessage::SetBuffer {
                    seq: Some(line.seq),
                    cursor: line.cursor,
                    text: line.text.clone(),
                    execute,
                });
                // The shell moves to the next seq when it applies this, so a second
                // insert before its report still lines up
                self.line = Line::new(line.seq + 1, line.text, line.cursor);
            },
            FigtermCommand::SetBuffer { text, cursor_position } => {
                let cursor = cursor_position.map_or_else(|| text.chars().count(), |cursor| cursor as usize);
                self.send(DesktopMessage::SetBuffer {
                    seq: None,
                    cursor,
                    text,
                    execute: false,
                });
            },
            FigtermCommand::SetInlineSuggestion { buffer, suffix } => {
                self.send(DesktopMessage::Ghost { buffer, suffix });
            },
            FigtermCommand::RunProcess {
                channel,
                executable,
                arguments,
                working_directory,
                env,
                timeout,
            } => {
                let request = ProcessRequest {
                    executable,
                    arguments,
                    working_directory,
                    env,
                    timeout,
                };
                let (cwd, shell_env) = (self.cwd.clone(), self.env.clone());
                tokio::spawn(async move {
                    channel.send(process::run_process(request, cwd, shell_env).await).ok();
                });
            },
        }
    }
}
