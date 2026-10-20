#![allow(dead_code)]

mod checks;

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::Display;
use std::fs::read_to_string;
use std::future::Future;
use std::path::{
    Path,
    PathBuf,
};
use std::process::{
    Command,
    ExitCode,
};
use std::sync::Arc;
use std::time::Duration;

use anstream::{
    eprintln,
    println,
};
use async_trait::async_trait;
use checks::{
    BashVersionCheck,
    FishVersionCheck,
    SshdConfigCheck,
};
use clap::Args;
use crossterm::style::Stylize;
use crossterm::terminal::{
    Clear,
    ClearType,
    disable_raw_mode,
    enable_raw_mode,
};
use crossterm::{
    cursor,
    execute,
};
use eyre::{
    ContextCompat,
    Result,
    WrapErr,
};
use futures::FutureExt;
use futures::future::BoxFuture;
use owo_colors::OwoColorize;
use regex::Regex;
use semver::Version;
use spinners::{
    Spinner,
    Spinners,
};
use tab_integrations::input_method::InputMethodError;
use tab_integrations::shell::{
    ShellIntegration,
    all_shell_integrations,
};
use tab_integrations::ssh::SshIntegration;
use tab_integrations::{
    Error as InstallationError,
    Integration,
};
use tab_ipc::{
    BufferedUnixStream,
    SendMessage,
    SendRecvMessage,
};
use tab_os_shim::{
    Context,
    Env,
    Os,
};
use tab_proto::local::DiagnosticsResponse;
use tab_settings::JsonStore;
use tab_util::directories::{
    remote_socket_path,
    settings_path,
};
use tab_util::env_var::{
    PROCESS_LAUNCHED_BY_TAB,
    TAB_PARENT,
    TAB_TERM,
    TABTERM_LINK_SOCKET,
    TABTERM_MODE,
    TABTERM_SESSION_ID,
};
use tab_util::macos::BUNDLE_CONTENTS_INFO_PLIST_PATH;
use tab_util::system_info::SupportLevel;
use tab_util::terminal::in_special_terminal;
use tab_util::{
    APP_BUNDLE_NAME,
    CLI_BINARY_NAME,
    CLI_CRATE_NAME,
    GITHUB_REPO_NAME,
    PRODUCT_NAME,
    PTY_BINARY_NAME,
    Shell,
    Terminal,
    directories,
    system_paths,
};
use tokio::io::AsyncBufReadExt;

use super::app::restart_fig;
use super::diagnostics::verify_integration;
use super::init::{
    inline_source_setting,
    inline_strategy,
};
use crate::util::desktop::{
    LaunchArgs,
    desktop_app_running,
    launch_fig_desktop,
};
use crate::util::{
    app_path_from_bundle_id,
    glob,
    glob_dir,
    is_executable_in_path,
};

#[derive(Debug, Args, PartialEq, Eq)]
pub struct DoctorArgs {
    /// Run all doctor tests, with no fixes
    #[arg(long, short = 'a')]
    pub all: bool,
    /// Error on warnings
    #[arg(long, short = 's')]
    pub strict: bool,
}

impl DoctorArgs {
    pub async fn execute(self) -> Result<ExitCode> {
        doctor_cli(self.all, self.strict).await
    }
}

enum DoctorFix {
    Sync(Box<dyn FnOnce() -> Result<()> + Send>),
    Async(BoxFuture<'static, Result<()>>),
}

enum DoctorError {
    Warning(Cow<'static, str>),
    Error {
        reason: Cow<'static, str>,
        info: Vec<Cow<'static, str>>,
        fix: Option<DoctorFix>,
        error: Option<eyre::Report>,
    },
}

impl std::fmt::Debug for DoctorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        match self {
            DoctorError::Warning(msg) => f.debug_struct("Warning").field("msg", msg).finish(),
            DoctorError::Error { reason, info, .. } => f
                .debug_struct("Error")
                .field("reason", reason)
                .field("info", info)
                .finish(),
        }
    }
}

impl std::fmt::Display for DoctorError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DoctorError::Warning(warning) => write!(f, "Warning: {warning}"),
            DoctorError::Error { reason, .. } => write!(f, "Error: {reason}"),
        }
    }
}

impl From<eyre::Report> for DoctorError {
    fn from(err: eyre::Report) -> Self {
        DoctorError::Error {
            reason: err.to_string().into(),
            info: vec![],
            fix: None,
            error: Some(err),
        }
    }
}

impl From<tab_util::Error> for DoctorError {
    fn from(err: tab_util::Error) -> Self {
        DoctorError::Error {
            reason: err.to_string().into(),
            info: vec![],
            fix: None,
            error: Some(eyre::Report::from(err)),
        }
    }
}

impl DoctorError {
    fn warning(reason: impl Into<Cow<'static, str>>) -> DoctorError {
        DoctorError::Warning(reason.into())
    }

    fn error(reason: impl Into<Cow<'static, str>>) -> DoctorError {
        DoctorError::Error {
            reason: reason.into(),
            info: vec![],
            fix: None,
            error: None,
        }
    }
}

macro_rules! doctor_warning {
    ($($arg:tt)*) => {{
        DoctorError::warning(format!($($arg)*))
    }}
}
pub(crate) use doctor_warning;

macro_rules! doctor_error {
    ($($arg:tt)*) => {{
        DoctorError::error(format!($($arg)*))
    }}
}
pub(crate) use doctor_error;

#[allow(unused_macros)]
macro_rules! doctor_fix {
    ({ reason: $reason:expr,fix: $fix:expr }) => {
        DoctorError::Error {
            reason: format!($reason).into(),
            info: vec![],
            fix: Some(DoctorFix::Sync(Box::new($fix))),
            error: None,
        }
    };
}
pub(crate) use doctor_fix;

macro_rules! doctor_fix_async {
    ({ reason: $reason:expr,fix: $fix:expr }) => {
        DoctorError::Error {
            reason: $reason.into(),
            info: vec![],
            fix: Some(DoctorFix::Async(Box::pin($fix))),
            error: None,
        }
    };
}

fn check_file_exists(path: impl AsRef<Path>) -> Result<()> {
    if !path.as_ref().exists() {
        eyre::bail!("No file at path {}", path.as_ref().display())
    }
    Ok(())
}

async fn check_socket_perms(path: impl AsRef<Path>) -> Result<(), DoctorError> {
    let path = path.as_ref();
    tab_ipc::validate_socket(&path).await.map_err(|err| DoctorError::Error {
        reason: format!("Failed to validate socket permissions: {err}").into(),
        info: vec![format!("Socket path: {path:?}").into()],
        fix: None,
        error: Some(err.into()),
    })
}

fn command_fix<A, I, D>(args: A, sleep_duration: D) -> Option<DoctorFix>
where
    A: IntoIterator<Item = I> + Send,
    I: AsRef<OsStr> + Send + 'static,
    D: Into<Option<Duration>> + Send + 'static,
{
    let args = args.into_iter().collect::<Vec<_>>();

    Some(DoctorFix::Sync(Box::new(move || {
        if let (Some(exe), Some(remaining)) = (args.first(), args.get(1..)) {
            if Command::new(exe).args(remaining).status()?.success() {
                if let Some(duration) = sleep_duration.into() {
                    let spinner = Spinner::new(Spinners::Dots, "Waiting for command to finish...".into());
                    std::thread::sleep(duration);
                    stop_spinner(Some(spinner)).ok();
                }
                return Ok(());
            }
        }
        eyre::bail!(
            "Failed to run {:?}",
            args.iter()
                .filter_map(|s| s.as_ref().to_str())
                .collect::<Vec<_>>()
                .join(" ")
        )
    })))
}

fn is_installed(app: Option<impl AsRef<OsStr>>) -> bool {
    match app.and_then(app_path_from_bundle_id) {
        Some(x) => !x.is_empty(),
        None => false,
    }
}

