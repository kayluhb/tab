use std::sync::Arc;

use macos_utils::bundle::get_bundle_path_for_executable;
use semver::Version;
use tab_integrations::Integration;
use tab_integrations::ssh::SshIntegration;
use tab_os_shim::Context;
use tab_util::directories::fig_data_dir;
use tracing::{
    error,
    info,
};

#[allow(unused_imports)]
use crate::utils::is_cargo_debug_build;

const PREVIOUS_VERSION_KEY: &str = "desktop.versionAtPreviousLaunch";

fn run_input_method_migration() {
    use tab_integrations::input_method::InputMethod;
    use tokio::time::{
        Duration,
        sleep,
    };
    use tracing::warn;

    let input_method = InputMethod::default();
    match input_method.target_bundle_path() {
        Ok(target_bundle_path) if target_bundle_path.exists() => {
            tokio::spawn(async move {
                input_method.terminate().ok();
                if let Err(err) = input_method.migrate().await {
                    warn!(%err, "Failed to migrate input method");
                }

                sleep(Duration::from_secs(1)).await;
                input_method.launch();
            });
        },
        Ok(_) => warn!("Input method bundle path does not exist"),
        Err(err) => warn!(%err, "Failed to get input method bundle path"),
    }
}

/// Run items at launch
#[allow(unused_variables)]
pub async fn run_install(ctx: Arc<Context>) {
    if let Err(err) = initialize_fig_dir(&tab_os_shim::Env::new()).await {
        error!(%err, "Failed to initialize data dir");
    }

    // Add any items that are only once per version
    if should_run_install_script() {
        run_input_method_migration();
    }

    if let Err(err) = set_previous_version(current_version()) {
        error!(%err, "Failed to set previous version");
    }

    // install vscode integration
    for variant in tab_integrations::vscode::variants_installed() {
        let integration = tab_integrations::vscode::VSCodeIntegration { variant };
        if integration.is_installed().await.is_err() {
            info!(
                "Attempting to install vscode integration for variant {}",
                integration.variant.application_name
            );
            if let Err(err) = integration.install().await {
                error!(%err, "Failed installing vscode integration for variant {}", integration.variant.application_name);
            }
        }
    }

    // update ssh integration
    if let Ok(ssh_integration) = SshIntegration::new() {
        if let Err(err) = ssh_integration.reinstall().await {
            error!(%err, "Failed updating ssh integration");
        }
    }

    // turn off iTerm's Job title component, which only ever shows tabterm. In the background: the
    // first time, it waits on the user answering macOS's and iTerm's permission prompts.
    tokio::spawn(async {
        tab_integrations::iterm::apply(&tab_settings::State::new(), &tab_integrations::iterm::SystemIterm).await;
    });
}

/// Symlink, and overwrite if it already exists and is invalid or not a symlink
async fn symlink(src: impl AsRef<std::path::Path>, dst: impl AsRef<std::path::Path>) -> Result<(), std::io::Error> {
    use std::io::ErrorKind;

    let src = src.as_ref();
    let dst = dst.as_ref();

    // Check if the link already exists
    match tokio::fs::symlink_metadata(dst).await {
        Ok(metadata) => {
            // If it's a symlink, check if it points to the right place
            if metadata.file_type().is_symlink() {
                if let Ok(read_link) = tokio::fs::read_link(dst).await {
                    if read_link == src {
                        return Ok(());
                    }
                }
            }

            // If it's not a symlink or it points to the wrong place, delete it
            tokio::fs::remove_file(dst).await?;
        },
        Err(err) if err.kind() == ErrorKind::NotFound => {},
        Err(err) => return Err(err),
    }

    // Create the symlink
    tokio::fs::symlink(src, dst).await
}

