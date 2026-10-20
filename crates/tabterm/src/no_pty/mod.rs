//! No-PTY mode: the shell reports its own state over a socket, so nothing sits
//! between the terminal and the shell, and the terminal sees the real command.
//!
//! The shell runs `tabterm --no-pty --shell-pid $$`, which prints a session id
//! and the shell link socket's path, one per line, and forks into the background. The background
//! process registers with the desktop like a PTY-mode session, serves the usual tabterm socket (for
//! `pre-cmd` and inline suggestions), and serves the shell link socket
//! ([`protocol`]). It exits when the shell does.

mod protocol;
mod session;

use std::io::Write as _;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::path::{
    Path,
    PathBuf,
};
use std::time::Duration;

use anyhow::{
    Context as _,
    Result,
    bail,
};
use flume::Sender;
use protocol::{
    LinkMessage,
    ShellMessage,
};
use session::{
    Session,
    action_awaits_insert,
};
use tab_log::{
    LogArgs,
    initialize_logging,
};
use tab_proto::figterm::figterm_request_message::Request as FigtermRequest;
use tab_proto::figterm::figterm_response_message::Response as FigtermResponse;
use tab_proto::figterm::intercept_request::{
    InterceptCommand,
    SetFigjsIntercepts,
    SetFigjsVisible,
};
use tab_proto::figterm::{
    self,
    FigtermRequestMessage,
    FigtermResponseMessage,
};
use tab_proto::local::EnvironmentVariable;
use tab_proto::remote::{
    Clientbound,
    Hostbound,
    clientbound,
    hostbound,
};
use tab_proto::remote_hooks::{
    hook_to_message,
    new_edit_buffer_hook,
    new_intercepted_key_hook,
    new_postexec_hook,
    new_preexec_hook,
    new_prompt_hook,
};
use tab_util::directories;
use tokio::io::{
    AsyncBufRead,
    AsyncBufReadExt,
    AsyncReadExt as _,
    AsyncWriteExt,
    BufReader,
};
use tokio::net::{
    UnixListener,
    UnixStream,
};
use tokio::sync::{
    mpsc,
    oneshot,
};
use tracing::{
    debug,
    error,
    info,
    warn,
};

use crate::history::{
    self,
    HistoryCommand,
};
use crate::inline_suggestion::INLINE_SUGGESTION;
use crate::interceptor::KeyInterceptor;
use crate::ipc::{
    spawn_figterm_ipc,
    spawn_remote_ipc,
};
use crate::message::{
    inline_shell_completion_response,
    spawn_run_process,
};
use crate::{
    INSERT_ON_NEW_CMD,
    MainLoopEvent,
    SHELL_ALIAS,
    SHELL_ENVIRONMENT_VARIABLES,
    autocomplete_enabled,
};

/// What a shell connection hands the main loop
enum ShellEvent {
    Message(ShellMessage, Option<oneshot::Sender<LinkMessage>>),
    PushConnected(mpsc::UnboundedSender<LinkMessage>),
}

/// Prints the session id, forks, and runs the session in the background child
pub fn start(shell_pid: i32) -> Result<()> {
    let session_id = uuid::Uuid::new_v4().simple().to_string();

    // Bind before forking, so the socket exists by the time the shell connects
    let socket_path = directories::shell_link_socket_path(&session_id)?;
    let listener = bind_std(&socket_path)?;

    // SAFETY: no other threads exist yet; the child only continues single-threaded
    match unsafe { nix::unistd::fork() }.context("fork")? {
        nix::unistd::ForkResult::Parent { .. } => {
            let mut stdout = std::io::stdout();
            writeln!(stdout, "{session_id}\n{}", socket_path.display())?;
            stdout.flush()?;
            Ok(())
        },
        nix::unistd::ForkResult::Child => {
            detach();
            let result = run(session_id, shell_pid, listener, &socket_path);
            std::fs::remove_file(&socket_path).ok();
            if let Err(err) = &result {
                error!(%err, "no-pty session failed");
            }
            // The child must never return into the parent's code
            #[allow(clippy::exit)]
            std::process::exit(i32::from(result.is_err()));
        },
    }
}