pub fn app_version(app: impl AsRef<OsStr>) -> Option<Version> {
    let app_path = app_path_from_bundle_id(app)?;
    let output = Command::new("defaults")
        .args([
            "read",
            &format!("{app_path}/{BUNDLE_CONTENTS_INFO_PLIST_PATH}"),
            "CFBundleShortVersionString",
        ])
        .output()
        .ok()?;
    let version = String::from_utf8_lossy(&output.stdout);
    Version::parse(version.trim()).ok()
}

const CHECKMARK: &str = "✔";
const DOT: &str = "●";
const CROSS: &str = "✘";

fn print_status_result(name: impl Display, status: &Result<(), DoctorError>, verbose: bool) {
    match status {
        Ok(()) => {
            println!("{} {name}", CHECKMARK.green());
        },
        Err(DoctorError::Warning(msg)) => {
            println!("{} {msg}", DOT.yellow());
        },
        Err(DoctorError::Error {
            reason, info, error, ..
        }) => {
            println!("{} {name}: {reason}", CROSS.red());
            if !info.is_empty() {
                println!();
                for infoline in info {
                    println!("  {infoline}");
                }
            }
            if let Some(error) = error {
                if verbose {
                    println!("  {error:?}");
                }
            }
        },
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
enum DoctorCheckType {
    NormalCheck,
    SoftCheck,
    NoCheck,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(unused)]
enum Platform {
    MacOs,
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Platform::MacOs,
            "linux" => Platform::Linux,
            "windows" => Platform::Windows,
            _ => Platform::Other,
        }
    }
}

#[async_trait]
trait DoctorCheck<T = ()>: Sync
where
    T: Sync + Send + Sized,
{
    fn name(&self) -> Cow<'static, str>;

    async fn get_type(&self, _: &T, _platform: Platform) -> DoctorCheckType {
        DoctorCheckType::NormalCheck
    }

    async fn check(&self, context: &T) -> Result<(), DoctorError>;
}

struct FigBinCheck;

#[async_trait]
impl DoctorCheck for FigBinCheck {
    fn name(&self) -> Cow<'static, str> {
        format!("{PRODUCT_NAME} data dir exists").into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        let path = directories::fig_data_dir().map_err(eyre::Report::from)?;
        Ok(check_file_exists(path)?)
    }
}

macro_rules! path_check {
    ($name:ident, $path:expr) => {
        struct $name;

        #[async_trait]
        impl DoctorCheck for $name {
            fn name(&self) -> Cow<'static, str> {
                concat!("PATH contains ~/", $path).into()
            }

            async fn check(&self, _: &()) -> Result<(), DoctorError> {
                match std::env::var("PATH").map(|path| path.contains($path)) {
                    Ok(true) => Ok(()),
                    _ => return Err(doctor_error!(concat!("Path does not contain ~/", $path))),
                }
            }
        }
    };
}

path_check!(LocalBinPathCheck, ".local/bin");

struct AppRunningCheck;

#[async_trait]
impl DoctorCheck for AppRunningCheck {
    fn name(&self) -> Cow<'static, str> {
        format!("{PRODUCT_NAME} is running").into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        if !desktop_app_running() {
            Err(DoctorError::Error {
                reason: format!("{PRODUCT_NAME} is not running").into(),
                info: vec![],
                fix: command_fix(vec![CLI_BINARY_NAME, "launch"], Duration::from_secs(5)),
                error: None,
            })
        } else {
            Ok(())
        }
    }

    async fn get_type(&self, _: &(), platform: Platform) -> DoctorCheckType {
        if platform == Platform::MacOs {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::NoCheck
        }
    }
}

struct DesktopSocketCheck;

#[async_trait]
impl DoctorCheck for DesktopSocketCheck {
    fn name(&self) -> Cow<'static, str> {
        format!("{PRODUCT_NAME} app socket exists").into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        let fig_socket_path = directories::desktop_socket_path().context("No socket path")?;
        let parent = fig_socket_path.parent().map(PathBuf::from);

        if let Some(parent) = parent {
            if !parent.exists() {
                return Err(DoctorError::Error {
                    reason: format!("{PRODUCT_NAME} socket parent directory does not exist").into(),
                    info: vec![format!("Path: {}", fig_socket_path.display()).into()],
                    fix: Some(DoctorFix::Sync(Box::new(|| {
                        std::fs::create_dir_all(parent)?;
                        Ok(())
                    }))),
                    error: None,
                });
            }
        }

        check_file_exists(&fig_socket_path).map_err(|_err| {
            doctor_fix_async!({
                reason: format!("{PRODUCT_NAME} socket missing"),
                fix: restart_fig()
            })
        })?;

        check_socket_perms(fig_socket_path).await
    }

    async fn get_type(&self, _: &(), platform: Platform) -> DoctorCheckType {
        if platform == Platform::MacOs {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::NoCheck
        }
    }
}

struct RemoteSocketCheck;

#[async_trait]
impl DoctorCheck for RemoteSocketCheck {
    fn name(&self) -> Cow<'static, str> {
        format!("{PRODUCT_NAME} socket exists").into()
    }

    async fn get_type(&self, _: &(), _: Platform) -> DoctorCheckType {
        DoctorCheckType::NormalCheck
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        let q_parent = tab_os_shim::Env::new().q_parent().ok();
        let remote_socket = remote_socket_path().map_err(|err| DoctorError::Error {
            reason: "Unable to get remote socket path".into(),
            info: vec![
                format!("{TAB_PARENT}: {q_parent:?}").into(),
                format!("Error: {err}").into(),
            ],
            fix: None,
            error: Some(err.into()),
        })?;

        // Check file exists
        check_file_exists(&remote_socket).map_err(|err| DoctorError::Error {
            reason: format!("{PRODUCT_NAME} socket missing").into(),
            info: vec![
                format!("Path: {remote_socket:?}").into(),
                format!("{TAB_PARENT}: {q_parent:?}").into(),
                format!("Error: {err}").into(),
            ],
            fix: None,
            error: Some(err),
        })?;

        // Check socket permissions
        check_socket_perms(&remote_socket).await?;

        // Check connecting to socket
        match tokio::net::UnixStream::connect(&remote_socket).await {
            Ok(_) => Ok(()),
            Err(err) => Err(DoctorError::Error {
                reason: "Failed to connect to remote socket".into(),
                info: vec![
                    "Ensure the desktop app is running".into(),
                    format!("Path: {remote_socket:?}").into(),
                    format!("{TAB_PARENT}: {q_parent:?}").into(),
                    format!("Error: {err}").into(),
                ],
                fix: None,
                error: Some(err.into()),
            }),
        }
    }
}

struct SettingsCorruptionCheck;

#[async_trait]
impl DoctorCheck for SettingsCorruptionCheck {
    fn name(&self) -> Cow<'static, str> {
        format!("{PRODUCT_NAME} settings corruption").into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        tab_settings::OldSettings::load().map_err(|_err| DoctorError::Error {
            reason: format!("{PRODUCT_NAME} settings file is corrupted").into(),
            info: vec![],
            fix: Some(DoctorFix::Sync(Box::new(|| {
                std::fs::write(settings_path()?, "{}")?;
                Ok(())
            }))),
            error: None,
        })?;

        Ok(())
    }
}

/// zsh runs without the PTY by default, so TAB_TERM is unset. tabterm runs in the background and
/// the shell reports to it on the link socket. Returns `None` when the PTY check applies, including
/// a PTY shell that inherited the no-PTY variables from a parent zsh.
fn no_pty_link_check(env: &tab_os_shim::Env) -> Option<Result<(), DoctorError>> {
    if env.get_os(TAB_TERM).is_some() || env.get(TABTERM_MODE).as_deref() != Ok("no-pty") {
        return None;
    }

    let session_id = env.get_os(TABTERM_SESSION_ID);
    let link_socket = env.get_os(TABTERM_LINK_SOCKET);
    // Connect instead of checking that the path exists: a killed tabterm leaves its socket file.
    let linked = session_id.is_some()
        && link_socket
            .as_ref()
            .is_some_and(|socket| std::os::unix::net::UnixStream::connect(socket).is_ok());
    if linked {
        return Some(Ok(()));
    }

    Some(Err(DoctorError::Error {
        reason: format!(
            "{PTY_BINARY_NAME} is not running in the background for this terminal, please try restarting your terminal"
        )
        .into(),
        info: vec![
            format!(
                "{TABTERM_SESSION_ID}={}",
                session_id.unwrap_or_default().to_string_lossy()
            )
            .into(),
            format!(
                "{TABTERM_LINK_SOCKET}={}",
                link_socket.unwrap_or_default().to_string_lossy()
            )
            .into(),
        ],
        fix: None,
        error: None,
    }))
}

