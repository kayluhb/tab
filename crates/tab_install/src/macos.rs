use std::ffi::CStr;
use std::path::Path;

use tab_integrations::{
    iterm,
    vscode,
};
use tab_util::consts::APP_BUNDLE_ID;
use tab_util::directories;
use tokio::fs;
use tokio::io::{
    AsyncReadExt,
    AsyncWriteExt,
};
use tracing::{
    error,
    warn,
};

use crate::Error;

async fn remove_in_dir_with_prefix_unless(dir: &Path, prefix: &str, unless: impl Fn(&str) -> bool) {
    if let Ok(mut entries) = fs::read_dir(dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            if let Some(name) = entry.file_name().to_str() {
                if name.starts_with(prefix) && !unless(name) {
                    fs::remove_file(entry.path()).await.ok();
                    fs::remove_dir_all(entry.path()).await.ok();
                }
            }
        }
    }
}

#[allow(unused_variables)]
pub(crate) async fn uninstall_desktop(ctx: &tab_os_shim::Context) -> Result<(), Error> {
    // TODO:
    // 1. Set title of running ttys "Restart this terminal to finish uninstalling Q..."
    // 2. Delete webview cache

    // Remove launch agents
    if let Ok(home) = directories::home_dir() {
        let launch_agents = home.join("Library").join("LaunchAgents");
        remove_in_dir_with_prefix_unless(&launch_agents, &format!("{APP_BUNDLE_ID}."), |p| p.contains("daemon")).await;
    } else {
        warn!("Could not find home directory");
    }

    // Delete Fig defaults on macOS
    tokio::process::Command::new("defaults")
        .args(["delete", APP_BUNDLE_ID])
        .output()
        .await
        .map_err(|err| warn!("Failed to delete defaults: {err}"))
        .ok();

    tokio::process::Command::new("defaults")
        .args(["delete", &format!("{APP_BUNDLE_ID}.shared")])
        .output()
        .await
        .map_err(|err| warn!("Failed to delete defaults: {err}"))
        .ok();

    uninstall_terminal_integrations().await;

    // Delete data dir
    if let Ok(fig_data_dir) = directories::fig_data_dir() {
        fs::remove_dir_all(&fig_data_dir)
            .await
            .map_err(|err| warn!("Failed to remove data dir: {err}"))
            .ok();
    }

    let app_path = tab_util::app_bundle_path();
    if app_path.exists() {
        fs::remove_dir_all(&app_path)
            .await
            .map_err(|err| warn!("Failed to remove {app_path:?}: {err}"))
            .ok();
    }

    Ok(())
}

pub async fn uninstall_terminal_integrations() {
    // Delete integrations
    if let Ok(home) = directories::home_dir() {
        // Delete iTerm integration
        for path in &[
            "Library/Application Support/iTerm2/Scripts/AutoLaunch/fig-iterm-integration.py",
            ".config/iterm2/AppSupport/Scripts/AutoLaunch/fig-iterm-integration.py",
            "Library/Application Support/iTerm2/Scripts/AutoLaunch/fig-iterm-integration.scpt",
        ] {
            fs::remove_file(home.join(path))
                .await
                .map_err(|err| warn!("Could not remove iTerm integration {path}: {err}"))
                .ok();
        }

        // Put back iTerm's Job title component, if Tab turned it off
        if iterm::undo(&tab_settings::State::new(), &iterm::SystemIterm).await == iterm::UndoOutcome::RestartIterm {
            eprintln!("Restart iTerm to bring back the job name in tab titles.");
        }

        // Delete VSCode integration
        for folder in vscode::all_extensions_dirs(&home) {
            for prefix in [vscode::OLD_EXTENSION_PREFIX, vscode::EXTENSION_PREFIX] {
                remove_in_dir_with_prefix_unless(&folder, prefix, |_| false).await;
            }
        }

        // Remove Hyper integration
        let hyper_path = home.join(".hyper.js");
        if hyper_path.exists() {
            // Read the config file
            match fs::File::open(&hyper_path).await {
                Ok(mut file) => {
                    let mut contents = String::new();
                    match file.read_to_string(&mut contents).await {
                        Ok(_) => {
                            contents = contents.replace("\"fig-hyper-integration\",", "");
                            contents = contents.replace("\"fig-hyper-integration\"", "");

                            // Write the config file
                            match fs::File::create(&hyper_path).await {
                                Ok(mut file) => {
                                    file.write_all(contents.as_bytes())
                                        .await
                                        .map_err(|err| warn!("Could not write to Hyper config: {err}"))
                                        .ok();
                                },
                                Err(err) => {
                                    warn!("Could not create Hyper config: {err}");
                                },
                            }
                        },
                        Err(err) => {
                            warn!("Could not read Hyper config: {err}");
                        },
                    }
                },
                Err(err) => {
                    warn!("Could not open Hyper config: {err}");
                },
            }
        }

        // Remove Kitty integration
        let kitty_path = home.join(".config").join("kitty").join("kitty.conf");
        if kitty_path.exists() {
            // Read the config file
            match fs::File::open(&kitty_path).await {
                Ok(mut file) => {
                    let mut contents = String::new();
                    match file.read_to_string(&mut contents).await {
                        Ok(_) => {
                            contents = contents.replace("watcher ${HOME}/.fig/tools/kitty-integration.py", "");
                            // Write the config file
                            match fs::File::create(&kitty_path).await {
                                Ok(mut file) => {
                                    file.write_all(contents.as_bytes())
                                        .await
                                        .map_err(|err| warn!("Could not write to Kitty config: {err}"))
                                        .ok();
                                },
                                Err(err) => {
                                    warn!("Could not create Kitty config: {err}");
                                },
                            }
                        },
                        Err(err) => {
                            warn!("Could not read Kitty config: {err}");
                        },
                    }
                },
                Err(err) => {
                    warn!("Could not open Kitty config: {err}");
                },
            }
        }
        // TODO: Add Jetbrains integration
    }
}

pub fn install(src: impl AsRef<CStr>, dst: impl AsRef<CStr>, same_bundle_name: bool) -> Result<(), Error> {
    // We want to swap the app bundles, like sparkle does
    // https://github.com/sparkle-project/Sparkle/blob/863f85b5f5398c03553f2544668b95816b2860db/Sparkle/SUFileManager.m#L235
    let status = unsafe {
        libc::renamex_np(
            src.as_ref().as_ptr(),
            dst.as_ref().as_ptr(),
            if same_bundle_name { libc::RENAME_SWAP } else { 0 },
        )
    };

    if status != 0 {
        let err = std::io::Error::last_os_error();

        error!(%err, "failed to swap app bundle");

        if matches!(err.kind(), std::io::ErrorKind::PermissionDenied) {
            return Err(Error::UpdateFailed(
                "Failed to swap app bundle due to permission denied. Try restarting the app.".into(),
            ));
        } else {
            return Err(Error::UpdateFailed(format!("Failed to swap app bundle: {err}")));
        }
    }

    Ok(())
}
