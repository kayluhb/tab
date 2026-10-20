//! Runs spec generators for a shell session. The desktop runs them itself, with
//! the environment and working directory the shell last reported, so no process
//! sits beside each shell. Ported from tabterm's `create_command`.

use std::path::{
    Path,
    PathBuf,
};
use std::time::Duration;

use tab_proto::fig::{
    EnvironmentVariable,
    RunProcessResponse,
};
use tab_proto::remote::hostbound::response::Response;
use tab_util::env_var::PROCESS_LAUNCHED_BY_TAB;
use tokio::process::Command;
use tracing::warn;

pub struct ProcessRequest {
    pub executable: String,
    pub arguments: Vec<String>,
    pub working_directory: Option<String>,
    pub env: Vec<EnvironmentVariable>,
    pub timeout: Option<Duration>,
}

/// Keep only absolute `PATH` entries, so relative entries can't resolve against
/// the working directory.
fn absolute_path_entries(path: &str) -> String {
    let entries = std::env::split_paths(path).filter(|entry| entry.is_absolute());
    std::env::join_paths(entries)
        .ok()
        .and_then(|joined| joined.into_string().ok())
        .unwrap_or_default()
}

/// The requested directory if it exists, else the shell's, else `/`
fn working_directory(requested: Option<&str>, shell_cwd: Option<&Path>) -> PathBuf {
    let existing_dir = |path: &Path| path.canonicalize().ok().filter(|path| path.is_dir());
    requested
        .and_then(|path| existing_dir(Path::new(path)))
        .or_else(|| shell_cwd.and_then(existing_dir))
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn create_command(executable: &str, working_directory: &Path, shell_env: &[(String, String)]) -> Command {
    let env: Vec<(String, String)> = shell_env
        .iter()
        .map(|(key, value)| match key.as_str() {
            // The child and its own children search PATH too, so filter it in the env.
            "PATH" => (key.clone(), absolute_path_entries(value)),
            _ => (key.clone(), value.clone()),
        })
        .collect();

    let mut cmd = if Path::new(executable).is_absolute() {
        Command::new(executable)
    } else {
        let path = env
            .iter()
            .find_map(|(key, value)| (key == "PATH").then_some(value.as_str()));
        which::which_in(executable, path, working_directory).map_or_else(|_| Command::new(executable), Command::new)
    };

    cmd.current_dir(working_directory);

    // SAFETY: this closure runs after forking and only affects the child. setsid
    // is async-signal-safe.
    unsafe {
        cmd.pre_exec(|| {
            // No controlling terminal
            nix::unistd::setsid()?;
            Ok(())
        });
    }

    if !env.is_empty() {
        cmd.env_clear();
        cmd.envs(env);
    }

    cmd.env_remove("LS_COLORS");
    cmd.env_remove("CLICOLOR_FORCE");
    cmd.env_remove("CLICOLOR");
    cmd.env_remove("COLORTERM");
    cmd.envs([
        (PROCESS_LAUNCHED_BY_TAB, "1"),
        ("HISTFILE", ""),
        ("HISTCONTROL", "ignoreboth"),
        ("TERM", "xterm-256color"),
        ("NO_COLOR", "1"),
    ]);

    cmd.kill_on_drop(true);
    cmd
}

pub async fn run_process(
    request: ProcessRequest,
    shell_cwd: Option<PathBuf>,
    shell_env: Vec<(String, String)>,
) -> Response {
    let cwd = working_directory(request.working_directory.as_deref(), shell_cwd.as_deref());
    let mut cmd = create_command(&request.executable, &cwd, &shell_env);
    cmd.args(&request.arguments);
    for var in request.env {
        match var.value {
            Some(value) => cmd.env(var.key, value),
            None => cmd.env_remove(var.key),
        };
    }

    let timeout = request.timeout.unwrap_or(Duration::from_secs(60));
    match tokio::time::timeout(timeout, cmd.output()).await {
        Ok(Ok(output)) => Response::RunProcess(RunProcessResponse {
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
            exit_code: output.status.code().unwrap_or(0),
        }),
        Ok(Err(err)) => {
            warn!(%err, executable = request.executable, "failed running executable");
            Response::Error(format!("failed running executable ({}): {err}", request.executable))
        },
        Err(_) => {
            warn!(
                executable = request.executable,
                timeout_ms = timeout.as_millis(),
                "timed out running executable"
            );
            Response::Error(format!(
                "timed out after {}ms running executable ({})",
                timeout.as_millis(),
                request.executable
            ))
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drops_relative_path_entries() {
        assert_eq!(absolute_path_entries("/bin:.:bin:/usr/bin"), "/bin:/usr/bin");
    }

    #[test]
    fn falls_back_through_working_directories() {
        let tmp = std::env::temp_dir().canonicalize().unwrap();
        assert_eq!(working_directory(Some(tmp.to_str().unwrap()), None), tmp);
        assert_eq!(working_directory(Some("/does/not/exist"), Some(&tmp)), tmp);
        assert_eq!(working_directory(None, None), PathBuf::from("/"));
    }

    #[tokio::test]
    async fn runs_with_the_shell_env() {
        let env = vec![
            ("PATH".to_owned(), "/bin:/usr/bin".to_owned()),
            ("TAB_TEST_VAR".to_owned(), "from-shell".to_owned()),
        ];
        let response = run_process(
            ProcessRequest {
                executable: "sh".into(),
                arguments: vec!["-c".into(), "printf %s \"$TAB_TEST_VAR\"".into()],
                working_directory: None,
                env: vec![],
                timeout: None,
            },
            None,
            env,
        )
        .await;
        match response {
            Response::RunProcess(output) => {
                assert_eq!(output.stdout, "from-shell");
                assert_eq!(output.exit_code, 0);
            },
            other => panic!("unexpected {other:?}"),
        }
    }
}