struct FigIntegrationsCheck;

#[async_trait]
impl DoctorCheck for FigIntegrationsCheck {
    fn name(&self) -> Cow<'static, str> {
        format!("{PRODUCT_NAME} terminal integrations").into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        if let Ok("WarpTerminal") = std::env::var("TERM_PROGRAM").as_deref() {
            return Err(DoctorError::error("WarpTerminal is not supported"));
        }

        if std::env::var_os("__PWSH_LOGIN_CHECKED").is_some() {
            return Err(DoctorError::error("Powershell is not supported"));
        }

        if std::env::var_os("INSIDE_EMACS").is_some() {
            return Err(DoctorError::error("Emacs is not supported"));
        }

        if let Ok("com.vandyke.SecureCRT") = std::env::var("__CFBundleIdentifier").as_deref() {
            return Err(DoctorError::error("SecureCRT is not supported"));
        }

        if std::env::var_os(PROCESS_LAUNCHED_BY_TAB).is_some() {
            return Err(doctor_error!("{PRODUCT_NAME} can not run in a process it launched"));
        }

        if tab_util::system_info::in_ci() {
            return Err(DoctorError::error("Doctor can not run in CI"));
        }

        // Check that ~/.local/bin/tabterm exists
        // TODO(grant): Check figterm exe exists
        // let figterm_path = fig_directories::fig_dir()
        //    .context("Could not find ~/.fig")?
        //    .join("bin")
        //    .join("figterm");

        // if !figterm_path.exists() {
        //    return Err(DoctorError::Error {
        //        reason: "figterm does not exist".into(),
        //        info: vec![],
        //        fix: None,
        //    });
        //}

        if let Some(result) = no_pty_link_check(&tab_os_shim::Env::new()) {
            return result;
        }

        match tab_os_shim::Env::new().q_term().as_deref() {
            Ok(env!("CARGO_PKG_VERSION")) => Ok(()),
            Ok(ver) if env!("CARGO_PKG_VERSION").ends_with("-dev") || ver.ends_with("-dev") => Err(doctor_warning!(
                "{PTY_BINARY_NAME} is running with a different version than {PRODUCT_NAME} CLI, it looks like you are running a development version of {PRODUCT_NAME} however"
            )),
            Ok(_) => Err(DoctorError::Error {
                reason: "This terminal is not running with the latest integration, please restart your terminal".into(),
                info: vec![format!("{TAB_TERM}={}", tab_os_shim::Env::new().q_term().unwrap_or_default()).into()],
                fix: None,
                error: None,
            }),
            Err(_) => Err(DoctorError::Error {
                reason: format!(
                    "{PTY_BINARY_NAME} is not running in this terminal, please try restarting your terminal"
                )
                .into(),
                info: vec![format!("{TAB_TERM}={}", tab_os_shim::Env::new().q_term().unwrap_or_default()).into()],
                fix: None,
                error: None,
            }),
        }
    }
}

struct InlineCheck;

#[async_trait]
impl DoctorCheck for InlineCheck {
    fn name(&self) -> Cow<'static, str> {
        "Inline suggestions".into()
    }

    async fn get_type(&self, _: &(), _: Platform) -> DoctorCheckType {
        let shell = get_shell_context().await;
        let inline_enabled = inline_strategy(Some(&inline_source_setting())).is_some();

        if matches!(shell, Ok(Some(Shell::Zsh))) && inline_enabled {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::NoCheck
        }
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        if tab_os_shim::Env::new().q_using_zsh_autosuggestions() {
            return Err(DoctorError::Warning(
                format!(
                    "zsh-autosuggestions is loaded; {PRODUCT_NAME} inline suggestions are inactive. To fix either:\n\
                     - Remove zsh-autosuggestions from {} and restart your terminal\n\
                     - Set Inline suggestions to off in the dashboard",
                    "~/.zshrc".bold()
                )
                .into(),
            ));
        }

        Ok(())
    }
}

struct PtySocketCheck;

#[async_trait]
impl DoctorCheck for PtySocketCheck {
    fn name(&self) -> Cow<'static, str> {
        "Tabterm Socket Check".into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        // Check that the socket exists
        let term_session = match std::env::var(TABTERM_SESSION_ID) {
            Ok(session) => session,
            Err(_) => {
                return Err(doctor_error!(
                    "Tabterm is not running, please restart your terminal. TABTERM_SESSION_ID is unset."
                ));
            },
        };
        let socket_path = tab_util::directories::figterm_socket_path(term_session).context("No tabterm socket path")?;

        check_socket_perms(&socket_path).await?;

        if let Err(err) = check_file_exists(&socket_path) {
            return Err(DoctorError::Error {
                reason: "Tried to find the socket file, but it wasn't there.".into(),
                info: vec![
                    format!("{PRODUCT_NAME} uses the /tmp directory for sockets.").into(),
                    "Did you delete files in /tmp? The OS will clear it automatically.".into(),
                    format!(
                        "Try making a new tab or window in your terminal, then run {} again.",
                        format!("{CLI_BINARY_NAME} doctor").magenta()
                    )
                    .into(),
                    format!("No file at path: {socket_path:?}").into(),
                ],
                fix: None,
                error: Some(err),
            });
        }

        // Connect to the socket
        let mut conn = match BufferedUnixStream::connect_timeout(&socket_path, Duration::from_secs(2)).await {
            Ok(connection) => connection,
            Err(err) => return Err(doctor_error!("Socket exists but could not connect: {err}")),
        };

        // Try sending an insert event and ensure it inserts what is expected
        enable_raw_mode().with_context(
            || "Your terminal doesn't support raw mode, which is required to verify that the {PTY_BINARY_NAME} socket works",
        )?;

        let test_message = format!("Testing {PTY_BINARY_NAME}...\n");
        let test_message_ = test_message.clone();