fn bind_std(path: &Path) -> Result<StdUnixListener> {
    use std::os::unix::fs::PermissionsExt;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::remove_file(path).ok();
    let listener = StdUnixListener::bind(path).with_context(|| format!("binding {}", path.display()))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

/// Leaves the shell's session and terminal, so the shell's `$(...)` returns and
/// the terminal never counts this process as a job
fn detach() {
    use std::os::fd::AsRawFd;

    nix::unistd::setsid().ok();
    if let Ok(null) = std::fs::OpenOptions::new().read(true).write(true).open("/dev/null") {
        for fd in 0..=2 {
            nix::unistd::dup2(null.as_raw_fd(), fd).ok();
        }
    }
}

fn shell_is_alive(pid: i32) -> bool {
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn run(session_id: String, shell_pid: i32, listener: StdUnixListener, socket_path: &Path) -> Result<()> {
    tab_settings::settings::init_global().ok();

    let _log_guard = initialize_logging(LogArgs {
        log_level: None,
        log_to_stdout: false,
        log_file_path: Some(directories::logs_dir()?.join(format!("tabterm-no-pty-{session_id}.log"))),
        delete_old_log_file: true,
    })
    .ok();
    info!(%session_id, shell_pid, socket = %socket_path.display(), "Starting no-pty session");

    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(main_loop(session_id, shell_pid, listener))
}

async fn main_loop(session_id: String, shell_pid: i32, listener: StdUnixListener) -> Result<()> {
    let (main_loop_tx, main_loop_rx) = flume::bounded::<MainLoopEvent>(16);
    let history_sender = history::spawn_history_task().await;
    let figterm_rx = spawn_figterm_ipc(&session_id).await?;
    let parent_id = tab_os_shim::Env::new().q_parent().ok();
    let (remote_tx, remote_rx, _stop_ipc_tx) = spawn_remote_ipc(session_id.clone(), parent_id, main_loop_tx).await?;

    let (shell_tx, mut shell_rx) = mpsc::unbounded_channel();
    tokio::spawn(accept_shells(UnixListener::from_std(listener)?, shell_tx));

    let mut interceptor = KeyInterceptor::new();
    interceptor.load_key_intercepts()?;
    let mut session = Session::new(session_id, shell_pid, interceptor);
    let mut push_tx: Option<mpsc::UnboundedSender<LinkMessage>> = None;
    let mut first_prompt = true;
    let autocomplete = autocomplete_enabled(&tab_os_shim::Env::new());

    let mut liveness = tokio::time::interval(Duration::from_secs(1));

    loop {
        tokio::select! {
            _ = liveness.tick() => {
                if !shell_is_alive(shell_pid) {
                    info!("Shell exited");
                    return Ok(());
                }
            },
            event = shell_rx.recv() => {
                let Some(event) = event else { return Ok(()) };
                match event {
                    ShellEvent::PushConnected(tx) => {
                        session.forget_bound_keys();
                        push_tx = Some(tx);
                    },
                    ShellEvent::Message(message, reply) => {
                        let link = handle_shell_message(&mut session, message, &remote_tx, &history_sender, autocomplete, &mut first_prompt);
                        if let (Some(reply), Some(link)) = (reply, link.reply) {
                            reply.send(link).ok();
                        }
                        if let (Some(push), Some(tx)) = (link.push, &push_tx) {
                            tx.send(push).ok();
                        }
                    },
                }
            },
            message = remote_rx.recv_async() => {
                let Ok(message) = message else { continue };
                if let Some(push) = handle_remote_message(&mut session, message, &remote_tx) {
                    if let Some(tx) = &push_tx {
                        tx.send(push).ok();
                    }
                }
            },
            request = figterm_rx.recv_async() => {
                let Ok((message, response_tx)) = request else { continue };
                if let Some(push) = handle_figterm_message(&mut session, message, response_tx) {
                    if let Some(tx) = &push_tx {
                        tx.send(push).ok();
                    }
                }
            },
            event = main_loop_rx.recv_async() => {
                // The remote connection broke: drop any interception so keys reach the shell
                if let Ok(MainLoopEvent::Insert { unlock: true, .. } | MainLoopEvent::UnlockInterception) = event {
                    session.interceptor.reset();
                }
            },
        }

        // Tell the shell which keys to bind whenever that changes: after the popup's
        // keybindings load, and as Esc comes and goes with the popup
        if let Some(tx) = &push_tx
            && let Some(keys) = session.take_bind_update()
        {
            tx.send(LinkMessage::Bind {
                keys: keys.into_iter().collect(),
            })
            .ok();
        }
    }
}

#[derive(Default)]
struct LinkOutput {
    reply: Option<LinkMessage>,
    push: Option<LinkMessage>,
}

fn handle_shell_message(
    session: &mut Session,
    message: ShellMessage,
    remote_tx: &Sender<Hostbound>,
    history_sender: &history::HistorySender,
    autocomplete: bool,
    first_prompt: &mut bool,
) -> LinkOutput {
    let send = |request| {
        if let Err(err) = remote_tx.send(hook_to_message(request)) {
            error!(%err, "Failed sending hook");
        }
    };

    match message {
        ShellMessage::Hello { pid, tty, shell_path } => {
            session.pid = pid;
            session.tty = tty;
            session.shell_path = shell_path;
        },
        ShellMessage::Push => {},
        // Kept where the PTY mode's UpdateShellContext keeps them
        ShellMessage::Env(pairs) => {
            *SHELL_ENVIRONMENT_VARIABLES.lock().unwrap() = pairs
                .into_iter()
                .map(|(key, value)| EnvironmentVariable {
                    key,
                    value: Some(value),
                })
                .collect();
        },
        ShellMessage::Alias(alias) => *SHELL_ALIAS.lock().unwrap() = Some(alias),
        ShellMessage::Prompt { exit_code, cwd } => {
            let cwd = PathBuf::from(cwd);
            if cwd.is_dir() {
                std::env::set_current_dir(&cwd).ok();
            }
            if let Some(finished) = session.prompt(exit_code, cwd) {
                send(new_postexec_hook(
                    session.context(),
                    finished.command.clone(),
                    finished.exit_code,
                ));
                if let Err(err) = history_sender.send(HistoryCommand::Insert(finished)) {
                    error!(%err, "Failed sending history");
                }
            }
            send(new_prompt_hook(session.context()));
            // PTY mode reads the fresh prompt off the screen; here an empty buffer hides the popup
            if autocomplete {
                send(new_edit_buffer_hook(session.context(), String::new(), 0, 0, None));
            }

            // Text queued for the next prompt, then the text the shell was started with
            let mut queued = INSERT_ON_NEW_CMD
                .lock()
                .unwrap()
                .take()
                .map(|(text, _, execute)| (text, execute));
            if std::mem::take(first_prompt) {
                queued = queued.or_else(|| {
                    std::env::var(tab_util::env_var::TAB_START_TEXT)
                        .ok()
                        .filter(|text| !text.is_empty())
                        .map(|text| (text, true))
                });
            }
            let reply = match queued {
                Some((text, execute)) => {
                    let cursor = text.chars().count();
                    session.set_buffer(text.clone(), cursor);
                    LinkMessage::SetBuffer {
                        seq: None,
                        cursor,
                        text,
                        execute,
                    }
                },
                None => LinkMessage::Nothing,
            };
            return LinkOutput {
                reply: Some(reply),
                ..Default::default()
            };
        },
        ShellMessage::PreExec { command } => {
            session.start_command(command);
            send(new_preexec_hook(session.context()));
        },
        ShellMessage::Buffer { seq, cursor, text } => {
            session.seq = seq;
            session.set_buffer(text, cursor);
            if autocomplete && !session.preexec {
                send(new_edit_buffer_hook(
                    session.context(),
                    session.buffer.clone(),
                    session.cursor_bytes() as i64,
                    0,
                    None,
                ));
            }
        },
        ShellMessage::Focus { focused } => {
            // Switching tabs or windows hides the popup; the next key shows it again
            if !focused {
                session.interceptor.reset();
                if autocomplete {
                    send(new_edit_buffer_hook(session.context(), String::new(), 0, 0, None));
                }
            }
        },
        ShellMessage::Key { name } => {
            let action = session.intercept(&name);
            if let Some(action) = &action {
                debug!(%name, %action, "Intercepted key");
                send(new_intercepted_key_hook(session.context(), action.clone(), name));
            }
            return LinkOutput {
                reply: Some(LinkMessage::Key {
                    taken: action.is_some(),
                    expect_buffer: action.as_deref().is_some_and(action_awaits_insert),
                }),
                ..Default::default()
            };
        },
    }
    LinkOutput::default()
}

/// Handles a request both sockets can carry. Returns a buffer or ghost text to push to the shell.
fn handle_request(session: &mut Session, request: FigtermRequest) -> (Option<LinkMessage>, Option<FigtermResponse>) {
    match request {
        FigtermRequest::InsertText(request) => {
            // A PTY types `insert_during_command` text into the running program. Without one, the
            // shell would apply it at the next prompt, so drop every insert while a command runs.
            if session.preexec {
                return (None, None);
            }
            let execute = session.apply_insert(&request);
            // Tagged with the buffer the insert was computed from, so the shell drops it
            // if the user typed since. The shell reports the result at the next seq.
            let push = LinkMessage::SetBuffer {
                seq: Some(session.seq),
                cursor: session.cursor,
                text: session.buffer.clone(),
                execute,
            };
            (Some(push), None)
        },
        FigtermRequest::SetBuffer(request) => {
            let cursor = request
                .cursor_position
                .map_or(request.text.chars().count(), |cursor| cursor as usize);
            session.set_buffer(request.text, cursor);
            // Replaces the whole line, so it applies whatever the shell holds
            let push = LinkMessage::SetBuffer {
                seq: None,
                cursor: session.cursor,
                text: session.buffer.clone(),
                execute: false,
            };
            (Some(push), None)
        },
        FigtermRequest::Intercept(request) => {
            match request.intercept_command {
                Some(InterceptCommand::SetFigjsIntercepts(SetFigjsIntercepts {
                    intercept_bound_keystrokes,
                    intercept_global_keystrokes,
                    actions,
                    override_actions,
                })) => {
                    session.interceptor.set_intercept_global(intercept_global_keystrokes);
                    session.interceptor.set_intercept(intercept_bound_keystrokes);
                    session.interceptor.set_actions(&actions, override_actions);
                },
                Some(InterceptCommand::SetFigjsVisible(SetFigjsVisible { visible })) => {
                    session.interceptor.set_window_visible(visible);
                },
                None => {},
            }
            (None, None)
        },
        FigtermRequest::SetInlineSuggestion(figterm::SetInlineSuggestionRequest { buffer, suffix }) => {
            // Pushed so the shell's ghost text needs no process per keystroke. The cache
            // still serves `inline-suggestion` callers on the tabterm socket.
            INLINE_SUGGESTION.set(buffer.clone(), suffix.clone());
            let push = LinkMessage::Ghost { buffer, suffix };
            (Some(push), None)
        },
        FigtermRequest::InsertOnNewCmd(command) => {
            *INSERT_ON_NEW_CMD.lock().unwrap() = Some((command.text, command.bracketed, command.execute));
            (None, None)
        },
        FigtermRequest::UpdateShellContext(request) => {
            if request.update_environment_variables {
                *SHELL_ENVIRONMENT_VARIABLES.lock().unwrap() = request.environment_variables;
            }
            if request.update_alias {
                *SHELL_ALIAS.lock().unwrap() = request.alias;
            }
            (None, None)
        },
        FigtermRequest::Diagnostics(_) => {
            let response = FigtermResponse::Diagnostics(figterm::DiagnosticsResponse {
                shell_context: Some(session.context()),
                edit_buffer: Some(session.buffer.clone()),
                cursor_position: session.cursor_bytes().try_into().ok(),
                ..Default::default()
            });
            (None, Some(response))
        },
        other => {
            debug!(?other, "Ignoring request not supported in no-pty mode");
            (None, None)
        },
    }
}

fn handle_remote_message(
    session: &mut Session,
    message: Clientbound,
    remote_tx: &Sender<Hostbound>,
) -> Option<LinkMessage> {
    use clientbound::request::Request;

    match message.packet {
        Some(clientbound::Packet::Ping(())) => {
            remote_tx
                .send(Hostbound {
                    packet: Some(hostbound::Packet::Pong(())),
                })
                .ok();
            None
        },
        Some(clientbound::Packet::Request(clientbound::Request { request, nonce })) => {
            let request = match request {
                Some(Request::RunProcess(request)) => {
                    spawn_run_process(request, session.cwd.clone(), nonce, remote_tx.clone());
                    return None;
                },
                Some(Request::InsertText(r)) => FigtermRequest::InsertText(r),
                Some(Request::SetBuffer(r)) => FigtermRequest::SetBuffer(r),
                Some(Request::Intercept(r)) => FigtermRequest::Intercept(r),
                Some(Request::SetInlineSuggestion(r)) => FigtermRequest::SetInlineSuggestion(r),
                Some(Request::InsertOnNewCmd(r)) => FigtermRequest::InsertOnNewCmd(r),
                Some(Request::Diagnostics(r)) => FigtermRequest::Diagnostics(r),
                other => {
                    warn!(?other, "unhandled request");
                    return None;
                },
            };
            let (push, response) = handle_request(session, request);
            if let Some(FigtermResponse::Diagnostics(diagnostics)) = response {
                remote_tx
                    .send(Hostbound {
                        packet: Some(hostbound::Packet::Response(hostbound::Response {
                            nonce,
                            response: Some(hostbound::response::Response::Diagnostics(diagnostics)),
                        })),
                    })
                    .ok();
            }
            push
        },
        packet => {
            warn!(?packet, "unhandled packet");
            None
        },
    }
}

fn handle_figterm_message(
    session: &mut Session,
    message: FigtermRequestMessage,
    response_tx: Sender<FigtermResponseMessage>,
) -> Option<LinkMessage> {
    match message.request? {
        // Answered from the popup's cached suggestion, without blocking the loop
        FigtermRequest::InlineShellCompletion(request) => {
            tokio::spawn(async move {
                let response = inline_shell_completion_response(&request.buffer).await;
                response_tx.send_async(response).await.ok();
            });
            None
        },
        request => {
            let (push, response) = handle_request(session, request);
            if let Some(response) = response {
                response_tx
                    .send(FigtermResponseMessage {
                        response: Some(response),
                    })
                    .ok();
            }
            push
        },
    }
}

async fn accept_shells(listener: UnixListener, shell_tx: mpsc::UnboundedSender<ShellEvent>) {
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                tokio::spawn(serve_shell(stream, shell_tx.clone()));
            },
            Err(err) => {
                error!(%err, "Failed accepting shell connection");
                return;
            },
        }
    }
}

