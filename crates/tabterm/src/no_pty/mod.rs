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
};
use flume::Sender;
use protocol::{
    LinkMessage,
    ShellMessage,
};
use session::{
    Session,
    action_inserts,
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
    AsyncBufReadExt,
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
                    ShellEvent::PushConnected(tx) => push_tx = Some(tx),
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
                    LinkMessage::SetBuffer { cursor, text, execute }
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
        ShellMessage::Buffer { cursor, text } => {
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
                    expect_buffer: action.as_deref().is_some_and(action_inserts),
                }),
                ..Default::default()
            };
        },
    }
    LinkOutput::default()
}

/// Handles a request both sockets can carry. Returns a buffer to push to the shell.
fn handle_request(session: &mut Session, request: FigtermRequest) -> (Option<LinkMessage>, Option<FigtermResponse>) {
    match request {
        FigtermRequest::InsertText(request) => {
            if session.preexec && !request.insert_during_command() {
                return (None, None);
            }
            let execute = session.apply_insert(&request);
            let push = LinkMessage::SetBuffer {
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
            let push = LinkMessage::SetBuffer {
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
        FigtermRequest::SetInlineSuggestion(request) => {
            INLINE_SUGGESTION.set(request.buffer, request.suffix);
            (None, None)
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

/// Serves one shell connection. A connection that starts with `push` only
/// receives buffers; any other connection sends messages and gets answers to
/// `key` and `prompt`.
async fn serve_shell(stream: UnixStream, shell_tx: mpsc::UnboundedSender<ShellEvent>) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    let mut first = true;
    while let Ok(Some(line)) = lines.next_line().await {
        let message = match ShellMessage::parse(&line) {
            Ok(message) => message,
            Err(err) => {
                warn!(%err, %line, "Bad message from shell");
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