        let write_handle: tokio::task::JoinHandle<Result<BufferedUnixStream, DoctorError>> = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs_f32(0.2)).await;

            let message = tab_proto::figterm::FigtermRequestMessage {
                request: Some(tab_proto::figterm::figterm_request_message::Request::InsertText(
                    tab_proto::figterm::InsertTextRequest {
                        insertion: Some(test_message_),
                        deletion: None,
                        offset: None,
                        immediate: Some(true),
                        insertion_buffer: None,
                        insert_during_command: Some(true),
                    },
                )),
            };

            conn.send_message(message).await.map_err(|err| doctor_error!("{err}"))?;

            Ok(conn)
        });

        let mut buffer = String::new();
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());

        let timeout = tokio::time::timeout(Duration::from_secs_f32(1.2), stdin.read_line(&mut buffer));

        let timeout_result: Result<(), DoctorError> = match timeout.await {
            Ok(Ok(_)) => {
                if buffer.trim() == test_message.trim() {
                    Ok(())
                } else {
                    Err(DoctorError::Warning(
                        format!(
                            "{PTY_BINARY_NAME} socket did not read buffer correctly, don't press any keys while the checks are running: {buffer:?}"
                        )
                        .into(),
                    ))
                }
            },
            Ok(Err(err)) => Err(doctor_error!("{PTY_BINARY_NAME} socket err: {}", err)),
            Err(_) => Err(doctor_error!("{PTY_BINARY_NAME} socket write timed out after 1s")),
        };

        disable_raw_mode().context("Failed to disable raw mode")?;

        let mut conn = match write_handle.await {
            Ok(Ok(conn)) => conn,
            Ok(Err(err)) => return Err(doctor_error!("Failed to write to {PTY_BINARY_NAME} socket: {err}")),
            Err(err) => return Err(doctor_error!("Failed to write to {PTY_BINARY_NAME} socket: {err}")),
        };

        timeout_result?;

        // Figterm diagnostics

        let message = tab_proto::figterm::FigtermRequestMessage {
            request: Some(tab_proto::figterm::figterm_request_message::Request::Diagnostics(
                tab_proto::figterm::DiagnosticsRequest {},
            )),
        };

        let response: Result<Option<tab_proto::figterm::FigtermResponseMessage>> = conn
            .send_recv_message_timeout(message, Duration::from_secs(1))
            .await
            .context("Failed to send/recv message");

        match response {
            Ok(Some(figterm_response)) => match figterm_response.response {
                Some(tab_proto::figterm::figterm_response_message::Response::Diagnostics(
                    tab_proto::figterm::DiagnosticsResponse {
                        zsh_autosuggestion_style,
                        fish_suggestion_style,
                        ..
                    },
                )) => {
                    if let Some(style) = zsh_autosuggestion_style {
                        if let Some(fg) = style.fg {
                            if let Some(tab_proto::figterm::term_color::Color::Indexed(i)) = fg.color {
                                if i == 15 {
                                    return Err(doctor_warning!(
                                        "ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE is set to the same style as your text, this must be different to detect what you've typed"
                                    ));
                                }
                            }
                        }
                    }

                    if let Some(style) = fish_suggestion_style {
                        if let Some(fg) = style.fg {
                            if let Some(tab_proto::figterm::term_color::Color::Indexed(i)) = fg.color {
                                if i == 15 {
                                    return Err(doctor_warning!(
                                        "The Fish suggestion color is set to the same style your text, this must be different in order to detect what you've typed"
                                    ));
                                }
                            }
                        }
                    }
                },
                _ => {
                    return Err(doctor_error!(
                        "Failed to receive expected message from {PTY_BINARY_NAME}"
                    ));
                },
            },
            Ok(None) => {
                return Err(doctor_error!(
                    "Received EOF when trying to receive {PTY_BINARY_NAME} diagnostics"
                ));
            },
            Err(err) => return Err(doctor_error!("Failed to receive {PTY_BINARY_NAME} diagnostics: {err}")),
        }

        Ok(())
    }
}

struct DotfileCheck {
    integration: Box<dyn ShellIntegration>,
}

impl DotfileCheck {
    fn short_path(&self) -> String {
        directories::home_dir()
            .ok()
            .and_then(|home_dir| self.integration.path().strip_prefix(&home_dir).ok().map(PathBuf::from))
            .map_or_else(
                || self.integration.path().display().to_string(),
                |path| format!("~/{}", path.display()),
            )
    }
}

#[async_trait]
impl DoctorCheck<Option<Shell>> for DotfileCheck {
    fn name(&self) -> Cow<'static, str> {
        let path = self.short_path();
        let shell = self.integration.get_shell();
        format!("{shell} {path} integration check").into()
    }

    async fn get_type(&self, current_shell: &Option<Shell>, _platform: Platform) -> DoctorCheckType {
        if let Some(shell) = current_shell {
            if *shell == self.integration.get_shell() {
                return DoctorCheckType::NormalCheck;
            }
        }

        if is_executable_in_path(self.integration.get_shell().to_string()) {
            DoctorCheckType::SoftCheck
        } else {
            DoctorCheckType::NoCheck
        }
    }

    async fn check(&self, _: &Option<Shell>) -> Result<(), DoctorError> {
        let fix_text = format!(
            "Run {} to reinstall shell integrations for {}",
            format!("{CLI_BINARY_NAME} integrations install dotfiles").magenta(),
            self.integration.get_shell()
        );
        match self.integration.is_installed().await {
            Ok(()) => Ok(()),
            Err(
                InstallationError::LegacyInstallation(msg)
                | InstallationError::NotInstalled(msg)
                | InstallationError::ImproperInstallation(msg),
            ) => {
                let fix_integration = self.integration.clone();
                Err(DoctorError::Error {
                    reason: msg,
                    info: vec![fix_text.into()],
                    fix: Some(DoctorFix::Async(
                        async move {
                            fix_integration.install().await?;
                            Ok(())
                        }
                        .boxed(),
                    )),
                    error: None,
                })
            },
            Err(err @ InstallationError::FileDoesNotExist(_)) => {
                let fix_integration = self.integration.clone();
                Err(DoctorError::Error {
                    reason: err.to_string().into(),
                    info: vec![fix_text.into()],
                    fix: Some(DoctorFix::Async(
                        async move {
                            fix_integration.install().await?;
                            Ok(())
                        }
                        .boxed(),
                    )),
                    error: Some(eyre::Report::new(err)),
                })
            },
            Err(err @ InstallationError::PermissionDenied { .. }) => {
                let message = err.verbose_message();
                Err(DoctorError::Error {
                    reason: message.title.into(),
                    info: vec![message.message.unwrap().into(), "".into()],
                    fix: None,
                    error: None,
                })
            },
            Err(err) => Err(DoctorError::Error {
                reason: err.to_string().into(),
                info: vec![],
                fix: None,
                error: Some(eyre::Report::new(err)),
            }),
        }
    }
}

pub fn dscl_read(value: impl AsRef<OsStr>) -> Result<String> {
    let username_command = Command::new("id").arg("-un").output().context("Could not get id")?;

    let username: String = String::from_utf8_lossy(&username_command.stdout).trim().into();

    let result = Command::new("dscl")
        .arg(".")
        .arg("-read")
        .arg(format!("/Users/{username}"))
        .arg(value)
        .output()
        .context("Could not read value")?;

    Ok(String::from_utf8_lossy(&result.stdout).trim().into())
}

struct ShellCompatibilityCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for ShellCompatibilityCheck {
    fn name(&self) -> Cow<'static, str> {
        "Compatible shell".into()
    }

    async fn check(&self, _: &DiagnosticsResponse) -> Result<(), DoctorError> {
        let shell_regex = Regex::new(r"(bash|fish|zsh|nu)").unwrap();

        let current_shell = tab_util::get_parent_process_exe();
        let current_shell_valid = current_shell
            .as_ref()
            .map(|path| path.to_string_lossy().to_string())
            .map(|s| {
                let is_match = shell_regex.is_match(&s);
                (s, is_match)
            });

        let default_shell = dscl_read("UserShell");
        let default_shell_valid = default_shell.as_ref().map(|s| (s, shell_regex.is_match(s)));

        match (current_shell_valid, default_shell_valid) {
            (Some((current_shell, false)), _) => {
                return Err(doctor_error!("Current shell {current_shell} incompatible"));
            },
            (_, Ok((default_shell, false))) => {
                return Err(doctor_error!("Default shell {default_shell} incompatible"));
            },
            (None, _) => return Err(doctor_error!("Could not get current shell")),
            (_, Err(_)) => Err(doctor_warning!("Could not get default shell")),
            _ => Ok(()),
        }
    }
}

struct SshIntegrationCheck;

#[async_trait]
impl DoctorCheck<()> for SshIntegrationCheck {
    fn name(&self) -> Cow<'static, str> {
        "SSH integration".into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        match SshIntegration::new() {
            Ok(integration) => match integration.is_installed().await {
                Ok(()) => Ok(()),
                Err(err) => Err(DoctorError::Error {
                    reason: err.to_string().into(),
                    info: vec![],
                    fix: Some(DoctorFix::Async(
                        async move {
                            integration.install().await?;
                            Ok(())
                        }
                        .boxed(),
                    )),
                    error: Some(eyre::Report::new(err)),
                }),
            },
            Err(err) => Err(DoctorError::Error {
                reason: err.to_string().into(),
                info: vec![],
                fix: None,
                error: Some(eyre::Report::new(err)),
            }),
        }
    }
}