pub async fn initialize_fig_dir(env: &tab_os_shim::Env) -> anyhow::Result<()> {
    use std::fs;

    use macos_utils::bundle::get_bundle_path;
    use tab_integrations::shell::ShellExt;
    use tab_util::Shell;
    use tab_util::consts::{
        APP_BUNDLE_ID,
        APP_PROCESS_NAME,
        CLI_BINARY_NAME,
        PTY_BINARY_NAME,
    };
    use tab_util::directories::home_dir;
    use tab_util::launchd_plist::{
        LaunchdPlist,
        create_launch_agent,
    };
    use tab_util::macos::BUNDLE_CONTENTS_MACOS_PATH;

    let local_bin = tab_util::directories::home_local_bin()?;
    if let Err(err) = fs::create_dir_all(&local_bin) {
        error!(%err, "Failed to create {local_bin:?}");
    }

    // Install figterm to ~/.local/bin
    match get_bundle_path_for_executable(PTY_BINARY_NAME) {
        Some(pty_path) => {
            let link = local_bin.join(PTY_BINARY_NAME);
            if let Err(err) = symlink(&pty_path, link).await {
                error!(%err, "Failed to symlink for {PTY_BINARY_NAME}: {pty_path:?}");
            }

            for shell in Shell::all() {
                let pty_shell_cpy = local_bin.join(format!("{shell} ({PTY_BINARY_NAME})"));
                let pty_path = pty_path.clone();

                tokio::spawn(async move {
                    // Check version if copy already exists, this is because everytime a copy is made the first start is
                    // kinda slow and we want to avoid that
                    if pty_shell_cpy.exists() {
                        let output = tokio::process::Command::new(&pty_shell_cpy)
                            .arg("--version")
                            .output()
                            .await
                            .ok();

                        let version = output
                            .as_ref()
                            .and_then(|output| std::str::from_utf8(&output.stdout).ok())
                            .map(|s| {
                                match s.strip_prefix(PTY_BINARY_NAME) {
                                    Some(s) => s,
                                    None => s,
                                }
                                .trim()
                            });

                        if version == Some(env!("CARGO_PKG_VERSION")) {
                            return;
                        }
                    }

                    if let Err(err) = tokio::fs::remove_file(&pty_shell_cpy).await {
                        error!(%err, "Failed to remove {PTY_BINARY_NAME} shell {shell:?} copy");
                    }
                    if let Err(err) = tokio::fs::copy(&pty_path, &pty_shell_cpy).await {
                        error!(%err, "Failed to copy {PTY_BINARY_NAME} to {}", pty_shell_cpy.display());
                    }
                });
            }
        },
        None => error!("Failed to find {PTY_BINARY_NAME} in bundle"),
    }

    // install the cli to ~/.local/bin
    match get_bundle_path_for_executable(CLI_BINARY_NAME) {
        Some(q_cli_path) => {
            let dest = local_bin.join(CLI_BINARY_NAME);
            if let Err(err) = symlink(&q_cli_path, dest).await {
                error!(%err, "Failed to symlink {CLI_BINARY_NAME}");
            }
        },
        None => error!("Failed to find {CLI_BINARY_NAME} in bundle"),
    }

    if let Some(bundle_path) = get_bundle_path() {
        let exe = bundle_path.join(BUNDLE_CONTENTS_MACOS_PATH).join(APP_PROCESS_NAME);
        let startup_launch_agent = LaunchdPlist::new(format!("{APP_BUNDLE_ID}.launcher"))
            .program_arguments([&exe.to_string_lossy(), "--is-startup", "--no-dashboard"])
            .associated_bundle_identifiers([APP_BUNDLE_ID])
            .run_at_load(true);

        create_launch_agent(&startup_launch_agent)?;

        let path = startup_launch_agent.get_file_path()?;
        std::process::Command::new("launchctl")
            .arg("load")
            .arg(&path)
            .status()
            .ok();
    }

    if let Ok(home) = home_dir() {
        let iterm_integration_path = home
            .join("Library")
            .join("Application Support")
            .join("iTerm2")
            .join("Scripts")
            .join("AutoLaunch")
            .join("fig-iterm-integration.scpt");

        if iterm_integration_path.exists() {
            std::fs::remove_file(&iterm_integration_path).ok();
        }
    }

    // Init the shell directory
    std::fs::create_dir(fig_data_dir()?.join("shell")).ok();
    for shell in tab_util::Shell::all().iter() {
        for script_integration in shell.get_script_integrations().unwrap_or_default() {
            if let Err(err) = script_integration.install().await {
                error!(%err, "Failed installing shell integration {}", script_integration.describe());
            }
        }

        for shell_integration in shell.get_shell_integrations(env).unwrap_or_default() {
            if let Err(err) = shell_integration.migrate().await {
                error!(%err, "Failed installing shell integration {}", shell_integration.describe());
            }
        }
    }

    Ok(())
}

fn should_run_install_script() -> bool {
    let current_version = current_version();
    let previous_version = match previous_version() {
        Some(previous_version) => previous_version,
        None => return true,
    };

    !is_cargo_debug_build() && current_version > previous_version
}

/// The current version of the desktop app
fn current_version() -> Version {
    Version::parse(env!("CARGO_PKG_VERSION")).unwrap()
}

/// The previous version of the desktop app stored in local state
fn previous_version() -> Option<Version> {
    tab_settings::state::get_string(PREVIOUS_VERSION_KEY)
        .ok()
        .flatten()
        .and_then(|ref v| Version::parse(v).ok())
}

fn set_previous_version(version: Version) -> anyhow::Result<()> {
    tab_settings::state::set_value(PREVIOUS_VERSION_KEY, version.to_string())?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn test_current_version() {
        current_version();
    }

    #[tokio::test]
    async fn test_symlink() {
        use tempfile::tempdir;

        let tmp_dir = tempdir().unwrap();
        let tmp_dir = tmp_dir.path();

        // folders
        let src_dir_1 = tmp_dir.join("dir_1");
        let src_dir_2 = tmp_dir.join("dir_2");
        let dst_dir = tmp_dir.join("dst");

        std::fs::create_dir_all(&src_dir_1).unwrap();
        std::fs::create_dir_all(&src_dir_2).unwrap();

        // Check that a new symlink is created
        assert!(!dst_dir.exists());
        symlink(&src_dir_1, &dst_dir).await.unwrap();
        assert!(dst_dir.exists());
        assert_eq!(dst_dir.read_link().unwrap(), src_dir_1);

        // Check that the symlink is updated
        symlink(&src_dir_2, &dst_dir).await.unwrap();
        assert!(dst_dir.exists());
        assert_eq!(dst_dir.read_link().unwrap(), src_dir_2);

        // files
        let src_file_1 = src_dir_1.join("file_1");
        let src_file_2 = src_dir_2.join("file_2");
        let dst_file = dst_dir.join("file");

        std::fs::write(&src_file_1, "content 1").unwrap();
        std::fs::write(&src_file_2, "content 2").unwrap();

        // Check that a new symlink is created
        assert!(!dst_file.exists());
        symlink(&src_file_1, &dst_file).await.unwrap();
        assert!(dst_file.exists());
        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), "content 1");

        // Check that the symlink is updated
        symlink(&src_file_2, &dst_file).await.unwrap();
        assert!(dst_file.exists());
        assert_eq!(std::fs::read_to_string(&dst_file).unwrap(), "content 2");
    }
}