/// Longest line tabterm reads from the shell; a longer one drops the connection
const MAX_LINE_BYTES: usize = 1 << 20;

/// Reads one line without its newline, failing if it is longer than
/// [`MAX_LINE_BYTES`]. Returns `None` at the end of the stream.
async fn read_line(reader: &mut (impl AsyncBufRead + Unpin), buf: &mut Vec<u8>) -> Result<Option<String>> {
    buf.clear();
    let read = (&mut *reader)
        .take(MAX_LINE_BYTES as u64 + 1)
        .read_until(b'\n', buf)
        .await?;
    if read == 0 {
        return Ok(None);
    }
    if buf.last() == Some(&b'\n') {
        buf.pop();
    }
    if buf.len() > MAX_LINE_BYTES {
        bail!("line longer than {MAX_LINE_BYTES} bytes");
    }
    Ok(Some(String::from_utf8_lossy(buf).into_owned()))
}

/// The pid of the process on the other end, if it runs as this user. The socket's
/// folder is already private; this is defence in depth.
fn peer_pid(stream: &UnixStream) -> Result<Option<i32>> {
    let cred = stream.peer_cred()?;
    if cred.uid() != nix::unistd::getuid().as_raw() {
        bail!("connection from uid {}", cred.uid());
    }
    Ok(cred.pid())
}