struct BundlePathCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for BundlePathCheck {
    fn name(&self) -> Cow<'static, str> {
        "Correct App Installation Location".into()
    }

    async fn check(&self, diagnostics: &DiagnosticsResponse) -> Result<(), DoctorError> {
        let path = diagnostics.path_to_bundle.clone();
        if path.contains(&format!("{}/{APP_BUNDLE_NAME}", system_paths::APPLICATIONS_DIR)) || path.contains(".toolbox")
        {
            Ok(())
        } else if path.contains(&format!("/Build/Products/Debug/{APP_BUNDLE_NAME}")) {
            Err(DoctorError::Warning(
                format!("Running debug build in {}", path.bold()).into(),
            ))
        } else {
            Err(DoctorError::Error {
                reason: format!("App is installed in {}", path.bold()).into(),
                info: vec![
                    format!("You need to install the app into {}.", system_paths::APPLICATIONS_DIR).into(),
                    "To fix: uninstall and reinstall in the correct location.".into(),
                    "Remember to drag the installed app into the Applications folder.".into(),
                ],
                fix: None,
                error: None,
            })
        }
    }
}

struct AutocompleteEnabledCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for AutocompleteEnabledCheck {
    fn name(&self) -> Cow<'static, str> {
        "Autocomplete is enabled".into()
    }

    async fn check(&self, _diagnostics: &DiagnosticsResponse) -> Result<(), DoctorError> {
        if !tab_settings::settings::get_bool_or("autocomplete.disable", false) {
            Ok(())
        } else {
            Err(DoctorError::Error {
                reason: "Autocomplete disabled.".into(),
                info: vec![
                    format!(
                        "To fix run: {}",
                        format!("{CLI_BINARY_NAME} settings autocomplete.disable false").magenta()
                    )
                    .into(),
                ],
                fix: None,
                error: None,
            })
        }
    }
}

macro_rules! dev_mode_check {
    ($struct_name:ident, $check_name:expr, $settings_module:ident, $setting_name:expr) => {
        struct $struct_name;

        #[async_trait]
        impl DoctorCheck for $struct_name {
            fn name(&self) -> Cow<'static, str> {
                $check_name.into()
            }

            async fn check(&self, _: &()) -> Result<(), DoctorError> {
                if let Ok(Some(true)) = tab_settings::$settings_module::get_bool($setting_name) {
                    Err(DoctorError::Warning(concat!($setting_name, " is enabled").into()))
                } else {
                    Ok(())
                }
            }
        }
    };
}

dev_mode_check!(
    AutocompleteDevModeCheck,
    "Autocomplete dev mode",
    settings,
    "autocomplete.developerMode"
);

dev_mode_check!(PluginDevModeCheck, "Plugin dev mode", state, "plugin.developerMode");

struct CliPathCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for CliPathCheck {
    fn name(&self) -> Cow<'static, str> {
        "Valid CLI path".into()
    }

    async fn check(&self, _: &DiagnosticsResponse) -> Result<(), DoctorError> {
        let path = std::env::current_exe().context("Could not get executable path.")?;

        let local_bin_path = directories::home_local_bin().unwrap().join(CLI_BINARY_NAME);

        if path == local_bin_path
            || path == Path::new(system_paths::USR_LOCAL_BIN).join(CLI_BINARY_NAME)
            || path == Path::new(system_paths::OPT_HOMEBREW_BIN).join(CLI_BINARY_NAME)
        {
            Ok(())
        } else if path.ends_with(Path::new("target/debug").join(CLI_BINARY_NAME))
            || path.ends_with(Path::new("target/release").join(CLI_BINARY_NAME))
            || path.ends_with(format!("target/debug/{CLI_CRATE_NAME}"))
            || path.ends_with(format!("target/release/{CLI_CRATE_NAME}"))
        {
            Err(doctor_warning!(
                "Running debug build in a non-standard location: {}",
                path.display().bold()
            ))
        } else {
            Err(doctor_error!(
                "CLI ({}) must be in {}",
                path.display(),
                local_bin_path.display()
            ))
        }
    }
}

struct AccessibilityCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for AccessibilityCheck {
    fn name(&self) -> Cow<'static, str> {
        "Accessibility enabled".into()
    }

    async fn check(&self, diagnostics: &DiagnosticsResponse) -> Result<(), DoctorError> {
        if diagnostics.accessibility != "true" {
            Err(DoctorError::Error {
                reason: "Accessibility is disabled".into(),
                info: vec![],
                fix: command_fix(
                    vec![CLI_BINARY_NAME, "debug", "prompt-accessibility"],
                    Duration::from_secs(1),
                ),
                // fix: Some(DoctorFix::Sync(Box::new(move || {
                //     println!("1. Try enabling accessibility in System Settings");
                //     if !Command::new(CLI_BINARY_NAME)
                //         .args(["debug", "prompt-accessibility"])
                //         .status()?
                //         .success()
                //     {
                //         bail!("Failed to open accessibility in System Settings: {CLI_BINARY_NAME} debug
                // prompt-accessibility");     }

                //     println!("2. Restarting");
                //     println!("3. Reset accessibility");

                //     Ok(())
                // }))),
                error: None,
            })
        } else {
            Ok(())
        }
    }
}

struct DotfilesSymlinkedCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for DotfilesSymlinkedCheck {
    fn name(&self) -> Cow<'static, str> {
        "Dotfiles symlinked".into()
    }

    async fn get_type(&self, diagnostics: &DiagnosticsResponse, _platform: Platform) -> DoctorCheckType {
        if diagnostics.symlinked == "true" {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::NoCheck
        }
    }

    async fn check(&self, _: &DiagnosticsResponse) -> Result<(), DoctorError> {
        Err(DoctorError::Warning(
            "It looks like your dotfiles are symlinked. If you need to make modifications, make sure they're made in \
             the right place."
                .into(),
        ))
    }
}

struct AutocompleteActiveCheck;

#[async_trait]
impl DoctorCheck<DiagnosticsResponse> for AutocompleteActiveCheck {
    fn name(&self) -> Cow<'static, str> {
        "Autocomplete is active".into()
    }

    async fn get_type(&self, diagnostics: &DiagnosticsResponse, _platform: Platform) -> DoctorCheckType {
        if diagnostics.autocomplete_active.is_some() {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::NoCheck
        }
    }

    async fn check(&self, diagnostics: &DiagnosticsResponse) -> Result<(), DoctorError> {
        if diagnostics.autocomplete_active() {
            Ok(())
        } else {
            Err(doctor_error!(
                "Autocomplete is currently inactive. Your desktop integration(s) may be broken!"
            ))
        }
    }
}

struct SupportedTerminalCheckContext {
    ctx: Arc<Context>,
    terminal: Option<Terminal>,
    in_special_terminal: Option<Terminal>,
}

struct SupportedTerminalCheck;

#[async_trait]
impl DoctorCheck<SupportedTerminalCheckContext> for SupportedTerminalCheck {
    fn name(&self) -> Cow<'static, str> {
        "Terminal support".into()
    }

    async fn get_type(&self, _: &SupportedTerminalCheckContext, platform: Platform) -> DoctorCheckType {
        if tab_util::system_info::is_remote() {
            DoctorCheckType::NoCheck
        } else {
            match platform {
                Platform::MacOs => DoctorCheckType::NormalCheck,
                // We can promote this to normal check once we have better terminal detection on other platforms,
                // also we should probably use process tree climbing instead of env vars
                _ => DoctorCheckType::SoftCheck,
            }
        }
    }

    async fn check(&self, context: &SupportedTerminalCheckContext) -> Result<(), DoctorError> {
        if context.terminal.is_none() {
            if let (Os::Linux, Some(pty)) = (context.ctx.platform().os(), &context.in_special_terminal) {
                Err(DoctorError::Warning(
                    format!(
                        "Unable to determine the current terminal. Try running {} outside of {} for a more accurate check.",
                        format!("{CLI_BINARY_NAME} doctor").magenta(),
                        pty,
                    ).into(),
                ))
            } else {
                Err(DoctorError::Error {
                    reason: format!(
                        "Unsupported terminal, if you believe this is a mistake or would like to see support for your terminal, open an issue at {}",
                        format!("https://github.com/{GITHUB_REPO_NAME}/issues").magenta()
                    ).into(),
                    info: vec![
                        format!(
                            "__CFBundleIdentifier: {}",
                            std::env::var("__CFBundleIdentifier").unwrap_or_else(|_| "<not-set>".into())
                        )
                            .into(),
                    ],
                    fix: None,
                    error: None,
                })
            }
        } else {
            Ok(())
        }
    }
}

struct ItermIntegrationCheck;

#[async_trait]
impl DoctorCheck<Option<Terminal>> for ItermIntegrationCheck {
    fn name(&self) -> Cow<'static, str> {
        "iTerm integration is enabled".into()
    }

    async fn get_type(&self, current_terminal: &Option<Terminal>, platform: Platform) -> DoctorCheckType {
        if platform == Platform::MacOs {
            if !is_installed(Terminal::Iterm.to_bundle_id().as_deref()) {
                DoctorCheckType::NoCheck
            } else if matches!(current_terminal.to_owned(), Some(Terminal::Iterm)) {
                DoctorCheckType::NormalCheck
            } else {
                DoctorCheckType::SoftCheck
            }
        } else {
            DoctorCheckType::NoCheck
        }
    }

    async fn check(&self, _: &Option<Terminal>) -> Result<(), DoctorError> {
        if let Some(version) = app_version("com.googlecode.iterm2") {
            if version < Version::new(3, 4, 0) {
                return Err(doctor_error!(
                    "iTerm version is incompatible with {PRODUCT_NAME}. Please update iTerm to latest version"
                ));
            }
        }
        Ok(())
    }
}

struct ItermBashIntegrationCheck;

#[async_trait]
impl DoctorCheck<SupportedTerminalCheckContext> for ItermBashIntegrationCheck {
    fn name(&self) -> Cow<'static, str> {
        "iTerm bash integration configured".into()
    }

    async fn get_type(&self, context: &SupportedTerminalCheckContext, platform: Platform) -> DoctorCheckType {
        if platform == Platform::MacOs {
            if Shell::current_shell() != Some(Shell::Bash) {
                return DoctorCheckType::NoCheck;
            }

            match directories::home_dir() {
                Ok(home) => {
                    if !home.join(".iterm2_shell_integration.bash").exists() {
                        DoctorCheckType::NoCheck
                    } else if matches!(context.terminal, Some(Terminal::Iterm)) {
                        DoctorCheckType::NormalCheck
                    } else {
                        DoctorCheckType::SoftCheck
                    }
                },
                Err(_) => DoctorCheckType::NoCheck,
            }
        } else {
            DoctorCheckType::NoCheck
        }
    }

    async fn check(&self, _: &SupportedTerminalCheckContext) -> Result<(), DoctorError> {
        let integration_file = directories::home_dir().unwrap().join(".iterm2_shell_integration.bash");
        let integration = read_to_string(integration_file).context("Could not read .iterm2_shell_integration.bash")?;

        match Regex::new(r"V(\d*\.\d*\.\d*)").unwrap().captures(&integration) {
            Some(captures) => {
                let version = captures.get(1).unwrap().as_str();
                if Version::new(0, 4, 0) > Version::parse(version).unwrap() {
                    return Err(doctor_error!(
                        "iTerm Bash Integration is out of date. Please update in iTerm's menu by selecting \"Install \
                         Shell Integration\". For more details see https://iterm2.com/documentation-shell-integration.html"
                    ));
                }
                Ok(())
            },
            None => Err(doctor_warning!(
                "iTerm's Bash Integration is installed, but we could not check the version in \
                 ~/.iterm2_shell_integration.bash. Integration may be out of date. You can try updating in iTerm's \
                 menu by selecting \"Install Shell Integration\"",
            )),
        }
    }
}

struct HyperIntegrationCheck;
#[async_trait]
impl DoctorCheck<Option<Terminal>> for HyperIntegrationCheck {
    fn name(&self) -> Cow<'static, str> {
        "Hyper integration is enabled".into()
    }

    async fn get_type(&self, current_terminal: &Option<Terminal>, _platform: Platform) -> DoctorCheckType {
        if !is_installed(Terminal::Hyper.to_bundle_id().as_deref()) {
            return DoctorCheckType::NoCheck;
        }

        if matches!(current_terminal.to_owned(), Some(Terminal::Hyper)) {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::SoftCheck
        }
    }

    async fn check(&self, _: &Option<Terminal>) -> Result<(), DoctorError> {
        let integration = verify_integration("co.zeit.hyper")
            .await
            .context("Could not verify Hyper integration")?;

        if integration != "installed!" {
            // Check ~/.hyper_plugins/local/fig-hyper-integration/index.js exists
            let integration_path = directories::home_dir()
                .context("Could not get home dir")?
                .join(".hyper_plugins/local/fig-hyper-integration/index.js");

            if !integration_path.exists() {
                return Err(doctor_error!("fig-hyper-integration plugin is missing."));
            }

            let config = read_to_string(
                directories::home_dir()
                    .context("Could not get home dir")?
                    .join(".hyper.js"),
            )
            .context("Could not read ~/.hyper.js")?;

            if !config.contains("fig-hyper-integration") {
                return Err(doctor_error!(
                    "fig-hyper-integration plugin needs to be added to localPlugins!"
                ));
            }
            return Err(doctor_error!("Unknown error with Hyper integration"));
        }

        Ok(())
    }
}

struct SystemVersionCheck;

#[async_trait]
impl DoctorCheck for SystemVersionCheck {
    fn name(&self) -> Cow<'static, str> {
        "OS is supported".into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        let os_version = tab_util::system_info::os_version().wrap_err("Could not get OS Version")?;
        match os_version.support_level() {
            SupportLevel::Supported => Ok(()),
            SupportLevel::SupportedWithCaveat { info } => Err(DoctorError::Warning(info)),
            SupportLevel::InDevelopment { info } => Err(DoctorError::Warning(
                format!(
                    "Support for {os_version} is in development. It may not work properly on your system.\n{}",
                    info.unwrap_or_default()
                )
                .into(),
            )),
            SupportLevel::Unsupported => Err(doctor_error!("{os_version} is not supported")),
        }
    }
}

struct VSCodeIntegrationCheck;

#[async_trait]
impl DoctorCheck<Option<Terminal>> for VSCodeIntegrationCheck {
    fn name(&self) -> Cow<'static, str> {
        "VSCode integration is enabled".into()
    }

    async fn get_type(&self, current_terminal: &Option<Terminal>, _platform: Platform) -> DoctorCheckType {
        if !is_installed(Terminal::VSCode.to_bundle_id().as_deref())
            && !is_installed(Terminal::VSCodeInsiders.to_bundle_id().as_deref())
        {
            return DoctorCheckType::NoCheck;
        }

        if matches!(
            current_terminal,
            Some(Terminal::VSCode | Terminal::VSCodeInsiders | Terminal::Cursor | Terminal::CursorNightly)
        ) {
            DoctorCheckType::NormalCheck
        } else {
            DoctorCheckType::SoftCheck
        }
    }

    async fn check(&self, _: &Option<Terminal>) -> Result<(), DoctorError> {
        let integration = verify_integration("com.microsoft.VSCode")
            .await
            .context("Could not verify VSCode integration")?;

        if integration != "installed!" {
            let mut missing = true;

            for dir in [".vscode", ".vscode-insiders", ".cursor", ".cursor-nightly"] {
                // Check if withfig.fig exists
                let extensions = directories::home_dir()
                    .context("Could not get home dir")?
                    .join(dir)
                    .join("extensions");

                let glob_set = glob([extensions.join("withfig.fig-").to_string_lossy()]).unwrap();

                let extensions = extensions.as_path();
                if let Ok(fig_extensions) = glob_dir(&glob_set, extensions) {
                    if fig_extensions.is_empty() {
                        missing = false;
                    }
                }
            }

            if missing {
                return Err(doctor_error!("VSCode integration is missing!"));
            }

            return Err(doctor_error!("Unknown error with VSCode integration!"));
        }
        Ok(())
    }
}