/// Serves one shell connection. A connection that starts with `push` only
/// receives buffers and ghost text; any other connection sends messages and
/// gets answers to `key` and `prompt`.
async fn serve_shell(stream: UnixStream, shell_tx: mpsc::UnboundedSender<ShellEvent>) {
    let peer_pid = match peer_pid(&stream) {
        Ok(pid) => pid,
        Err(err) => {
            warn!(%err, "Refused shell connection");
            return;
        },
    };
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();

    let mut first = true;
    loop {
        let line = match read_line(&mut reader, &mut buf).await {
            Ok(Some(line)) => line,
            Ok(None) => return,
            Err(err) => {
                warn!(%err, "Dropping shell connection");
                return;
            },
        };
        let mut message = match ShellMessage::parse(&line) {
            Ok(message) => message,
            Err(err) => {
                // Not the line itself: it can hold the user's command line
                warn!(%err, "Bad message from shell");
                continue;
            },
        };

        if std::mem::take(&mut first) && message == ShellMessage::Push {
            let (push_tx, mut push_rx) = mpsc::unbounded_channel::<LinkMessage>();
            shell_tx.send(ShellEvent::PushConnected(push_tx)).ok();
            while let Some(push) = push_rx.recv().await {
                if writer.write_all(push.to_line().as_bytes()).await.is_err() {
                    break;
                }
            }
            return;
        }

        // Trust the kernel over the pid the shell reports
        if let (ShellMessage::Hello { pid, .. }, Some(peer_pid)) = (&mut message, peer_pid) {
            *pid = peer_pid;
        }

        let reply = if matches!(message, ShellMessage::Key { .. } | ShellMessage::Prompt { .. }) {
            let (reply_tx, reply_rx) = oneshot::channel();
            shell_tx.send(ShellEvent::Message(message, Some(reply_tx))).ok();
            reply_rx.await.ok()
        } else {
            shell_tx.send(ShellEvent::Message(message, None)).ok();
            None
        };

        if let Some(reply) = reply {
            if writer.write_all(reply.to_line().as_bytes()).await.is_err() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use tab_proto::figterm::{
        InsertTextRequest,
        SetBufferRequest,
        SetInlineSuggestionRequest,
    };

    use super::*;

    fn session(buffer: &str, seq: u64) -> Session {
        let mut session = Session::new("abc".into(), 1, KeyInterceptor::new());
        session.set_buffer(buffer.into(), buffer.chars().count());
        session.seq = seq;
        session
    }

    #[test]
    fn insert_is_tagged_with_the_buffer_it_was_computed_from() {
        let mut s = session("git ch", 5);
        let (push, response) = handle_request(
            &mut s,
            FigtermRequest::InsertText(InsertTextRequest {
                insertion: Some("eckout".into()),
                ..Default::default()
            }),
        );
        assert!(response.is_none());
        assert_eq!(
            push,
            Some(LinkMessage::SetBuffer {
                seq: Some(5),
                cursor: 12,
                text: "git checkout".into(),
                execute: false,
            })
        );
    }

    #[test]
    fn insert_during_a_command_is_dropped() {
        let mut s = session("", 5);
        s.preexec = true;
        let (push, response) = handle_request(
            &mut s,
            FigtermRequest::InsertText(InsertTextRequest {
                insertion: Some("Testing tabterm...\n".into()),
                immediate: Some(true),
                insert_during_command: Some(true),
                ..Default::default()
            }),
        );
        assert!(push.is_none(), "{push:?}");
        assert!(response.is_none());
        assert_eq!(s.buffer, "");
    }

    fn shell_message(session: &mut Session, message: ShellMessage) -> LinkOutput {
        let (remote_tx, _remote_rx) = flume::unbounded();
        let (history_tx, _history_rx) = flume::unbounded();
        handle_shell_message(session, message, &remote_tx, &history_tx, false, &mut false)
    }

    #[test]
    fn insert_is_tagged_with_the_seq_the_shell_reported() {
        let mut s = session("", 0);
        shell_message(&mut s, ShellMessage::Buffer {
            seq: 9,
            cursor: 2,
            text: "ls".into(),
        });
        assert_eq!(s.seq, 9);
        let (push, _) = handle_request(
            &mut s,
            FigtermRequest::InsertText(InsertTextRequest {
                insertion: Some(" -la".into()),
                ..Default::default()
            }),
        );
        assert!(
            matches!(push, Some(LinkMessage::SetBuffer { seq: Some(9), .. })),
            "{push:?}"
        );
    }

    #[test]
    fn keeps_the_env_and_aliases_the_shell_sends() {
        // The only test that writes these statics
        let mut s = session("", 0);
        shell_message(&mut s, ShellMessage::Env(vec![("A".into(), "1".into())]));
        assert_eq!(*SHELL_ENVIRONMENT_VARIABLES.lock().unwrap(), [EnvironmentVariable {
            key: "A".into(),
            value: Some("1".into()),
        }]);
        shell_message(&mut s, ShellMessage::Env(vec![]));
        assert!(SHELL_ENVIRONMENT_VARIABLES.lock().unwrap().is_empty());

        shell_message(&mut s, ShellMessage::Alias("ll='ls -l'".into()));
        assert_eq!(SHELL_ALIAS.lock().unwrap().as_deref(), Some("ll='ls -l'"));
    }

    #[tokio::test]
    async fn peer_pid_is_this_process_for_a_local_pair() {
        let (stream, _other) = UnixStream::pair().unwrap();
        assert_eq!(peer_pid(&stream).unwrap(), Some(std::process::id() as i32));
    }

    #[tokio::test]
    async fn hello_takes_the_pid_from_the_kernel() {
        let (mut client, server) = UnixStream::pair().unwrap();
        let (shell_tx, mut shell_rx) = mpsc::unbounded_channel();
        tokio::spawn(serve_shell(server, shell_tx));
        client.write_all(b"hello\t1\t\t\n").await.unwrap();
        match shell_rx.recv().await {
            Some(ShellEvent::Message(ShellMessage::Hello { pid, .. }, None)) => {
                assert_eq!(pid, std::process::id() as i32);
            },
            _ => panic!("expected hello"),
        }
    }

    #[test]
    fn set_buffer_applies_to_any_buffer() {
        let mut s = session("ls", 3);
        let (push, _) = handle_request(
            &mut s,
            FigtermRequest::SetBuffer(SetBufferRequest {
                text: "pwd".into(),
                cursor_position: None,
            }),
        );
        assert_eq!(
            push,
            Some(LinkMessage::SetBuffer {
                seq: None,
                cursor: 3,
                text: "pwd".into(),
                execute: false,
            })
        );
    }

    #[tokio::test]
    async fn reads_lines_up_to_the_limit() {
        let mut input = format!("buffer\t1\t2\tls\n{}\nlast", "a".repeat(MAX_LINE_BYTES)).into_bytes();
        let mut reader = BufReader::new(input.as_slice());
        let mut buf = Vec::new();
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap().as_deref(),
            Some("buffer\t1\t2\tls")
        );
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Some("a".repeat(MAX_LINE_BYTES))
        );
        assert_eq!(read_line(&mut reader, &mut buf).await.unwrap().as_deref(), Some("last"));
        assert_eq!(read_line(&mut reader, &mut buf).await.unwrap(), None);

        // A full-length last line needs no newline
        input = "a".repeat(MAX_LINE_BYTES).into_bytes();
        let mut reader = BufReader::new(input.as_slice());
        assert_eq!(
            read_line(&mut reader, &mut buf).await.unwrap(),
            Some("a".repeat(MAX_LINE_BYTES))
        );

        input = "a".repeat(MAX_LINE_BYTES + 1).into_bytes();
        let mut reader = BufReader::new(input.as_slice());
        assert!(read_line(&mut reader, &mut buf).await.is_err());
    }

    #[test]
    fn inline_suggestion_is_pushed_as_ghost_text() {
        let mut s = session("git ch", 1);
        let (push, response) = handle_request(
            &mut s,
            FigtermRequest::SetInlineSuggestion(SetInlineSuggestionRequest {
                buffer: "git ch".into(),
                suffix: "eckout".into(),
            }),
        );
        assert!(response.is_none());
        assert_eq!(
            push,
            Some(LinkMessage::Ghost {
                buffer: "git ch".into(),
                suffix: "eckout".into(),
            })
        );
    }
}