struct ImeStatusCheck;

#[async_trait]
impl DoctorCheck<SupportedTerminalCheckContext> for ImeStatusCheck {
    fn name(&self) -> Cow<'static, str> {
        "Input Method".into()
    }

    async fn get_type(&self, context: &SupportedTerminalCheckContext, _platform: Platform) -> DoctorCheckType {
        match &context.terminal {
            Some(current_terminal) if current_terminal.supports_macos_input_method() => DoctorCheckType::NormalCheck,
            _ => DoctorCheckType::NoCheck,
        }
    }

    async fn check(&self, context: &SupportedTerminalCheckContext) -> Result<(), DoctorError> {
        use macos_utils::applications::running_applications;
        use tab_integrations::Integration;
        use tab_integrations::input_method::InputMethod;

        let input_method = InputMethod::default();
        if let Err(e) = input_method.is_installed().await {
            tab_settings::state::set_value("input-method.enabled", true).ok();

            match e {
                InstallationError::InputMethod(InputMethodError::NotRunning) => {
                    return Err(doctor_fix!({
                            reason: "Input method is not running",
                            fix: move || {
                                input_method.launch();
                                Ok(())
                            }
                    }));
                },
                InstallationError::InputMethod(_) => {
                    return Err(DoctorError::Error {
                        reason: e.to_string().into(),
                        info: vec![
                            format!(
                                "Run {} to enable it",
                                format!("{CLI_BINARY_NAME} integrations install input-method").magenta()
                            )
                            .into(),
                        ],
                        fix: None,
                        error: Some(e.into()),
                    });
                },
                _ => {
                    return Err(DoctorError::Error {
                        reason: "Input Method is not installed".into(),
                        info: vec![
                            format!(
                                "Run {} to enable it",
                                format!("{CLI_BINARY_NAME} integrations install input-method").magenta()
                            )
                            .into(),
                        ],
                        fix: None,
                        error: Some(e.into()),
                    });
                },
            }
        }

        match &context.terminal {
            Some(terminal) if terminal.supports_macos_input_method() => {
                let app = running_applications()
                    .into_iter()
                    .find(|app| app.bundle_identifier.as_deref() == terminal.to_bundle_id().as_deref());

                if let Some(app) = app {
                    if !input_method.enabled_for_terminal_instance(terminal, app.process_identifier) {
                        return Err(DoctorError::Error {
                            reason: format!("Not enabled for {terminal}").into(),
                            info: vec![
                                format!(
                                    "Restart {} [{}] to enable autocomplete in this terminal.",
                                    terminal, app.process_identifier
                                )
                                .into(),
                            ],
                            fix: None,
                            error: None,
                        });
                    }
                }
            },
            _ => (),
        }

        Ok(())
    }
}

struct DashboardHostCheck;

#[async_trait]
impl DoctorCheck for DashboardHostCheck {
    fn name(&self) -> Cow<'static, str> {
        "Dashboard is loading from the correct URL".into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        match tab_settings::settings::get_string("developer.dashboard.host")
            .ok()
            .flatten()
        {
            Some(host) => {
                if host.contains("localhost") {
                    Err(DoctorError::Warning(
                        format!("developer.dashboard.host = {host}, delete this setting if Dashboard fails to load")
                            .into(),
                    ))
                } else {
                    Ok(())
                }
            },
            None => Ok(()),
        }
    }
}

struct AutocompleteHostCheck;

#[async_trait]
impl DoctorCheck for AutocompleteHostCheck {
    fn name(&self) -> Cow<'static, str> {
        "Autocomplete is loading from the correct URL".into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        match tab_settings::settings::get_string("developer.autocomplete.host")
            .ok()
            .flatten()
        {
            Some(host) => {
                if host.contains("localhost") {
                    Err(DoctorError::Warning(
                        format!(
                            "developer.autocomplete.host = {host}, delete this setting if Autocomplete fails to load"
                        )
                        .into(),
                    ))
                } else {
                    Ok(())
                }
            },
            None => Ok(()),
        }
    }
}

struct ToolboxInstalledCheck;

#[async_trait]
impl DoctorCheck for ToolboxInstalledCheck {
    fn name(&self) -> Cow<'static, str> {
        "Jetbrains Toolbox Check".into()
    }

    async fn check(&self, _: &()) -> Result<(), DoctorError> {
        if Terminal::is_jetbrains_terminal()
            && macos_utils::url::path_for_application("com.jetbrains.toolbox").is_some()
        {
            doctor_warning!("apps install through jetbrains toolbox are not supported");
        }

        Ok(())
    }
}

async fn run_checks_with_context<T, Fut>(
    header: impl AsRef<str>,
    checks: Vec<&dyn DoctorCheck<T>>,
    get_context: impl Fn() -> Fut,
    config: CheckConfiguration,
    spinner: &mut Option<Spinner>,
) -> Result<()>
where
    T: Sync + Send,
    Fut: Future<Output = Result<T>>,
{
    if config.all {
        println!("{}", header.as_ref().dark_grey());
    }
    let mut context = match get_context().await {
        Ok(c) => c,
        Err(e) => {
            println!("Failed to get context: {e:?}");
            eyre::bail!(e);
        },
    };
    for check in checks {
        let name = check.name();
        let check_type: DoctorCheckType = check.get_type(&context, Platform::current()).await;

        if check_type == DoctorCheckType::NoCheck {
            continue;
        }

        let mut result = check.check(&context).await;

        if !config.strict && check_type == DoctorCheckType::SoftCheck {
            if let Err(DoctorError::Error { reason, .. }) = result {
                result = Err(DoctorError::Warning(reason));
            }
        }

        if config.all || result.is_err() {
            stop_spinner(spinner.take())?;
            print_status_result(&name, &result, config.all);
        }

        if config.all {
            continue;
        }

        if let Err(DoctorError::Error { reason, fix, error, .. }) = result {
            if let Some(fixfn) = fix {
                println!("Attempting to fix automatically...");
                if let Err(err) = match fixfn {
                    DoctorFix::Sync(fixfn) => fixfn(),
                    DoctorFix::Async(fixfn) => fixfn.await,
                } {
                    println!("Failed to fix: {err}");
                } else {
                    println!("Re-running check...");
                    println!();
                    if let Ok(new_context) = get_context().await {
                        context = new_context;
                    }
                    let fix_result = check.check(&context).await;
                    print_status_result(&name, &fix_result, config.all);
                    match fix_result {
                        Err(DoctorError::Error { .. }) => {},
                        _ => {
                            continue;
                        },
                    }
                }
            }
            println!();
            match error {
                Some(err) => eyre::bail!(err),
                None => eyre::bail!(reason),
            }
        }
    }

    if config.all {
        println!();
    }

    Ok(())
}

async fn get_shell_context() -> Result<Option<Shell>> {
    Ok(Shell::current_shell())
}

async fn get_terminal_context() -> Result<SupportedTerminalCheckContext> {
    let ctx = Context::new();
    let terminal = Terminal::parent_terminal(&Context::new());
    let in_special_terminal = in_special_terminal(&ctx);
    Ok(SupportedTerminalCheckContext {
        ctx,
        terminal,
        in_special_terminal,
    })
}

async fn get_null_context() -> Result<()> {
    Ok(())
}

async fn run_checks(
    header: String,
    checks: Vec<&dyn DoctorCheck>,
    config: CheckConfiguration,
    spinner: &mut Option<Spinner>,
) -> Result<()> {
    run_checks_with_context(header, checks, get_null_context, config, spinner).await
}

fn stop_spinner(spinner: Option<Spinner>) -> Result<()> {
    if let Some(mut sp) = spinner {
        sp.stop();
        execute!(std::io::stdout(), Clear(ClearType::CurrentLine), cursor::Show)?;
        println!();
    }

    Ok(())
}

#[derive(Copy, Clone)]
struct CheckConfiguration {
    all: bool,
    strict: bool,
}

// Doctor
pub async fn doctor_cli(all: bool, strict: bool) -> Result<ExitCode> {
    {
        use nix::unistd::geteuid;
        if geteuid().is_root() {
            eprintln!("{}", "Running doctor as root is not supported.".red().bold());
            if !all {
                eprintln!(
                    "{}",
                    "If you know what you're doing, run the command again with --all.".red()
                );
                return Ok(ExitCode::FAILURE);
            }
        }
    }

    let config = CheckConfiguration { all, strict };

    let mut spinner: Option<Spinner> = None;
    if !config.all {
        spinner = Some(Spinner::new(Spinners::Dots, "Running checks...".into()));
        execute!(std::io::stdout(), cursor::Hide)?;

        ctrlc::set_handler(move || {
            execute!(std::io::stdout(), cursor::Show).ok();
            #[allow(clippy::exit)]
            std::process::exit(1);
        })?;
    }

    // Remove update lock on doctor runs to fix bad state if update crashed.
    if let Ok(update_lock) = tab_util::directories::update_lock_path(&Context::new()) {
        if update_lock.exists() {
            std::fs::remove_file(update_lock).ok();
        }
    }

    // Try to launch the desktop app
    launch_fig_desktop(LaunchArgs {
        verbose: false,
        ..LaunchArgs::default()
    })
    .ok();

    let (integrations, errors) = all_shell_integrations(&Env::new());
    if let Some((_, err)) = errors.into_iter().next() {
        return Err(err.into());
    }
    let shell_integrations: Vec<_> = integrations
        .into_iter()
        .map(|integration| DotfileCheck { integration })
        .collect();

    let mut all_dotfile_checks: Vec<&dyn DoctorCheck<_>> = vec![];
    all_dotfile_checks.extend(shell_integrations.iter().map(|p| p as &dyn DoctorCheck<_>));

    let status: Result<()> = async {
        run_checks_with_context(
            "Let's check your dotfiles...",
            all_dotfile_checks,
            get_shell_context,
            config,
            &mut spinner,
        )
        .await?;

        run_checks(
            format!("Let's make sure {PRODUCT_NAME} is set up correctly..."),
            vec![
                &FigBinCheck,
                &LocalBinPathCheck,
                &SettingsCorruptionCheck,
                &SshdConfigCheck,
                &FigIntegrationsCheck,
                // &SshIntegrationCheck,
            ],
            config,
            &mut spinner,
        )
        .await?;

        if tab_util::manifest::is_full() {
            run_checks(
                "Let's make sure the app is running...".into(),
                vec![&AppRunningCheck, &DesktopSocketCheck],
                config,
                &mut spinner,
            )
            .await?;
        }

        run_checks(
            "Let's see if the app is in a working state...".into(),
            vec![
                &PtySocketCheck,
                &AutocompleteDevModeCheck,
                &PluginDevModeCheck,
                &DashboardHostCheck,
                &AutocompleteHostCheck,
                &InlineCheck,
            ],
            config,
            &mut spinner,
        )
        .await?;

        run_checks(
            "Let's check if your system is compatible...".into(),
            vec![
                &SystemVersionCheck,
                &BashVersionCheck,
                &FishVersionCheck,
                &ToolboxInstalledCheck,
            ],
            config,
            &mut spinner,
        )
        .await
        .ok();

        if tab_util::manifest::is_minimal() {
            return Ok(());
        }

        {
            run_checks_with_context(
                format!("Let's check {}...", format!("{CLI_BINARY_NAME} diagnostic").bold()),
                vec![
                    &ShellCompatibilityCheck,
                    &BundlePathCheck,
                    &AutocompleteEnabledCheck,
                    &CliPathCheck,
                    &AccessibilityCheck,
                    &DotfilesSymlinkedCheck,
                ],
                super::diagnostics::get_diagnostics,
                config,
                &mut spinner,
            )
            .await?;
        }

        run_checks_with_context(
            "Let's check your terminal integrations...",
            vec![
                &SupportedTerminalCheck,
                // &ItermIntegrationCheck,
                &ItermBashIntegrationCheck,
                // TODO: re-enable on macos once IME/terminal integrations are sorted
                // #[cfg(not(target_os = "macos"))]
                // &HyperIntegrationCheck,
                // #[cfg(not(target_os = "macos"))]
                // &VSCodeIntegrationCheck,
                &ImeStatusCheck,
            ],
            get_terminal_context,
            config,
            &mut spinner,
        )
        .await?;

        Ok(())
    }
    .await;

    let is_error = status.is_err();

    stop_spinner(spinner)?;

    if is_error {
        println!();
        println!("{} Doctor found errors. Please fix them and try again.", CROSS.red());
        println!();
        println!(
            "If you are not sure how to fix it, please open an issue at {} to let us know!",
            format!("https://github.com/{GITHUB_REPO_NAME}/issues").magenta()
        );
        println!();
    } else {
        // If early exit is disabled, no errors are thrown
        if !config.all {
            println!("{} Everything looks good!", CHECKMARK.green());
        }
        println!();
        println!(
            "  {PRODUCT_NAME} still not working? Open an issue at {} to let us know!",
            format!("https://github.com/{GITHUB_REPO_NAME}/issues").magenta()
        );
        println!();
    }

    if tab_settings::state::get_bool_or("doctor.prompt-restart-terminal", false) {
        println!(
            "  {}{}",
            "PS. Autocomplete won't work in any existing terminal sessions, ".bold(),
            "only new ones.".bold().italic()
        );
        println!("  (You might want to restart your terminal emulator)");
        tab_settings::state::set_value("doctor.prompt-restart-terminal", false)?;
    }

    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use std::os::unix::net::UnixListener;

    use super::*;

    fn check(vars: &[(&str, &str)]) -> Option<Result<(), DoctorError>> {
        no_pty_link_check(&tab_os_shim::Env::from_slice(vars))
    }

    #[test]
    fn no_pty_link_check_passes_with_a_live_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("link.sock");
        let _listener = UnixListener::bind(&socket).unwrap();
        let socket = socket.to_str().unwrap();

        let result = check(&[
            (TABTERM_MODE, "no-pty"),
            (TABTERM_SESSION_ID, "session"),
            (TABTERM_LINK_SOCKET, socket),
        ]);
        assert!(matches!(result, Some(Ok(()))), "{result:?}");
    }

    #[test]
    fn no_pty_link_check_fails_with_a_stale_socket() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("link.sock");
        drop(UnixListener::bind(&socket).unwrap());
        assert!(socket.exists());
        let socket = socket.to_str().unwrap();

        let result = check(&[
            (TABTERM_MODE, "no-pty"),
            (TABTERM_SESSION_ID, "session"),
            (TABTERM_LINK_SOCKET, socket),
        ]);
        assert!(matches!(result, Some(Err(_))), "{result:?}");
    }

    #[test]
    fn no_pty_link_check_fails_without_a_session_id() {
        let dir = tempfile::tempdir().unwrap();
        let socket = dir.path().join("link.sock");
        let _listener = UnixListener::bind(&socket).unwrap();

        let result = check(&[
            (TABTERM_MODE, "no-pty"),
            (TABTERM_LINK_SOCKET, socket.to_str().unwrap()),
        ]);
        assert!(matches!(result, Some(Err(_))), "{result:?}");
    }

    #[test]
    fn no_pty_link_check_defers_to_the_pty_check() {
        assert!(check(&[]).is_none());
        // A PTY shell started from a no-PTY zsh inherits TABTERM_MODE.
        assert!(
            check(&[
                (TAB_TERM, "1.0.0"),
                (TABTERM_MODE, "no-pty"),
                (TABTERM_SESSION_ID, "session"),
            ])
            .is_none()
        );
    }
}
